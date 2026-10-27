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
//!
//! Colors are edited at one of three levels, with global -> account -> room
//! precedence: all accounts (`[colors]`), the active account
//! (`account_overrides.<account>.colors`) or the open room of the active
//! account (`account_overrides.<account>.room_appearance.<room>.colors`). A
//! token a level omits inherits from the level below; Reset removes only
//! the edited level's value. "This room" edits are bound to the room the
//! editor last showed (a room scope from `theme_studio::scope`): an edit made
//! after the open room changed, before the editor re-projected, is refused
//! rather than written to another room.
//!
//! Interaction-state and per-control icon colors are advanced. The list
//! hides them until "Show advanced colors" is chosen, unless a search
//! matches them, the edited level customizes them, or a group has nothing
//! else. A search result selecting one shows them.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use morpheus_config::registry::{
    self, COLOR_TOKENS, ColorToken, Fallback, TokenGroup, TokenSource,
};
use morpheus_config::{AccountOverrides, Config, ConfigStore, ThemePack};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{
    AppWindow, ColorEditorState, ColorGroupItem, ColorTokenItem, StyleTokens, save_config_change,
    save_config_edit,
};

const HEX_HINT: &str =
    "Use #RRGGBB or #RRGGBBAA, with alpha last. The last valid color stays applied.";
const EMPTY_HINT: &str = "Type a color, or use Reset to return to the theme or built-in color.";
const STALE_UNDO_HINT: &str =
    "This color was changed outside this window after your change; nothing was undone.";
const MAX_FILTER_CHARS: usize = 64;
/// WCAG contrast for normal-size text; below it the editor only advises.
const READABLE_CONTRAST: f64 = 4.5;
/// Text tokens and the surface each is drawn on. Choosing either token of a
/// pair shows their contrast as advice; nothing is ever corrected.
const TEXT_PAIRS: &[(&str, &str)] = &[
    ("ink", "canvas"),
    ("muted", "canvas"),
    ("message", "canvas"),
    ("sender", "canvas"),
    ("timestamp", "canvas"),
    ("link", "canvas"),
    ("own_message", "own_surface"),
    ("accent_ink", "accent"),
    ("unread_ink", "unread"),
    ("selection_ink", "selection"),
    ("error_ink", "error_surface"),
    ("success_ink", "success_surface"),
    ("destructive_ink", "destructive"),
    ("nav_selected_ink", "nav_selected"),
    ("mention_ink", "mention"),
];
/// The group-list entry after the color groups: the window settings.
const WINDOW_GROUP: usize = TokenGroup::ALL.len();
const ROOM_CHANGED_HINT: &str =
    "The open room changed before this edit; nothing was saved. Check the color and try again.";
const NO_ROOM_HINT: &str = "Open a room of this account to edit its colors; nothing was saved.";

/// The level whose overrides the editor changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum EditScope {
    /// `[colors]`, for all accounts.
    #[default]
    All,
    /// The active account's colors.
    Account(String),
    /// The open room's colors in the active account.
    Room(String, String),
    /// "This room" is chosen but no room is open: nothing can be edited.
    Unavailable,
}

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
    /// The level the undo steps and the Reset all snapshot belong to.
    scope: EditScope,
    /// The active account and edited scope of the last projection. Only a
    /// change refills the hex field, so the live refresh that follows every
    /// room update never replaces a partial value being typed.
    view: Option<(String, EditScope)>,
    /// For "This room": the room scope captured when the editor last showed
    /// that room. Edits are written only while it is current.
    room_scope: Option<crate::theme_studio::scope::Scope>,
}

