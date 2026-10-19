//! Style profiles in Settings: a searchable list of saved looks with one
//! selected profile; Save current look, Rename, Duplicate and Delete; Import
//! and Export through the system file dialog; and a transient Preview with
//! Apply or Cancel. The file format and every file operation live in
//! `morpheus_config::style_profiles`; this module only drives the window.
//!
//! A preview applies a profile's look over the last accepted configuration
//! and saves nothing. Cancel restores exactly the look shown before it.
//! Anything that could make a rollback overwrite newer settings ends the
//! preview first: an accepted configuration with a different look (an
//! external edit or a save), an account switch or detach, and any Settings
//! save. A save that would change the look while previewing is refused,
//! because the value it carries may have been read from the preview.
//! Apply writes the profile's look into the settings file as re-read, so an
//! invalid file is never overwritten and other settings are kept.
//!
//! Profile names are shown in this window only. List keys are opaque values
//! of this session, never file names or paths, and messages never include a
//! path. Import and export run the system dialog and the bounded file work on
//! a worker thread, one at a time; nothing chosen is opened or applied
//! automatically.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use morpheus_config::style_profiles::{
    self, ProfileError, ProfileFile, ProfileStore, StyleProfile,
};
use morpheus_config::{
    ChatStyle, Config, ConfigError, ConfigStore, InputError, LoadedConfig, MessageLayout,
    ThemeMode, ThemePack,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{AppWindow, StyleProfileItem, StyleProfileState};

const MAX_FILTER_CHARS: usize = 64;
const PREVIEW_FIRST: &str = "Apply or cancel the preview first.";

/// One listed file and what reading it gave.
struct Row {
    key: String,
    file: ProfileFile,
    profile: Result<StyleProfile, String>,
}

enum Confirm {
    None,
    Delete(ProfileFile),
    /// Saving or importing `profile` under a name another file already uses.
    Replace {
        file: ProfileFile,
        profile: StyleProfile,
    },
}

/// A shown preview. `base` is the accepted look it replaced; `profile` and
/// `theme` are exactly what is shown, which Apply saves.
struct Preview {
    file: ProfileFile,
    name: String,
    profile: StyleProfile,
    theme: Option<ThemePack>,
    base: (Config, Option<ThemePack>),
    account: String,
}

struct State {
    config: Option<ConfigStore>,
    profiles: Option<ProfileStore>,
    rows: Vec<Row>,
    /// Opaque keys of this session, stable per file while it is listed.
    keys: BTreeMap<ProfileFile, String>,
    next_key: u64,
    selected: Option<ProfileFile>,
    confirm: Confirm,
    preview: Option<Preview>,
    truncated: bool,
    message: String,
    error: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            config: None,
            profiles: None,
            rows: Vec::new(),
            keys: BTreeMap::new(),
            next_key: 1,
            selected: None,
            confirm: Confirm::None,
            preview: None,
            truncated: false,
            message: String::new(),
            error: false,
        }
    }
}

thread_local! {
    // Slint callbacks, hooks and projections run on the UI thread only.
    static STATE: RefCell<State> = RefCell::new(State::default());
    // Rows update in place while the shown profiles are unchanged, so focus
    // in the list survives a save or a reload.
    static ITEMS: Rc<VecModel<StyleProfileItem>> = Rc::new(VecModel::default());
}

/// One system file dialog, with its file work, at a time.
static DIALOG: AtomicBool = AtomicBool::new(false);

struct DialogPermit;

