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
    NotificationDefaults, NotificationMode, RoomNotificationRule,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
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
        &self,
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
                        rules.reload_levels(operation, selection.clone());
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
            let result = if add {
                manager
                    .add_notification_keyword_selected(&selection, &keyword)
                    .await
            } else {
                manager
                    .remove_notification_keyword_selected(&selection, &keyword)
                    .await
            };
            let weak = rules.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
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
                    Err(_) => global.set_keyword_error(
                        if add {
                            "The homeserver did not confirm the keyword; it was not added."
                        } else {
                            "The homeserver did not confirm removing the keyword."
                        }
                        .into(),
                    ),
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
    fn keywords_are_plain_bounded_text() {
        assert_eq!(normalize_keyword("  on   call ").as_deref(), Ok("on call"));
        for input in ["", "   ", "re*lease", "[urgent]", "who?", "a\u{202e}b", "a\tb"] {
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
