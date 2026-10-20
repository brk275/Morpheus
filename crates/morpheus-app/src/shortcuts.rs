//! Application shortcuts share the config validator and normal navigation callbacks.
//! The Slint focus scope bubbles keys here only after the focused control declines
//! them; text movement, editing, and modal focus management keep precedence.
//! Bindings are parsed once per accepted configuration, never per key press.
//!
//! The editor lists every registry action. A combination is recorded from a
//! key press or typed, validated by the same parser as `config.toml`, and
//! saved at once through the re-read file; a conflict names the other action
//! and saves nothing. Undo restores the previous table only while the file
//! still holds the table this window saved. No action sends, removes, logs
//! out, switches accounts or changes a protected setting.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use morpheus_config::{
    Config, ConfigStore, Shortcut, ShortcutAction, ShortcutKey, ShortcutModifiers,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, platform::Key};

use crate::{AppWindow, RoomItem, ShortcutItem};

// Keep every candidate visible even at 200% text scale in the minimum window.
const PICKER_LIMIT: usize = 6;
const MAX_FILTER_CHARS: usize = 64;
const STALE_UNDO: &str =
    "The shortcuts were changed outside this window after your change; nothing was undone.";

#[derive(Default)]
struct Editor {
    /// The table before and after this window's last change.
    undo: Option<(BTreeMap<String, String>, BTreeMap<String, String>)>,
    message: String,
    error: bool,
    /// The bindings of the last accepted configuration.
    source: BTreeMap<String, String>,
}

thread_local! {
    // Slint callbacks and projections run on the UI thread only.
    static ACTIVE: RefCell<Vec<(ShortcutAction, Shortcut)>> = const { RefCell::new(Vec::new()) };
    static EDITOR: RefCell<Editor> = RefCell::new(Editor::default());
    // Rows update in place while the shown actions are unchanged, so a
    // focused field or button in the list keeps its focus after a save or a
    // reload; a different set of actions replaces them.
    static ROWS: Rc<VecModel<ShortcutItem>> = Rc::new(VecModel::default());
}

pub fn apply_config(ui: &AppWindow, config: &Config) {
    // Accepted configurations are validated, so every binding parses.
    let active = ShortcutAction::ALL
        .into_iter()
        .filter_map(|action| {
            let value = config.shortcuts.get(action.name())?;
            Some((action, Shortcut::parse(value).ok()?))
        })
        .collect();
    ACTIVE.with(|bindings| *bindings.borrow_mut() = active);
    EDITOR.with(|editor| {
        let mut editor = editor.borrow_mut();
        // An edit made elsewhere ends this window's undo step.
        if editor
            .undo
            .as_ref()
            .is_some_and(|(_, after)| *after != config.shortcuts)
        {
            editor.undo = None;
        }
        editor.source = config.shortcuts.clone();
    });
    project(ui);
}

fn set_message(text: impl Into<String>, error: bool) {
    EDITOR.with(|editor| {
        let mut editor = editor.borrow_mut();
        editor.message = text.into();
        editor.error = error;
    });
}

/// Every action matching every word of the filter, in registry order.
fn shown_actions(filter: &str, bindings: &BTreeMap<String, String>) -> Vec<ShortcutAction> {
    let filter = filter.trim().to_lowercase();
    let words: Vec<&str> = filter.split_whitespace().collect();
    ShortcutAction::ALL
        .into_iter()
        .filter(|action| {
            let haystack = format!(
                "{} {} {} {}",
                action.label(),
                action.group().label(),
                action.name().replace('_', " "),
                bindings.get(action.name()).map_or("", String::as_str)
            )
            .to_lowercase();
            words.iter().all(|word| haystack.contains(word))
        })
        .collect()
}

