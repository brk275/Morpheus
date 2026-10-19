//! Opening a `NavigationTarget`: the exact saved account, joined room and,
//! for a thread, its thread panel.
//!
//! A target in the visible account opens directly. A target in another
//! account, or while no account is open, first goes through the same entry
//! points as the account menu and Manage accounts, so the security gate, the
//! leave-call confirmation and the local app password all still apply; the
//! target then waits as one pending request. It opens only when that exact
//! account becomes active while the request is still current: a newer
//! activation, a switch to any other account, the user's own navigation or
//! two minutes without the switch completing all end it. Every later step is
//! fenced by the account's selection and the live navigation epoch, so an
//! A → B → A sequence never lets an earlier request navigate.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_matrix::{AccountInfo, AccountSelection, MatrixCoreError, MessageInfo};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use tokio::{runtime::Handle, sync::mpsc, time::MissedTickBehavior};

use crate::{AppWindow, MessageItem, live::LiveUi, navigation_target::NavigationTarget};

/// How long a target waits for its account's protected switch, including
/// the app-password prompt and the leave-call confirmation.
const PENDING_TTL: Duration = Duration::from_secs(120);
const ACCOUNT_GONE: &str = "The account for this notification is no longer saved.";
const ROOM_GONE: &str = "The room for this notification is no longer joined.";
const REPLY_OPEN: &str = "Send or cancel the reply or edit you are writing first.";
const INVITE_OPEN: &str = "Accept or decline this invitation in the room list.";
const NOT_OPENED: &str = "This notification could not be opened. Try again.";
const DIALOG_OPEN: &str = "Finish the open account dialog, then open the notification again.";

#[derive(Clone, Debug)]
struct Pending {
    generation: u64,
    target: NavigationTarget,
    deadline: Instant,
}

/// The one request waiting for an account switch. Pure state, so the
/// supersession rules are checked without a window.
#[derive(Debug, Default)]
struct PendingSlot(Option<Pending>);

impl PendingSlot {
    fn wait(&mut self, generation: u64, target: NavigationTarget, now: Instant) {
        self.0 = Some(Pending {
            generation,
            target,
            deadline: now + PENDING_TTL,
        });
    }

    fn clear(&mut self) {
        self.0 = None;
    }

    fn clear_if(&mut self, generation: u64) {
        if self.0.as_ref().is_some_and(|pending| pending.generation == generation) {
            self.0 = None;
        }
    }

    /// The user's own navigation ends a waiting request, except a switch to
    /// the very account it waits for (the leave-call confirmation re-enters
    /// the account menu's switch with that account).
    fn user_navigated(&mut self, account_id: Option<&str>) {
        if self
            .0
            .as_ref()
            .is_none_or(|pending| Some(pending.target.account_id()) != account_id)
        {
            self.0 = None;
        }
    }

    /// The active account changed: any other account ends the request. Its
    /// own account, or none (a failed or locked switch), leaves it waiting.
    fn account_changed(&mut self, active: Option<&AccountInfo>, now: Instant) {
        self.expire(now);
        if let (Some(pending), Some(active)) = (&self.0, active)
            && active.id != pending.target.account_id()
        {
            self.0 = None;
        }
    }

    fn expire(&mut self, now: Instant) {
        if self.0.as_ref().is_some_and(|pending| now >= pending.deadline) {
            self.0 = None;
        }
    }

    fn peek(&self) -> Option<Pending> {
        self.0.clone()
    }

    /// Consume the request exactly once, only if it is still `generation`.
    fn take_if(&mut self, generation: u64) -> Option<Pending> {
        if self.0.as_ref().is_some_and(|pending| pending.generation == generation) {
            self.0.take()
        } else {
            None
        }
    }
}

pub(crate) struct Activator {
    live: Arc<LiveUi>,
    ui: Weak<AppWindow>,
    runtime: Handle,
    pending: Mutex<PendingSlot>,
    generation: AtomicU64,
}

