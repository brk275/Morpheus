//! The room header's notification panel for the room open in the
//! conversation, and the header bell's muted state.
//!
//! The level is the account's Matrix push rule for that room, read from and
//! written to the homeserver through the same selection-fenced core API as
//! Settings › Notifications. The sound, preview and highlight switches are
//! this device's `account_overrides.<account>.room_notifications` entry,
//! saved through the same synchronous path as the other notification
//! switches; each can only remove something from that room's banners.
//!
//! Opening captures the account's selection, the room and an operation
//! number on the UI thread; a result is shown only while all three are still
//! current and the panel is open for exactly that account and room. A level
//! is shown as saved only after the homeserver accepted it, a request already
//! sent is never reported as undone, and errors use fixed wording. Closing,
//! or the window showing another account or room, clears every row.

use std::sync::{Arc, Mutex, MutexGuard, atomic::Ordering};

use morpheus_config::{Config, ConfigStore, MAX_ROOM_NOTIFICATION_OVERRIDES, RoomNotificationLocal};
use morpheus_matrix::{
    AccountSelection, MatrixCoreError, NotificationDefaults, NotificationMode, PUSH_RULE_SYNC_WAIT,
    PushRuleSyncWatch, RoomNotificationRule,
};
use slint::{ComponentHandle, Weak};
use tokio::runtime::Handle;

use super::rules::{Rules, level};
use super::uncertainty::{ChangeScope, SyncMark};
use crate::{AppWindow, RoomNotificationState, live::LiveUi, notifications::DesktopNotifications};

const MAX_NAME_CHARS: usize = 80;

const READ_FAILED: &str = "This room's level could not be read. Read it again, or try later.";
const NOT_JOINED: &str = "This room is no longer joined on this account.";
// A change the homeserver did not confirm may have reached it in part or in
// full; the panel says so and keeps its levels available. The level shown is
// this device's copy until a sync observed after the change brings the
// homeserver's rules; only then is it called what they say.
const NOT_CONFIRMED: &str = "The homeserver did not confirm the whole change; part or all of it may have reached it. The level shown is this device's copy until a sync brings its rules.";
const SYNCED: &str = "Sync brought the homeserver's rules after the unconfirmed change; the level shown is what they say, which may include part of that change.";
const NOT_SYNCED: &str = "No sync has brought the homeserver's rules since the unconfirmed change; the level shown may differ from what it holds.";

/// What the panel may say once the wait after an unconfirmed change ended,
/// and whether the change stays unconfirmed: only an observed fresh sync
/// ends it.
fn after_wait(fresh: bool) -> (&'static str, bool) {
    if fresh {
        (SYNCED, false)
    } else {
        (NOT_SYNCED, true)
    }
}
const LOCAL_FULL: &str =
    "Up to 256 rooms per account can have their own device settings; clear another room first.";
const LOCAL_FAILED: &str =
    "This device's settings were not saved; the configuration status says why.";

/// One device switch; `on` always means it removes something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Switch {
    Quiet,
    HidePreview,
    NoHighlights,
}

fn switched(mut local: RoomNotificationLocal, switch: Switch, on: bool) -> RoomNotificationLocal {
    match switch {
        Switch::Quiet => local.quiet = on,
        Switch::HidePreview => local.hide_preview = on,
        Switch::NoHighlights => local.no_highlights = on,
    }
    local
}

/// Whether saving `local` would give the account more rooms with their own
/// device behavior than the configuration allows.
fn over_bound(
    config: &Config,
    account_id: &str,
    room_id: &str,
    local: RoomNotificationLocal,
) -> bool {
    let rooms = config
        .account_overrides
        .get(account_id)
        .map(|overrides| &overrides.room_notifications);
    !local.is_default()
        && !rooms.is_some_and(|rooms| rooms.contains_key(room_id))
        && rooms.map_or(0, |rooms| rooms.len()) >= MAX_ROOM_NOTIFICATION_OVERRIDES
}

/// The panel's level choice: 0 removes the room's own rule.
fn chosen(choice: i32) -> Option<Option<NotificationMode>> {
    match choice {
        0 => Some(None),
        1 => Some(Some(NotificationMode::AllMessages)),
        2 => Some(Some(NotificationMode::MentionsAndKeywords)),
        3 => Some(Some(NotificationMode::Mute)),
        _ => None,
    }
}