fn project(ui: &AppWindow) {
    EDITOR.with(|editor| {
        let editor = editor.borrow();
        let items: Vec<ShortcutItem> =
            shown_actions(ui.get_shortcut_filter().as_str(), &editor.source)
                .into_iter()
                .map(|action| {
                    let binding = editor.source.get(action.name()).map_or("", String::as_str);
                    let default = action.default_binding().unwrap_or("");
                    ShortcutItem {
                        action: action.name().into(),
                        label: action.label().into(),
                        group: action.group().label().into(),
                        binding: binding.into(),
                        default_binding: default.into(),
                        customized: binding != default,
                    }
                })
                .collect();
        ROWS.with(|model| {
            let same_actions = model.row_count() == items.len()
                && model
                    .iter()
                    .zip(&items)
                    .all(|(shown, item)| shown.action == item.action);
            if same_actions {
                for (index, item) in items.into_iter().enumerate() {
                    if model.row_data(index).as_ref() != Some(&item) {
                        model.set_row_data(index, item);
                    }
                }
            } else {
                model.set_vec(items);
            }
            ui.set_shortcut_items(ModelRc::from(model.clone()));
        });
        ui.set_shortcut_message(editor.message.as_str().into());
        ui.set_shortcut_message_error(editor.error);
        ui.set_shortcut_can_undo(editor.undo.is_some());
    });
}

/// Save one action's binding, or disable it with `None`, decided on the
/// re-read file. A combination already used by another action saves nothing
/// and names that action.
fn set_binding(
    ui: &AppWindow,
    store: &ConfigStore,
    action: ShortcutAction,
    value: Option<Shortcut>,
) {
    let mut conflict = None;
    let mut step = None;
    let saved = crate::save_config_edit(ui, store, |config| {
        if let Some(shortcut) = &value {
            conflict = ShortcutAction::ALL.into_iter().find(|other| {
                *other != action
                    && config
                        .shortcuts
                        .get(other.name())
                        .and_then(|bound| Shortcut::parse(bound).ok())
                        .is_some_and(|bound| bound.conflicts_with(shortcut))
            });
            if conflict.is_some() {
                return false;
            }
        }
        let before = config.shortcuts.clone();
        match &value {
            Some(shortcut) => {
                config
                    .shortcuts
                    .insert(action.name().into(), shortcut.canonical());
            }
            None => {
                config.shortcuts.remove(action.name());
            }
        }
        if config.shortcuts == before {
            return false;
        }
        step = Some((before, config.shortcuts.clone()));
        true
    });
    match (conflict, saved, step) {
        (Some(other), _, _) => set_message(
            format!(
                "{} is already used by {}. Change or disable that shortcut first.",
                value
                    .map(|shortcut| shortcut.canonical())
                    .unwrap_or_default(),
                other.label()
            ),
            true,
        ),
        (None, Some(_), Some(step)) => {
            EDITOR.with(|editor| editor.borrow_mut().undo = Some(step));
            set_message(
                match &value {
                    Some(shortcut) => {
                        format!("{} is now {}.", action.label(), shortcut.canonical())
                    }
                    None => format!("{} has no shortcut.", action.label()),
                },
                false,
            );
        }
        (None, None, Some(_)) => set_message(ui.get_config_error().to_string(), true),
        // Nothing changed, or the file could not be read.
        (None, _, None) => {
            let error = ui.get_config_error();
            if error.is_empty() {
                set_message("", false);
            } else {
                set_message(error.to_string(), true);
            }
        }
    }
    project(ui);
}

/// The registry action an editor callback names; any other text is ignored.
fn named_action(name: &str) -> Option<ShortcutAction> {
    ShortcutAction::parse(name)
}

