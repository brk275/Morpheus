//! The theme studio's transactional draft. Look edits are held in memory
//! over the accepted settings file, keyed by their exact `config.toml`
//! paths. Each edit remembers the value the file held when it was first
//! made, so Apply can tell a concurrent change of the same setting (a
//! conflict, which the person resolves) from an unrelated one (kept). Undo
//! and redo are bounded. Account-scoped edits belong to one account and are
//! dropped, never moved, when another account becomes active. This module is
//! pure; `super` drives the window.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use morpheus_config::{AccountOverrides, Config};
use serde_json::{Map, Value};

/// Undo and redo steps kept per session; the oldest step is dropped first.
pub(crate) const MAX_HISTORY: usize = 64;

/// Sections whose fields make the look. Excluded below are the accessibility
/// choices that stay with the person (text size, Reduce motion, the focus
/// ring) and room organization (grouping and unread-first sorting); those
/// keep saving at once.
const SECTIONS: &[&str] = &[
    "appearance",
    "typography",
    "geometry",
    "chat",
    "conversation",
    "layout",
    "window",
];
const EXCLUDED: &[(&str, &str)] = &[
    ("appearance", "scale_percent"),
    ("appearance", "reduce_motion"),
    ("geometry", "focus_width"),
    ("layout", "group_direct_messages"),
    ("layout", "sort_unread_first"),
];

/// One look setting, by its place in `config.toml`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LookKey {
    /// `[section] field`.
    Field(&'static str, String),
    /// `[colors] token`.
    Color(String),
    /// The whole `[senders]` table.
    Senders,
    /// `[account_overrides.<id>] theme`.
    AccountTheme(String),
    /// `[account_overrides.<id>.colors] token`.
    AccountColor(String, String),
    /// `[account_overrides.<id>.room_appearance.<room>] theme`.
    RoomTheme(String, String),
    /// `[account_overrides.<id>.room_appearance.<room>.colors] token`.
    RoomColor(String, String, String),
}

/// The studio's reset groups, in the order the window lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    Theme,
    Typography,
    Geometry,
    Messages,
    Layout,
    Window,
    Colors,
    Senders,
}

impl Group {
    pub(crate) const ALL: [Group; 8] = [
        Group::Theme,
        Group::Typography,
        Group::Geometry,
        Group::Messages,
        Group::Layout,
        Group::Window,
        Group::Colors,
        Group::Senders,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Group::Theme => "Theme and motion",
            Group::Typography => "Fonts and type sizes",
            Group::Geometry => "Shapes and spacing",
            Group::Messages => "Messages and header",
            Group::Layout => "Panes and navigation",
            Group::Window => "Window transparency",
            Group::Colors => "Colors for all accounts",
            Group::Senders => "Sender name colors",
        }
    }
}

impl LookKey {
    /// The local account an account-scoped key belongs to.
    pub(crate) fn account(&self) -> Option<&str> {
        match self {
            LookKey::AccountTheme(id)
            | LookKey::AccountColor(id, _)
            | LookKey::RoomTheme(id, _)
            | LookKey::RoomColor(id, _, _) => Some(id),
            _ => None,
        }
    }

    pub(crate) fn group(&self) -> Group {
        match self {
            LookKey::Field("appearance", _) | LookKey::AccountTheme(_) | LookKey::RoomTheme(..) => {
                Group::Theme
            }
            LookKey::Field("typography", _) => Group::Typography,
            LookKey::Field("geometry", _) => Group::Geometry,
            LookKey::Field("layout", _) => Group::Layout,
            LookKey::Field("window", _) => Group::Window,
            LookKey::Field(_, _) => Group::Messages,
            LookKey::Color(_) | LookKey::AccountColor(_, _) | LookKey::RoomColor(..) => {
                Group::Colors
            }
            LookKey::Senders => Group::Senders,
        }
    }

    /// Whether this key chooses the theme mode or pack, so a preview must
    /// load the theme pack the draft names.
    pub(crate) fn chooses_theme(&self) -> bool {
        match self {
            LookKey::AccountTheme(_) | LookKey::RoomTheme(..) => true,
            LookKey::Field("appearance", field) => field == "theme" || field == "theme_pack",
            _ => false,
        }
    }
}

/// The look of a configuration: every look key it sets. A key it omits (an
/// absent color override, an unset optional field) is absent.
pub(crate) type Look = BTreeMap<LookKey, Value>;