impl Activator {
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: Handle,
        activations: Option<mpsc::Receiver<NavigationTarget>>,
    ) -> Arc<Self> {
        let activator = Arc::new(Self {
            live,
            ui: ui.as_weak(),
            runtime: runtime.clone(),
            pending: Mutex::new(PendingSlot::default()),
            generation: AtomicU64::new(0),
        });
        if let Some(mut activations) = activations {
            let weak = Arc::downgrade(&activator);
            runtime.spawn(async move {
                while let Some(target) = activations.recv().await {
                    let Some(activator) = weak.upgrade() else {
                        return;
                    };
                    let opener = activator.clone();
                    if activator
                        .ui
                        .upgrade_in_event_loop(move |ui| opener.open(&ui, target))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        runtime.spawn(watch_switches(Arc::downgrade(&activator)));
        activator
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, PendingSlot> {
        self.pending.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    /// The user navigated on their own: a waiting request ends unless the
    /// navigation is the switch to its account. Call from the account menu's
    /// switch (with that account's ID), room and Space selection (`None`).
    pub(crate) fn user_navigated(&self, account_id: Option<&str>) {
        self.slot().user_navigated(account_id);
    }

    /// Open `target` for the user. Call on the UI thread. Supersedes any
    /// earlier request. A target in another account starts the protected
    /// switch and opens only after it completes.
    pub(crate) fn open(self: &Arc<Self>, ui: &AppWindow, target: NavigationTarget) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        if ui.window().is_minimized() {
            ui.window().set_minimized(false);
        }
        let live_session = self.live.is_live.load(Ordering::Acquire) && ui.get_live_session();
        if live_session && ui.get_active_account_local_id().as_str() == target.account_id() {
            self.slot().clear();
            self.navigate(ui, target, generation);
            return;
        }
        // A banner must not replace an app-password or leave-call dialog the
        // user is already answering.
        if ui.get_access_open() || ui.get_leave_call_dialog() {
            self.slot().clear();
            ui.set_session_error(DIALOG_OPEN.into());
            return;
        }
        self.slot().wait(generation, target.clone(), Instant::now());
        if live_session {
            // The account menu's own switch: security gate, leave-call
            // confirmation and app password, in that order.
            let Some(index) = ui
                .get_account_options()
                .iter()
                .find(|option| option.local_id.as_str() == target.account_id())
                .map(|option| option.index)
            else {
                self.slot().clear_if(generation);
                ui.set_session_error(ACCOUNT_GONE.into());
                return;
            };
            ui.invoke_switch_account(index);
        } else {
            // No account is open (locked, signed out of the last one, or the
            // offline preview): Manage accounts' protected activation.
            ui.invoke_activate_saved_account(target.account_id().into());
        }
        // A switch refused at once (a security operation, another switch in
        // progress) leaves nothing to wait for.
        if !ui.get_account_switch_pending() && !ui.get_leave_call_dialog() && !ui.get_access_open()
        {
            self.slot().clear_if(generation);
        }
    }

    /// Select the target's room in the visible account, then its thread.
    fn navigate(self: &Arc<Self>, ui: &AppWindow, target: NavigationTarget, generation: u64) {
        let account_id = target.account_id().to_owned();
        let room_id = target.room_id().to_owned();
        if !ui.get_live_session() || ui.get_active_account_local_id().as_str() != account_id {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(&account_id) else {
            return;
        };
        let other_room = ui.get_active_room_id().as_str() != room_id;
        // Replies and edits have no saved draft; never discard one.
        if other_room && ui.get_composer_mode() != 0 && !ui.get_composer_text().is_empty() {
            ui.set_session_error(REPLY_OPEN.into());
            return;
        }
        ui.invoke_cancel_room_settings();
        ui.invoke_close_image_preview();
        ui.set_page(0);
        let activator = self.clone();
        if !other_room {
            // Already open: keep its timeline and scroll position.
            if let Some(root) = target.thread_root().map(str::to_owned) {
                self.runtime.spawn(async move {
                    activator
                        .open_thread(selection, &account_id, &room_id, &root, generation)
                        .await;
                });
            }
            return;
        }
        crate::reset_room_search(ui);
        let navigation_epoch = self.live.invalidate_navigation();
        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let opened = activator
                .live
                .open_room_target_for(&account_id, room_id.clone(), navigation_epoch, weak.clone())
                .await;
            match opened {
                Ok(true) => {}
                // Superseded by newer navigation or an account change.
                Ok(false) => return,
                Err(error) => {
                    if activator.is_current(generation)
                        && activator.live.navigation_is_current(navigation_epoch)
                    {
                        let gone = matches!(error, MatrixCoreError::RoomNotFound(_));
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            if ui.get_active_account_local_id().as_str() != account_id {
                                return;
                            }
                            // An invitation banner opens its account; the
                            // invitation itself waits in the room list.
                            if gone
                                && ui
                                    .get_invitations()
                                    .iter()
                                    .any(|invite| invite.id.as_str() == room_id)
                            {
                                ui.set_transfer_notice(INVITE_OPEN.into());
                            } else if gone {
                                ui.set_session_error(ROOM_GONE.into());
                            } else {
                                ui.set_session_error(NOT_OPENED.into());
                            }
                        });
                    }
                    return;
                }
            }
            if let Some(root) = target.thread_root() {
                activator
                    .open_thread(selection, &account_id, &room_id, root, generation)
                    .await;
            }
        });
    }

    async fn open_thread(
        self: &Arc<Self>,
        selection: AccountSelection,
        account_id: &str,
        room_id: &str,
        root: &str,
        generation: u64,
    ) {
        if !selection.is_current() || !self.is_current(generation) {
            return;
        }
        // Prefer the loaded timeline's copy of the root; otherwise load just
        // that event, which the user asked to open.
        let (loaded, receive) = tokio::sync::oneshot::channel();
        let wanted = root.to_owned();
        if self
            .ui
            .upgrade_in_event_loop(move |ui| {
                let _ = loaded.send(
                    ui.get_messages()
                        .iter()
                        .any(|message| message.event_id.as_str() == wanted),
                );
            })
            .is_err()
        {
            return;
        }
        let fetched: Option<MessageInfo> = if receive.await.unwrap_or(false) {
            None
        } else {
            tokio::time::timeout(
                Duration::from_secs(15),
                self.live
                    .manager
                    .search_result_context_for(account_id, room_id, root, 1),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|context| {
                context
                    .into_iter()
                    .find(|message| message.event_id.as_deref() == Some(root))
            })
        };
        let activator = self.clone();
        let account_id = account_id.to_owned();
        let room_id = room_id.to_owned();
        let root = root.to_owned();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if !selection.is_current()
                || !activator.is_current(generation)
                || !ui.get_live_session()
                || ui.get_active_account_local_id().as_str() != account_id
                || ui.get_active_room_id().as_str() != room_id
            {
                return;
            }
            let item = ui
                .get_messages()
                .iter()
                .find(|message| message.event_id.as_str() == root)
                .or_else(|| fetched.as_ref().map(crate::live::message_item));
            show_thread(&ui, &root, item);
        });
    }
}