fn key_from_slint(text: &str) -> Option<ShortcutKey> {
    let mut characters = text.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    if character.is_ascii_alphanumeric() {
        return Some(ShortcutKey::Character(character.to_ascii_uppercase()));
    }
    for (key, mapped) in [
        (Key::UpArrow, ShortcutKey::Up),
        (Key::DownArrow, ShortcutKey::Down),
        (Key::LeftArrow, ShortcutKey::Left),
        (Key::RightArrow, ShortcutKey::Right),
        (Key::Home, ShortcutKey::Home),
        (Key::End, ShortcutKey::End),
        (Key::PageUp, ShortcutKey::PageUp),
        (Key::PageDown, ShortcutKey::PageDown),
        (Key::F1, ShortcutKey::Function(1)),
        (Key::F2, ShortcutKey::Function(2)),
        (Key::F3, ShortcutKey::Function(3)),
        (Key::F4, ShortcutKey::Function(4)),
        (Key::F5, ShortcutKey::Function(5)),
        (Key::F6, ShortcutKey::Function(6)),
        (Key::F7, ShortcutKey::Function(7)),
        (Key::F8, ShortcutKey::Function(8)),
        (Key::F9, ShortcutKey::Function(9)),
        (Key::F10, ShortcutKey::Function(10)),
        (Key::F11, ShortcutKey::Function(11)),
        (Key::F12, ShortcutKey::Function(12)),
    ] {
        if character == char::from(key) {
            return Some(mapped);
        }
    }
    None
}

/// Whether a key press is only a modifier, which recording waits past.
fn is_modifier_key(text: &str) -> bool {
    [
        Key::Shift,
        Key::ShiftR,
        Key::Control,
        Key::ControlR,
        Key::Alt,
        Key::AltGr,
        Key::Meta,
        Key::MetaR,
    ]
    .into_iter()
    .any(|key| text == SharedString::from(key).as_str())
}

fn physical_modifiers(mut modifiers: ShortcutModifiers, macos: bool) -> ShortcutModifiers {
    // Slint 1.18 swaps Control and Meta on macOS to make native editing portable.
    if macos {
        std::mem::swap(&mut modifiers.control, &mut modifiers.meta);
    }
    modifiers
}

/// The portable spelling of a pressed combination: the platform's primary
/// modifier is written `Mod`, so the binding works on every platform.
fn recorded_combination(key: ShortcutKey, pressed: ShortcutModifiers, macos: bool) -> String {
    let modifiers = physical_modifiers(pressed, macos);
    let (primary, other, other_name) = if macos {
        (modifiers.meta, modifiers.control, "Ctrl")
    } else {
        (modifiers.control, modifiers.meta, "Meta")
    };
    let mut parts: Vec<String> = Vec::new();
    for (held, name) in [
        (primary, "Mod"),
        (other, other_name),
        (modifiers.alt, "Alt"),
        (modifiers.shift, "Shift"),
    ] {
        if held {
            parts.push(name.into());
        }
    }
    parts.push(match key {
        ShortcutKey::Character(character) => character.to_string(),
        ShortcutKey::Up => "Up".into(),
        ShortcutKey::Down => "Down".into(),
        ShortcutKey::Left => "Left".into(),
        ShortcutKey::Right => "Right".into(),
        ShortcutKey::Home => "Home".into(),
        ShortcutKey::End => "End".into(),
        ShortcutKey::PageUp => "PageUp".into(),
        ShortcutKey::PageDown => "PageDown".into(),
        ShortcutKey::Function(number) => format!("F{number}"),
    });
    parts.join("+")
}

/// The validator's reason, without the field path or any rejected value.
fn parse_problem(value: &str) -> Result<Shortcut, String> {
    Shortcut::parse(value).map_err(|error| match error {
        morpheus_config::ConfigError::Field { problem, .. } => {
            format!("That combination cannot be a shortcut: {problem}.")
        }
        _ => "That combination cannot be a shortcut.".into(),
    })
}

fn match_action(
    bindings: &[(ShortcutAction, Shortcut)],
    text: &str,
    modifiers: ShortcutModifiers,
    macos: bool,
    blocked: bool,
) -> Option<ShortcutAction> {
    if blocked {
        return None;
    }
    let key = key_from_slint(text)?;
    let modifiers = physical_modifiers(modifiers, macos);
    bindings.iter().find_map(|(action, shortcut)| {
        (shortcut.key == key && shortcut.modifiers(macos) == modifiers).then_some(*action)
    })
}

