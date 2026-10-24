//! Explicit Copy text requests from exact, still-shown immutable message rows.
//!
//! Capabilities contain weak model/row references, never message text. Slint's
//! existing platform handles clipboard serving; this module adds no application worker and
//! reports only a request, because the pinned Platform setter has no result.

use crate::{AppWindow, MessageItem, live::LiveUi};
use morpheus_matrix::{AccountSelection, MessageInfo, MessageSendState};
use slint::{ComponentHandle, Model};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::{Rc, Weak},
    sync::{Arc, Weak as SyncWeak, atomic::Ordering},
};

const MAX_CAPABILITIES: usize = 512;
const MAX_TEXT_BYTES: usize = 64 * 1024;

type MessageModel = dyn Model<Data = MessageItem>;

pub(crate) struct CopyScope {
    live: SyncWeak<LiveUi>,
    selection: AccountSelection,
    room: String,
    thread: bool,
    root: String,
    view: crate::audio_attachments::ViewLease,
    enabled: bool,
    withdrawal: Rc<Cell<u64>>,
    withdrawal_at: u64,
}
impl CopyScope {
    fn target_holds(&self, ui: &AppWindow) -> bool {
        let Some(live) = self.live.upgrade() else {
            return false;
        };
        self.selection.is_current()
            && !live.action_admission_closed()
            && self.view.live.load(Ordering::Acquire)
            && self.view.navigation.load(Ordering::Acquire) == self.view.navigation_at
            && self.view.generation.load(Ordering::Acquire) == self.view.generation_at
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == self.selection.account_id()
            && ui.get_active_room_id().as_str() == self.room
            && (!self.thread
                || (ui.get_thread_root_id().as_str() == self.root
                    && ui.get_show_side_panel()
                    && ui.get_side_panel_tab() == 1))
    }
    fn holds(&self, ui: &AppWindow) -> bool {
        self.enabled
            && self.withdrawal_at != u64::MAX
            && self.withdrawal.get() == self.withdrawal_at
            && ui_allowed(ui)
            && self.target_holds(ui)
            && self.view.gate.ringing_allowed(self.view.security_at)
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    model: usize,
    row: usize,
    index: usize,
    scope: usize,
}
struct Entry {
    key: Key,
    model: Option<Weak<MessageModel>>,
    row: SyncWeak<MessageInfo>,
    scope: Weak<CopyScope>,
}
#[derive(Default)]
struct Book {
    next: u64,
    entries: HashMap<u64, Entry>,
    keys: HashMap<Key, u64>,
    order: VecDeque<u64>,
}
impl Book {
    fn insert(&mut self, entry: Entry) -> String {
        if let Some(id) = self.keys.get(&entry.key) {
            return id.to_string();
        }
        let Some(id) = self.next.checked_add(1) else {
            return String::new();
        };
        self.next = id;
        if self.entries.len() >= MAX_CAPABILITIES {
            if let Some(old) = self.order.pop_front() {
                if let Some(entry) = self.entries.remove(&old) {
                    self.keys.remove(&entry.key);
                }
            }
        }
        self.keys.insert(entry.key, id);
        self.entries.insert(id, entry);
        self.order.push_back(id);
        id.to_string()
    }
    fn clear(&mut self) {
        // Never reset the counter: an old delegate cannot regain authority.
        self.entries.clear();
        self.keys.clear();
        self.order.clear();
    }
}
struct RootRow {
    row: SyncWeak<MessageInfo>,
    scope: Rc<CopyScope>,
}
struct Owner {
    ui: slint::Weak<AppWindow>,
    live: SyncWeak<LiveUi>,
    book: Book,
    root: Option<RootRow>,
    withdrawal: Rc<Cell<u64>>,
}
thread_local! { static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) }; }

fn ui_allowed(ui: &AppWindow) -> bool {
    ui.get_viewport_seek_allowed() && !ui.get_settings_reveal_protected()
}