impl DialogPermit {
    fn acquire() -> Option<Self> {
        DIALOG
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for DialogPermit {
    fn drop(&mut self) {
        DIALOG.store(false, Ordering::Release);
    }
}

fn with_state<R>(update: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| update(&mut state.borrow_mut()))
}

fn set_message(text: impl Into<String>, error: bool) {
    with_state(|state| {
        state.message = text.into();
        state.error = error;
    });
}

pub(crate) fn register(ui: &AppWindow, store: &ConfigStore) {
    let profiles = ProfileStore::for_config(store).ok();
    let available = profiles.is_some();
    with_state(|state| {
        state.config = Some(store.clone());
        state.profiles = profiles;
    });
    if !available {
        set_message("Style profiles are unavailable: the settings folder was not found.", true);
    }
    refresh(ui);

    let weak = ui.as_weak();
    ui.on_style_profile_filter_changed(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let bounded: String = text.chars().take(MAX_FILTER_CHARS).collect();
        if bounded.as_str() != text.as_str() {
            ui.set_style_profile_filter(bounded.into());
        }
        project(&ui);
    });
    let weak = ui.as_weak();
    ui.on_style_profile_chosen(move |key| {
        let Some(ui) = weak.upgrade() else { return };
        let name = with_state(|state| {
            let row = state.rows.iter().find(|row| row.key == key.as_str())?;
            state.selected = Some(row.file.clone());
            state.confirm = Confirm::None;
            row.profile.as_ref().ok().map(|profile| profile.name.clone())
        });
        if let Some(name) = name {
            ui.set_style_profile_name_input(name.into());
        }
        set_message("", false);
        project(&ui);
    });
    let weak = ui.as_weak();
    ui.on_style_profiles_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            set_message("", false);
            refresh(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_save_current(move || {
        if let Some(ui) = weak.upgrade() {
            save_current(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_rename(move || {
        if let Some(ui) = weak.upgrade() {
            rename(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_duplicate(move || {
        if let Some(ui) = weak.upgrade() {
            duplicate(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_delete(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(file) = with_state(|state| state.selected.clone()) else {
            return;
        };
        with_state(|state| state.confirm = Confirm::Delete(file));
        set_message("", false);
        project(&ui);
    });
    let weak = ui.as_weak();
    ui.on_style_profile_confirm(move || {
        if let Some(ui) = weak.upgrade() {
            confirm(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_dismiss(move || {
        let Some(ui) = weak.upgrade() else { return };
        with_state(|state| state.confirm = Confirm::None);
        set_message("Nothing was changed.", false);
        project(&ui);
    });
    let weak = ui.as_weak();
    ui.on_style_profile_preview(move || {
        if let Some(ui) = weak.upgrade() {
            start_preview(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_apply(move || {
        if let Some(ui) = weak.upgrade() {
            apply(&ui, false);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_preview_apply(move || {
        if let Some(ui) = weak.upgrade() {
            apply(&ui, true);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_cancel_preview(move || {
        if let Some(ui) = weak.upgrade() {
            cancel_preview(&ui, "");
            notify(&ui, "Preview cancelled. Your look is restored.", false);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_notice_dismiss(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_style_profile_notice("".into());
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_import(move || {
        if let Some(ui) = weak.upgrade() {
            import(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_style_profile_export(move || {
        if let Some(ui) = weak.upgrade() {
            export(&ui);
        }
    });
}

/// Re-list the profiles folder, keeping the selection while its file exists.
fn refresh(ui: &AppWindow) {
    let listing = with_state(|state| state.profiles.as_ref().map(ProfileStore::list));
    match listing {
        None => {}
        Some(Err(error)) => {
            with_state(|state| state.rows.clear());
            set_message(format!("Could not list style profiles: {error}."), true);
        }
        Some(Ok(listing)) => with_state(|state| {
            state.truncated = listing.truncated;
            let mut rows = Vec::with_capacity(listing.entries.len());
            for entry in listing.entries {
                let key = match state.keys.get(&entry.file) {
                    Some(key) => key.clone(),
                    None => {
                        let key = format!("profile-{}", state.next_key);
                        state.next_key += 1;
                        state.keys.insert(entry.file.clone(), key.clone());
                        key
                    }
                };
                rows.push(Row {
                    key,
                    file: entry.file,
                    profile: entry.profile.map_err(|error| error.to_string()),
                });
            }
            rows.sort_by_cached_key(|row| match &row.profile {
                Ok(profile) => (0, profile.name.to_lowercase()),
                Err(_) => (1, row.file.as_str().to_owned()),
            });
            state
                .keys
                .retain(|file, _| rows.iter().any(|row| row.file == *file));
            if state
                .selected
                .as_ref()
                .is_some_and(|selected| !rows.iter().any(|row| row.file == *selected))
            {
                state.selected = None;
                state.confirm = Confirm::None;
            }
            state.rows = rows;
        }),
    }
    project(ui);
}

/// The look shown now, ignoring a preview: what Save current look saves and
/// what a profile is compared with.
fn accepted_look() -> Option<(Config, Option<ThemePack>)> {
    with_state(|state| state.preview.as_ref().map(|preview| preview.base.clone()))
        .or_else(crate::appearance::remembered_theme_source)
}

fn theme_label(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::Dark => "Dark",
        ThemeMode::Light => "Light",
        ThemeMode::HighContrast => "High contrast",
        ThemeMode::System => "System",
    }
}

fn layout_label(chat: &ChatStyle) -> &'static str {
    match chat.layout {
        MessageLayout::Stacked => "stacked messages",
        MessageLayout::Compact => "compact messages",
        MessageLayout::Columns => "column messages",
    }
}

/// A short description of a profile's look, from validated values only.
fn summary(profile: &StyleProfile) -> String {
    let appearance = &profile.appearance;
    let mut parts = vec![theme_label(appearance.theme).to_owned()];
    if let Some(pack) = &appearance.theme_pack {
        parts.push(format!("{pack} theme"));
    }
    let family = profile
        .typography
        .message_font_family
        .as_deref()
        .unwrap_or(&appearance.font_family);
    parts.push(format!(
        "{} {} px",
        crate::directory_plain_text(family, 32),
        profile.typography.message_size
    ));
    parts.push(layout_label(&profile.chat).to_owned());
    if !profile.colors.is_empty() {
        parts.push(match profile.colors.len() {
            1 => "1 custom color".to_owned(),
            count => format!("{count} custom colors"),
        });
    }
    if let Some(colors) = &profile.senders.colors {
        parts.push(format!("{} sender colors", colors.len()));
    }
    parts.join(" · ")
}

fn shown_name(name: &str) -> String {
    crate::directory_plain_text(name, style_profiles::MAX_PROFILE_NAME_CHARS)
}

/// Publish the list, the selected profile and the preview state.
fn project(ui: &AppWindow) {
    let filter = ui.get_style_profile_filter().to_lowercase();
    let words: Vec<&str> = filter.split_whitespace().collect();
    let current = accepted_look();
    with_state(|state| {
        let matches_current = |profile: &StyleProfile| {
            current
                .as_ref()
                .is_some_and(|(config, _)| profile.matches(config))
        };
        let items: Vec<StyleProfileItem> = state
            .rows
            .iter()
            .filter_map(|row| {
                let (name, detail, valid, matches) = match &row.profile {
                    Ok(profile) => (
                        shown_name(&profile.name),
                        summary(profile),
                        true,
                        matches_current(profile),
                    ),
                    Err(problem) => (
                        format!("Invalid profile file {}", row.file.as_str()),
                        problem.clone(),
                        false,
                        false,
                    ),
                };
                let haystack = format!("{name} {detail}").to_lowercase();
                words
                    .iter()
                    .all(|word| haystack.contains(word))
                    .then(|| StyleProfileItem {
                        key: row.key.as_str().into(),
                        name: name.into(),
                        detail: detail.into(),
                        valid,
                        current: matches,
                    })
            })
            .collect();
        ITEMS.with(|model| {
            let same_rows = model.row_count() == items.len()
                && model.iter().zip(&items).all(|(shown, item)| shown.key == item.key);
            if same_rows {
                for (index, item) in items.into_iter().enumerate() {
                    if model.row_data(index).as_ref() != Some(&item) {
                        model.set_row_data(index, item);
                    }
                }
            } else {
                model.set_vec(items);
            }
            ui.set_style_profile_items(ModelRc::from(model.clone()));
        });
        let selected = state
            .selected
            .as_ref()
            .and_then(|file| state.rows.iter().find(|row| row.file == *file));
        let previewing = state.preview.as_ref();
        let confirm_text = match &state.confirm {
            Confirm::None => String::new(),
            Confirm::Delete(file) => {
                let name = state
                    .rows
                    .iter()
                    .find(|row| row.file == *file)
                    .and_then(|row| row.profile.as_ref().ok())
                    .map_or_else(|| "this profile".to_owned(), |profile| shown_name(&profile.name));
                format!("Delete {name}? Its file is removed; your current look does not change.")
            }
            Confirm::Replace { profile, .. } => format!(
                "A profile named {} exists. Replace it?",
                shown_name(&profile.name)
            ),
        };
        let mut editor = StyleProfileState {
            confirm: match state.confirm {
                Confirm::None => 0,
                Confirm::Delete(_) => 1,
                Confirm::Replace { .. } => 2,
            },
            confirm_text: confirm_text.into(),
            previewing: previewing.is_some(),
            truncated: state.truncated,
            ..Default::default()
        };
        if let Some(row) = selected {
            editor.key = row.key.as_str().into();
            match &row.profile {
                Ok(profile) => {
                    editor.name = shown_name(&profile.name).into();
                    editor.summary = summary(profile).into();
                    editor.valid = true;
                    editor.matches_current = matches_current(profile);
                    editor.previewed = previewing.is_some_and(|preview| preview.file == row.file);
                }
                Err(problem) => {
                    editor.name = format!("Invalid profile file {}", row.file.as_str()).into();
                    editor.problem = format!(
                        "{problem}. Fix the file in a text editor and refresh, or delete it."
                    )
                    .into();
                }
            }
        }
        ui.set_style_profile_selected_key(editor.key.clone());
        ui.set_style_profile_state(editor);
        ui.set_style_profile_message(state.message.as_str().into());
        ui.set_style_profile_message_error(state.error);
        ui.set_style_preview_active(previewing.is_some());
        ui.set_style_preview_name(
            previewing
                .map(|preview| shown_name(&preview.name))
                .unwrap_or_default()
                .into(),
        );
    });
}

fn typed_name(ui: &AppWindow) -> Result<String, ProfileError> {
    style_profiles::validate_name(ui.get_style_profile_name_input().as_str())
}

fn store() -> Option<ProfileStore> {
    with_state(|state| state.profiles.clone())
}

/// Select a file and show its name in the name field.
fn select(ui: &AppWindow, file: ProfileFile, name: &str) {
    with_state(|state| {
        state.selected = Some(file);
        state.confirm = Confirm::None;
    });
    ui.set_style_profile_name_input(name.into());
}

/// The listed file whose profile is named `name`, ignoring case.
fn file_named(name: &str) -> Option<ProfileFile> {
    let name = name.trim().to_lowercase();
    with_state(|state| {
        state
            .rows
            .iter()
            .find(|row| {
                row.profile
                    .as_ref()
                    .is_ok_and(|profile| profile.name.trim().to_lowercase() == name)
            })
            .map(|row| row.file.clone())
    })
}

/// Save `profile` as a new file, or ask to replace the one with its name.
fn create_or_confirm(ui: &AppWindow, profile: StyleProfile, saved: &str) {
    let Some(store) = store() else { return };
    match store.create(&profile) {
        Ok(file) => {
            refresh(ui);
            select(ui, file, &profile.name);
            set_message(format!("{saved} {}.", shown_name(&profile.name)), false);
        }
        Err(ProfileError::NameTaken) => {
            // The list may be stale; the name decides, not the cached rows.
            refresh(ui);
            match file_named(&profile.name) {
                Some(file) => {
                    with_state(|state| {
                        state.selected = Some(file.clone());
                        state.confirm = Confirm::Replace { file, profile };
                    });
                    set_message("", false);
                }
                None => set_message("A profile with this name already exists.", true),
            }
        }
        Err(error) => set_message(format!("Could not save the profile: {error}."), true),
    }
    project(ui);
}

fn save_current(ui: &AppWindow) {
    if with_state(|state| state.preview.is_some()) {
        set_message(PREVIEW_FIRST, true);
        project(ui);
        return;
    }
    let name = match typed_name(ui) {
        Ok(name) => name,
        Err(error) => {
            set_message(format!("Type a name first: {error}."), true);
            project(ui);
            return;
        }
    };
    let Some((config, _)) = accepted_look() else {
        set_message("There is no accepted look to save yet.", true);
        project(ui);
        return;
    };
    create_or_confirm(ui, StyleProfile::from_config(&name, &config), "Saved");
}

fn selected_file(ui: &AppWindow) -> Option<ProfileFile> {
    let file = with_state(|state| state.selected.clone());
    if file.is_none() {
        set_message("Choose a profile in the list first.", true);
        project(ui);
    }
    file
}

fn rename(ui: &AppWindow) {
    let Some(file) = selected_file(ui) else { return };
    let Some(store) = store() else { return };
    let result = typed_name(ui).and_then(|name| store.rename(&file, &name));
    match result {
        Ok(profile) => {
            // A previewed profile keeps previewing under its new name.
            with_state(|state| {
                if let Some(preview) = state.preview.as_mut().filter(|preview| preview.file == file) {
                    preview.name.clone_from(&profile.name);
                }
            });
            set_message(format!("Renamed to {}.", shown_name(&profile.name)), false);
        }
        Err(error) => set_message(format!("Could not rename the profile: {error}."), true),
    }
    refresh(ui);
}

fn duplicate(ui: &AppWindow) {
    let Some(file) = selected_file(ui) else { return };
    let Some(store) = store() else { return };
    let result = typed_name(ui).and_then(|name| {
        store
            .duplicate(&file, &name)
            .map(|copy| (copy, name))
    });
    match result {
        Ok((copy, name)) => {
            refresh(ui);
            select(ui, copy, &name);
            set_message(format!("Saved a copy as {}.", shown_name(&name)), false);
        }
        Err(ProfileError::NameTaken) => {
            set_message("Type a new name for the copy first.", true);
        }
        Err(error) => set_message(format!("Could not duplicate the profile: {error}."), true),
    }
    project(ui);
}

fn confirm(ui: &AppWindow) {
    let confirm = with_state(|state| std::mem::replace(&mut state.confirm, Confirm::None));
    let Some(store) = store() else { return };
    match confirm {
        Confirm::None => {}
        Confirm::Delete(file) => {
            // The previewed profile cannot be applied once its file is gone.
            if with_state(|state| state.preview.as_ref().is_some_and(|p| p.file == file)) {
                cancel_preview(ui, "");
            }
            match store.delete(&file) {
                Ok(()) => set_message("Profile deleted. Your current look did not change.", false),
                Err(error) => set_message(format!("Could not delete the profile: {error}."), true),
            }
            refresh(ui);
        }
        Confirm::Replace { file, profile } => {
            // A previewed profile whose file is replaced is no longer what
            // its preview shows.
            if with_state(|state| state.preview.as_ref().is_some_and(|p| p.file == file)) {
                cancel_preview(ui, "");
            }
            match store.replace(&file, &profile) {
                Ok(()) => {
                    select(ui, file, &profile.name);
                    set_message(format!("Replaced {}.", shown_name(&profile.name)), false);
                }
                Err(error) => set_message(format!("Could not replace the profile: {error}."), true),
            }
            refresh(ui);
        }
    }
}

/// Fixed text for a theme pack the profile names that cannot be loaded.
fn theme_problem(error: &ConfigError) -> &'static str {
    match error {
        ConfigError::Input(InputError::Missing) => {
            "This profile uses a theme pack that is not in your themes folder"
        }
        _ => "This profile's theme pack could not be loaded",
    }
}

fn start_preview(ui: &AppWindow) {
    let Some(file) = selected_file(ui) else { return };
    let (Some(store), Some(config_store)) = with_state(|state| (state.profiles.clone(), state.config.clone())) else {
        return;
    };
    let profile = match store.read(&file) {
        Ok(profile) => profile,
        Err(error) => {
            set_message(format!("Could not preview the profile: {error}."), true);
            refresh(ui);
            return;
        }
    };
    // A second preview replaces the first and keeps the original base.
    let Some(base) = accepted_look() else {
        set_message("There is no accepted look to preview over yet.", true);
        project(ui);
        return;
    };
    let mut candidate = base.0.clone();
    profile.apply_to(&mut candidate);
    if let Err(error) = candidate.validate() {
        set_message(format!("Could not preview the profile: {error}."), true);
        project(ui);
        return;
    }
    let theme = match config_store.load_theme(&candidate) {
        Ok(theme) => theme,
        Err(error) => {
            set_message(format!("{}. Nothing was changed.", theme_problem(&error)), true);
            project(ui);
            return;
        }
    };
    let account = ui.get_active_account_local_id().to_string();
    let name = profile.name.clone();
    let shown_theme = theme.clone();
    with_state(|state| {
        state.confirm = Confirm::None;
        let base = state
            .preview
            .take()
            .map_or(base, |previous| previous.base);
        state.preview = Some(Preview {
            file,
            name: name.clone(),
            profile,
            theme: shown_theme,
            base,
            account,
        });
    });
    ui.set_style_profile_notice("".into());
    crate::apply_appearance(ui, &candidate, theme.as_ref(), false);
    set_message(
        format!(
            "Previewing {}. Nothing is saved until you apply it; Cancel restores your look.",
            shown_name(&name)
        ),
        false,
    );
    project(ui);
}

/// End a preview and restore the look it replaced. `note` replaces the
/// message when not empty.
fn cancel_preview(ui: &AppWindow, note: &str) {
    let Some(preview) = with_state(|state| state.preview.take()) else {
        return;
    };
    let (config, theme) = &preview.base;
    crate::apply_appearance(ui, config, theme.as_ref(), true);
    if !note.is_empty() {
        set_message(note, false);
    }
    project(ui);
}

/// Apply the previewed profile (`previewed`) or the selected one. Either way
/// a preview ends first, without restoring it: the save shows the new look.
fn apply(ui: &AppWindow, previewed: bool) {
    let (Some(store), Some(config_store)) =
        with_state(|state| (state.profiles.clone(), state.config.clone()))
    else {
        return;
    };
    // The preview bar applies exactly the look it shows, not the file as it
    // is now; the Settings card applies the selected profile's file.
    let profile = if previewed {
        let Some(profile) =
            with_state(|state| state.preview.as_ref().map(|preview| preview.profile.clone()))
        else {
            return;
        };
        profile
    } else {
        let Some(file) = selected_file(ui) else { return };
        match store.read(&file) {
            Ok(profile) => profile,
            Err(error) => {
                cancel_preview(ui, "");
                notify(ui, format!("Could not apply the profile: {error}."), true);
                refresh(ui);
                return;
            }
        }
    };
    // Check the theme pack against the look as it would be applied, so a
    // missing pack is reported before anything is saved.
    if let Some((mut candidate, _)) = accepted_look() {
        profile.apply_to(&mut candidate);
        if let Err(error) = config_store.load_theme(&candidate) {
            set_message(format!("{}. Nothing was changed.", theme_problem(&error)), true);
            project(ui);
            return;
        }
    }
    // End the preview without restoring it: the save below applies the same
    // look, and the save guard must not treat it as an edit made during it.
    let preview = with_state(|state| state.preview.take());
    let saved = crate::save_config_edit(ui, &config_store, |config| {
        profile.apply_to(config);
        true
    });
    match saved {
        Some(_) => notify(ui, format!("Applied {}.", shown_name(&profile.name)), false),
        None => {
            if let Some(preview) = preview {
                let (config, theme) = &preview.base;
                crate::apply_appearance(ui, config, theme.as_ref(), true);
            }
            notify(
                ui,
                "Could not apply the profile: the settings file has a problem (shown above). Nothing was changed.",
                true,
            );
        }
    }
    project(ui);
}

/// A preview outcome: shown in the Settings card and in a notice on every
/// page (a polite live region), because a preview can end on any page.
fn notify(ui: &AppWindow, text: impl Into<String>, error: bool) {
    let text = text.into();
    ui.set_style_profile_notice(text.as_str().into());
    ui.set_style_profile_notice_error(error);
    set_message(text, error);
    project(ui);
}

/// Called after every accepted configuration is applied, so the list shows
/// which profile matches the look now.
pub(crate) fn config_applied(ui: &AppWindow) {
    project(ui);
}

/// Whether a style profile preview is shown.
pub(crate) fn previewing() -> bool {
    with_state(|state| state.preview.is_some())
}

/// Called by a Settings save while previewing, with the re-read file before
/// and after the change. The preview ends and its look is restored. A change
/// that leaves the look alone may be saved; one that changes the look is
/// refused, because its value may have been read from the preview.
pub(crate) fn end_preview_for_save(ui: &AppWindow, before: &Config, after: &Config) -> bool {
    if !previewing() {
        return true;
    }
    let keeps_look = style_profiles::same_look(before, after);
    cancel_preview(ui, "");
    if keeps_look {
        notify(ui, "The preview ended because a setting changed.", false);
    } else {
        notify(
            ui,
            "The preview was cancelled and your look restored; that change was not saved. Make it again to save it.",
            true,
        );
    }
    project(ui);
    keeps_look
}

/// Called for every accepted configuration. While previewing, a
/// configuration with the same look as the preview's base (the watcher
/// re-reading a save of other settings) keeps the preview: it becomes the
/// base, and the previewed look is shown over it again, so its other
/// settings (text size, Reduce motion, the focus ring, grouping, sorting)
/// take effect at once. Any other configuration ends the preview, and the
/// caller applies it.
pub(crate) fn keeps_preview(ui: &AppWindow, loaded: &LoadedConfig) -> bool {
    let kept = with_state(|state| {
        let preview = state.preview.as_mut()?;
        let (base, theme) = &preview.base;
        if style_profiles::same_look(base, &loaded.config) && *theme == loaded.theme {
            preview.base = (loaded.config.clone(), loaded.theme.clone());
            let mut candidate = loaded.config.clone();
            preview.profile.apply_to(&mut candidate);
            Some(Some((candidate, preview.theme.clone())))
        } else {
            state.preview = None;
            Some(None)
        }
    });
    match kept {
        None => false,
        Some(Some((candidate, theme))) => {
            crate::apply_appearance(ui, &candidate, theme.as_ref(), false);
            true
        }
        Some(None) => {
            notify(ui, "The preview ended because the settings changed.", false);
            project(ui);
            false
        }
    }
}

/// Called whenever the active account is published. A preview started for
/// another account, or before a detach, is cancelled.
pub(crate) fn check_account(ui: &AppWindow) {
    let changed = with_state(|state| {
        state.preview.as_ref().is_some_and(|preview| {
            preview.account.as_str() != ui.get_active_account_local_id().as_str()
        })
    });
    if changed {
        cancel_preview(ui, "");
        notify(ui, "Switching accounts cancelled the preview.", false);
    }
}

fn import(ui: &AppWindow) {
    if store().is_none() {
        return;
    }
    let Some(permit) = DialogPermit::acquire() else {
        set_message("A file dialog is already open.", true);
        project(ui);
        return;
    };
    ui.set_style_profile_busy(true);
    let weak = ui.as_weak();
    let spawned = std::thread::Builder::new()
        .name("morpheus-style-profile-import".into())
        .spawn(move || {
            // macOS runs the panel on the main thread for this worker.
            let chosen = rfd::FileDialog::new()
                .set_title("Import a style profile")
                .add_filter("Morpheus style profile", &["toml"])
                .pick_file();
            let result = chosen.map(|path| {
                style_profiles::import_profile(&path).map_err(|error| error.to_string())
            });
            drop(permit);
            let _ = weak.upgrade_in_event_loop(move |ui| finish_import(&ui, result));
        });
    if spawned.is_err() {
        ui.set_style_profile_busy(false);
        set_message("The file dialog could not open.", true);
        project(ui);
    }
}

fn finish_import(ui: &AppWindow, result: Option<Result<StyleProfile, String>>) {
    ui.set_style_profile_busy(false);
    match result {
        None => {}
        Some(Err(problem)) => {
            set_message(format!("Could not import that file: {problem}. Nothing was added."), true);
            project(ui);
        }
        // Imported, never applied: preview it to see the look.
        Some(Ok(profile)) => create_or_confirm(ui, profile, "Imported"),
    }
}

fn export(ui: &AppWindow) {
    let Some(file) = selected_file(ui) else { return };
    let Some(store) = store() else { return };
    let profile = match store.read(&file) {
        Ok(profile) => profile,
        Err(error) => {
            set_message(format!("Could not export the profile: {error}."), true);
            refresh(ui);
            return;
        }
    };
    let Some(permit) = DialogPermit::acquire() else {
        set_message("A file dialog is already open.", true);
        project(ui);
        return;
    };
    ui.set_style_profile_busy(true);
    let suggested = style_profiles::export_file_name(&profile.name);
    let documents = directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(std::path::Path::to_path_buf));
    let name = profile.name.clone();
    let weak = ui.as_weak();
    let spawned = std::thread::Builder::new()
        .name("morpheus-style-profile-export".into())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Export style profile")
                .add_filter("Morpheus style profile", &["toml"])
                .set_file_name(suggested);
            if let Some(documents) = documents {
                dialog = dialog.set_directory(documents);
            }
            // The system dialog asks before replacing an existing file.
            let result = dialog.save_file().map(|path| {
                style_profiles::export_profile(&profile, &path).map_err(|error| error.to_string())
            });
            drop(permit);
            let _ = weak.upgrade_in_event_loop(move |ui| finish_export(&ui, &name, result));
        });
    if spawned.is_err() {
        ui.set_style_profile_busy(false);
        set_message("The file dialog could not open.", true);
        project(ui);
    }
}

fn finish_export(ui: &AppWindow, name: &str, result: Option<Result<(), String>>) {
    ui.set_style_profile_busy(false);
    match result {
        None => return,
        Some(Ok(())) => set_message(format!("Exported {}.", shown_name(name)), false),
        Some(Err(problem)) => set_message(format!("Could not export the profile: {problem}."), true),
    }
    project(ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_describe_the_look_without_files_or_paths() {
        let mut config = Config::default();
        config.appearance.theme = ThemeMode::Light;
        config.appearance.theme_pack = Some("paper".into());
        config.typography.message_font_family = Some("serif".into());
        config.typography.message_size = 16;
        config.chat.layout = MessageLayout::Columns;
        config.colors.insert("link".into(), "#335F86".into());
        config.senders.colors = Some(vec!["#123456".into(), "#654321".into()]);
        let text = summary(&StyleProfile::from_config("Reading", &config));
        assert_eq!(
            text,
            "Light · paper theme · serif 16 px · column messages · 1 custom color · 2 sender colors"
        );
        assert_eq!(
            summary(&StyleProfile::named("Plain")),
            "Dark · Inter 14 px · stacked messages"
        );
    }

    #[test]
    fn the_dialog_permit_admits_one_operation_at_a_time() {
        let first = DialogPermit::acquire().expect("free");
        assert!(DialogPermit::acquire().is_none());
        drop(first);
        assert!(DialogPermit::acquire().is_some());
    }
}