/// Present the thread panel exactly as the timeline's thread action does,
/// then start its feed through the same callback.
fn show_thread(ui: &AppWindow, root: &str, item: Option<MessageItem>) {
    let (sender, body) = match &item {
        Some(item) => {
            ui.set_thread_root_sender(item.sender.clone());
            ui.set_thread_root_is_own(item.is_own);
            ui.set_thread_root_palette_index(item.sender_palette_index);
            ui.set_thread_root_accent(item.accent);
            ui.set_thread_root_body(item.body.clone());
            ui.set_thread_root_rich_body(item.rich_body.clone());
            ui.set_thread_root_rich_accessible_body(item.rich_accessible_body.clone());
            (item.sender.clone(), item.body.clone())
        }
        None => {
            ui.set_thread_root_sender("".into());
            ui.set_thread_root_is_own(false);
            ui.set_thread_root_palette_index(0);
            ui.set_thread_root_accent(Default::default());
            ui.set_thread_root_body("".into());
            ui.set_thread_root_rich_body(Default::default());
            ui.set_thread_root_rich_accessible_body(
                "The first message of this thread could not be loaded.".into(),
            );
            (Default::default(), Default::default())
        }
    };
    ui.set_thread_root_id(root.into());
    ui.set_thread_composer_text("".into());
    ui.set_thread_send_pending(false);
    ui.set_thread_messages(ModelRc::new(VecModel::<MessageItem>::default()));
    ui.invoke_show_thread_panel();
    ui.invoke_open_thread(root.into(), sender, body);
}