/// Capture the caller's ORIGINAL account capability before its existing
/// publication guards run. Callers pass this value through only if those
/// original account/feed/navigation/visible-target guards all succeed.
/// No same-ID account lookup is permitted here or in row_data.
pub(crate) fn capture_publication(
    ui: &AppWindow,
    selection: &AccountSelection,
    thread: bool,
) -> Option<Rc<CopyScope>> {
    OWNER.with(|owner| {
        let owner = owner.borrow();
        let owner = owner.as_ref()?;
        let live = owner.live.upgrade()?;
        if !ui.get_live_session() || live.action_admission_closed() {
            return None;
        }
        let room = ui.get_active_room_id().to_string();
        if room.is_empty() || room.len() > 4096 {
            return None;
        }
        let scope = Rc::new(CopyScope {
            live: Arc::downgrade(&live),
            selection: selection.clone(),
            room,
            thread,
            root: if thread {
                ui.get_thread_root_id().to_string()
            } else {
                String::new()
            },
            view: live.attachment_audio_lease(thread),
            enabled: ui_allowed(ui),
            withdrawal: owner.withdrawal.clone(),
            withdrawal_at: owner.withdrawal.get(),
        });
        scope.target_holds(ui).then_some(scope)
    })
}

/// Reuse metadata only for the same LiveUi counters/target and still-current
/// original selection. In one LiveUi manager only one selection epoch is
/// current; capture_publication receives that manager's caller capability.
pub(crate) fn same_scope(a: &Rc<CopyScope>, b: &Rc<CopyScope>) -> bool {
    SyncWeak::ptr_eq(&a.live, &b.live)
        && a.selection.is_current()
        && b.selection.is_current()
        && a.selection.account_id() == b.selection.account_id()
        && a.room == b.room
        && a.thread == b.thread
        && a.root == b.root
        && a.enabled == b.enabled
        && Rc::ptr_eq(&a.withdrawal, &b.withdrawal)
        && a.withdrawal_at == b.withdrawal_at
        && Arc::ptr_eq(&a.view.navigation, &b.view.navigation)
        && Arc::ptr_eq(&a.view.generation, &b.view.generation)
        && Arc::ptr_eq(&a.view.gate, &b.view.gate)
        && a.view.navigation_at == b.view.navigation_at
        && a.view.generation_at == b.view.generation_at
        && a.view.security_at == b.view.security_at
}

/// A UI context change may renew only the SAME original target/selection.
/// It cannot bind retained old rows to a newly selected account or navigation.
pub(crate) fn context_scope(old: Option<&Rc<CopyScope>>) -> Option<Rc<CopyScope>> {
    let old = old?;
    OWNER.with(|owner| {
        let owner = owner.borrow();
        let owner = owner.as_ref()?;
        let ui = owner.ui.upgrade()?;
        if !old.target_holds(&ui) {
            return None;
        }
        let live = old.live.upgrade()?;
        if old.enabled == ui_allowed(&ui)
            && old.view.security_at == live.security_gate.ringing_generation()
            && old.withdrawal_at == old.withdrawal.get()
        {
            return Some(old.clone());
        }
        let next = Rc::new(CopyScope {
            live: old.live.clone(),
            selection: old.selection.clone(),
            room: old.room.clone(),
            thread: old.thread,
            root: old.root.clone(),
            view: live.attachment_audio_lease(old.thread),
            enabled: ui_allowed(&ui),
            withdrawal: old.withdrawal.clone(),
            withdrawal_at: old.withdrawal.get(),
        });
        // A navigation may change on another thread during capture. The
        // original monotonic target must still hold after taking the new
        // security/view snapshot; otherwise no future era is rebound.
        old.target_holds(&ui).then_some(next)
    })
}

