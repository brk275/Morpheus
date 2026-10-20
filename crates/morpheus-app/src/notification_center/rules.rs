//! Settings › Notifications: the banner privacy switch, the account's default
//! levels and searchable per-room rules.
//!
//! Levels are the account's Matrix push rules, read from and written to the
//! homeserver through the selection-fenced core API. Every request captures
//! the visible account's selection and an operation number on the UI thread;
//! a result is shown only while both are still current. A change is shown as
//! saved only after the homeserver accepted it. Errors use fixed wording and
//! never repeat server text.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, atomic::Ordering},
};

use morpheus_config::{AccountOverrides, Config, ConfigStore};
use morpheus_matrix::{
    AccountSelection, MAX_NOTIFICATION_KEYWORD_CHARS, MAX_NOTIFICATION_KEYWORDS, MatrixCoreError,
    NotificationDefaults, NotificationMode, PUSH_RULE_SYNC_WAIT, PushRuleSyncWatch,
    RoomNotificationRule,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;

use super::uncertainty::{ChangeScope, ChangeTicket, SyncMark, Uncertainty};
use crate::{
    AppWindow, NotificationRoomItem, NotificationRulesState, RoomNotificationState, live::LiveUi,
    notifications::DesktopNotifications,
};

/// Rows listed at once; a search narrows the rest.
const MAX_LISTED: usize = 100;

// A change the homeserver did not confirm may have reached it in part or in
// full (a default level is two rules, for encrypted and unencrypted rooms, and
// both can land), so none of these says it is unchanged. What is shown is
// this device's copy until a sync observed after the change brings the
// homeserver's rules; the controls stay available.
const ROOM_UNCONFIRMED: &str = "The homeserver did not confirm the whole change; part or all of it may have reached it. Levels shown are this device's copy until a sync brings its rules.";
const DEFAULT_UNCONFIRMED: &str = "The homeserver did not confirm the whole default level; part or all of it may have reached it. Levels shown are this device's copy until a sync brings its rules.";
const KEYWORD_ADD_UNCONFIRMED: &str = "The homeserver did not confirm the keyword; it may still have been added. Keywords shown are this device's copy until a sync brings its rules.";
const KEYWORD_REMOVE_UNCONFIRMED: &str = "The homeserver did not confirm removing the keyword; it may still have been removed. Keywords shown are this device's copy until a sync brings its rules.";
// After waiting for that sync: only one actually observed makes what is shown
// the homeserver's rules, and even then the change may have landed in part.
const SYNCED: &str = "Sync brought the homeserver's rules after the unconfirmed change; what is shown is what they say, which may include part of that change.";
const NOT_SYNCED: &str = "No sync has brought the homeserver's rules since an unconfirmed change; levels and keywords shown may differ from what it holds.";

/// What may be said once the wait after an unconfirmed change ended, and
/// whether the change stays unconfirmed: only an observed fresh sync ends it.
fn after_wait(fresh: bool) -> (&'static str, bool) {
    if fresh {
        (SYNCED, false)
    } else {
        (NOT_SYNCED, true)
    }
}
const MAX_QUERY_CHARS: usize = 128;
const MAX_NAME_CHARS: usize = 80;

struct Row {
    room_id: String,
    name: String,
    folded: String,
    direct: bool,
    custom: Option<NotificationMode>,
    effective: NotificationMode,
    pending: bool,
}

impl Row {
    fn new(rule: RoomNotificationRule) -> Self {
        let name = crate::directory_plain_text(&rule.name, MAX_NAME_CHARS);
        let name = if name.is_empty() {
            rule.room_id.clone()
        } else {
            name
        };
        Self {
            folded: name.to_lowercase(),
            name,
            room_id: rule.room_id,
            direct: rule.is_direct,
            custom: rule.custom,
            effective: rule.effective,
            pending: false,
        }
    }

    fn item(&self) -> NotificationRoomItem {
        NotificationRoomItem {
            room_id: self.room_id.as_str().into(),
            name: self.name.as_str().into(),
            mode: self.custom.map_or(0, level),
            effective: level(self.effective),
            direct: self.direct,
            pending: self.pending,
        }
    }
}

pub(super) fn level(mode: NotificationMode) -> i32 {
    match mode {
        NotificationMode::AllMessages => 1,
        NotificationMode::MentionsAndKeywords => 2,
        NotificationMode::Mute => 3,
    }
}

#[derive(Default)]
struct State {
    operation: u64,
    selection: Option<AccountSelection>,
    rows: Vec<Row>,
    /// Lowercase search text.
    query: String,
    loaded: bool,
    /// The account's Matrix keywords exactly as stored in its push rules.
    keywords: Vec<String>,
}

pub(super) struct Rules {
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    ui: Weak<AppWindow>,
    runtime: Handle,
    store: ConfigStore,
    state: Mutex<State>,
    uncertainty: Mutex<Uncertainty>,
}

impl Rules {
    pub(super) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        notifications: Arc<DesktopNotifications>,
        store: ConfigStore,
        runtime: Handle,
    ) -> Arc<Self> {
        let rules = Arc::new(Self {
            live,
            notifications,
            ui: ui.as_weak(),
            runtime,
            store: store.clone(),
            state: Mutex::new(State::default()),
            uncertainty: Mutex::new(Uncertainty::default()),
        });
        let global = ui.global::<NotificationRulesState>();
        let refresh = rules.clone();
        global.on_refresh(move || refresh.refresh());
        let search = rules.clone();
        global.on_search(move |query| search.search(&query));
        let choose = rules.clone();
        global.on_choose_room_mode(move |room_id, mode| choose.choose_room_mode(&room_id, mode));
        let defaults = rules.clone();
        global.on_choose_default(move |direct, mode| defaults.choose_default(direct, mode));
        let privacy = rules.clone();
        global.on_set_show_room_and_sender(move |show| {
            let Some(ui) = privacy.ui.upgrade() else {
                return;
            };
            // The same synchronous path as the other notification switches:
            // re-read the file, save, then apply to the presenter at once.
            crate::save_notification_config_change(
                &ui,
                &store,
                &privacy.notifications,
                &privacy.live.voice_spaces,
                |config| config.notifications.show_room_and_sender = show,
            );
        });
        let account = rules.clone();
        global.on_set_account_enabled(move |enabled| account.set_account_enabled(enabled));
        let add = rules.clone();
        global.on_add_keyword(move |keyword| add.add_keyword(&keyword));
        let remove = rules.clone();
        global.on_remove_keyword(move |keyword| remove.remove_keyword(&keyword));
        rules
    }

    /// Turn desktop notifications (and incoming-call alerts) on or off for the
    /// visible account only, as `account_overrides.<account>.notifications_enabled`.
    fn set_account_enabled(&self, enabled: bool) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let account_id = ui.get_active_account_local_id().to_string();
        if !ui.get_live_session() || account_id.is_empty() {
            return;
        }
        crate::save_notification_config_change(
            &ui,
            &self.store,
            &self.notifications,
            &self.live.voice_spaces,
            |config| {
                let unused = {
                    let overrides = config
                        .account_overrides
                        .entry(account_id.clone())
                        .or_default();
                    // On follows the global switch; only off is stored.
                    overrides.notifications_enabled = (!enabled).then_some(false);
                    *overrides == AccountOverrides::default()
                };
                if unused {
                    config.account_overrides.remove(&account_id);
                }
            },
        );
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn begin_change(
        &self,
        selection: &AccountSelection,
        scope: ChangeScope,
    ) -> ChangeTicket {
        self.uncertainty
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin(selection.account_id(), scope)
    }

    pub(super) fn complete_change(&self, ticket: ChangeTicket, confirmed: bool) -> SyncMark {
        self.uncertainty
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .complete(ticket, confirmed)
    }

    pub(super) fn sync_mark(&self) -> SyncMark {
        self.uncertainty
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mark()
    }

    pub(super) fn unconfirmed(&self, account: &str, room: Option<&str>) -> bool {
        self.uncertainty
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .has(account, room)
    }

    pub(super) fn synced(&self, account: &str, mark: SyncMark) {
        self.uncertainty
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .synced(account, mark);
    }

    /// A write can end after its original view was replaced and its next
    /// cached read already finished. Warn the current view from shared state,
    /// without applying that retired write's result to it.
    pub(super) fn publish_uncertainty(&self, ui: &AppWindow) {
        let account_id = ui.get_active_account_local_id();
        let account_id = account_id.as_str();
        if !ui.get_live_session()
            || !self
                .live
                .manager
                .account_selection(account_id)
                .is_some_and(|selection| selection.is_current())
        {
            return;
        }
        let listed = {
            let state = self.state();
            state.loaded
                && state.selection.as_ref().is_some_and(|selection| {
                    selection.is_current() && selection.account_id() == account_id
                })
        };
        if listed && self.unconfirmed(account_id, None) {
            ui.global::<NotificationRulesState>()
                .set_notice(NOT_SYNCED.into());
        }
        let room_id = ui.global::<RoomNotificationState>().get_room_id();
        if self.unconfirmed(account_id, Some(room_id.as_str())) {
            super::room_panel::show_uncertainty(ui, account_id, room_id.as_str());
        }
    }

    /// Whether a result for `operation` under `selection` may still be shown.
    fn owns(&self, ui: &AppWindow, operation: u64, selection: &AccountSelection) -> bool {
        self.state().operation == operation
            && selection.is_current()
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == selection.account_id()
    }

    /// Load the visible account's rules. A reopened section, or one showing
    /// another account, starts with an empty search, in the box as here.
    fn refresh(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<NotificationRulesState>();
        let account_id = ui.get_active_account_local_id().to_string();
        let selection = if ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && !account_id.is_empty()
        {
            self.live.manager.account_selection(&account_id)
        } else {
            None
        };
        let operation = {
            let mut state = self.state();
            state.operation = state.operation.wrapping_add(1);
            state.selection = selection.clone();
            state.rows.clear();
            state.keywords.clear();
            state.loaded = false;
            state.query.clear();
            publish(&global, &state);
            publish_keywords(&global, &state.keywords);
            state.operation
        };
        // The account's own switch follows the file for the newly shown account.
        if let Ok(config) = self.store.load() {
            apply_account_switch(&global, &config, &account_id);
        }
        global.set_error("".into());
        global.set_notice("".into());
        global.set_defaults_busy(false);
        global.set_keywords_busy(false);
        global.set_keyword_error("".into());
        global.set_query("".into());
        global.set_keyword_input("".into());
        global.set_default_group(0);
        global.set_default_direct(0);
        global.set_default_group_mixed(false);
        global.set_default_direct_mixed(false);
        let Some(selection) = selection else {
            global.set_loading(false);
            return;
        };
        global.set_loading(true);
        let rules = self.clone();
        self.runtime.spawn(async move {
            let manager = rules.live.manager.clone();
            let (rooms, defaults, keywords) = tokio::join!(
                manager.room_notification_rules_selected(&selection),
                manager.notification_defaults_selected(&selection),
                manager.notification_keywords_selected(&selection),
            );
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                rules.apply_loaded(&ui, operation, &selection, rooms, defaults, keywords);
            });
        });
    }

    fn apply_loaded(
        self: &Arc<Self>,
        ui: &AppWindow,
        operation: u64,
        selection: &AccountSelection,
        rooms: Result<Vec<RoomNotificationRule>, MatrixCoreError>,
        defaults: Result<NotificationDefaults, MatrixCoreError>,
        keywords: Result<Vec<String>, MatrixCoreError>,
    ) {
        if !self.owns(ui, operation, selection) {
            return;
        }
        let global = ui.global::<NotificationRulesState>();
        global.set_loading(false);
        match keywords {
            Ok(keywords) => {
                self.state().keywords = keywords;
                publish_keywords(&global, &self.state().keywords);
            }
            Err(_) => global.set_keyword_error("Keywords could not be read.".into()),
        }
        let defaults_read = match defaults {
            Ok(defaults) => {
                apply_defaults(&global, defaults);
                true
            }
            Err(_) => false,
        };
        let mut state = self.state();
        match rooms {
            Ok(rooms) => {
                state.rows = rooms.into_iter().map(Row::new).collect();
                state.loaded = true;
                if !defaults_read {
                    global.set_error("Default levels could not be read. Try again later.".into());
                }
            }
            Err(_) => {
                global.set_error("Notification rules could not be read. Try again later.".into());
            }
        }
        publish(&global, &state);
        drop(state);
        if self.unconfirmed(selection.account_id(), None) {
            // What was just read is this device's copy, not yet confirmed.
            global.set_notice(NOT_SYNCED.into());
            self.await_fresh(operation, selection.clone(), None, self.sync_mark());
        }
    }

    fn search(&self, query: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let mut state = self.state();
        state.query = query
            .chars()
            .take(MAX_QUERY_CHARS)
            .collect::<String>()
            .trim()
            .to_lowercase();
        publish(&ui.global::<NotificationRulesState>(), &state);
    }

    fn choose_room_mode(self: &Arc<Self>, room_id: &str, choice: i32) {
        let mode = match choice {
            0 => None,
            1 => Some(NotificationMode::AllMessages),
            2 => Some(NotificationMode::MentionsAndKeywords),
            3 => Some(NotificationMode::Mute),
            _ => return,
        };
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<NotificationRulesState>();
        let (operation, selection) = {
            let mut state = self.state();
            let Some(selection) = state.selection.clone() else {
                return;
            };
            if !state.loaded
                || !selection.is_current()
                || ui.get_active_account_local_id().as_str() != selection.account_id()
            {
                return;
            }
            let Some(row) = state.rows.iter_mut().find(|row| row.room_id == room_id) else {
                return;
            };
            if row.pending || row.custom == mode {
                return;
            }
            row.pending = true;
            update_row(&global, row);
            (state.operation, selection)
        };
        global.set_error("".into());
        global.set_notice("".into());
        let rules = self.clone();
        let room_id = room_id.to_owned();
        self.runtime.spawn(async move {
            let manager = rules.live.manager.clone();
            let ticket = rules.begin_change(&selection, ChangeScope::Room(room_id.clone()));
            // Opened before the change, so a sync during or right after it is
            // not missed.
            let mut watch = manager.watch_push_rules_selected(&selection).await.ok();
            let result = manager
                .set_room_notification_mode_selected(&selection, &room_id, mode)
                .await;
            // Completion belongs to the account/scope, not this UI opening.
            let mark = rules.complete_change(ticket, result.is_ok());
            if let Some(watch) = &mut watch {
                watch.change_finished();
            }
            // Delivery follows an accepted rule even if this view has moved on.
            if let Ok(rule) = &result
                && selection.is_current()
            {
                rules.notifications.note_room_mode(
                    selection.account_id(),
                    &rule.room_id,
                    rule.effective,
                );
            }
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                rules.finish_room_mode(&ui, operation, &selection, &room_id, result, watch, mark);
            });
        });
    }

    fn finish_room_mode(
        self: &Arc<Self>,
        ui: &AppWindow,
        operation: u64,
        selection: &AccountSelection,
        room_id: &str,
        result: Result<RoomNotificationRule, MatrixCoreError>,
        watch: Option<PushRuleSyncWatch>,
        mark: SyncMark,
    ) {
        self.publish_uncertainty(ui);
        if let Ok(rule) = &result
            && selection.is_current()
        {
            // The header bell shows a confirmed level for the open room.
            super::room_panel::header_rule(ui, selection.account_id(), rule);
        }
        if !self.owns(ui, operation, selection) {
            return;
        }
        let global = ui.global::<NotificationRulesState>();
        let mut state = self.state();
        let Some(row) = state.rows.iter_mut().find(|row| row.room_id == room_id) else {
            return;
        };
        row.pending = false;
        match result {
            Ok(rule) => {
                row.custom = rule.custom;
                row.effective = rule.effective;
                global.set_notice(format!("Saved for {}.", row.name).into());
            }
            Err(MatrixCoreError::RoomNotFound(_)) => {
                global.set_error("That room is no longer joined.".into());
            }
            Err(_) => {
                global.set_error(ROOM_UNCONFIRMED.into());
                update_row(&global, row);
                drop(state);
                self.await_fresh(operation, selection.clone(), watch, mark);
                return;
            }
        }
        update_row(&global, row);
    }

    /// A level the room panel saved: the listed row takes it while this list
    /// shows the same account selection and the row has no change of its
    /// own waiting.
    pub(super) fn room_rule_saved(
        &self,
        ui: &AppWindow,
        selection: &AccountSelection,
        rule: &RoomNotificationRule,
    ) {
        let mut state = self.state();
        // Two current selections of one account are the same epoch.
        let same = state
            .selection
            .as_ref()
            .is_some_and(|own| own.is_current() && own.account_id() == selection.account_id());
        if !same || !state.loaded || !selection.is_current() {
            return;
        }
        let Some(row) = state
            .rows
            .iter_mut()
            .find(|row| row.room_id == rule.room_id)
        else {
            return;
        };
        if row.pending {
            return;
        }
        row.custom = rule.custom;
        row.effective = rule.effective;
        update_row(&ui.global::<NotificationRulesState>(), row);
    }

    fn choose_default(self: &Arc<Self>, direct: bool, choice: i32) {
        let mode = match choice {
            1 => NotificationMode::AllMessages,
            2 => NotificationMode::MentionsAndKeywords,
            _ => return,
        };
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<NotificationRulesState>();
        if global.get_defaults_busy() {
            return;
        }
        let (operation, selection) = {
            let state = self.state();
            let Some(selection) = state.selection.clone() else {
                return;
            };
            if !state.loaded
                || !selection.is_current()
                || ui.get_active_account_local_id().as_str() != selection.account_id()
            {
                return;
            }
            (state.operation, selection)
        };
        global.set_defaults_busy(true);
        global.set_error("".into());
        global.set_notice("".into());
        let rules = self.clone();
        self.runtime.spawn(async move {
            let manager = rules.live.manager.clone();
            let ticket = rules.begin_change(&selection, ChangeScope::Default(direct));
            let mut watch = manager.watch_push_rules_selected(&selection).await.ok();
            let result = manager
                .set_notification_default_selected(&selection, direct, mode)
                .await;
            let mark = rules.complete_change(ticket, result.is_ok());
            if let Some(watch) = &mut watch {
                watch.change_finished();
            }
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                rules.publish_uncertainty(&ui);
                if !rules.owns(&ui, operation, &selection) {
                    return;
                }
                let global = ui.global::<NotificationRulesState>();
                global.set_defaults_busy(false);
                match result {
                    Ok(defaults) => {
                        // Rooms following the default now show the new level.
                        rules.reload_levels(operation, selection.clone());
                        apply_defaults(&global, defaults);
                        global.set_notice("Default level saved.".into());
                    }
                    Err(_) => {
                        global.set_error(DEFAULT_UNCONFIRMED.into());
                        rules.await_fresh(operation, selection.clone(), watch, mark);
                    }
                }
            });
        });
    }

    /// Reread the rooms' levels after a default changed. The list, its
    /// search, the keywords and any room change still waiting for the
    /// homeserver stay as they are; only rows without a pending change take
    /// the levels read.
    fn reload_levels(self: &Arc<Self>, operation: u64, selection: AccountSelection) {
        let rules = self.clone();
        self.runtime.spawn(async move {
            let rooms = rules
                .live
                .manager
                .room_notification_rules_selected(&selection)
                .await;
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if !rules.owns(&ui, operation, &selection) {
                    return;
                }
                let global = ui.global::<NotificationRulesState>();
                let Ok(rooms) = rooms else {
                    global.set_error(
                        "The default level was saved, but room levels could not be reread.".into(),
                    );
                    return;
                };
                let mut state = rules.state();
                let changed = apply_levels(&mut state.rows, rooms);
                // Listed rows update in place, keeping keyboard focus.
                for index in changed {
                    update_row(&global, &state.rows[index]);
                }
            });
        });
    }
}