/// The value of `key` in `look`; absent is `null`.
pub(crate) fn value<'a>(look: &'a Look, key: &LookKey) -> &'a Value {
    look.get(key).unwrap_or(&Value::Null)
}

pub(crate) fn look(config: &Config) -> Look {
    let mut look = Look::new();
    let Ok(Value::Object(root)) = serde_json::to_value(config) else {
        return look;
    };
    for &section in SECTIONS {
        let Some(Value::Object(fields)) = root.get(section) else {
            continue;
        };
        for (field, value) in fields {
            let excluded = EXCLUDED
                .iter()
                .any(|&(skip_section, skip_field)| skip_section == section && skip_field == field);
            if excluded || value.is_null() {
                continue;
            }
            look.insert(LookKey::Field(section, field.clone()), value.clone());
        }
    }
    if let Some(Value::Object(colors)) = root.get("colors") {
        for (token, value) in colors {
            look.insert(LookKey::Color(token.clone()), value.clone());
        }
    }
    if let Some(senders) = root.get("senders").filter(|value| !value.is_null()) {
        look.insert(LookKey::Senders, senders.clone());
    }
    if let Some(Value::Object(accounts)) = root.get("account_overrides") {
        for (id, overrides) in accounts {
            if let Some(theme) = overrides.get("theme").filter(|value| !value.is_null()) {
                look.insert(LookKey::AccountTheme(id.clone()), theme.clone());
            }
            if let Some(Value::Object(colors)) = overrides.get("colors") {
                for (token, value) in colors {
                    look.insert(
                        LookKey::AccountColor(id.clone(), token.clone()),
                        value.clone(),
                    );
                }
            }
            if let Some(Value::Object(rooms)) = overrides.get("room_appearance") {
                for (room, appearance) in rooms {
                    if let Some(theme) = appearance.get("theme").filter(|value| !value.is_null()) {
                        look.insert(LookKey::RoomTheme(id.clone(), room.clone()), theme.clone());
                    }
                    if let Some(Value::Object(colors)) = appearance.get("colors") {
                        for (token, value) in colors {
                            look.insert(
                                LookKey::RoomColor(id.clone(), room.clone(), token.clone()),
                                value.clone(),
                            );
                        }
                    }
                }
            }
        }
    }
    look
}

/// Why a draft could not be laid over a configuration. It carries no value.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OverlayError;

fn object<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let slot = parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut().expect("just made an object")
}

fn put(map: &mut Map<String, Value>, key: &str, value: &Value) {
    if value.is_null() {
        map.remove(key);
    } else {
        map.insert(key.to_owned(), value.clone());
    }
}

/// `base` with each key of `values` set to its value (`null` removes an
/// override or clears an optional field). Account override entries the
/// values leave empty are removed, as the color editor does.
pub(crate) fn set_look<'a>(
    base: &Config,
    values: impl IntoIterator<Item = (&'a LookKey, &'a Value)>,
) -> Result<Config, OverlayError> {
    let Ok(Value::Object(mut root)) = serde_json::to_value(base) else {
        return Err(OverlayError);
    };
    let mut accounts = BTreeSet::new();
    for (key, value) in values {
        match key {
            LookKey::Field(section, field) => {
                // Optional fields take `null`; every other field always has
                // a value in a look.
                object(&mut root, section).insert(field.clone(), value.clone());
            }
            LookKey::Color(token) => put(object(&mut root, "colors"), token, value),
            LookKey::Senders => put(&mut root, "senders", value),
            LookKey::AccountTheme(id) => {
                accounts.insert(id.clone());
                let overrides = object(object(&mut root, "account_overrides"), id);
                overrides.insert("theme".to_owned(), value.clone());
            }
            LookKey::AccountColor(id, token) => {
                accounts.insert(id.clone());
                let overrides = object(object(&mut root, "account_overrides"), id);
                put(object(overrides, "colors"), token, value);
            }
            LookKey::RoomTheme(id, room) => {
                accounts.insert(id.clone());
                let overrides = object(object(&mut root, "account_overrides"), id);
                let appearance = object(object(overrides, "room_appearance"), room);
                appearance.insert("theme".to_owned(), value.clone());
            }
            LookKey::RoomColor(id, room, token) => {
                accounts.insert(id.clone());
                let overrides = object(object(&mut root, "account_overrides"), id);
                let appearance = object(object(overrides, "room_appearance"), room);
                put(object(appearance, "colors"), token, value);
            }
        }
    }
    let mut config: Config =
        serde_json::from_value(Value::Object(root)).map_err(|_| OverlayError)?;
    for id in accounts {
        // A room left with nothing of its own inherits everything; its entry
        // goes, and so does an account entry left empty.
        if let Some(overrides) = config.account_overrides.get_mut(&id) {
            overrides.room_appearance.retain(|_, room| !room.is_empty());
        }
        if config.account_overrides.get(&id) == Some(&AccountOverrides::default()) {
            config.account_overrides.remove(&id);
        }
    }
    Ok(config)
}