/// Whether `account_id` and `room_id` are the live account and the room open
/// in the window now. UI thread only.
fn on_screen(ui: &AppWindow, account_id: &str, room_id: &str) -> bool {
    ui.get_live_session()
        && !account_id.is_empty()
        && !room_id.is_empty()
        && ui.get_active_account_local_id().as_str() == account_id
        && ui.get_active_room_id().as_str() == room_id
}

/// Show a confirmed or read level on the header bell while its room is the
/// one open for that account. UI thread only.
pub(super) fn header_rule(ui: &AppWindow, account_id: &str, rule: &RoomNotificationRule) {
    if on_screen(ui, account_id, &rule.room_id) {
        ui.global::<RoomNotificationState>()
            .set_header_muted(rule.effective == NotificationMode::Mute);
    }
}

/// Shared uncertainty may arrive from a retired Settings or room-panel
/// operation. Only the currently open account/room receives its warning.
pub(super) fn show_uncertainty(ui: &AppWindow, account_id: &str, room_id: &str) {
    let global = ui.global::<RoomNotificationState>();
    if global.get_open()
        && global.get_account_id().as_str() == account_id
        && global.get_room_id().as_str() == room_id
        && on_screen(ui, account_id, room_id)
    {
        global.set_notice(NOT_SYNCED.into());
    }
}

#[derive(Default)]
struct State {
    /// Bumped by every open, retry and close; a result is shown only while
    /// its number is current.
    operation: u64,
    selection: Option<AccountSelection>,
    room_id: String,
    /// The rule last read or confirmed by the homeserver.
    rule: Option<RoomNotificationRule>,
    /// The account and room of a level change that was sent and has not
    /// finished. It outlives a close, so a reopened panel never sends a
    /// second change over it.
    in_flight: Option<(String, String)>,
    /// Bumped for every room the header shows; a bell read is used only
    /// while its number is current.
    header: u64,
}

pub(super) struct RoomPanel {
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    rules: Arc<Rules>,
    ui: Weak<AppWindow>,
    runtime: Handle,
    store: ConfigStore,
    state: Mutex<State>,
}