fn eligible(info: &MessageInfo) -> bool {
    info.event_id.as_deref().is_some_and(|id| !id.is_empty())
        && !info.is_local_echo
        && info.send_state == MessageSendState::Sent
        && info.attachment_name.is_none()
        && info.poll.is_none()
        && info.body.len() <= MAX_TEXT_BYTES
        && info
            .rich_markdown
            .as_ref()
            .is_none_or(|s| s.len() <= MAX_TEXT_BYTES)
        && info
            .rich_accessible
            .as_ref()
            .is_none_or(|s| s.len() <= MAX_TEXT_BYTES)
}
fn accepted_text(info: &MessageInfo) -> Option<String> {
    if !eligible(info) {
        return None;
    }
    let item = crate::live::message_item(info);
    let text = item.rich_accessible_body.as_str();
    (!text.is_empty() && text.len() <= MAX_TEXT_BYTES && !text.contains('\0'))
        .then(|| text.to_owned())
}

/// Cheap projection: bounded metadata, weak references and a deduplicated key.
/// No account lookup, formatting, IO, observer or clipboard operation.
pub(crate) fn row_token(
    model: Weak<MessageModel>,
    scope: Option<&Rc<CopyScope>>,
    index: usize,
    row: &Arc<MessageInfo>,
) -> String {
    let Some(scope) = scope else {
        return String::new();
    };
    if !scope.enabled || !eligible(row) {
        return String::new();
    }
    let key = Key {
        model: model.as_ptr() as *const () as usize,
        row: Arc::as_ptr(row) as usize,
        index,
        scope: Rc::as_ptr(scope) as usize,
    };
    OWNER.with(|owner| {
        owner
            .borrow_mut()
            .as_mut()
            .map_or_else(String::new, |owner| {
                owner.book.insert(Entry {
                    key,
                    model: Some(model),
                    row: Arc::downgrade(row),
                    scope: Rc::downgrade(scope),
                })
            })
    })
}

/// Withdraw on account/room/private-presentation retirement. Existing OS
/// clipboard ownership is deliberately untouched: only explicit Copy writes.
pub(crate) fn retire(ui: &AppWindow) {
    OWNER.with(|owner| {
        if let Some(owner) = owner.borrow_mut().as_mut() {
            owner.book.clear();
            owner.root = None;
        }
    });
    ui.set_message_copy_notice("".into());
    ui.set_thread_root_copy_token("".into());
}

fn root_matches(ui: &AppWindow, row: &MessageInfo) -> bool {
    let item = crate::live::message_item(row);
    row.event_id.as_deref() == Some(ui.get_thread_root_id().as_str())
        && ui.get_thread_root_sender() == item.sender
        && ui.get_thread_root_body() == item.body
        && ui.get_thread_root_rich_accessible_body() == item.rich_accessible_body
}

/// A root's exact accepted source from the already-admitted thread feed.
/// Keep only a weak row; no extra root plaintext survives feed withdrawal.
pub(crate) fn publish_thread_root(
    ui: &AppWindow,
    row: Option<&Arc<MessageInfo>>,
    scope: Option<Rc<CopyScope>>,
) {
    ui.set_thread_root_copy_token("".into());
    let row = row.filter(|row| eligible(row) && root_matches(ui, row));
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        let Some(owner) = owner.as_mut() else {
            return;
        };
        owner.root = None;
        let (Some(row), Some(scope)) = (row, scope) else {
            return;
        };
        if !scope.thread {
            return;
        }
        let key = Key {
            model: 0,
            row: Arc::as_ptr(row) as usize,
            index: 0,
            scope: Rc::as_ptr(&scope) as usize,
        };
        // A new source publication replaces the root capability, including
        // same-ID edits. Remove any old dedup key before minting its successor.
        if let Some(old) = owner.book.keys.remove(&key) {
            owner.book.entries.remove(&old);
            owner.book.order.retain(|id| *id != old);
        }
        let token = owner.book.insert(Entry {
            key,
            model: None,
            row: Arc::downgrade(row),
            scope: Rc::downgrade(&scope),
        });
        owner.root = Some(RootRow {
            row: Arc::downgrade(row),
            scope,
        });
        ui.set_thread_root_copy_token(token.into());
    });
}