fn blocked(ui: &AppWindow) -> bool {
    ui.get_show_sign_in()
        || ui.get_page() == 1
        || ui.get_account_menu_open()
        || ui.get_manage_accounts_open()
        || ui.get_access_open()
        || ui.get_room_actions_open()
        || ui.get_room_admin_open()
        || ui.get_member_confirm_open()
        || ui.get_native_capture_picker_open()
        || ui.get_leave_call_dialog()
        || !ui.get_security_recovery_key().is_empty()
        || ui.get_quick_switcher_open()
        || ui.get_people_open()
        || crate::space_explorer::is_open(ui)
        || crate::matrix_links::is_open(ui)
        || ui.get_account_profile_open()
        || crate::member_privacy::is_open(ui)
        || crate::space_management::is_open(ui)
        || crate::room_addresses::is_open(ui)
        || ui.get_image_preview_open()
        || !ui.get_shortcut_recording().is_empty()
        || ui.get_poll_dialog_open()
        || ui.global::<crate::RoomNotificationState>().get_open()
        || ui.global::<crate::RoomActivity>().get_seen_open()
}

fn visible_rooms(ui: &AppWindow) -> Vec<RoomItem> {
    crate::navigation::visible_rooms(ui)
}

fn unread_room(rooms: &[RoomItem], current: &str, backwards: bool) -> Option<SharedString> {
    if rooms.is_empty() {
        return None;
    }
    let start = rooms.iter().position(|room| room.id == current);
    (1..=rooms.len()).find_map(|distance| {
        let index = match (start, backwards) {
            (Some(start), false) => (start + distance) % rooms.len(),
            (Some(start), true) => (start + rooms.len() - distance) % rooms.len(),
            (None, false) => distance - 1,
            (None, true) => rooms.len() - distance,
        };
        let room = &rooms[index];
        (room.unread > 0 && room.id != current).then(|| room.id.clone())
    })
}

/// The neighbouring visible room, wrapping at either end; with no current
/// room, the first or the last one.
fn adjacent_room(rooms: &[RoomItem], current: &str, backwards: bool) -> Option<SharedString> {
    let count = rooms.len();
    let index = match (rooms.iter().position(|room| room.id == current), backwards) {
        (_, _) if count == 0 => return None,
        (Some(index), false) => (index + 1) % count,
        (Some(index), true) => (index + count - 1) % count,
        (None, false) => 0,
        (None, true) => count - 1,
    };
    let room = &rooms[index];
    (room.id != current).then(|| room.id.clone())
}

fn matching_rooms(rooms: Vec<RoomItem>, query: &str) -> (Vec<RoomItem>, usize) {
    let query = query.trim().to_lowercase();
    let mut matching: Vec<_> = rooms
        .into_iter()
        .filter(|room| {
            room.name.to_lowercase().contains(&query) || room.id.to_lowercase().contains(&query)
        })
        .collect();
    let count = matching.len();
    matching.truncate(PICKER_LIMIT);
    (matching, count)
}

fn update_picker(ui: &AppWindow) {
    let (matching, count) = matching_rooms(visible_rooms(ui), &ui.get_quick_switcher_query());
    ui.set_quick_switcher_match_count(count as i32);
    ui.set_quick_switcher_results(ModelRc::new(VecModel::from(matching)));
    ui.set_quick_switcher_index(if count == 0 { -1 } else { 0 });
}

fn open_picker(ui: &AppWindow) {
    if blocked(ui) {
        return;
    }
    // A link, and a joined link's message still to come, end first.
    crate::matrix_links::clear(ui);
    ui.set_quick_switcher_account_id(ui.get_active_account_local_id());
    ui.set_quick_switcher_account_index(ui.get_active_account_index());
    ui.set_quick_switcher_space_id(ui.get_active_space_id());
    ui.set_quick_switcher_query("".into());
    update_picker(ui);
    ui.set_quick_switcher_open(true);
}