impl Rules {
    /// After a change the homeserver did not confirm, wait (bounded) for a
    /// sync observed after it, with `watch` opened before the change (or,
    /// without one, from now on). Only then are the levels, defaults and
    /// keywords read again and called the homeserver's rules; a timeout keeps
    /// the change unconfirmed and says so. Nothing is sent.
    fn await_fresh(
        self: &Arc<Self>,
        operation: u64,
        selection: AccountSelection,
        watch: Option<PushRuleSyncWatch>,
        mark: SyncMark,
    ) {
        let rules = self.clone();
        self.runtime.spawn(async move {
            let manager = rules.live.manager.clone();
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
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if fresh {
                    rules.synced(selection.account_id(), mark);
                }
                if !rules.owns(&ui, operation, &selection) {
                    return;
                }
                let global = ui.global::<NotificationRulesState>();
                let (text, unconfirmed) = after_wait(fresh);
                if unconfirmed {
                    global.set_error(text.into());
                    return;
                }
                if rules.unconfirmed(selection.account_id(), None) {
                    global.set_error(NOT_SYNCED.into());
                    return;
                }
                global.set_error("".into());
                global.set_keyword_error("".into());
                global.set_notice(text.into());
                rules.reload_all(operation, selection);
            });
        });
    }

    /// Read the levels, defaults and keywords again for the same view,
    /// keeping its search and any room change still waiting.
    fn reload_all(self: &Arc<Self>, operation: u64, selection: AccountSelection) {
        let rules = self.clone();
        self.runtime.spawn(async move {
            let manager = rules.live.manager.clone();
            let (defaults, keywords) = tokio::join!(
                manager.notification_defaults_selected(&selection),
                manager.notification_keywords_selected(&selection),
            );
            let weak = rules.ui.clone();
            let shown = rules.clone();
            let shown_selection = selection.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if !shown.owns(&ui, operation, &shown_selection) {
                    return;
                }
                let global = ui.global::<NotificationRulesState>();
                if let Ok(defaults) = defaults {
                    apply_defaults(&global, defaults);
                }
                if let Ok(keywords) = keywords {
                    shown.state().keywords = keywords;
                    publish_keywords(&global, &shown.state().keywords);
                }
            });
            rules.reload_levels(operation, selection);
        });
    }
}