/// Transfer the clicked row's exact original selection/source into the new
/// thread generation, synchronously after begin_thread_request. No ID lookup
/// can reconstruct a missing or stale row capability.
pub(crate) fn thread_opened(ui: &AppWindow) {
    let Ok(id) = ui.get_thread_root_copy_token().as_str().parse::<u64>() else {
        return;
    };
    let bound = OWNER.with(|owner| {
        let owner = owner.borrow();
        let owner = owner.as_ref()?;
        let entry = owner.book.entries.get(&id)?;
        let old = entry.scope.upgrade()?;
        if !old.holds(ui) || old.thread {
            return None;
        }
        let row = entry.row.upgrade()?;
        let model = entry.model.as_ref()?.upgrade()?;
        let shown = ui.get_messages();
        let shown_rc: Rc<MessageModel> = shown.clone().try_into().ok()?;
        if !Rc::ptr_eq(&model, &shown_rc)
            || !crate::timeline_model::has_exact_row(&shown, entry.key.index, &row)
            || !root_matches(ui, &row)
        {
            return None;
        }
        let live = old.live.upgrade()?;
        let scope = Rc::new(CopyScope {
            live: old.live.clone(),
            selection: old.selection.clone(),
            room: old.room.clone(),
            thread: true,
            root: ui.get_thread_root_id().to_string(),
            view: live.attachment_audio_lease(true),
            enabled: ui_allowed(ui),
            withdrawal: old.withdrawal.clone(),
            withdrawal_at: old.withdrawal_at,
        });
        (old.holds(ui) && scope.holds(ui)).then_some((scope, row))
    });
    let Some((scope, row)) = bound else {
        ui.set_thread_root_copy_token("".into());
        return;
    };
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        let Some(owner) = owner.as_mut() else {
            return;
        };
        let token = owner.book.insert(Entry {
            key: Key {
                model: 0,
                row: Arc::as_ptr(&row) as usize,
                index: 0,
                scope: Rc::as_ptr(&scope) as usize,
            },
            model: None,
            row: Arc::downgrade(&row),
            scope: Rc::downgrade(&scope),
        });
        owner.root = Some(RootRow {
            row: Arc::downgrade(&row),
            scope,
        });
        ui.set_thread_root_copy_token(token.into());
    });
}

/// Canonical explicit protection/navigation entries call this BEFORE their
/// visible changes. Deferred changed handlers alone can miss one-batch roundtrips.
pub(crate) fn withdraw(ui: &AppWindow) {
    OWNER.with(|owner| {
        if let Some(owner) = owner.borrow().as_ref() {
            owner
                .withdrawal
                .set(owner.withdrawal.get().saturating_add(1));
        }
    });
    context_changed(ui);
}

fn context_changed(ui: &AppWindow) {
    // Renew only an already-bound exact target, never a fresh account lookup.
    ui.set_message_copy_notice("".into());
    crate::timeline_model::refresh_copy_context(&ui.get_messages());
    crate::timeline_model::refresh_copy_context(&ui.get_thread_messages());
    let root = OWNER.with(|owner| {
        owner
            .borrow()
            .as_ref()?
            .root
            .as_ref()
            .map(|root| (root.scope.clone(), root.row.clone()))
    });
    if let Some((old, row)) = root {
        let next = context_scope(Some(&old));
        let row = row.upgrade().filter(|row| root_matches(ui, row));
        match (next, row) {
            (Some(next), Some(row)) if !Rc::ptr_eq(&old, &next) => {
                OWNER.with(|owner| {
                    let mut owner = owner.borrow_mut();
                    let Some(owner) = owner.as_mut() else {
                        return;
                    };
                    let token = if next.enabled {
                        owner.book.insert(Entry {
                            key: Key {
                                model: 0,
                                row: Arc::as_ptr(&row) as usize,
                                index: 0,
                                scope: Rc::as_ptr(&next) as usize,
                            },
                            model: None,
                            row: Arc::downgrade(&row),
                            scope: Rc::downgrade(&next),
                        })
                    } else {
                        String::new()
                    };
                    owner.root = Some(RootRow {
                        row: Arc::downgrade(&row),
                        scope: next,
                    });
                    ui.set_thread_root_copy_token(token.into());
                });
            }
            (Some(_), Some(_)) => {}
            _ => {
                OWNER.with(|owner| {
                    if let Some(owner) = owner.borrow_mut().as_mut() {
                        owner.root = None;
                    }
                });
                ui.set_thread_root_copy_token("".into());
            }
        }
    }
}