fn picker_context_current(ui: &AppWindow) -> bool {
    // Do not rely on deferred Slint `changed` handlers to retire a snapshot:
    // two accounts may both contain the same Matrix room ID.
    ui.get_quick_switcher_account_id() == ui.get_active_account_local_id()
        && ui.get_quick_switcher_account_index() == ui.get_active_account_index()
        && ui.get_quick_switcher_space_id() == ui.get_active_space_id()
}

/// Run one matched action. Returns whether it applied; an action without a
/// target here, such as Search with no open room, leaves the key unused.
fn run(ui: &AppWindow, action: ShortcutAction) -> bool {
    let in_room = ui.get_live_session() && !ui.get_active_room_id().is_empty();
    match action {
        ShortcutAction::QuickSwitcher => open_picker(ui),
        ShortcutAction::Search if in_room => ui.invoke_focus_room_search(),
        ShortcutAction::NextUnread | ShortcutAction::PreviousUnread => {
            let backwards = action == ShortcutAction::PreviousUnread;
            let Some(room) = unread_room(&visible_rooms(ui), &ui.get_active_room_id(), backwards)
            else {
                return false;
            };
            ui.set_page(0);
            ui.invoke_open_room(room);
        }
        // Always the conversation view: cycling never joins a voice space.
        ShortcutAction::NextRoom | ShortcutAction::PreviousRoom => {
            let backwards = action == ShortcutAction::PreviousRoom;
            let Some(room) = adjacent_room(&visible_rooms(ui), &ui.get_active_room_id(), backwards)
            else {
                return false;
            };
            ui.set_page(0);
            ui.invoke_open_room(room);
        }
        ShortcutAction::GoHome => ui.invoke_go_home(),
        // Like the Settings entries, which also refresh the security status.
        ShortcutAction::OpenSettings => {
            crate::matrix_links::clear(ui);
            ui.set_page(1);
            ui.invoke_open_security();
        }
        ShortcutAction::SearchSettings => {
            ui.invoke_focus_settings_search();
            ui.invoke_open_security();
        }
        ShortcutAction::FocusComposer if in_room && ui.get_page() == 0 => {
            ui.invoke_focus_composer(false)
        }
        ShortcutAction::OpenEmojiPicker
            if in_room && ui.get_mention_ui_allowed() && !ui.get_composer_busy() =>
        {
            ui.invoke_open_emoji(false)
        }
        ShortcutAction::ToggleDetailsPanel => ui.invoke_set_side_panel(!ui.get_show_side_panel()),
        ShortcutAction::ShowMembersPanel => ui.invoke_show_members_panel(),
        ShortcutAction::ShowThreadPanel if !ui.get_thread_root_id().is_empty() => {
            ui.invoke_show_thread_panel()
        }
        // Opens the menu and focuses its trigger; every entry still needs
        // its own activation and confirmation.
        ShortcutAction::OpenAccountMenu => ui.invoke_open_account_menu(),
        _ => return false,
    }
    true
}