impl RoomPanel {
    pub(super) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        notifications: Arc<DesktopNotifications>,
        rules: Arc<Rules>,
        store: ConfigStore,
        runtime: Handle,
    ) -> Arc<Self> {
        let panel = Arc::new(Self {
            live,
            notifications,
            rules,
            ui: ui.as_weak(),
            runtime,
            store,
            state: Mutex::new(State::default()),
        });
        let global = ui.global::<RoomNotificationState>();
        let open = panel.clone();
        global.on_request_open(move |account_id, room_id| open.open(&account_id, &room_id));
        let shown = panel.clone();
        global.on_room_shown(move |account_id, room_id| shown.room_shown(&account_id, &room_id));
        let choose = panel.clone();
        global.on_choose(move |choice| choose.choose(choice));
        let quiet = panel.clone();
        global.on_set_quiet(move |on| quiet.set_switch(Switch::Quiet, on));
        let preview = panel.clone();
        global.on_set_hide_preview(move |on| preview.set_switch(Switch::HidePreview, on));
        let highlights = panel.clone();
        global.on_set_no_highlights(move |on| highlights.set_switch(Switch::NoHighlights, on));
        let retry = panel.clone();
        global.on_retry(move || retry.retry());
        let close = panel.clone();
        global.on_close(move || close.close());
        let settings = panel.clone();
        global.on_open_settings(move || settings.open_settings());
        panel
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Whether a result for `operation` under `selection` may still be shown.
    fn owns(&self, ui: &AppWindow, operation: u64, selection: &AccountSelection) -> bool {
        let state = self.state();
        state.operation == operation
            && selection.is_current()
            && ui.global::<RoomNotificationState>().get_open()
            && on_screen(ui, selection.account_id(), &state.room_id)
    }

    /// The open panel's account selection and room while both are still the
    /// ones on screen.
    fn current(&self, ui: &AppWindow) -> Option<(AccountSelection, String)> {
        let state = self.state();
        let selection = state.selection.clone()?;
        let global = ui.global::<RoomNotificationState>();
        let shown = global.get_open()
            && selection.is_current()
            && global.get_account_id().as_str() == selection.account_id()
            && global.get_room_id().as_str() == state.room_id
            && on_screen(ui, selection.account_id(), &state.room_id);
        shown.then(|| (selection, state.room_id.clone()))
    }

    /// Open the panel for exactly the live account and the room open now.
    fn open(self: &Arc<Self>, account_id: &str, room_id: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if !on_screen(&ui, account_id, room_id) || !self.live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(account_id) else {
            return;
        };
        let (operation, waiting) = {
            let mut state = self.state();
            state.operation = state.operation.wrapping_add(1);
            state.selection = Some(selection.clone());
            state.room_id = room_id.to_owned();
            state.rule = None;
            let waiting = state
                .in_flight
                .as_ref()
                .is_some_and(|(account, room)| account == account_id && room == room_id);
            (state.operation, waiting)
        };
        let global = ui.global::<RoomNotificationState>();
        clear(&global);
        let name = crate::directory_plain_text(&ui.get_active_room_name(), MAX_NAME_CHARS);
        global.set_account_id(account_id.into());
        global.set_room_id(room_id.into());
        global.set_room_name(if name.is_empty() {
            "this room".into()
        } else {
            name.into()
        });
        global.set_pending(waiting);
        if let Ok(config) = self.store.load() {
            apply_local(&global, &config, account_id, room_id);
        }
        global.set_open(true);
        self.read(&ui, operation, selection, room_id.to_owned());
    }

    fn retry(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some((selection, room_id)) = self.current(&ui) else {
            return;
        };
        let operation = {
            let mut state = self.state();
            state.operation = state.operation.wrapping_add(1);
            state.rule = None;
            state.operation
        };
        self.read(&ui, operation, selection, room_id);
    }

    /// Read the room's rule and the account default it would follow. Both
    /// come from the push rules the SDK holds from sync; no request is sent.
    fn read(
        self: &Arc<Self>,
        ui: &AppWindow,
        operation: u64,
        selection: AccountSelection,
        room_id: String,
    ) {
        let global = ui.global::<RoomNotificationState>();
        global.set_loading(true);
        global.set_error("".into());
        global.set_notice("".into());
        global.set_mode(0);
        global.set_effective(0);
        global.set_default_level(0);
        let panel = self.clone();
        self.runtime.spawn(async move {
            let manager = panel.live.manager.clone();
            let (rule, defaults) = tokio::join!(
                manager.room_notification_rule_selected(&selection, &room_id),
                manager.notification_defaults_selected(&selection),
            );
            let weak = panel.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                panel.apply_read(&ui, operation, &selection, rule, defaults);
            });
        });
    }

    fn apply_read(
        self: &Arc<Self>,
        ui: &AppWindow,
        operation: u64,
        selection: &AccountSelection,
        rule: Result<RoomNotificationRule, MatrixCoreError>,
        defaults: Result<NotificationDefaults, MatrixCoreError>,
    ) {
        if !self.owns(ui, operation, selection) {
            return;
        }
        let global = ui.global::<RoomNotificationState>();
        global.set_loading(false);
        match rule {
            Ok(rule) => {
                // An unreadable default only leaves its name out.
                if let Ok(defaults) = defaults {
                    let default = if rule.is_direct {
                        defaults.direct
                    } else {
                        defaults.group
                    };
                    global.set_default_level(level(default));
                }
                header_rule(ui, selection.account_id(), &rule);
                let unconfirmed = self
                    .rules
                    .unconfirmed(selection.account_id(), Some(&rule.room_id));
                self.show_rule(&global, rule);
                if unconfirmed {
                    // What was just read is this device's copy, not confirmed.
                    global.set_notice(NOT_SYNCED.into());
                    self.await_fresh(operation, selection.clone(), None, self.rules.sync_mark());
                }
            }
            Err(MatrixCoreError::RoomNotFound(_)) => global.set_error(NOT_JOINED.into()),
            Err(_) => global.set_error(READ_FAILED.into()),
        }
    }

    fn show_rule(&self, global: &RoomNotificationState<'_>, rule: RoomNotificationRule) {
        global.set_mode(rule.custom.map_or(0, level));
        global.set_effective(level(rule.effective));
        global.set_direct(rule.is_direct);
        self.state().rule = Some(rule);
    }

    /// A deliberate level choice for exactly the account and room shown,
    /// after their level was read, one change at a time.
    fn choose(self: &Arc<Self>, choice: i32) {
        let Some(mode) = chosen(choice) else {
            return;
        };
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some((selection, room_id)) = self.current(&ui) else {
            return;
        };
        let global = ui.global::<RoomNotificationState>();
        // An unconfirmed change keeps the levels available; a level that
        // could not be read does not offer them (no rule is known).
        if global.get_loading() {
            return;
        }
        let operation = {
            let mut state = self.state();
            let Some(rule) = &state.rule else {
                return;
            };
            if state.in_flight.is_some() || rule.room_id != room_id || rule.custom == mode {
                return;
            }
            state.in_flight = Some((selection.account_id().to_owned(), room_id.clone()));
            state.operation
        };
        global.set_pending(true);
        global.set_notice("".into());
        let panel = self.clone();
        self.runtime.spawn(async move {
            let manager = panel.live.manager.clone();
            let ticket = panel
                .rules
                .begin_change(&selection, ChangeScope::Room(room_id.clone()));
            // Opened before the change, so a sync during or right after it is
            // not missed.
            let mut watch = manager.watch_push_rules_selected(&selection).await.ok();
            let result = manager
                .set_room_notification_mode_selected(&selection, &room_id, mode)
                .await;
            // Record independently of a close, refresh or account navigation.
            // Drain only after this mark: an earlier event cannot settle a
            // failure that was recorded between completion and that drain.
            let mark = panel.rules.complete_change(ticket, result.is_ok());
            if let Some(watch) = &mut watch {
                watch.change_finished();
            }
            // Delivery follows an accepted rule even if the panel has closed.
            if let Ok(rule) = &result
                && selection.is_current()
            {
                panel.notifications.note_room_mode(
                    selection.account_id(),
                    &rule.room_id,
                    rule.effective,
                );
            }
            let weak = panel.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                panel.finish_choice(&ui, operation, &selection, result, watch, mark);
            });
        });
    }

    fn finish_choice(
        self: &Arc<Self>,
        ui: &AppWindow,
        operation: u64,
        selection: &AccountSelection,
        result: Result<RoomNotificationRule, MatrixCoreError>,
        watch: Option<PushRuleSyncWatch>,
        mark: SyncMark,
    ) {
        self.state().in_flight = None;
        self.rules.publish_uncertainty(ui);
        if let Ok(rule) = &result
            && selection.is_current()
        {
            // Settings › Notifications and the header bell show it too.
            self.rules.room_rule_saved(ui, selection, rule);
            header_rule(ui, selection.account_id(), rule);
        }
        let global = ui.global::<RoomNotificationState>();
        global.set_pending(false);
        if !self.owns(ui, operation, selection) {
            // A panel reopened for the same room while this waited shows the
            // confirmed level too, once it has read its own.
            let reread = self.state().rule.is_some();
            if let Ok(rule) = result
                && reread
                && selection.is_current()
                && self
                    .current(ui)
                    .is_some_and(|(_, room_id)| room_id == rule.room_id)
            {
                self.show_rule(&global, rule);
            }
            return;
        }
        match result {
            Ok(rule) => {
                global.set_notice(
                    if rule.custom.is_none() {
                        "Saved: this room follows the account default."
                    } else {
                        "Saved on the homeserver."
                    }
                    .into(),
                );
                self.show_rule(&global, rule);
            }
            Err(MatrixCoreError::RoomNotFound(_)) => global.set_error(NOT_JOINED.into()),
            Err(_) => {
                global.set_error(NOT_CONFIRMED.into());
                self.await_fresh(operation, selection.clone(), watch, mark);
            }
        }
    }

    /// After a change the homeserver did not confirm, wait (bounded) for a
    /// sync observed after it, with `watch` opened before the change (or,
    /// without one, from now on). Only then is the level read again and
    /// called what the homeserver's rules say; a timeout keeps the change
    /// unconfirmed and says so. Nothing is sent.
    fn await_fresh(
        self: &Arc<Self>,
        operation: u64,
        selection: AccountSelection,
        watch: Option<PushRuleSyncWatch>,
        mark: SyncMark,
    ) {
        let panel = self.clone();
        self.runtime.spawn(async move {
            let manager = panel.live.manager.clone();
            let watch = match watch {
                Some(watch) => Ok(watch),
                None => manager.watch_push_rules_selected(&selection).await,
            };
            let Ok(mut watch) = watch else {
                return;
            };
            let Ok(fresh) = manager
                .fresh_push_rules_selected(&selection, &mut watch, PUSH_RULE_SYNC_WAIT)
                .await
            else {
                return;
            };
            let weak = panel.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                panel.finish_wait(&ui, operation, selection, fresh, mark);
            });
        });
    }

    fn finish_wait(
        self: &Arc<Self>,
        ui: &AppWindow,
        operation: u64,
        selection: AccountSelection,
        fresh: bool,
        mark: SyncMark,
    ) {
        if fresh {
            self.rules.synced(selection.account_id(), mark);
        }
        if !self.owns(ui, operation, &selection) {
            return;
        }
        let global = ui.global::<RoomNotificationState>();
        let (text, unconfirmed) = after_wait(fresh);
        if unconfirmed {
            global.set_error(text.into());
            return;
        }
        let room_id = self.state().room_id.clone();
        if self.rules.unconfirmed(selection.account_id(), Some(&room_id)) {
            global.set_error(NOT_SYNCED.into());
            return;
        }
        // The level sync brought is what the homeserver's rules say now.
        self.read(ui, operation, selection, room_id);
        global.set_notice(text.into());
    }

    /// Save one device switch for the room shown, through the same
    /// synchronous path as every notification switch, so delivery follows
    /// it before this returns.
    fn set_switch(&self, switch: Switch, on: bool) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some((selection, room_id)) = self.current(&ui) else {
            return;
        };
        let account_id = selection.account_id().to_owned();
        let global = ui.global::<RoomNotificationState>();
        global.set_local_error("".into());
        let Ok(config) = self.store.load() else {
            global.set_local_error(LOCAL_FAILED.into());
            return;
        };
        let local = switched(
            config.room_notification_local(&account_id, &room_id),
            switch,
            on,
        );
        if over_bound(&config, &account_id, &room_id, local) {
            global.set_local_error(LOCAL_FULL.into());
            return;
        }
        let saved = crate::save_notification_config_change(
            &ui,
            &self.store,
            &self.notifications,
            &self.live.voice_spaces,
            |config| {
                // Applied to the file as reread, so the other switches keep
                // what it holds.
                let local = switched(
                    config.room_notification_local(&account_id, &room_id),
                    switch,
                    on,
                );
                config.set_room_notification_local(&account_id, &room_id, local);
            },
        );
        if !saved {
            global.set_local_error(LOCAL_FAILED.into());
        }
        // Show what the file holds now, saved or not.
        if let Ok(config) = self.store.load() {
            apply_local(&global, &config, &account_id, &room_id);
        }
    }

    /// Close the panel and clear its rows. A level change already sent keeps
    /// going and is never reported as undone.
    fn close(&self) {
        {
            let mut state = self.state();
            state.operation = state.operation.wrapping_add(1);
            state.selection = None;
            state.room_id.clear();
            state.rule = None;
        }
        if let Some(ui) = self.ui.upgrade() {
            clear(&ui.global::<RoomNotificationState>());
        }
    }

    /// Settings › Notifications › Rooms, revealed and focused; nothing is
    /// written.
    fn open_settings(&self) {
        self.close();
        if let Some(ui) = self.ui.upgrade() {
            crate::settings_search::reveal(&ui, "notifications.rooms");
        }
    }

    /// The header now shows this account and room: reread the bell's level.
    fn room_shown(self: &Arc<Self>, account_id: &str, room_id: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let header = {
            let mut state = self.state();
            state.header = state.header.wrapping_add(1);
            state.header
        };
        ui.global::<RoomNotificationState>().set_header_muted(false);
        if !on_screen(&ui, account_id, room_id) || !self.live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(selection) = self.live.manager.account_selection(account_id) else {
            return;
        };
        let panel = self.clone();
        let room_id = room_id.to_owned();
        self.runtime.spawn(async move {
            // The push rules the SDK holds from sync; no request is sent.
            let rule = panel
                .live
                .manager
                .room_notification_rule_selected(&selection, &room_id)
                .await;
            let weak = panel.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if panel.state().header != header || !selection.is_current() {
                    return;
                }
                if let Ok(rule) = rule {
                    header_rule(&ui, selection.account_id(), &rule);
                }
            });
        });
    }
}