/// Complete a waiting request once its account is active and shown. Checked
/// on every account change and once a second while a request waits, since
/// choosing an account that is already active and syncing publishes no
/// change. A switch in progress holds the account transition; waiting for it
/// lets its activation and room list finish first.
async fn watch_switches(activator: std::sync::Weak<Activator>) {
    let Some(manager) = activator
        .upgrade()
        .map(|activator| activator.live.manager.clone())
    else {
        return;
    };
    let mut active = manager.subscribe_active_account();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let changed = tokio::select! {
            result = active.changed() => {
                if result.is_err() {
                    return;
                }
                true
            }
            _ = tick.tick() => false,
        };
        let Some(activator) = activator.upgrade() else {
            return;
        };
        let now = Instant::now();
        if changed {
            let account = active.borrow_and_update().clone();
            activator.slot().account_changed(account.as_ref(), now);
        } else {
            activator.slot().expire(now);
        }
        let Some(pending) = activator.slot().peek() else {
            continue;
        };
        drop(activator.live.account_transition.lock().await);
        if manager
            .active_account()
            .await
            .is_none_or(|account| account.id != pending.target.account_id())
        {
            continue;
        }
        let Some(selection) = manager.account_selection(pending.target.account_id()) else {
            continue;
        };
        let opener = activator.clone();
        let _ = activator.ui.upgrade_in_event_loop(move |ui| {
            if !selection.is_current()
                || !ui.get_live_session()
                || ui.get_account_switch_pending()
                || ui.get_active_account_local_id().as_str() != pending.target.account_id()
            {
                return;
            }
            // Release the slot before navigating: navigation may run callbacks.
            let taken = opener.slot().take_if(pending.generation);
            if let Some(pending) = taken {
                opener.navigate(&ui, pending.target, pending.generation);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(account: &str) -> NavigationTarget {
        NavigationTarget::new(account, "!room:example.org", Some("$event"), None).unwrap()
    }

    fn account(id: &str) -> AccountInfo {
        AccountInfo {
            id: id.into(),
            user_id: format!("@{id}:example.org"),
            device_id: "DEVICE".into(),
            homeserver: "https://example.org".into(),
        }
    }

    #[test]
    fn a_waiting_target_opens_only_for_its_own_account_once() {
        let now = Instant::now();
        let mut slot = PendingSlot::default();
        slot.wait(1, target("alice"), now);
        // A failed or locked switch leaves no active account: keep waiting.
        slot.account_changed(None, now);
        slot.account_changed(Some(&account("alice")), now);
        assert_eq!(slot.peek().map(|pending| pending.generation), Some(1));
        // Only the current request is consumed, and only once.
        assert!(slot.take_if(0).is_none());
        let opened = slot.take_if(1).unwrap();
        assert_eq!(opened.target.account_id(), "alice");
        assert!(slot.take_if(1).is_none());
        assert!(slot.peek().is_none());
    }

    #[test]
    fn another_account_newer_requests_navigation_and_expiry_end_a_target() {
        let now = Instant::now();
        let mut slot = PendingSlot::default();
        // A → B → A: B becoming active ends the request for A for good.
        slot.wait(1, target("alice"), now);
        slot.account_changed(Some(&account("bob")), now);
        slot.account_changed(Some(&account("alice")), now);
        assert!(slot.peek().is_none());

        // A newer activation replaces the older; the older cannot clear or
        // consume it.
        slot.wait(2, target("alice"), now);
        slot.wait(3, target("bob"), now);
        slot.clear_if(2);
        assert!(slot.take_if(2).is_none());
        assert_eq!(slot.peek().map(|pending| pending.generation), Some(3));

        // The leave-call confirmation re-enters the switch to the same
        // account and keeps the request; any other navigation ends it.
        slot.user_navigated(Some("bob"));
        assert!(slot.peek().is_some());
        slot.user_navigated(Some("alice"));
        assert!(slot.peek().is_none());
        slot.wait(4, target("alice"), now);
        slot.user_navigated(None);
        assert!(slot.peek().is_none());

        // An abandoned app-password prompt cannot open the room later.
        slot.wait(5, target("alice"), now);
        slot.expire(now + PENDING_TTL);
        assert!(slot.peek().is_none());
        slot.wait(6, target("alice"), now);
        slot.account_changed(Some(&account("alice")), now + PENDING_TTL);
        assert!(slot.take_if(6).is_none());
    }
}
