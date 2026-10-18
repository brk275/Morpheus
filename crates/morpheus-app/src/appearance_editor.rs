//! The Appearance workspace: color groups, one selected token, hex entry and
//! sparse `[colors]` overrides in `config.toml`.
//!
//! A complete valid `#RRGGBB` or `#RRGGBBAA` value is saved at once through
//! the same path as every other Settings change, which re-reads the file
//! first, so an invalid hand edit is never overwritten. Partial or invalid
//! input only shows a hint; the last valid appearance stays applied. Undo is
//! presentation state of this window; the file remains the record.
//!
//! The applied configuration can lag the file by up to one watcher poll, so
//! every write is decided on the re-read file: each undo step and the Reset
//! all snapshot are taken there, inside the save, and kept only once the save
//! succeeded. Undo applies only while the color still holds the value this
//! window saved, so it never overwrites a later edit made elsewhere.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use morpheus_config::registry::{self, COLOR_TOKENS, ColorToken, Fallback, TokenGroup, TokenSource};
use morpheus_config::{Config, ConfigStore, ThemePack};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{
    AppWindow, ColorEditorState, ColorGroupItem, ColorTokenItem, StyleTokens, save_config_change,
    save_config_edit,
};

const HEX_HINT: &str = "Use #RRGGBB or #RRGGBBAA, with alpha last. The last valid color stays applied.";
const EMPTY_HINT: &str = "Type a color, or use Reset to return to the theme or built-in color.";
const STALE_UNDO_HINT: &str =
    "This color was changed outside this window after your change; nothing was undone.";
const MAX_FILTER_CHARS: usize = 64;
/// The group-list entry after the color groups: the window settings.
const WINDOW_GROUP: usize = TokenGroup::ALL.len();

#[derive(Default)]
enum ResetAll {
    #[default]
    Idle,
    /// The next activation removes every override.
    Confirm,
    /// Overrides removed by the last Reset all; the next activation restores them.
    Removed(BTreeMap<String, String>),
}

/// One token's last change in this window, as found in and written to the file.
struct UndoStep {
    before: Option<String>,
    after: Option<String>,
}

#[derive(Default)]
struct Session {
    /// The last change of each token in this window.
    previous: HashMap<&'static str, UndoStep>,
    hint: String,
    reset_all: ResetAll,
    /// The configuration and theme last applied, for projecting selection
    /// and filter changes without reading the file.
    source: Option<(Config, Option<ThemePack>)>,
}

thread_local! {
    // Slint callbacks and projections run on the UI thread only.
    static SESSION: RefCell<Session> = RefCell::new(Session::default());
}