/// `target` with the look of `source` and everything else of `target`.
pub(crate) fn with_look_of(target: &Config, source: &Config) -> Result<Config, OverlayError> {
    let wanted = look(source);
    let present = look(target);
    let keys: BTreeSet<&LookKey> = wanted.keys().chain(present.keys()).collect();
    set_look(target, keys.into_iter().map(|key| (key, value(&wanted, key))))
}

/// `config` with every key of `group` back to its built-in value. Colors
/// reset only the table for all accounts; account colors have their own
/// Reset all in the color editor.
pub(crate) fn reset_group(config: &Config, group: Group) -> Result<Config, OverlayError> {
    let defaults = look(&Config::default());
    let present = look(config);
    let keys: BTreeSet<&LookKey> = defaults
        .keys()
        .chain(present.keys())
        .filter(|key| key.group() == group && key.account().is_none())
        .collect();
    set_look(config, keys.into_iter().map(|key| (key, value(&defaults, key))))
}

/// One drafted setting: the value the file held when it was first edited
/// in this session, and the value the person chose.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Edit {
    pub(crate) base: Value,
    pub(crate) value: Value,
}

pub(crate) type Edits = BTreeMap<LookKey, Edit>;

/// `config` with the drafted values.
pub(crate) fn overlay(config: &Config, edits: &Edits) -> Result<Config, OverlayError> {
    set_look(config, edits.iter().map(|(key, edit)| (key, &edit.value)))
}

/// How Apply treats settings changed both here and in the file meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// Write this window's values.
    KeepMine,
    /// Keep the file's values for those settings; the rest is written.
    KeepFile,
}

#[derive(Default)]
pub(crate) struct Draft {
    edits: Edits,
    undo: VecDeque<Edits>,
    redo: VecDeque<Edits>,
}

fn push_bounded(stack: &mut VecDeque<Edits>, edits: Edits) {
    stack.push_back(edits);
    while stack.len() > MAX_HISTORY {
        stack.pop_front();
    }
}

impl Draft {
    pub(crate) fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
    pub(crate) fn len(&self) -> usize {
        self.edits.len()
    }
    pub(crate) fn edits(&self) -> &Edits {
        &self.edits
    }
    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub(crate) fn chooses_theme(&self) -> bool {
        self.edits.keys().any(LookKey::chooses_theme)
    }

    /// Record one edit: the shown look went from `before` to `after` while
    /// the file held `file`. A setting set back to the file's value is no
    /// longer drafted. Returns whether the draft changed.
    pub(crate) fn record(&mut self, file: &Look, before: &Look, after: &Look) -> bool {
        let mut next = self.edits.clone();
        let keys: BTreeSet<&LookKey> = before.keys().chain(after.keys()).collect();
        for key in keys {
            let new = value(after, key);
            if value(before, key) == new {
                continue;
            }
            let held = value(file, key);
            if new == held {
                next.remove(key);
            } else if let Some(edit) = next.get_mut(key) {
                edit.value = new.clone();
            } else {
                next.insert(
                    key.clone(),
                    Edit {
                        base: held.clone(),
                        value: new.clone(),
                    },
                );
            }
        }
        if next == self.edits {
            return false;
        }
        let previous = std::mem::replace(&mut self.edits, next);
        push_bounded(&mut self.undo, previous);
        self.redo.clear();
        true
    }