/// Take the levels read for rows without a change waiting for the
/// homeserver; returns the rows that changed.
fn apply_levels(rows: &mut [Row], rooms: Vec<RoomNotificationRule>) -> Vec<usize> {
    let levels = rooms
        .into_iter()
        .map(|rule| (rule.room_id, (rule.custom, rule.effective)))
        .collect::<HashMap<_, _>>();
    let mut changed = Vec::new();
    for (index, row) in rows.iter_mut().enumerate() {
        if row.pending {
            continue;
        }
        if let Some(&(custom, effective)) = levels.get(&row.room_id)
            && (row.custom, row.effective) != (custom, effective)
        {
            row.custom = custom;
            row.effective = effective;
            changed.push(index);
        }
    }
    changed
}

impl Rules {
    fn add_keyword(self: &Arc<Self>, input: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<NotificationRulesState>();
        if global.get_keywords_busy() {
            return;
        }
        let keyword = match normalize_keyword(input) {
            Ok(keyword) => keyword,
            Err(message) => {
                global.set_keyword_error(message.into());
                return;
            }
        };
        let (operation, selection) = {
            let state = self.state();
            let Some(selection) = state.selection.clone() else {
                return;
            };
            if !state.loaded
                || !selection.is_current()
                || ui.get_active_account_local_id().as_str() != selection.account_id()
            {
                return;
            }
            let folded = keyword.to_lowercase();
            if state
                .keywords
                .iter()
                .any(|known| known.to_lowercase() == folded)
            {
                global.set_keyword_error("That keyword is already in the list.".into());
                return;
            }
            if state.keywords.len() >= MAX_NOTIFICATION_KEYWORDS {
                global.set_keyword_error("Add at most 32 keywords.".into());
                return;
            }
            (state.operation, selection)
        };
        self.change_keywords(&global, operation, selection, keyword, true);
    }

