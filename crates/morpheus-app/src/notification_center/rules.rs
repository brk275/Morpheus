//! Settings › Notifications: the banner privacy switch, the account's default
//! levels and searchable per-room rules.
//!
//! Levels are the account's Matrix push rules, read from and written to the
//! homeserver through the selection-fenced core API. Every request captures
//! the visible account's selection and an operation number on the UI thread;
//! a result is shown only while both are still current. A change is shown as
//! saved only after the homeserver accepted it. Errors use fixed wording and
//! never repeat server text.

use std::sync::{Arc, Mutex, MutexGuard, atomic::Ordering};

use morpheus_config::{Config, ConfigStore};
use morpheus_matrix::{
    AccountSelection, MatrixCoreError, NotificationDefaults, NotificationMode,
    RoomNotificationRule,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use tokio::runtime::Handle;

use crate::{
    AppWindow, NotificationRoomItem, NotificationRulesState, live::LiveUi,
    notifications::DesktopNotifications,
};

/// Rows listed at once; a search narrows the rest.
const MAX_LISTED: usize = 100;
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

fn level(mode: NotificationMode) -> i32 {
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
}

pub(super) struct Rules {
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    ui: Weak<AppWindow>,
    runtime: Handle,
    state: Mutex<State>,
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
            state: Mutex::new(State::default()),
        });
        let global = ui.global::<NotificationRulesState>();
        let refresh = rules.clone();
        global.on_refresh(move || refresh.refresh(false));
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
        rules
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Whether a result for `operation` under `selection` may still be shown.
    fn owns(&self, ui: &AppWindow, operation: u64, selection: &AccountSelection) -> bool {
        self.state().operation == operation
            && selection.is_current()
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == selection.account_id()
    }

    /// Load the visible account's rules. A reopened section starts with an
    /// empty search; a reload after changing a default keeps it.
    fn refresh(self: &Arc<Self>, keep_query: bool) {
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
            state.loaded = false;
            if !keep_query {
                state.query.clear();
            }
            publish(&global, &state);
            state.operation
        };
        global.set_error("".into());
        global.set_notice("".into());
        global.set_defaults_busy(false);
        if !keep_query {
            global.set_default_group(0);
            global.set_default_direct(0);
            global.set_default_group_mixed(false);
            global.set_default_direct_mixed(false);
        }
        let Some(selection) = selection else {
            global.set_loading(false);
            return;
        };
        global.set_loading(true);
        let rules = self.clone();
        self.runtime.spawn(async move {
            let manager = rules.live.manager.clone();
            let (rooms, defaults) = tokio::join!(
                manager.room_notification_rules_selected(&selection),
                manager.notification_defaults_selected(&selection),
            );
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                rules.apply_loaded(&ui, operation, &selection, rooms, defaults);
            });
        });
    }

    fn apply_loaded(
        &self,
        ui: &AppWindow,
        operation: u64,
        selection: &AccountSelection,
        rooms: Result<Vec<RoomNotificationRule>, MatrixCoreError>,
        defaults: Result<NotificationDefaults, MatrixCoreError>,
    ) {
        if !self.owns(ui, operation, selection) {
            return;
        }
        let global = ui.global::<NotificationRulesState>();
        global.set_loading(false);
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
            let result = rules
                .live
                .manager
                .set_room_notification_mode_selected(&selection, &room_id, mode)
                .await;
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
                rules.finish_room_mode(&ui, operation, &selection, &room_id, result);
            });
        });
    }

    fn finish_room_mode(
        &self,
        ui: &AppWindow,
        operation: u64,
        selection: &AccountSelection,
        room_id: &str,
        result: Result<RoomNotificationRule, MatrixCoreError>,
    ) {
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
                global.set_error(
                    "The homeserver did not confirm the change; the room keeps its shown level."
                        .into(),
                );
            }
        }
        update_row(&global, row);
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
            let result = rules
                .live
                .manager
                .set_notification_default_selected(&selection, direct, mode)
                .await;
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if !rules.owns(&ui, operation, &selection) {
                    return;
                }
                let global = ui.global::<NotificationRulesState>();
                global.set_defaults_busy(false);
                match result {
                    Ok(defaults) => {
                        // Rooms following the default now show the new level.
                        rules.refresh(true);
                        apply_defaults(&global, defaults);
                        global.set_notice("Default level saved.".into());
                    }
                    Err(_) => global.set_error(
                        "The homeserver did not confirm the default level; it is unchanged here."
                            .into(),
                    ),
                }
            });
        });
    }
}

/// The privacy switch follows each accepted configuration.
pub(super) fn apply_config(ui: &AppWindow, config: &Config) {
    ui.global::<NotificationRulesState>()
        .set_show_room_and_sender(config.notifications.show_room_and_sender);
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
}