/// What the shown color of the selected token is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shown {
    /// The color the window applies for the edited scope.
    Applied,
    /// The edited level's own value; the named more specific level (the
    /// open room or the active account) applies its own color instead.
    ScopeValue(&'static str),
    /// The color the named more specific level makes. The edited level's
    /// value comes from below it and is not shown.
    MoreSpecific(&'static str),
}

/// Advice on the contrast of the selected text color and its surface, as the
/// window shows them now (a draft included), or empty for other tokens.
fn contrast_note(ui: &AppWindow, id: &str) -> String {
    let Some(&(text, surface)) = TEXT_PAIRS
        .iter()
        .find(|(text, surface)| *text == id || *surface == id)
    else {
        return String::new();
    };
    let shown = |token: &str| {
        let color = ui.invoke_token_color(token.into());
        [color.red(), color.green(), color.blue(), color.alpha()]
    };
    let [red, green, blue, alpha] = shown(surface);
    let opaque_surface = [red, green, blue, u8::MAX];
    let ratio = crate::sender_palette_editor::contrast(shown(text), opaque_surface);
    let label =
        |token: &'static str| registry::color_token(token).map_or(token, |token| token.label);
    let pair = format!("{} on {}", label(text), label(surface));
    let assumption = if alpha < u8::MAX || ui.get_window_opacity_percent() < 100 {
        " Assuming an opaque background; transparency changes the actual contrast."
    } else {
        ""
    };
    if ratio >= READABLE_CONTRAST {
        format!("{pair}: {ratio:.1}:1 contrast.{assumption}")
    } else {
        format!(
            "{pair}: {ratio:.1}:1 contrast. At least 4.5:1 is easier to read; this is advice, and nothing is changed.{assumption}"
        )
    }
}

/// Whether the window's open room is a room whose colors can be edited (a
/// Matrix room ID, not a view such as Discover).
fn room_editable(room: &str) -> bool {
    room.starts_with('!') && room.len() > 1
}

/// The level whose colors the editor changes. Only the published active
/// account can be chosen, which the window publishes after a successful
/// activation and clears when no account is active, and only the open room
/// of that account.
fn edit_scope(ui: &AppWindow) -> EditScope {
    let account = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    match ui.get_color_scope() {
        1 if !account.is_empty() => EditScope::Account(account.to_string()),
        2 if !account.is_empty() && room_editable(room.as_str()) => {
            EditScope::Room(account.to_string(), room.to_string())
        }
        2 => EditScope::Unavailable,
        _ => EditScope::All,
    }
}

/// The overrides of one level as `config` holds them.
fn scoped<'a>(config: &'a Config, scope: &EditScope) -> Option<&'a BTreeMap<String, String>> {
    match scope {
        EditScope::All => Some(&config.colors),
        EditScope::Account(id) => config
            .account_overrides
            .get(id)
            .map(|overrides| &overrides.colors),
        EditScope::Room(id, room) => config
            .room_appearance(Some(id), Some(room))
            .map(|appearance| &appearance.colors),
        EditScope::Unavailable => None,
    }
}

fn scoped_color<'a>(config: &'a Config, scope: &EditScope, id: &str) -> Option<&'a String> {
    scoped(config, scope).and_then(|colors| colors.get(id))
}

/// Change the overrides of one level, removing a room or account entry left
/// empty. `Unavailable` edits a throwaway table: nothing is written.
fn edit_scoped<R>(
    config: &mut Config,
    scope: &EditScope,
    edit: impl FnOnce(&mut BTreeMap<String, String>) -> R,
) -> R {
    match scope {
        EditScope::All => edit(&mut config.colors),
        EditScope::Unavailable => edit(&mut BTreeMap::new()),
        EditScope::Account(id) => {
            let overrides = config.account_overrides.entry(id.clone()).or_default();
            let result = edit(&mut overrides.colors);
            if *overrides == AccountOverrides::default() {
                config.account_overrides.remove(id);
            }
            result
        }
        EditScope::Room(id, room) => {
            let overrides = config.account_overrides.entry(id.clone()).or_default();
            let appearance = overrides.room_appearance.entry(room.clone()).or_default();
            let result = edit(&mut appearance.colors);
            let empty = appearance.is_empty();
            if empty {
                overrides.room_appearance.remove(room);
            }
            if *overrides == AccountOverrides::default() {
                config.account_overrides.remove(id);
            }
            result
        }
    }
}