    /// `shown` is the sanitized text of a listed keyword; the stored rule it
    /// was made from is removed.
    fn remove_keyword(self: &Arc<Self>, shown: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<NotificationRulesState>();
        if global.get_keywords_busy() {
            return;
        }
        let (operation, selection, keyword) = {
            let state = self.state();
            let Some(selection) = state.selection.clone() else {
                return;
            };
            if !selection.is_current()
                || ui.get_active_account_local_id().as_str() != selection.account_id()
            {
                return;
            }
            let Some(keyword) = state
                .keywords
                .iter()
                .find(|keyword| display_keyword(keyword) == shown)
                .cloned()
            else {
                return;
            };
            (state.operation, selection, keyword)
        };
        self.change_keywords(&global, operation, selection, keyword, false);
    }

    fn change_keywords(
        self: &Arc<Self>,
        global: &NotificationRulesState<'_>,
        operation: u64,
        selection: AccountSelection,
        keyword: String,
        add: bool,
    ) {
        global.set_keywords_busy(true);
        global.set_keyword_error("".into());
        global.set_notice("".into());
        let rules = self.clone();
        self.runtime.spawn(async move {
            let manager = rules.live.manager.clone();
            let ticket = rules.begin_change(&selection, ChangeScope::Keywords);
            let mut watch = manager.watch_push_rules_selected(&selection).await.ok();
            let result = if add {
                manager
                    .add_notification_keyword_selected(&selection, &keyword)
                    .await
            } else {
                manager
                    .remove_notification_keyword_selected(&selection, &keyword)
                    .await
            };
            let mark = rules.complete_change(ticket, result.is_ok());
            if let Some(watch) = &mut watch {
                watch.change_finished();
            }
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                rules.publish_uncertainty(&ui);
                if !rules.owns(&ui, operation, &selection) {
                    return;
                }
                let global = ui.global::<NotificationRulesState>();
                global.set_keywords_busy(false);
                match result {
                    Ok(keywords) => {
                        rules.state().keywords = keywords;
                        publish_keywords(&global, &rules.state().keywords);
                        if add {
                            global.set_keyword_input("".into());
                        }
                        let notice = if add {
                            "Keyword saved."
                        } else {
                            "Keyword removed."
                        };
                        global.set_notice(notice.into());
                    }
                    Err(_) => {
                        global.set_keyword_error(
                            if add {
                                KEYWORD_ADD_UNCONFIRMED
                            } else {
                                KEYWORD_REMOVE_UNCONFIRMED
                            }
                            .into(),
                        );
                        rules.await_fresh(operation, selection.clone(), watch, mark);
                    }
                }
            });
        });
    }
}