pub fn register(ui: &AppWindow, store: ConfigStore) {
    let weak = ui.as_weak();
    ui.on_dispatch_shortcut(move |text, control, alt, shift, meta| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        let modifiers = ShortcutModifiers {
            control,
            alt,
            shift,
            meta,
        };
        let action = ACTIVE.with(|bindings| {
            match_action(
                &bindings.borrow(),
                &text,
                modifiers,
                cfg!(target_os = "macos"),
                blocked(&ui),
            )
        });
        action.is_some_and(|action| run(&ui, action))
    });

    let weak = ui.as_weak();
    ui.on_open_quick_switcher(move || {
        if let Some(ui) = weak.upgrade() {
            open_picker(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_filter_quick_switcher(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_quick_switcher_open() && picker_context_current(&ui) {
                update_picker(&ui);
            } else {
                ui.set_quick_switcher_open(false);
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_choose_quick_room(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !ui.get_quick_switcher_open() {
            return;
        }
        if !picker_context_current(&ui) {
            ui.set_quick_switcher_open(false);
            return;
        }
        let Some(room) = usize::try_from(index)
            .ok()
            .and_then(|index| ui.get_quick_switcher_results().row_data(index))
        else {
            return;
        };
        // The model may have changed while the picker was open. Only current
        // visible rooms can navigate, through the same account-bound callback.
        if !visible_rooms(&ui)
            .iter()
            .any(|current| current.id == room.id)
        {
            update_picker(&ui);
            return;
        }
        ui.invoke_close_quick_switcher();
        ui.set_page(0);
        ui.invoke_open_room(room.id);
    });

    let weak = ui.as_weak();
    ui.on_shortcut_filter_changed(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let bounded: String = text.chars().take(MAX_FILTER_CHARS).collect();
        if bounded.as_str() != text.as_str() {
            ui.set_shortcut_filter(bounded.into());
        }
        project(&ui);
    });
    let weak = ui.as_weak();
    ui.on_shortcut_record(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        if let Some(action) = named_action(name.as_str()) {
            ui.set_shortcut_recording(action.name().into());
            set_message(
                format!(
                    "Press the new combination for {}, or Escape to cancel.",
                    action.label()
                ),
                false,
            );
            project(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_shortcut_cancel_recording(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_shortcut_recording(SharedString::new());
        set_message("", false);
        project(&ui);
    });
    let weak = ui.as_weak();
    let record_store = store.clone();
    ui.on_shortcut_recorded(move |name, text, control, alt, shift, meta| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        let Some(action) = named_action(name.as_str()) else {
            return false;
        };
        // Wait past modifier-only presses for the key itself.
        if is_modifier_key(text.as_str()) {
            return true;
        }
        let Some(key) = key_from_slint(text.as_str()) else {
            set_message(
                "Use A–Z, 0–9, an arrow, Home, End, PageUp, PageDown or F1–F12 with Mod, Ctrl, Meta or Alt.",
                true,
            );
            project(&ui);
            return true;
        };
        let pressed = ShortcutModifiers {
            control,
            alt,
            shift,
            meta,
        };
        let combination = recorded_combination(key, pressed, cfg!(target_os = "macos"));
        match parse_problem(&combination) {
            Ok(shortcut) => {
                ui.set_shortcut_recording(SharedString::new());
                set_binding(&ui, &record_store, action, Some(shortcut));
            }
            // Keep recording so another combination can be tried.
            Err(reason) => {
                set_message(reason, true);
                project(&ui);
            }
        }
        true
    });
    let weak = ui.as_weak();
    let typed_store = store.clone();
    ui.on_shortcut_typed(move |name, value| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(action) = named_action(name.as_str()) else {
            return;
        };
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        match parse_problem(value) {
            Ok(shortcut) => set_binding(&ui, &typed_store, action, Some(shortcut)),
            Err(reason) => {
                set_message(reason, true);
                project(&ui);
            }
        }
    });
    let weak = ui.as_weak();
    let disable_store = store.clone();
    ui.on_shortcut_disable(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        if let Some(action) = named_action(name.as_str()) {
            set_binding(&ui, &disable_store, action, None);
        }
    });
    let weak = ui.as_weak();
    let default_store = store.clone();
    ui.on_shortcut_use_default(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(action) = named_action(name.as_str()) else {
            return;
        };
        let default = action
            .default_binding()
            .and_then(|value| Shortcut::parse(value).ok());
        set_binding(&ui, &default_store, action, default);
    });
    let weak = ui.as_weak();
    let undo_store = store.clone();
    ui.on_shortcut_undo(move || {
        let Some(ui) = weak.upgrade() else { return };
        let step = EDITOR.with(|editor| editor.borrow_mut().undo.take());
        let Some((before, after)) = step else { return };
        let mut superseded = false;
        let saved = crate::save_config_edit(&ui, &undo_store, |config| {
            // Undo only the change this window made, not a later one.
            if config.shortcuts != after {
                superseded = true;
                return false;
            }
            config.shortcuts = before.clone();
            true
        });
        if superseded {
            set_message(STALE_UNDO, true);
        } else if saved.is_some() {
            set_message("The last shortcut change was undone.", false);
        } else {
            // Keep the step when the file could not be changed.
            EDITOR.with(|editor| editor.borrow_mut().undo = Some((before, after)));
            set_message(ui.get_config_error().to_string(), true);
        }
        project(&ui);
    });
    let weak = ui.as_weak();
    ui.on_reset_shortcuts(move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut step = None;
        let saved = crate::save_config_edit(&ui, &store, |config| {
            let defaults = morpheus_config::default_shortcuts();
            if config.shortcuts == defaults {
                return false;
            }
            step = Some((
                std::mem::replace(&mut config.shortcuts, defaults.clone()),
                defaults,
            ));
            true
        });
        if let (Some(_), Some(step)) = (saved, step) {
            EDITOR.with(|editor| editor.borrow_mut().undo = Some(step));
            set_message("Every shortcut uses its default again.", false);
        } else if !ui.get_config_error().is_empty() {
            set_message(ui.get_config_error().to_string(), true);
        }
        project(&ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, name: &str, unread: i32) -> RoomItem {
        RoomItem {
            id: id.into(),
            name: name.into(),
            unread,
            ..Default::default()
        }
    }

    fn parsed(entries: &[(ShortcutAction, &str)]) -> Vec<(ShortcutAction, Shortcut)> {
        entries
            .iter()
            .map(|(action, value)| (*action, Shortcut::parse(value).unwrap()))
            .collect()
    }

    #[test]
    fn dispatch_matches_exact_modifiers_with_slints_macos_mapping() {
        let bindings = parsed(&[(ShortcutAction::QuickSwitcher, "Mod+K")]);
        let primary = ShortcutModifiers {
            control: true,
            ..Default::default()
        };
        for macos in [false, true] {
            assert_eq!(
                match_action(&bindings, "k", primary, macos, false),
                Some(ShortcutAction::QuickSwitcher)
            );
            assert_eq!(match_action(&bindings, "K", primary, macos, true), None);
            assert_eq!(
                match_action(
                    &bindings,
                    "k",
                    ShortcutModifiers {
                        shift: true,
                        ..primary
                    },
                    macos,
                    false
                ),
                None
            );
            for text in ["", "Escape", "é", "kk", "\t", "\u{1b}"] {
                assert_eq!(match_action(&bindings, text, primary, macos, false), None);
            }
        }
        let control = parsed(&[(ShortcutAction::Search, "Ctrl+F")]);
        assert_eq!(match_action(&control, "f", primary, true, false), None);
        assert_eq!(
            match_action(
                &control,
                "f",
                ShortcutModifiers {
                    meta: true,
                    ..Default::default()
                },
                true,
                false
            ),
            Some(ShortcutAction::Search)
        );
        let arrow = parsed(&[(ShortcutAction::NextUnread, "Alt+Down")]);
        assert_eq!(
            match_action(
                &arrow,
                &SharedString::from(Key::DownArrow),
                ShortcutModifiers {
                    alt: true,
                    ..Default::default()
                },
                false,
                false
            ),
            Some(ShortcutAction::NextUnread)
        );
    }

    #[test]
    fn unread_navigation_wraps_skips_read_and_current_and_handles_missing_selection() {
        let rooms = vec![room("a", "A", 1), room("b", "B", 0), room("c", "C", 2)];
        assert_eq!(unread_room(&rooms, "a", false).as_deref(), Some("c"));
        assert_eq!(unread_room(&rooms, "a", true).as_deref(), Some("c"));
        assert_eq!(unread_room(&rooms, "c", false).as_deref(), Some("a"));
        assert_eq!(unread_room(&rooms, "missing", true).as_deref(), Some("c"));
        assert_eq!(unread_room(&rooms, "missing", false).as_deref(), Some("a"));
        assert!(unread_room(&rooms[..1], "a", false).is_none());
        assert!(unread_room(&[], "a", false).is_none());
    }

    #[test]
    fn room_cycling_wraps_in_displayed_order_and_never_reopens_the_current_room() {
        let rooms = vec![room("a", "A", 0), room("b", "B", 0), room("c", "C", 0)];
        assert_eq!(adjacent_room(&rooms, "a", false).as_deref(), Some("b"));
        assert_eq!(adjacent_room(&rooms, "c", false).as_deref(), Some("a"));
        assert_eq!(adjacent_room(&rooms, "a", true).as_deref(), Some("c"));
        assert_eq!(
            adjacent_room(&rooms, "missing", false).as_deref(),
            Some("a")
        );
        assert_eq!(adjacent_room(&rooms, "missing", true).as_deref(), Some("c"));
        assert!(adjacent_room(&rooms[..1], "a", false).is_none());
        assert!(adjacent_room(&[], "a", true).is_none());
    }

    #[test]
    fn picker_filters_names_and_ids_and_bounds_visible_results() {
        let rooms = (0..12)
            .map(|index| room(&format!("!room{index}:example.org"), "Design", 0))
            .collect::<Vec<_>>();
        let (items, total) = matching_rooms(rooms.clone(), " DESIGN ");
        assert_eq!((items.len(), total), (PICKER_LIMIT, 12));
        let (items, total) = matching_rooms(rooms.clone(), "room11:");
        assert_eq!(total, 1);
        assert_eq!(items[0].id, "!room11:example.org");
        assert_eq!(matching_rooms(rooms, "absent").1, 0);
    }

    #[test]
    fn recorded_keys_become_portable_validated_combinations() {
        let primary = ShortcutModifiers {
            control: true,
            ..Default::default()
        };
        // Slint reports the primary modifier as control on every platform.
        for macos in [false, true] {
            let combination = recorded_combination(ShortcutKey::Character('J'), primary, macos);
            assert_eq!(combination, "Mod+J");
            assert_eq!(Shortcut::parse(&combination).unwrap().canonical(), "Mod+J");
        }
        let shifted = ShortcutModifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            recorded_combination(ShortcutKey::Down, shifted, false),
            "Alt+Shift+Down"
        );
        // The other platform modifier keeps its own name.
        let meta = ShortcutModifiers {
            meta: true,
            ..Default::default()
        };
        assert_eq!(
            recorded_combination(ShortcutKey::Function(5), meta, false),
            "Meta+F5"
        );
        assert_eq!(
            recorded_combination(ShortcutKey::Function(5), meta, true),
            "Ctrl+F5"
        );
        // Editing and window keys stay reserved, with the validator's reason.
        for reserved in ["Mod+C", "Mod+V", "Alt+A", "K", "Mod+Shift+1"] {
            let reason = parse_problem(reserved).unwrap_err();
            assert!(reason.starts_with("That combination cannot be a shortcut"));
            assert!(!reason.contains("shortcuts."), "{reason}");
        }
        assert!(is_modifier_key(SharedString::from(Key::Shift).as_str()));
        assert!(!is_modifier_key("k"));
    }

    #[test]
    fn every_action_has_editor_metadata_and_the_filter_matches_every_word() {
        let defaults = morpheus_config::default_shortcuts();
        for action in ShortcutAction::ALL {
            assert!(!action.label().is_empty());
            let default = action.default_binding().unwrap();
            assert_eq!(
                defaults.get(action.name()).map(String::as_str),
                Some(default)
            );
            assert_eq!(Shortcut::parse(default).unwrap().canonical(), default);
        }
        let names = |filter: &str| {
            shown_actions(filter, &defaults)
                .into_iter()
                .map(ShortcutAction::name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("details panel"), ["toggle_details_panel"]);
        assert_eq!(names("mod+k"), ["quick_switcher"]);
        assert_eq!(names("ACCOUNT"), ["open_account_menu"]);
        assert_eq!(names("").len(), ShortcutAction::ALL.len());
        assert!(names("no such shortcut").is_empty());
    }
}