/// Undo steps and a Reset all snapshot never cross levels, accounts or
/// rooms: a changed scope drops them.
fn sync_scope(scope: &EditScope) {
    SESSION.with(|session| {
        let mut session = session.borrow_mut();
        if session.scope != *scope {
            session.previous.clear();
            session.reset_all = ResetAll::Idle;
            session.scope = scope.clone();
        }
    });
}

/// Whether an edit may be written in `scope` now. A "This room" edit is
/// bound to the room the editor last showed: after a room change that the
/// (deferred) room hook has not re-projected yet, it is refused, never
/// moved to another room. Without an open room nothing is written.
fn edit_allowed(ui: &AppWindow, scope: &EditScope) -> bool {
    match scope {
        EditScope::Unavailable => {
            set_hint(NO_ROOM_HINT);
            false
        }
        EditScope::Room(..) => {
            let (captured, shown) = SESSION.with(|session| {
                let session = session.borrow();
                (session.room_scope.clone(), session.view.clone())
            });
            let current = captured
                .is_some_and(|captured| crate::theme_studio::scope::current(ui, &captured))
                && shown.is_some_and(|(_, shown)| shown == *scope);
            if !current {
                set_hint(ROOM_CHANGED_HINT);
            }
            current
        }
        EditScope::All | EditScope::Account(_) => true,
    }
}

thread_local! {
    // Slint callbacks and projections run on the UI thread only.
    static SESSION: RefCell<Session> = RefCell::new(Session::default());
}