/// Every row back to its closed, empty value.
fn clear(global: &RoomNotificationState<'_>) {
    global.set_open(false);
    global.set_account_id("".into());
    global.set_room_id("".into());
    global.set_room_name("".into());
    global.set_loading(false);
    global.set_error("".into());
    global.set_notice("".into());
    global.set_mode(0);
    global.set_effective(0);
    global.set_default_level(0);
    global.set_direct(false);
    global.set_pending(false);
    global.set_quiet(false);
    global.set_hide_preview(false);
    global.set_no_highlights(false);
    global.set_local_error("".into());
}

/// The room's device switches and the ceilings they cannot raise.
fn apply_local(
    global: &RoomNotificationState<'_>,
    config: &Config,
    account_id: &str,
    room_id: &str,
) {
    let local = config.room_notification_local(account_id, room_id);
    global.set_quiet(local.quiet);
    global.set_hide_preview(local.hide_preview);
    global.set_no_highlights(local.no_highlights);
    global.set_sounds_on(config.notifications.sounds);
    // A preview also needs the room and sender shown.
    global.set_previews_on(
        config.notifications.show_message_preview && config.notifications.show_room_and_sender,
    );
    global.set_highlights_set(
        config
            .notifications
            .highlight_words
            .iter()
            .any(|word| !word.trim().is_empty()),
    );
    global.set_account_on(config.notifications_enabled_for(account_id));
}