fn resolve(ui: &AppWindow, token: &str, thread_root: bool) -> Option<String> {
    if !ui_allowed(ui) {
        return None;
    }
    if token.is_empty() || token.len() > 20 || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: u64 = token.parse().ok()?;
    OWNER.with(|owner| {
        let owner = owner.borrow();
        let entry = owner.as_ref()?.book.entries.get(&id)?;
        let scope = entry.scope.upgrade()?;
        if !scope.holds(ui) {
            return None;
        }
        let row = entry.row.upgrade()?;
        if thread_root && !root_matches(ui, &row) {
            return None;
        }
        if entry.model.is_none() {
            let root = owner.as_ref()?.root.as_ref()?;
            if !Rc::ptr_eq(&root.scope, &scope)
                || !SyncWeak::ptr_eq(&root.row, &entry.row)
                || !root_matches(ui, &row)
            {
                return None;
            }
            return accepted_text(&row);
        }
        let model = entry.model.as_ref()?.upgrade()?;
        let shown = if scope.thread {
            ui.get_thread_messages()
        } else {
            ui.get_messages()
        };
        let shown_rc: Rc<MessageModel> = shown.clone().try_into().ok()?;
        if !Rc::ptr_eq(&model, &shown_rc) {
            return None;
        }
        if !crate::timeline_model::has_exact_row(&shown, entry.key.index, &row) {
            return None;
        }
        accepted_text(&row)
    })
}

fn request_clipboard(ui: &AppWindow, text: &str) {
    #[cfg(test)]
    if RECORDING.with(|recording| {
        if let Some(recording) = recording.borrow().as_ref() {
            recording.requests.set(recording.requests.get() + 1);
            *recording.last.borrow_mut() = Some(text.to_owned());
            true
        } else {
            false
        }
    }) {
        return;
    }
    slint::private_unstable_api::re_exports::WindowInner::from_pub(ui.window())
        .context()
        .platform()
        .set_clipboard_text(text, slint::platform::Clipboard::DefaultClipboard);
}

#[cfg(test)]
struct RecordedRequests {
    requests: Cell<usize>,
    last: RefCell<Option<String>>,
}
#[cfg(test)]
thread_local! { static RECORDING: RefCell<Option<Rc<RecordedRequests>>> = const { RefCell::new(None) }; }

/// Records the exact registered callback's final handoff, not a second policy
/// implementation. Native production Clipboard route is unchanged. No reads.
#[cfg(test)]
pub(crate) struct RecordingClipboard(Rc<RecordedRequests>);
#[cfg(test)]
impl RecordingClipboard {
    pub(crate) fn start() -> Self {
        let recording = Rc::new(RecordedRequests {
            requests: Cell::new(0),
            last: RefCell::new(None),
        });
        RECORDING.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "only one synthetic clipboard recorder");
            *slot = Some(recording.clone());
        });
        Self(recording)
    }
    pub(crate) fn requests(&self) -> usize {
        self.0.requests.get()
    }
    pub(crate) fn last_matches(&self, expected: &str) -> bool {
        self.0.last.borrow().as_deref() == Some(expected)
    }
}
#[cfg(test)]
impl Drop for RecordingClipboard {
    fn drop(&mut self) {
        RECORDING.with(|slot| *slot.borrow_mut() = None);
    }
}