pub(super) fn register(ui: &AppWindow, store: &ConfigStore) {
    ui.set_color_groups(ModelRc::new(VecModel::from(group_items())));
    let weak = ui.as_weak();
    ui.on_color_filter_changed(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let bounded: String = text.chars().take(MAX_FILTER_CHARS).collect();
        if bounded.as_str() != text.as_str() {
            ui.set_color_filter(bounded.into());
        }
        reproject(&ui, false);
    });
    let weak = ui.as_weak();
    ui.on_color_group_chosen(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else { return };
        if index == WINDOW_GROUP {
            crate::settings_search::reveal(&ui, "appearance.window_opacity");
            return;
        }
        let Some(group) = TokenGroup::ALL.get(index) else { return };
        ui.set_color_group_index(index as i32);
        ui.set_color_filter(SharedString::new());
        if let Some(first) = COLOR_TOKENS.iter().find(|token| token.group == *group) {
            ui.set_color_selected_id(first.id.into());
        }
        clear_hint();
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    ui.on_color_token_chosen(move |id| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(token) = registry::color_token(id.as_str()) else { return };
        ui.set_color_selected_id(token.id.into());
        clear_hint();
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    let save_store = store.clone();
    ui.on_color_hex_edited(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(token) = registry::color_token(ui.get_color_selected_id().as_str()) else {
            return;
        };
        let Some(rgba) = registry::parse_hex_color(text.trim()) else {
            set_hint(if text.trim().is_empty() { EMPTY_HINT } else { HEX_HINT });
            reproject(&ui, false);
            return;
        };
        clear_hint();
        // Typing the color already shown saves nothing, so an inherited color
        // does not become an override. This only ever skips a write, so a
        // view older than the file cannot overwrite it.
        if effective_rgba(&ui, token.id) == rgba {
            reproject(&ui, false);
            return;
        }
        save_override(&ui, &save_store, token.id, Some(registry::format_hex_color(rgba)));
        // The save projected the new file before its undo step was recorded.
        reproject(&ui, false);
    });
    let weak = ui.as_weak();
    let reset_store = store.clone();
    ui.on_color_reset(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(token) = registry::color_token(ui.get_color_selected_id().as_str()) else {
            return;
        };
        clear_hint();
        save_override(&ui, &reset_store, token.id, None);
        // Show the inherited color in the field again.
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    let undo_store = store.clone();
    ui.on_color_undo(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(token) = registry::color_token(ui.get_color_selected_id().as_str()) else {
            return;
        };
        let step = SESSION.with(|session| session.borrow_mut().previous.remove(token.id));
        let Some(step) = step else { return };
        clear_hint();
        let mut superseded = false;
        let saved = save_config_edit(&ui, &undo_store, |config| {
            // Undo only the change this window made, not a later one.
            if !same_color(config.colors.get(token.id), step.after.as_ref()) {
                superseded = true;
                return false;
            }
            match &step.before {
                Some(value) => config.colors.insert(token.id.into(), value.clone()),
                None => config.colors.remove(token.id),
            };
            true
        });
        if superseded {
            set_hint(STALE_UNDO_HINT);
        } else if saved.is_none() {
            // Keep the undo step when the file could not be changed.
            SESSION.with(|session| session.borrow_mut().previous.insert(token.id, step));
        }
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    let all_store = store.clone();
    ui.on_color_reset_all(move || {
        let Some(ui) = weak.upgrade() else { return };
        let state = SESSION.with(|session| std::mem::take(&mut session.borrow_mut().reset_all));
        // Whether the file changed, which ends every single-color undo step.
        let mut written = false;
        let next = match state {
            ResetAll::Idle => ResetAll::Confirm,
            ResetAll::Confirm => {
                // The overrides the file holds now, not the ones last applied.
                let mut removed = BTreeMap::new();
                let saved = save_config_edit(&ui, &all_store, |config| {
                    removed = std::mem::take(&mut config.colors);
                    !removed.is_empty()
                });
                written = saved.is_some();
                if written {
                    ResetAll::Removed(removed)
                } else {
                    ResetAll::Idle
                }
            }
            ResetAll::Removed(removed) => {
                // `None` until the file was read; then whether anything is
                // left to restore. A color set again since is kept.
                let mut restorable = None;
                let saved = save_config_edit(&ui, &all_store, |config| {
                    let mut any = false;
                    for (id, value) in &removed {
                        if !config.colors.contains_key(id) {
                            config.colors.insert(id.clone(), value.clone());
                            any = true;
                        }
                    }
                    restorable = Some(any);
                    any
                });
                written = saved.is_some();
                if written || restorable == Some(false) {
                    ResetAll::Idle
                } else {
                    // The file could not be read or written: keep the snapshot
                    // so the restore can be tried again.
                    ResetAll::Removed(removed)
                }
            }
        };
        SESSION.with(|session| {
            let mut session = session.borrow_mut();
            session.reset_all = next;
            if written {
                session.previous.clear();
            }
        });
        reproject(&ui, true);
    });
    let weak = ui.as_weak();
    let preview_store = store.clone();
    ui.on_preview_window_opacity(move |value| {
        let Some(ui) = weak.upgrade() else { return };
        let percent = opacity_percent(value);
        // Preview only; the saved file still holds the last valid value.
        ui.global::<StyleTokens>()
            .set_window_opacity(f32::from(percent) / 100.0);
        // Keyboard, accessibility and number edits have no release event of
        // their own in every case; save once the value has settled.
        let weak = ui.as_weak();
        let store = preview_store.clone();
        OPACITY_COMMIT.with(|timer| {
            timer.start(slint::TimerMode::SingleShot, OPACITY_SETTLE, move || {
                if let Some(ui) = weak.upgrade() {
                    commit_window_opacity(&ui, &store, percent);
                }
            });
        });
    });
    let weak = ui.as_weak();
    let window_store = store.clone();
    ui.on_set_window_opacity(move |value| {
        let Some(ui) = weak.upgrade() else { return };
        OPACITY_COMMIT.with(slint::Timer::stop);
        commit_window_opacity(&ui, &window_store, opacity_percent(value));
    });
}

const OPACITY_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

thread_local! {
    static OPACITY_COMMIT: slint::Timer = slint::Timer::default();
}

fn opacity_percent(value: i32) -> u8 {
    let (minimum, maximum) = (
        *registry::WINDOW_OPACITY_RANGE.start(),
        *registry::WINDOW_OPACITY_RANGE.end(),
    );
    value.clamp(minimum.into(), maximum.into()) as u8
}

/// Save the window opacity. A rejected save (for example because the file
/// on disk became invalid) restores the last valid appearance, and both
/// controls are resynchronized to the saved value either way.
fn commit_window_opacity(ui: &AppWindow, store: &ConfigStore, percent: u8) {
    let saved = save_config_change(ui, store, |config| {
        config.window.opacity_percent = percent;
    });
    if saved.is_none() {
        current_config(|config| crate::appearance::apply_config(ui, config));
    }
    ui.set_window_opacity_revision(ui.get_window_opacity_revision().wrapping_add(1));
}

/// Project the workspace for an accepted configuration. Called from every
/// configuration application, including reloads of external edits.
pub(super) fn project(ui: &AppWindow, config: &Config, theme: Option<&ThemePack>) {
    SESSION.with(|session| {
        let mut session = session.borrow_mut();
        session.source = Some((config.clone(), theme.cloned()));
        // An edit made elsewhere ends a pending Reset all confirmation and
        // the undo step of every color it changed.
        if matches!(session.reset_all, ResetAll::Confirm) {
            session.reset_all = ResetAll::Idle;
        }
        session
            .previous
            .retain(|id, step| same_color(config.colors.get(*id), step.after.as_ref()));
    });
    reproject(ui, false);
}

fn current_config<R>(read: impl FnOnce(&Config) -> R) -> Option<R> {
    SESSION.with(|session| {
        let session = session.borrow();
        session.source.as_ref().map(|(config, _)| read(config))
    })
}

fn set_hint(text: &str) {
    SESSION.with(|session| session.borrow_mut().hint = text.into());
}

fn clear_hint() {
    set_hint("");
}

/// Save one override (or remove it with `None`), keeping one undo step. The
/// step's prior value is the one in the re-read file, and nothing is written
/// when the file already holds the requested color.
fn save_override(ui: &AppWindow, store: &ConfigStore, id: &'static str, value: Option<String>) {
    let mut before = None;
    let saved = save_config_edit(ui, store, |config| {
        if same_color(config.colors.get(id), value.as_ref()) {
            return false;
        }
        before = Some(match &value {
            Some(value) => config.colors.insert(id.into(), value.clone()),
            None => config.colors.remove(id),
        });
        true
    });
    if let (Some(_), Some(before)) = (saved, before) {
        SESSION.with(|session| {
            let mut session = session.borrow_mut();
            session.previous.insert(id, UndoStep { before, after: value });
            session.reset_all = ResetAll::Idle;
        });
    }
}

/// Whether two optional override values name the same color; spelling and
/// case do not matter. Values in a loaded configuration are always valid.
fn same_color(left: Option<&String>, right: Option<&String>) -> bool {
    left.map(|value| registry::parse_hex_color(value))
        == right.map(|value| registry::parse_hex_color(value))
}

fn group_items() -> Vec<ColorGroupItem> {
    let mut items: Vec<ColorGroupItem> = TokenGroup::ALL
        .iter()
        .map(|group| ColorGroupItem {
            id: group.id().into(),
            label: group.label().into(),
            count: COLOR_TOKENS.iter().filter(|token| token.group == *group).count() as i32,
            window: false,
        })
        .collect();
    items.push(ColorGroupItem {
        id: "window".into(),
        label: "Window".into(),
        count: 0,
        window: true,
    });
    items
}

/// Tokens matching every word of `filter`, or the tokens of `group`.
fn shown_tokens(group: usize, filter: &str) -> Vec<&'static ColorToken> {
    let filter = filter.trim().to_lowercase();
    let words: Vec<&str> = filter.split_whitespace().collect();
    if words.is_empty() {
        let Some(group) = TokenGroup::ALL.get(group) else {
            return Vec::new();
        };
        return COLOR_TOKENS
            .iter()
            .filter(|token| token.group == *group)
            .collect();
    }
    COLOR_TOKENS
        .iter()
        .filter(|token| {
            let haystack = format!(
                "{} {} {} {} {}",
                token.label,
                token.id,
                token.group.label(),
                token.keywords.join(" "),
                token.description
            )
            .to_lowercase();
            words.iter().all(|word| haystack.contains(word))
        })
        .collect()
}

fn effective_rgba(ui: &AppWindow, id: &str) -> [u8; 4] {
    let color = ui.invoke_token_color(id.into());
    [color.red(), color.green(), color.blue(), color.alpha()]
}

/// Where the shown color of `token` comes from, as fixed text.
fn source_text(config: &Config, theme: Option<&ThemePack>, token: &ColorToken) -> String {
    if config.colors.contains_key(token.id) {
        return "Your color, saved in config.toml".into();
    }
    let from_theme = match token.source {
        TokenSource::Base => theme.is_some(),
        TokenSource::Role => theme.and_then(|theme| theme.roles.get(token.id)).is_some(),
    };
    if from_theme {
        return "From the selected theme".into();
    }
    match token.fallback {
        Fallback::Palette => "Built-in palette".into(),
        Fallback::Token(other) => match registry::color_token(other) {
            Some(other) => format!("Follows {}", other.label),
            None => "Built-in default".into(),
        },
        Fallback::Legacy { .. } => "Built-in default".into(),
    }
}

/// Publish the groups, the shown tokens and the selected token's editor.
/// `refill` replaces the hex field with the shown color; otherwise the field
/// is only replaced when it holds a valid value that differs from it, so a
/// partial value being typed is never overwritten.
fn reproject(ui: &AppWindow, refill: bool) {
    SESSION.with(|session| {
        let session = session.borrow();
        let default = (Config::default(), None);
        let (config, theme) = session.source.as_ref().unwrap_or(&default);
        let theme = theme.as_ref();
        let group = usize::try_from(ui.get_color_group_index()).unwrap_or(0);
        let tokens = shown_tokens(group, ui.get_color_filter().as_str());
        let selected = registry::color_token(ui.get_color_selected_id().as_str())
            .filter(|token| tokens.iter().any(|shown| shown.id == token.id))
            .or_else(|| tokens.first().copied())
            .or_else(|| registry::color_token(ui.get_color_selected_id().as_str()))
            .unwrap_or(&COLOR_TOKENS[0]);
        let refill = refill || ui.get_color_selected_id().as_str() != selected.id;
        ui.set_color_selected_id(selected.id.into());
        let items: Vec<ColorTokenItem> = tokens
            .iter()
            .map(|token| ColorTokenItem {
                id: token.id.into(),
                label: token.label.into(),
                group: token.group.label().into(),
                overridden: config.colors.contains_key(token.id),
            })
            .collect();
        ui.set_color_tokens(ModelRc::new(VecModel::from(items)));
        ui.set_color_override_count(config.colors.len() as i32);
        ui.set_color_reset_all_notice(
            match &session.reset_all {
                ResetAll::Idle => String::new(),
                ResetAll::Confirm => format!(
                    "Activate again to remove all {} customized colors",
                    config.colors.len()
                ),
                ResetAll::Removed(removed) => {
                    format!("Restore the {} removed colors", removed.len())
                }
            }
            .into(),
        );
        ui.set_color_editor(ColorEditorState {
            id: selected.id.into(),
            label: selected.label.into(),
            description: selected.description.into(),
            group: selected.group.label().into(),
            source: source_text(config, theme, selected).into(),
            overridden: config.colors.contains_key(selected.id),
            can_undo: session.previous.contains_key(selected.id),
            hint: session.hint.as_str().into(),
        });
        let shown = effective_rgba(ui, selected.id);
        let typed = registry::parse_hex_color(ui.get_color_hex_input().trim());
        if refill || typed.is_some_and(|typed| typed != shown) {
            ui.set_color_hex_input(registry::format_hex_color(shown).into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_every_word_across_groups_and_groups_list_their_tokens() {
        let link: Vec<_> = shown_tokens(0, "LINK").iter().map(|token| token.id).collect();
        assert_eq!(link, ["link"]);
        let badge: Vec<_> = shown_tokens(0, "badge text").iter().map(|token| token.id).collect();
        assert_eq!(badge, ["unread_ink"]);
        assert!(shown_tokens(0, "no such color").is_empty());
        let surfaces = shown_tokens(0, "  ");
        assert!(surfaces.iter().all(|token| token.group == TokenGroup::Surfaces));
        assert_eq!(surfaces.len(), group_items()[0].count as usize);
        assert!(shown_tokens(WINDOW_GROUP, "").is_empty());
        let groups = group_items();
        assert_eq!(groups.len(), TokenGroup::ALL.len() + 1);
        assert!(groups.last().unwrap().window);
        let total: i32 = groups.iter().map(|group| group.count).sum();
        assert_eq!(total as usize, COLOR_TOKENS.len());
    }

    #[test]
    fn sources_name_the_layer_that_supplies_the_color() {
        let mut config = Config::default();
        let theme = morpheus_config::BuiltInStylePreset::Paper.theme().unwrap();
        let canvas = registry::color_token("canvas").unwrap();
        let timestamp = registry::color_token("timestamp").unwrap();
        let scrim = registry::color_token("scrim").unwrap();
        assert_eq!(source_text(&config, None, canvas), "Built-in palette");
        assert_eq!(source_text(&config, Some(&theme), canvas), "From the selected theme");
        assert_eq!(source_text(&config, None, timestamp), "Follows Secondary text");
        assert_eq!(source_text(&config, None, scrim), "Built-in default");
        config.colors.insert("canvas".into(), "#000000".into());
        assert_eq!(
            source_text(&config, Some(&theme), canvas),
            "Your color, saved in config.toml"
        );
    }
}