/// Trim and collapse whitespace; refuse what a push-rule glob or the
/// notification text could misread.
fn normalize_keyword(input: &str) -> Result<String, &'static str> {
    if input.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
    }) {
        return Err("Keywords cannot contain control or direction characters.");
    }
    if input.contains(['*', '?', '[', ']']) {
        return Err("Keywords cannot contain *, ?, [ or ].");
    }
    let keyword = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if keyword.is_empty() {
        return Err("Enter a word or phrase.");
    }
    if keyword.chars().count() > MAX_NOTIFICATION_KEYWORD_CHARS {
        return Err("A keyword can be at most 64 characters.");
    }
    Ok(keyword)
}

/// Keywords may come from other clients; list them as plain text.
fn display_keyword(keyword: &str) -> String {
    crate::directory_plain_text(keyword, MAX_NOTIFICATION_KEYWORD_CHARS)
}

fn publish_keywords(global: &NotificationRulesState<'_>, keywords: &[String]) {
    global.set_keywords(ModelRc::new(VecModel::from(
        keywords
            .iter()
            .map(|keyword| SharedString::from(display_keyword(keyword)))
            .collect::<Vec<_>>(),
    )));
}

/// The privacy and account switches follow each accepted configuration.
pub(super) fn apply_config(ui: &AppWindow, config: &Config) {
    let global = ui.global::<NotificationRulesState>();
    global.set_show_room_and_sender(config.notifications.show_room_and_sender);
    apply_account_switch(&global, config, ui.get_active_account_local_id().as_str());
}