pub(super) fn register(ui: &AppWindow, store: &ConfigStore) {
    ui.set_color_groups(ModelRc::new(VecModel::from(group_items())));
    let weak = ui.as_weak();
    ui.on_color_scope_chosen(move |scope| {
        let Some(ui) = weak.upgrade() else { return };
        // "This account" needs a published active account; "This room" also
        // needs an open room of it.
        let account = !ui.get_active_account_local_id().is_empty();
        let scope = match scope {
            1 if account => 1,
            2 if account && room_editable(ui.get_active_room_id().as_str()) => 2,
            _ => 0,
        };
        ui.set_color_scope(scope);
        clear_hint();
        reproject(&ui, true);
    });
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
    ui.on_color_advanced_toggled(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_color_show_advanced(!ui.get_color_show_advanced());
        reproject(&ui, false);
    });
    let weak = ui.as_weak();
    ui.on_color_group_chosen(move |index| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        if index == WINDOW_GROUP {
            crate::settings_search::reveal(&ui, "appearance.window_opacity");
            return;
        }
        let Some(group) = TokenGroup::ALL.get(index) else {
            return;
        };
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
        let Some(token) = registry::color_token(id.as_str()) else {
            return;
        };
        // A queued click can name a token from the previous group/filter.
        // Honor that exact allowlisted ID instead of projecting a different
        // group's first token and editing the wrong color next.
        let group = usize::try_from(ui.get_color_group_index()).unwrap_or(0);
        if !shown_tokens(group, ui.get_color_filter().as_str())
            .iter()
            .any(|shown| shown.id == token.id)
        {
            select_token(&ui, token);
            return;
        }
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
            set_hint(if text.trim().is_empty() {
                EMPTY_HINT
            } else {
                HEX_HINT
            });
            reproject(&ui, false);
            return;
        };
        clear_hint();
        // Typing the color already shown saves nothing, so an inherited color
        // does not become an override. This only ever skips a write, so a
        // view older than the file cannot overwrite it. A color the active
        // account sets itself is not the value for all accounts.
        let (shown, shown_as) = SESSION.with(|session| {
            let session = session.borrow();
            let default = (Config::default(), None);
            let (config, theme) = session.source.as_ref().unwrap_or(&default);
            shown_color(&ui, config, theme.as_ref(), &edit_scope(&ui), token)
        });
        if shown == rgba && !matches!(shown_as, Shown::MoreSpecific(_)) {
            reproject(&ui, false);
            return;
        }
        save_override(
            &ui,
            &save_store,
            token.id,
            Some(registry::format_hex_color(rgba)),
        );
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
        let scope = edit_scope(&ui);
        sync_scope(&scope);
        if !edit_allowed(&ui, &scope) {
            reproject(&ui, true);
            return;
        }
        let step = SESSION.with(|session| session.borrow_mut().previous.remove(token.id));
        let Some(step) = step else { return };
        clear_hint();
        let mut superseded = false;
        let saved = save_config_edit(&ui, &undo_store, |config| {
            // Undo only the change this window made, not a later one.
            let current = scoped_color(config, &scope, token.id);
            if !same_color(current, step.after.as_ref()) {
                superseded = true;
                return false;
            }
            edit_scoped(config, &scope, |colors| match &step.before {
                Some(value) => colors.insert(token.id.into(), value.clone()),
                None => colors.remove(token.id),
            });
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
        let scope = edit_scope(&ui);
        sync_scope(&scope);
        if !edit_allowed(&ui, &scope) {
            reproject(&ui, true);
            return;
        }
        let state = SESSION.with(|session| std::mem::take(&mut session.borrow_mut().reset_all));
        // Whether the file changed, which ends every single-color undo step.
        let mut written = false;
        let next = match state {
            ResetAll::Idle => ResetAll::Confirm,
            ResetAll::Confirm => {
                // The overrides the file holds now, not the ones last applied.
                let mut removed = BTreeMap::new();
                let saved = save_config_edit(&ui, &all_store, |config| {
                    removed = edit_scoped(config, &scope, std::mem::take);
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
                    let any = edit_scoped(config, &scope, |colors| {
                        let mut any = false;
                        for (id, value) in &removed {
                            if !colors.contains_key(id) {
                                colors.insert(id.clone(), value.clone());
                                any = true;
                            }
                        }
                        any
                    });
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
    // A newly applied appearance supersedes an unsaved opacity preview. Its
    // delayed callback must not write the older preview over a live reload.
    OPACITY_COMMIT.with(slint::Timer::stop);
    SESSION.with(|session| {
        let mut session = session.borrow_mut();
        session.source = Some((config.clone(), theme.cloned()));
        // An edit made elsewhere ends a pending Reset all confirmation and
        // the undo step of every color it changed.
        if matches!(session.reset_all, ResetAll::Confirm) {
            session.reset_all = ResetAll::Idle;
        }
        let scope = session.scope.clone();
        session
            .previous
            .retain(|id, step| same_color(scoped_color(config, &scope, id), step.after.as_ref()));
    });
    reproject(ui, false);
    // Previewing changes each control locally, not the saved percentage.
    // Resynchronize even when a reload accepts that same saved percentage.
    ui.set_window_opacity_revision(ui.get_window_opacity_revision().wrapping_add(1));
}

/// Re-project after the active account or the open room changed. Scoped
/// undo steps and a Reset all snapshot of the previous account or room are
/// dropped, and with no active account the editor returns to the colors for
/// all accounts. "This room" stays chosen without an open room, editing
/// nothing until one is open.
pub(super) fn refresh(ui: &AppWindow) {
    if ui.get_active_account_local_id().is_empty() {
        ui.set_color_scope(0);
    }
    reproject(ui, false);
}

/// Select one token for editing, as a Settings search result does: its group
/// with no filter, and its shown color in the hex field. Nothing is saved.
pub(super) fn select_token(ui: &AppWindow, token: &'static ColorToken) {
    // A search result lands on the real editor even for an advanced color.
    if matches!(token.source, TokenSource::State) {
        ui.set_color_show_advanced(true);
    }
    if let Some(index) = TokenGroup::ALL
        .iter()
        .position(|group| *group == token.group)
    {
        ui.set_color_group_index(index as i32);
    }
    ui.set_color_filter(SharedString::new());
    ui.set_color_selected_id(token.id.into());
    clear_hint();
    reproject(ui, true);
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
    let scope = edit_scope(ui);
    sync_scope(&scope);
    if !edit_allowed(ui, &scope) {
        return;
    }
    let mut before = None;
    let saved = save_config_edit(ui, store, |config| {
        if same_color(scoped_color(config, &scope, id), value.as_ref()) {
            return false;
        }
        before = Some(edit_scoped(config, &scope, |colors| match &value {
            Some(value) => colors.insert(id.into(), value.clone()),
            None => colors.remove(id),
        }));
        true
    });
    if let (Some(_), Some(before)) = (saved, before) {
        SESSION.with(|session| {
            let mut session = session.borrow_mut();
            session.previous.insert(
                id,
                UndoStep {
                    before,
                    after: value,
                },
            );
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
            count: COLOR_TOKENS
                .iter()
                .filter(|token| token.group == *group)
                .count() as i32,
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

/// The tokens the list shows from `all` (the group's or the search's): the
/// advanced ones (interaction states and per-control icon colors) only when
/// asked for, searched for, customized at the edited level or selected, or
/// when the group has nothing else.
fn visible_tokens(
    all: &[&'static ColorToken],
    filter: &str,
    advanced: bool,
    customized: impl Fn(&str) -> bool,
    selected: &str,
) -> Vec<&'static ColorToken> {
    if advanced || !filter.trim().is_empty() {
        return all.to_vec();
    }
    let basic: Vec<&'static ColorToken> = all
        .iter()
        .copied()
        .filter(|token| {
            !matches!(token.source, TokenSource::State)
                || customized(token.id)
                || token.id == selected
        })
        .collect();
    if basic.is_empty() {
        all.to_vec()
    } else {
        basic
    }
}

fn effective_rgba(ui: &AppWindow, id: &str) -> [u8; 4] {
    let color = ui.invoke_token_color(id.into());
    [color.red(), color.green(), color.blue(), color.alpha()]
}

/// The more specific level whose own colors decide how `token` looks while
/// `scope` is edited, named for the editor: the open room's colors (unless
/// the room itself is edited), then the active account's (when all accounts
/// are edited). A level decides when it sets the token, or a token it
/// follows, before the edited level, a global color or the theme does.
fn decided_above(
    config: &Config,
    theme: Option<&ThemePack>,
    scope: &EditScope,
    active: &str,
    room: &str,
    token: &'static ColorToken,
) -> Option<&'static str> {
    if active.is_empty() {
        return None;
    }
    let mut levels: Vec<(&'static str, &BTreeMap<String, String>)> = Vec::new();
    if !matches!(scope, EditScope::Room(..)) {
        if let Some(own) = config.room_appearance(Some(active), Some(room)) {
            levels.push(("the open room", &own.colors));
        }
    }
    if matches!(scope, EditScope::All) {
        if let Some(own) = config.account_overrides.get(active) {
            levels.push(("the active account", &own.colors));
        }
    }
    levels.retain(|(_, colors)| !colors.is_empty());
    if levels.is_empty() {
        return None;
    }
    let edited = scoped(config, scope);
    let mut step = Some(token);
    // Each fallback names a different token; the bound only guards a cycle.
    for _ in 0..COLOR_TOKENS.len() {
        let Some(token) = step else { break };
        if let Some((who, _)) = levels
            .iter()
            .find(|(_, colors)| colors.contains_key(token.id))
        {
            return Some(*who);
        }
        let themed = match token.source {
            TokenSource::Base => theme.is_some(),
            TokenSource::Role => theme.and_then(|theme| theme.roles.get(token.id)).is_some(),
            TokenSource::State => theme.and_then(|theme| theme.states.get(token.id)).is_some(),
        };
        let set_here = edited.is_some_and(|colors| colors.contains_key(token.id));
        if themed || set_here || config.colors.contains_key(token.id) {
            return None;
        }
        step = match token.fallback {
            Fallback::Token(other) => registry::color_token(other),
            Fallback::Palette | Fallback::Legacy { .. } => None,
        };
    }
    None
}

/// The color the editor shows for `token` at the edited level. The window
/// applies the most specific level's colors (global -> account -> room).
/// When a more specific level decides the token, the edited level's own
/// value is shown if the file holds one, and the color is marked otherwise.
fn shown_color(
    ui: &AppWindow,
    config: &Config,
    theme: Option<&ThemePack>,
    scope: &EditScope,
    token: &'static ColorToken,
) -> ([u8; 4], Shown) {
    let applied = effective_rgba(ui, token.id);
    let active = ui.get_active_account_local_id();
    let room = ui.get_active_room_id();
    let Some(who) = decided_above(config, theme, scope, active.as_str(), room.as_str(), token)
    else {
        return (applied, Shown::Applied);
    };
    match scoped_color(config, scope, token.id).and_then(|value| registry::parse_hex_color(value)) {
        Some(own) => (own, Shown::ScopeValue(who)),
        None => (applied, Shown::MoreSpecific(who)),
    }
}

/// Where the shown color of `token` comes from, as fixed text, for the
/// edited level.
fn source_text(
    config: &Config,
    theme: Option<&ThemePack>,
    token: &ColorToken,
    scope: &EditScope,
) -> String {
    if scoped_color(config, scope, token.id).is_some() {
        match scope {
            EditScope::Room(..) => return "Your color for this room, saved in config.toml".into(),
            EditScope::Account(_) => {
                return "Your color for this account, saved in config.toml".into();
            }
            EditScope::All | EditScope::Unavailable => {}
        }
    }
    if let EditScope::Room(account, _) = scope
        && scoped_color(config, &EditScope::Account(account.clone()), token.id).is_some()
    {
        return "Your color for this account; a value here applies to this room only".into();
    }
    if config.colors.contains_key(token.id) {
        return match scope {
            EditScope::All | EditScope::Unavailable => "Your color, saved in config.toml".into(),
            EditScope::Account(_) => {
                "Your color for all accounts; a value here applies to this account only".into()
            }
            EditScope::Room(..) => {
                "Your color for all accounts; a value here applies to this room only".into()
            }
        };
    }
    let from_theme = match token.source {
        TokenSource::Base => theme.is_some(),
        TokenSource::Role => theme.and_then(|theme| theme.roles.get(token.id)).is_some(),
        TokenSource::State => theme.and_then(|theme| theme.states.get(token.id)).is_some(),
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
    let scope = edit_scope(ui);
    sync_scope(&scope);
    ui.set_color_room_available(
        !ui.get_active_account_local_id().is_empty()
            && room_editable(ui.get_active_room_id().as_str()),
    );
    let view = (ui.get_active_account_local_id().to_string(), scope.clone());
    let moved = SESSION.with(|session| {
        let mut session = session.borrow_mut();
        let moved = session.view.as_ref() != Some(&view);
        // "This room" edits are bound to the room shown from here on.
        if moved || session.room_scope.is_none() {
            session.room_scope = matches!(scope, EditScope::Room(..))
                .then(|| crate::theme_studio::scope::capture_room(ui));
        }
        // Undo steps and a Reset all snapshot never cross active accounts,
        // in either scope.
        if session
            .view
            .as_ref()
            .is_some_and(|(active, _)| *active != view.0)
        {
            session.previous.clear();
            session.reset_all = ResetAll::Idle;
        }
        session.view = Some(view);
        moved
    });
    let refill = refill || moved;
    SESSION.with(|session| {
        let session = session.borrow();
        let default = (Config::default(), None);
        let (config, theme) = session.source.as_ref().unwrap_or(&default);
        let theme = theme.as_ref();
        let scope_colors = scoped(config, &scope);
        let overridden = |id: &str| scope_colors.is_some_and(|colors| colors.contains_key(id));
        let override_count = scope_colors.map_or(0, BTreeMap::len);
        let group = usize::try_from(ui.get_color_group_index()).unwrap_or(0);
        let filter = ui.get_color_filter();
        let all = shown_tokens(group, filter.as_str());
        let tokens = visible_tokens(
            &all,
            filter.as_str(),
            ui.get_color_show_advanced(),
            &overridden,
            ui.get_color_selected_id().as_str(),
        );
        ui.set_color_advanced_hidden((all.len() - tokens.len()) as i32);
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
                overridden: overridden(token.id),
            })
            .collect();
        // Keep the list's delegates, and keyboard focus in it, when nothing
        // changed, as after a live refresh or the reload of this window's save.
        let current = ui.get_color_tokens();
        if current.row_count() != items.len()
            || current
                .iter()
                .zip(&items)
                .any(|(shown, item)| shown != *item)
        {
            ui.set_color_tokens(ModelRc::new(VecModel::from(items)));
        }
        ui.set_color_override_count(override_count as i32);
        ui.set_color_reset_all_notice(
            match &session.reset_all {
                ResetAll::Idle => String::new(),
                ResetAll::Confirm => {
                    format!("Activate again to remove all {override_count} customized colors")
                }
                ResetAll::Removed(removed) => {
                    format!("Restore the {} removed colors", removed.len())
                }
            }
            .into(),
        );
        let (shown, shown_as) = shown_color(ui, config, theme, &scope, selected);
        let source = source_text(config, theme, selected, &scope);
        let reach = match scope {
            EditScope::All => "to all accounts",
            EditScope::Account(_) => "to this account",
            EditScope::Room(..) | EditScope::Unavailable => "to this room",
        };
        let contrast = contrast_note(ui, selected.id);
        ui.set_color_editor(ColorEditorState {
            id: selected.id.into(),
            label: selected.label.into(),
            description: selected.description.into(),
            group: selected.group.label().into(),
            source: match (&scope, shown_as) {
                (EditScope::Unavailable, _) => {
                    "Open a room of this account to edit its colors".to_owned()
                }
                (_, Shown::Applied) => source,
                (_, Shown::ScopeValue(who)) => {
                    format!("{source}. In this view {who} uses its own color")
                }
                (_, Shown::MoreSpecific(who)) => {
                    format!("{source}. Shown with {who}'s own color; a value here applies {reach}")
                }
            }
            .into(),
            overridden: overridden(selected.id),
            can_undo: session.previous.contains_key(selected.id),
            hint: session.hint.as_str().into(),
            contrast: contrast.into(),
        });
        let [red, green, blue, alpha] = shown;
        ui.set_color_swatch(slint::Color::from_argb_u8(alpha, red, green, blue));
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
    fn contrast_pairs_name_real_tokens_once() {
        let mut texts = std::collections::HashSet::new();
        for &(text, surface) in TEXT_PAIRS {
            assert!(registry::color_token(text).is_some(), "{text}");
            assert!(registry::color_token(surface).is_some(), "{surface}");
            assert!(texts.insert(text), "{text} is paired twice");
        }
    }

    #[test]
    fn filters_match_every_word_across_groups_and_groups_list_their_tokens() {
        let link: Vec<_> = shown_tokens(0, "LINK")
            .iter()
            .map(|token| token.id)
            .collect();
        assert_eq!(link, ["link"]);
        let badge: Vec<_> = shown_tokens(0, "badge text")
            .iter()
            .map(|token| token.id)
            .collect();
        // "Navigation and badges" holds the selected-room text color.
        assert_eq!(badge, ["unread_ink", "nav_selected_ink", "mention_ink"]);
        assert!(shown_tokens(0, "no such color").is_empty());
        let surfaces = shown_tokens(0, "  ");
        assert!(
            surfaces
                .iter()
                .all(|token| token.group == TokenGroup::Surfaces)
        );
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
        assert_eq!(
            source_text(&config, None, canvas, &EditScope::All),
            "Built-in palette"
        );
        assert_eq!(
            source_text(&config, Some(&theme), canvas, &EditScope::All),
            "From the selected theme"
        );
        assert_eq!(
            source_text(&config, None, timestamp, &EditScope::All),
            "Follows Secondary text"
        );
        assert_eq!(
            source_text(&config, None, scrim, &EditScope::All),
            "Built-in default"
        );
        config.colors.insert("canvas".into(), "#000000".into());
        assert_eq!(
            source_text(&config, Some(&theme), canvas, &EditScope::All),
            "Your color, saved in config.toml"
        );
    }
}
