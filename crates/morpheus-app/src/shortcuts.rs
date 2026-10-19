//! Application shortcuts share the config validator and normal navigation callbacks.
//! The Slint focus scope bubbles keys here only after the focused control declines
//! them; text movement, editing, and modal focus management keep precedence.

use std::collections::BTreeMap;

use morpheus_config::{
    Config, ConfigStore, Shortcut, ShortcutAction, ShortcutKey, ShortcutModifiers,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, platform::Key};

use crate::{AppWindow, RoomItem};

// Keep every candidate visible even at 200% text scale in the minimum window.
const PICKER_LIMIT: usize = 6;

pub fn apply_config(ui: &AppWindow, config: &Config) {
    let binding = |action: ShortcutAction| {
        config
            .shortcuts
            .get(action.name())
            .map(String::as_str)
            .unwrap_or("")
            .into()
    };
    ui.set_shortcut_quick_switcher(binding(ShortcutAction::QuickSwitcher));
    ui.set_shortcut_search(binding(ShortcutAction::Search));
    ui.set_shortcut_next_unread(binding(ShortcutAction::NextUnread));
    ui.set_shortcut_previous_unread(binding(ShortcutAction::PreviousUnread));
    if !ui.get_shortcuts_dirty() {
        reset_editor(ui);
    }
}

fn reset_editor(ui: &AppWindow) {
    ui.set_shortcut_edit_quick_switcher(ui.get_shortcut_quick_switcher());
    ui.set_shortcut_edit_search(ui.get_shortcut_search());
    ui.set_shortcut_edit_next_unread(ui.get_shortcut_next_unread());
    ui.set_shortcut_edit_previous_unread(ui.get_shortcut_previous_unread());
    ui.set_shortcuts_dirty(false);
}

fn editor_bindings(
    values: [SharedString; 4],
) -> Result<BTreeMap<String, String>, morpheus_config::ConfigError> {
    let mut bindings = BTreeMap::new();
    for (action, value) in ShortcutAction::ALL.into_iter().zip(values) {
        if !value.trim().is_empty() {
            bindings.insert(
                action.name().into(),
                Shortcut::parse(value.trim())?.canonical(),
            );
        }
    }
    morpheus_config::validate_shortcuts(&bindings)?;
    Ok(bindings)
}

fn save_bindings(ui: &AppWindow, store: &ConfigStore, bindings: BTreeMap<String, String>) {
    if crate::save_config_change(ui, store, |config| config.shortcuts = bindings).is_some() {
        reset_editor(ui);
        ui.set_shortcut_error("".into());
        ui.set_shortcut_notice("Shortcuts saved.".into());
    } else {
        ui.set_shortcut_error(ui.get_config_error());
        ui.set_shortcut_notice("".into());
    }
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

fn physical_modifiers(mut modifiers: ShortcutModifiers, macos: bool) -> ShortcutModifiers {
    // Slint 1.18 swaps Control and Meta on macOS to make native editing portable.
    if macos {
        std::mem::swap(&mut modifiers.control, &mut modifiers.meta);
    }
    modifiers
}

fn match_action(
    bindings: &[(ShortcutAction, SharedString)],
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
    bindings.iter().find_map(|(action, value)| {
        let shortcut = Shortcut::parse(value).ok()?;
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
        || ui.get_roster_confirm_open()
        || ui.get_space_manager_view().confirm_open
        || ui.get_native_capture_picker_open()
        || ui.get_leave_call_dialog()
        || !ui.get_security_recovery_key().is_empty()
        || ui.get_quick_switcher_open()
        || ui.get_image_preview_open()
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

pub fn register(ui: &AppWindow, store: ConfigStore) {
    let weak = ui.as_weak();
    ui.on_dispatch_shortcut(move |text, control, alt, shift, meta| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        let bindings = [
            (
                ShortcutAction::QuickSwitcher,
                ui.get_shortcut_quick_switcher(),
            ),
            (ShortcutAction::Search, ui.get_shortcut_search()),
            (ShortcutAction::NextUnread, ui.get_shortcut_next_unread()),
            (
                ShortcutAction::PreviousUnread,
                ui.get_shortcut_previous_unread(),
            ),
        ];
        let action = match_action(
            &bindings,
            &text,
            ShortcutModifiers {
                control,
                alt,
                shift,
                meta,
            },
            cfg!(target_os = "macos"),
            blocked(&ui),
        );
        match action {
            Some(ShortcutAction::QuickSwitcher) => open_picker(&ui),
            Some(ShortcutAction::Search)
                if ui.get_live_session() && !ui.get_active_room_id().is_empty() =>
            {
                ui.invoke_focus_room_search();
            }
            Some(ShortcutAction::NextUnread | ShortcutAction::PreviousUnread) => {
                let backwards = action == Some(ShortcutAction::PreviousUnread);
                if let Some(room) =
                    unread_room(&visible_rooms(&ui), &ui.get_active_room_id(), backwards)
                {
                    ui.set_page(0);
                    ui.invoke_open_room(room);
                }
            }
            _ => return false,
        }
        true
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
    let save_store = store.clone();
    ui.on_save_shortcuts(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match editor_bindings([
            ui.get_shortcut_edit_quick_switcher(),
            ui.get_shortcut_edit_search(),
            ui.get_shortcut_edit_next_unread(),
            ui.get_shortcut_edit_previous_unread(),
        ]) {
            Ok(bindings) => save_bindings(&ui, &save_store, bindings),
            Err(error) => {
                ui.set_shortcut_error(error.to_string().into());
                ui.set_shortcut_notice("".into());
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_reset_shortcuts(move || {
        if let Some(ui) = weak.upgrade() {
            save_bindings(&ui, &store, morpheus_config::default_shortcuts());
        }
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

    #[test]
    fn dispatch_matches_exact_modifiers_with_slints_macos_mapping() {
        let bindings = [(ShortcutAction::QuickSwitcher, "Mod+K".into())];
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
        let control = [(ShortcutAction::Search, "Ctrl+F".into())];
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
        let arrow = [(ShortcutAction::NextUnread, "Alt+Down".into())];
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
    fn editor_disables_blanks_normalizes_and_rejects_batch_conflicts() {
        let bindings =
            editor_bindings(["mod+j".into(), " Mod+F ".into(), "".into(), "  ".into()]).unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings["quick_switcher"], "Mod+J");
        assert!(editor_bindings(["Mod+K".into(), "Ctrl+K".into(), "".into(), "".into()]).is_err());
        assert!(editor_bindings(["Mod+Tab".into(), "".into(), "".into(), "".into()]).is_err());
    }
}