fn apply_account_switch(global: &NotificationRulesState<'_>, config: &Config, account_id: &str) {
    global.set_account_enabled(
        config
            .account_overrides
            .get(account_id)
            .and_then(|overrides| overrides.notifications_enabled)
            .unwrap_or(true),
    );
}

fn apply_defaults(global: &NotificationRulesState<'_>, defaults: NotificationDefaults) {
    global.set_default_group(level(defaults.group));
    global.set_default_direct(level(defaults.direct));
    global.set_default_group_mixed(defaults.group_mixed);
    global.set_default_direct_mixed(defaults.direct_mixed);
}

/// Publish the rows matching the search, at most `MAX_LISTED` of them.
fn publish(global: &NotificationRulesState<'_>, state: &State) {
    let matching = state
        .rows
        .iter()
        .filter(|row| state.query.is_empty() || row.folded.contains(&state.query))
        .collect::<Vec<_>>();
    global.set_total(state.rows.len().min(i32::MAX as usize) as i32);
    global.set_matching(matching.len().min(i32::MAX as usize) as i32);
    global.set_rooms(ModelRc::new(VecModel::from(
        matching
            .iter()
            .take(MAX_LISTED)
            .map(|row| row.item())
            .collect::<Vec<_>>(),
    )));
}

/// Update one listed row in place, keeping keyboard focus on its controls.
fn update_row(global: &NotificationRulesState<'_>, row: &Row) {
    let rooms = global.get_rooms();
    if let Some(index) = rooms
        .iter()
        .position(|item| item.room_id.as_str() == row.room_id)
    {
        rooms.set_row_data(index, row.item());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfirmed_changes_never_claim_what_the_homeserver_holds() {
        for text in [
            ROOM_UNCONFIRMED,
            DEFAULT_UNCONFIRMED,
            KEYWORD_ADD_UNCONFIRMED,
            KEYWORD_REMOVE_UNCONFIRMED,
        ] {
            let lower = text.to_lowercase();
            // A partly applied change is possible: no claim that nothing
            // changed, and no claim of a rollback.
            for claim in [
                "unchanged",
                "not changed",
                "keeps",
                "was not added",
                "rolled back",
            ] {
                assert!(!lower.contains(claim), "{text}");
            }
            assert!(lower.contains("may") && lower.contains("sync"), "{text}");
        }
        // The default level is two rules, and both can land.
        assert!(DEFAULT_UNCONFIRMED.contains("part or all of it may have reached it"));
    }

    #[test]
    fn only_an_observed_fresh_sync_ends_an_unconfirmed_change() {
        // A timeout keeps it unconfirmed and never calls the copy current.
        let (text, unconfirmed) = after_wait(false);
        assert!(unconfirmed);
        assert_eq!(text, NOT_SYNCED);
        assert!(text.contains("may differ") && !text.contains("what they say"));
        // A sync observed after the change does, and still keeps the doubt
        // about the change itself.
        let (text, unconfirmed) = after_wait(true);
        assert!(!unconfirmed);
        assert!(text.contains("may include part of that change"));
    }

    #[test]
    fn keywords_are_plain_bounded_text() {
        assert_eq!(normalize_keyword("  on   call ").as_deref(), Ok("on call"));
        for input in [
            "",
            "   ",
            "re*lease",
            "[urgent]",
            "who?",
            "a\u{202e}b",
            "a\tb",
        ] {
            assert!(normalize_keyword(input).is_err(), "{input:?}");
        }
        let long = "a".repeat(MAX_NOTIFICATION_KEYWORD_CHARS + 1);
        assert!(normalize_keyword(&long).is_err());
    }

    #[test]
    fn rows_show_sanitized_names_and_the_level_that_applies() {
        let row = Row::new(RoomNotificationRule {
            room_id: "!room:example.org".into(),
            name: "Team\u{202e} chat".into(),
            is_direct: false,
            custom: None,
            effective: NotificationMode::MentionsAndKeywords,
        });
        let item = row.item();
        assert!(!item.name.contains('\u{202e}'));
        assert_eq!((item.mode, item.effective), (0, 2));
        let muted = Row::new(RoomNotificationRule {
            room_id: "!quiet:example.org".into(),
            name: String::new(),
            is_direct: true,
            custom: Some(NotificationMode::Mute),
            effective: NotificationMode::Mute,
        });
        // An unnamed room is listed by its ID rather than as a blank row.
        assert_eq!(muted.item().name.as_str(), "!quiet:example.org");
        assert_eq!((muted.item().mode, muted.item().effective), (3, 3));
    }

    #[test]
    fn a_new_default_reaches_rows_without_a_pending_change() {
        let rule = |room: &str, effective| RoomNotificationRule {
            room_id: room.into(),
            name: room.into(),
            is_direct: false,
            custom: None,
            effective,
        };
        let all = NotificationMode::AllMessages;
        let mentions = NotificationMode::MentionsAndKeywords;
        let mut rows = vec![
            Row::new(rule("!a:example.org", all)),
            Row::new(rule("!b:example.org", all)),
        ];
        rows[1].pending = true;
        let read = vec![
            rule("!a:example.org", mentions),
            rule("!b:example.org", mentions),
        ];
        assert_eq!(apply_levels(&mut rows, read), vec![0]);
        assert_eq!(rows[0].effective, mentions);
        // A room whose own change waits for the homeserver keeps it.
        assert_eq!(rows[1].effective, all);
    }
}