    pub(crate) fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop_back() else {
            return false;
        };
        let current = std::mem::replace(&mut self.edits, previous);
        push_bounded(&mut self.redo, current);
        true
    }

    pub(crate) fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop_back() else {
            return false;
        };
        let current = std::mem::replace(&mut self.edits, next);
        push_bounded(&mut self.undo, current);
        true
    }

    pub(crate) fn discard(&mut self) {
        *self = Self::default();
    }

    /// Drafted settings the file has meanwhile changed to a third value.
    pub(crate) fn conflicts(&self, file: &Look) -> Vec<LookKey> {
        self.edits
            .iter()
            .filter(|(key, edit)| {
                let held = value(file, key);
                *held != edit.base && *held != edit.value
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// The edits Apply writes over `file`.
    pub(crate) fn resolved(&self, file: &Look, resolution: Resolution) -> Edits {
        let conflicts: BTreeSet<LookKey> = match resolution {
            Resolution::KeepMine => BTreeSet::new(),
            Resolution::KeepFile => self.conflicts(file).into_iter().collect(),
        };
        self.edits
            .iter()
            .filter(|(key, _)| !conflicts.contains(*key))
            .map(|(key, edit)| (key.clone(), edit.clone()))
            .collect()
    }

    /// Drop every account-scoped edit that is not for `account`, from the
    /// draft and its history, so undo can never bring one back. Returns how
    /// many drafted settings were dropped.
    pub(crate) fn retain_account(&mut self, account: &str) -> usize {
        let keep = |edits: &mut Edits| {
            edits.retain(|key, _| key.account().is_none_or(|owner| owner == account))
        };
        let before = self.edits.len();
        keep(&mut self.edits);
        self.undo.iter_mut().for_each(keep);
        self.redo.iter_mut().for_each(keep);
        before - self.edits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(section: &'static str, name: &str) -> LookKey {
        LookKey::Field(section, name.to_owned())
    }

    #[test]
    fn the_look_leaves_out_accessibility_and_room_organization() {
        let look = look(&Config::default());
        assert!(look.contains_key(&field("appearance", "theme")));
        assert!(look.contains_key(&field("geometry", "corner_radius")));
        assert!(look.contains_key(&field("window", "opacity_percent")));
        for &(section, name) in EXCLUDED {
            assert!(!look.contains_key(&field(section, name)), "{section}.{name}");
        }
        let behavior = |key: &LookKey| matches!(key, LookKey::Field("notifications" | "media", _));
        assert!(!look.keys().any(behavior));
    }

    #[test]
    fn overlay_round_trips_and_keeps_everything_else() {
        let mut file = Config::default();
        file.notifications.enabled = false;
        file.colors.insert("link".into(), "#112233".into());
        let mut edited = file.clone();
        edited.geometry.corner_radius = 3;
        edited.colors.remove("link");
        edited.colors.insert("canvas".into(), "#445566AA".into());
        edited
            .account_overrides
            .entry("acct".into())
            .or_default()
            .colors
            .insert("accent".into(), "#778899".into());
        let mut draft = Draft::default();
        assert!(draft.record(&look(&file), &look(&file), &look(&edited)));
        let shown = overlay(&file, draft.edits()).unwrap();
        assert_eq!(shown, edited);
        // Notifications and other behavior are never part of the draft.
        assert!(!shown.notifications.enabled);
        // Setting a value back to the file's removes it from the draft.
        assert!(draft.record(&look(&file), &look(&edited), &look(&file)));
        assert!(draft.is_empty());
        assert_eq!(overlay(&file, draft.edits()).unwrap(), file);
    }

    #[test]
    fn undo_and_redo_are_bounded_and_a_new_edit_clears_redo() {
        let file = Config::default();
        let mut draft = Draft::default();
        let mut shown = file.clone();
        for radius in 0..(MAX_HISTORY as u16 + 10) {
            let mut next = shown.clone();
            next.geometry.corner_radius = radius + 1;
            assert!(draft.record(&look(&file), &look(&shown), &look(&next)));
            shown = next;
        }
        let mut steps = 0;
        while draft.undo() {
            steps += 1;
        }
        assert_eq!(steps, MAX_HISTORY);
        assert!(draft.redo());
        let current = overlay(&file, draft.edits()).unwrap();
        let mut next = current.clone();
        next.typography.message_size += 1;
        assert!(draft.record(&look(&file), &look(&current), &look(&next)));
        assert!(!draft.can_redo());
    }

    #[test]
    fn apply_conflicts_only_on_settings_changed_to_a_third_value() {
        let base = Config::default();
        let mut mine = base.clone();
        mine.geometry.corner_radius = 2;
        mine.typography.message_size = 18;
        let mut draft = Draft::default();
        draft.record(&look(&base), &look(&base), &look(&mine));

        let mut file = base.clone();
        // Same setting, another value: a conflict.
        file.geometry.corner_radius = 9;
        // Unrelated setting: kept, never overwritten.
        file.chat.show_avatars = !file.chat.show_avatars;
        // Same setting, same value: no conflict.
        file.typography.message_size = 18;
        let file_look = look(&file);
        assert_eq!(
            draft.conflicts(&file_look),
            vec![field("geometry", "corner_radius")]
        );

        let keep_mine = draft.resolved(&file_look, Resolution::KeepMine);
        let mine_wins = overlay(&file, &keep_mine).unwrap();
        assert_eq!(mine_wins.geometry.corner_radius, 2);
        assert_eq!(mine_wins.chat.show_avatars, file.chat.show_avatars);
        let keep_file = draft.resolved(&file_look, Resolution::KeepFile);
        let file_wins = overlay(&file, &keep_file).unwrap();
        assert_eq!(file_wins.geometry.corner_radius, 9);
        assert_eq!(file_wins.typography.message_size, 18);
    }

    #[test]
    fn another_accounts_edits_are_dropped_from_the_draft_and_its_history() {
        let file = Config::default();
        let mut first = file.clone();
        first
            .account_overrides
            .entry("first".into())
            .or_default()
            .colors
            .insert("accent".into(), "#010203".into());
        first.geometry.corner_radius = 4;
        let mut draft = Draft::default();
        draft.record(&look(&file), &look(&file), &look(&first));
        let mut second = first.clone();
        second.geometry.corner_radius = 5;
        draft.record(&look(&file), &look(&first), &look(&second));

        assert_eq!(draft.retain_account("second"), 1);
        assert_eq!(draft.len(), 1);
        while draft.undo() {
            assert!(draft.edits().keys().all(|key| key.account().is_none()));
        }
        let shown = overlay(&file, draft.edits()).unwrap();
        assert!(shown.account_overrides.is_empty());
    }

    #[test]
    fn group_reset_touches_only_its_group() {
        let mut config = Config::default();
        config.geometry.corner_radius = 1;
        config.typography.message_size = 20;
        config.colors.insert("link".into(), "#123456".into());
        let reset = reset_group(&config, Group::Geometry).unwrap();
        assert_eq!(reset.geometry, Config::default().geometry);
        assert_eq!(reset.typography.message_size, 20);
        assert_eq!(reset.colors.get("link").map(String::as_str), Some("#123456"));
        let reset = reset_group(&config, Group::Colors).unwrap();
        assert!(reset.colors.is_empty());
    }

    #[test]
    fn with_look_of_moves_only_the_look() {
        let mut look_source = Config::default();
        look_source.geometry.corner_radius = 7;
        let mut target = Config::default();
        target.notifications.sounds = !target.notifications.sounds;
        target.geometry.corner_radius = 1;
        let merged = with_look_of(&target, &look_source).unwrap();
        assert_eq!(merged.geometry.corner_radius, 7);
        assert_eq!(merged.notifications.sounds, target.notifications.sounds);
    }

    #[test]
    fn theme_choices_are_recognized() {
        assert!(field("appearance", "theme").chooses_theme());
        assert!(field("appearance", "theme_pack").chooses_theme());
        assert!(LookKey::AccountTheme("a".into()).chooses_theme());
        assert!(!field("appearance", "font_family").chooses_theme());
        assert!(!LookKey::Color("link".into()).chooses_theme());
    }

    #[test]
    fn room_keys_draft_like_account_keys_and_belong_to_their_account() {
        let room = "!harbor:example.org";
        let config = Config::default();
        let theme = LookKey::RoomTheme("a".into(), room.into());
        let color = LookKey::RoomColor("a".into(), room.into(), "link".into());
        let light = Value::from("light");
        let link = Value::from("#123456");
        let set = set_look(&config, [(&theme, &light), (&color, &link)]).unwrap();
        let shown = set.room_appearance(Some("a"), Some(room)).unwrap();
        assert_eq!(shown.theme, Some(morpheus_config::ThemeMode::Light));
        assert_eq!(shown.colors["link"], "#123456");
        let found = look(&set);
        assert_eq!(found.get(&theme), Some(&light));
        assert_eq!(found.get(&color), Some(&link));
        assert_eq!(theme.group(), Group::Theme);
        assert_eq!(color.group(), Group::Colors);
        assert!(theme.chooses_theme());
        assert_eq!(color.account(), Some("a"));
        // Removing both leaves no empty room or account entry.
        let cleared = set_look(&set, [(&theme, &Value::Null), (&color, &Value::Null)]).unwrap();
        assert!(cleared.account_overrides.is_empty());
        // A room's drafted settings go with their account, never to another.
        let mut draft = Draft::default();
        assert!(draft.record(&look(&config), &look(&config), &look(&set)));
        assert_eq!(draft.len(), 2);
        assert_eq!(draft.retain_account("b"), 2);
        assert!(draft.is_empty());
    }
}