pub(crate) fn register(ui: &AppWindow, live: &Arc<LiveUi>) {
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        // Even a replacement controller must not reuse a queued old serial.
        let next = owner.as_ref().map_or(0, |old| old.book.next);
        *owner = Some(Owner {
            ui: ui.as_weak(),
            live: Arc::downgrade(live),
            book: Book {
                next,
                ..Book::default()
            },
            root: None,
            withdrawal: Rc::new(Cell::new(0)),
        });
    });
    let weak = ui.as_weak();
    ui.on_copy_message_text(move |token, thread_root| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Some(text) = resolve(&ui, token.as_str(), thread_root) else {
            ui.set_message_copy_notice(
                "Copy is unavailable for this displayed row. Reopen the conversation or thread."
                    .into(),
            );
            return;
        };
        // Actual UI-thread handoff after every authority check. X11 keeps the
        // existing backend ClipboardPair/server. Wayland uses the existing
        // library background selection server (no exposed join receipt).
        // Platform has no success result: neither copy nor cleanup is confirmed.
        request_clipboard(&ui, &text);
        ui.set_message_copy_notice("Copy requested.".into());
    });
    let weak = ui.as_weak();
    ui.on_message_copy_context_changed(move || {
        if let Some(ui) = weak.upgrade() {
            context_changed(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_message_copy_withdraw(move || {
        if let Some(ui) = weak.upgrade() {
            withdraw(&ui);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::ModelRc;

    #[test]
    fn copy_uses_only_the_accepted_displayed_plain_projection_and_bounds_its_bytes() {
        let mut row = crate::timeline_model::tests::fixture(1);
        row.body = "Unrelated Matrix fallback".into();
        row.rich_markdown = Some("Visible **accepted** text".into());
        row.rich_accessible = Some("Visible accepted text".into());
        assert_eq!(
            accepted_text(&row).as_deref(),
            Some("Visible accepted text")
        );
        row.rich_markdown = None;
        assert_eq!(
            accepted_text(&row).as_deref(),
            Some("Unrelated Matrix fallback")
        );
        row.body = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(accepted_text(&row).is_none());
        row.body = "Visible\0text".into();
        assert!(accepted_text(&row).is_none());
        row.body = "Attachment caption".into();
        row.attachment_name = Some("private.keys".into());
        assert!(accepted_text(&row).is_none());
        row.attachment_name = None;
        row.is_local_echo = true;
        assert!(accepted_text(&row).is_none());
    }

    #[test]
    fn bounded_weak_capabilities_deduplicate_and_retirement_never_reuses_an_old_serial() {
        let mut book = Book::default();
        let row = Arc::new(crate::timeline_model::tests::fixture(1));
        let model = crate::timeline_model::update_shared(ModelRc::default(), vec![row.clone()]);
        let model_rc: Rc<MessageModel> = model.clone().try_into().ok().unwrap();
        let make = |index| Entry {
            key: Key {
                model: 1,
                row: Arc::as_ptr(&row) as usize,
                index,
                scope: 1,
            },
            model: Some(Rc::downgrade(&model_rc)),
            row: Arc::downgrade(&row),
            scope: Weak::new(),
        };
        let first = book.insert(make(0));
        assert_eq!(book.insert(make(0)), first);
        for index in 1..=MAX_CAPABILITIES {
            book.insert(make(index));
        }
        assert_eq!(book.entries.len(), MAX_CAPABILITIES);
        assert_eq!(book.keys.len(), MAX_CAPABILITIES);
        assert_eq!(book.order.len(), MAX_CAPABILITIES);
        assert!(!book.entries.contains_key(&first.parse::<u64>().unwrap()));
        let weak_row = Arc::downgrade(&row);
        // Capabilities hold neither model rows nor plaintext after the actual
        // displayed model/source owners go away.
        drop(model_rc);
        drop(model);
        drop(row);
        assert!(weak_row.upgrade().is_none());
        book.clear();
        let replacement = book.insert(Entry {
            key: Key {
                model: 1,
                row: 1,
                index: 0,
                scope: 1,
            },
            model: None,
            row: SyncWeak::new(),
            scope: Weak::new(),
        });
        assert_ne!(first, replacement);
        assert_eq!(book.entries.len(), 1);
    }
}