/// An accepted configuration reaches an open panel's switches and ceilings.
pub(super) fn apply_config(ui: &AppWindow, config: &Config) {
    let global = ui.global::<RoomNotificationState>();
    if global.get_open() {
        let (account_id, room_id) = (global.get_account_id(), global.get_room_id());
        apply_local(&global, config, &account_id, &room_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_switches_change_only_their_own_flag() {
        let none = RoomNotificationLocal::default();
        let quiet = switched(none, Switch::Quiet, true);
        assert!(quiet.quiet && !quiet.hide_preview && !quiet.no_highlights);
        let both = switched(quiet, Switch::HidePreview, true);
        assert!(both.quiet && both.hide_preview && !both.no_highlights);
        let marked = switched(both, Switch::NoHighlights, true);
        assert!(marked.no_highlights);
        // Turning every switch off again removes nothing, so it is not stored.
        let off = [Switch::Quiet, Switch::HidePreview, Switch::NoHighlights]
            .into_iter()
            .fold(marked, |local, switch| switched(local, switch, false));
        assert!(off.is_default());
    }

    #[test]
    fn a_new_room_is_refused_past_the_per_account_bound() {
        let quiet = switched(RoomNotificationLocal::default(), Switch::Quiet, true);
        let mut config = Config::default();
        for index in 0..MAX_ROOM_NOTIFICATION_OVERRIDES {
            config.set_room_notification_local("work", &format!("!r{index}:example.org"), quiet);
        }
        assert!(over_bound(&config, "work", "!new:example.org", quiet));
        // A listed room may still change, any room may clear, and other
        // accounts have their own bound.
        assert!(!over_bound(&config, "work", "!r0:example.org", RoomNotificationLocal::default()));
        assert!(!over_bound(&config, "work", "!r0:example.org", quiet));
        assert!(!over_bound(&config, "work", "!new:example.org", RoomNotificationLocal::default()));
        assert!(!over_bound(&config, "home", "!new:example.org", quiet));
    }

    #[test]
    fn only_the_four_level_choices_are_accepted() {
        assert_eq!(chosen(0), Some(None));
        assert_eq!(chosen(1), Some(Some(NotificationMode::AllMessages)));
        assert_eq!(chosen(2), Some(Some(NotificationMode::MentionsAndKeywords)));
        assert_eq!(chosen(3), Some(Some(NotificationMode::Mute)));
        for choice in [-1, 4, i32::MAX] {
            assert_eq!(chosen(choice), None);
        }
    }

    #[test]
    fn a_timeout_never_calls_this_devices_copy_the_homeservers_rules() {
        let (text, unconfirmed) = after_wait(false);
        assert!(unconfirmed, "a timeout keeps the change unconfirmed");
        assert_eq!(text, NOT_SYNCED);
        assert!(text.contains("may differ") && !text.contains("what they say"));
        // Only an observed fresh sync makes the level the homeserver's, and
        // the change itself may still have landed in part.
        let (text, unconfirmed) = after_wait(true);
        assert!(!unconfirmed);
        assert!(text.contains("what they say") && text.contains("may include part"));
        assert!(NOT_CONFIRMED.contains("part or all of it may have reached it"));
    }

    #[test]
    fn failure_wording_is_fixed_and_never_claims_an_undo() {
        for message in [
            READ_FAILED,
            NOT_JOINED,
            NOT_CONFIRMED,
            SYNCED,
            NOT_SYNCED,
            LOCAL_FULL,
            LOCAL_FAILED,
        ] {
            let lower = message.to_lowercase();
            let claims = ["undone", "reverted", "rolled back", "cancelled", "unchanged", "keeps"];
            for claim in claims {
                assert!(!lower.contains(claim), "{message}");
            }
            assert!(!message.contains('!') && !message.contains('@'), "{message}");
        }
    }
}
