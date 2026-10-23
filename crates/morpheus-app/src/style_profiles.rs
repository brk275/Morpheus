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
//!
//! Scope fences are shared with theme packs (`theme_studio::scope`). Work
//! captures its scope (appearance epoch, account, operation) before its
//! first asynchronous step:
//! - Import, before its dialog: the answer adds a profile only while that
//!   scope is current.
//! - A preview, when it starts: Apply from the preview bar checks it again
//!   itself before writing, and the account hook cancels a preview whose
//!   scope ended (an account switch, A -> B -> A included, a detach or a new
//!   settings context).
//! - Export, before its dialog: the write is admitted only in that scope,
//!   and only while the profile file still holds exactly what was captured.
//!   The admitted write runs on a worker, never the UI thread, with the
//!   dialog permit held, and is reported once. If the scope changed while it
//!   ran, it is reported without naming the profile, and never as "nothing
//!   written" after a real write.
//! A system file dialog cannot be cancelled from here; a stale answer is
//! ignored.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use morpheus_config::style_profiles::{
    self, ProfileError, ProfileFile, ProfileStore, StyleProfile,
};
use morpheus_config::{
    ChatStyle, Config, ConfigError, ConfigStore, InputError, LoadedConfig, MessageLayout,
    ThemeMode, ThemePack,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::theme_studio::scope::{self, Scope};
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
    /// Captured when the preview started; Apply from the preview bar acts
    /// only while it is current.
    scope: Scope,
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
    /// The export admitted in this scope whose write is running on a
    /// worker; reported once when it ends.
    writing: Option<Scope>,
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
            writing: None,
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

/// One operation, including the dialog-to-export handoff. A worker registry
/// retains this same lease until the actual worker AND supervisor were joined.
/// A callback result or body-level permit drop cannot admit another dialog.
static DIALOG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static DIALOG_WORKERS: OnceLock<Mutex<DialogWorkers>> = OnceLock::new();
fn workers() -> &'static Mutex<DialogWorkers> {
    DIALOG_WORKERS.get_or_init(Default::default)
}
struct DialogLease(Arc<AtomicBool>);
impl Drop for DialogLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
#[derive(Clone)]
pub(crate) struct DialogPermit {
    _lease: Arc<DialogLease>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppearanceDrainError {
    Pending,
    Worker,
    Ownership,
}
impl std::fmt::Display for AppearanceDrainError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Pending => "A file dialog or appearance export is still open. Close the dialog before quitting.",
            Self::Worker => "An appearance worker failed; clean shutdown could not be confirmed.",
            Self::Ownership => "Appearance worker ownership could not be confirmed.",
        })
    }
}
impl std::error::Error for AppearanceDrainError {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppearanceObservation {
    Pending,
    Released(Result<(), AppearanceDrainError>),
    Retained,
}
struct DialogWorker {
    supervisor: Option<std::thread::JoinHandle<()>>,
    outcome: Arc<Mutex<Option<Result<(), AppearanceDrainError>>>>,
    // Including the supervisor's real join, not just its completion flag.
    _lease: DialogPermit,
    retained: bool,
}
#[derive(Default)]
struct DialogWorkers {
    closed: bool,
    failed: Option<AppearanceDrainError>,
    owners: Vec<DialogWorker>,
}
impl DialogWorkers {
    fn observe(&mut self) -> AppearanceObservation {
        let mut index = 0;
        while index < self.owners.len() {
            let owner = &mut self.owners[index];
            if owner.retained
                || !owner
                    .supervisor
                    .as_ref()
                    .is_some_and(|thread| thread.is_finished())
            {
                index += 1;
                continue;
            }
            // is_finished only permits this actual join; it is no receipt.
            let joined = owner.supervisor.take().map(|thread| thread.join());
            let result = owner
                .outcome
                .lock()
                .ok()
                .and_then(|mut result| result.take());
            if !matches!(joined, Some(Ok(()))) || result.is_none() {
                owner.retained = true;
                self.failed = Some(AppearanceDrainError::Ownership);
                index += 1;
            } else {
                if let Some(Err(error)) = result {
                    self.failed = Some(error);
                }
                // Only a real worker/TLS join followed by supervisor join
                // releases the registry's share of the operation lease.
                drop(self.owners.swap_remove(index));
            }
        }
        if self.owners.iter().any(|owner| owner.retained) {
            AppearanceObservation::Retained
        } else if !self.owners.is_empty() {
            AppearanceObservation::Pending
        } else {
            AppearanceObservation::Released(self.failed.map_or(Ok(()), Err))
        }
    }

    fn spawn(
        &mut self,
        permit: &DialogPermit,
        name: &'static str,
        body: impl FnOnce() + Send + 'static,
        spawn_failed: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        let _ = self.observe();
        // One global operation can hand its permit from a completed dialog
        // body to one file writer while that dialog's TLS is still retiring.
        // Its retained lease forbids a third/unrelated operation.
        if self.closed || self.failed.is_some() || self.owners.len() >= 2 {
            return Err(std::io::Error::other(
                "Appearance work is closing or still owned",
            ));
        }
        let outcome = Arc::new(Mutex::new(None));
        let publish = outcome.clone();
        let (allow, admitted) = std::sync::mpsc::sync_channel::<()>(1);
        let supervisor = std::thread::Builder::new()
            .name("morpheus-appearance-join".into())
            .spawn(move || {
                // No native dialog/worker exists until the actual supervisor
                // handle has been published in the registry below.
                let result = if admitted.recv().is_err() {
                    Err(AppearanceDrainError::Ownership)
                } else {
                    match std::thread::Builder::new().name(name.into()).spawn(body) {
                        Ok(worker) => worker.join().map_err(|_| AppearanceDrainError::Worker),
                        Err(_) => {
                            spawn_failed();
                            Err(AppearanceDrainError::Worker)
                        }
                    }
                };
                if let Ok(mut target) = publish.lock() {
                    *target = Some(result);
                }
            })?;
        self.owners.push(DialogWorker {
            supervisor: Some(supervisor),
            outcome,
            _lease: permit.clone(),
            retained: false,
        });
        allow
            .send(())
            .map_err(|_| std::io::Error::other("Appearance supervisor did not admit work"))
    }
}
impl DialogPermit {
    pub(crate) fn unavailable_message() -> &'static str {
        match workers().lock() {
            Ok(workers) if workers.closed => {
                "Morpheus is closing; no new file operation was started."
            }
            Ok(workers) if workers.failed.is_some() => {
                "An earlier appearance worker failed. New file operations remain blocked."
            }
            Ok(_) => "A file dialog or appearance export is already active.",
            Err(_) => {
                "Appearance worker ownership could not be confirmed. New file operations remain blocked."
            }
        }
    }
    pub(crate) fn acquire() -> Option<Self> {
        let mut workers = workers().lock().ok()?;
        let _ = workers.observe();
        let busy = DIALOG
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone();
        Self::acquire_in(&mut workers, busy)
    }
    fn acquire_in(workers: &mut DialogWorkers, busy: Arc<AtomicBool>) -> Option<Self> {
        let _ = workers.observe();
        if workers.closed || workers.failed.is_some() {
            return None;
        }
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self {
                _lease: Arc::new(DialogLease(busy)),
            })
    }
    pub(crate) fn spawn(
        &self,
        name: &'static str,
        body: impl FnOnce() + Send + 'static,
        spawn_failed: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        workers()
            .lock()
            .map_err(|_| std::io::Error::other("Appearance ownership is unavailable"))?
            .spawn(self, name, body, spawn_failed)
    }
}

/// Called on the UI thread before cleanup yields. Queued dialog answers keep
/// their original scope, now permanently invalid; admitted writers still run
/// and report their actual result. No file/profile is deleted here.
pub(crate) fn close_application_admission() {
    if let Ok(mut workers) = workers().lock() {
        workers.closed = true;
    }
    scope::close();
    with_state(|state| {
        state.config = None;
        state.profiles = None;
        state.preview = None;
        state.confirm = Confirm::None;
    });
    crate::theme_studio::close_file_admission();
}
pub(crate) fn appearance_observation() -> AppearanceObservation {
    workers()
        .lock()
        .map(|mut workers| workers.observe())
        .unwrap_or(AppearanceObservation::Retained)
}
pub(crate) async fn wait_for_appearance_release() -> Result<(), AppearanceDrainError> {
    let limit = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match appearance_observation() {
            AppearanceObservation::Released(result) => return result,
            AppearanceObservation::Retained => return Err(AppearanceDrainError::Ownership),
            AppearanceObservation::Pending if std::time::Instant::now() >= limit => {
                return Err(AppearanceDrainError::Pending);
            }
            AppearanceObservation::Pending => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await
            }
        }
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
        set_message(
            "Style profiles are unavailable: the settings folder was not found.",
            true,
        );
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
            row.profile
                .as_ref()
                .ok()
                .map(|profile| profile.name.clone())
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
                && model
                    .iter()
                    .zip(&items)
                    .all(|(shown, item)| shown.key == item.key);
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
                    .map_or_else(
                        || "this profile".to_owned(),
                        |profile| shown_name(&profile.name),
                    );
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
    let Some(file) = selected_file(ui) else {
        return;
    };
    let Some(store) = store() else { return };
    let result = typed_name(ui).and_then(|name| store.rename(&file, &name));
    match result {
        Ok(profile) => {
            // A previewed profile keeps previewing under its new name.
            with_state(|state| {
                if let Some(preview) = state
                    .preview
                    .as_mut()
                    .filter(|preview| preview.file == file)
                {
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
    let Some(file) = selected_file(ui) else {
        return;
    };
    let Some(store) = store() else { return };
    let result =
        typed_name(ui).and_then(|name| store.duplicate(&file, &name).map(|copy| (copy, name)));
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

const DRAFT_FIRST: &str = "Apply or discard your unsaved appearance changes first.";

fn start_preview(ui: &AppWindow) {
    // A preview, the theme studio's draft and a theme pack preview all own
    // the shown look.
    if crate::theme_studio::drafting() || crate::theme_studio::pack_previewing() {
        set_message(DRAFT_FIRST, true);
        project(ui);
        return;
    }
    let Some(file) = selected_file(ui) else {
        return;
    };
    let (Some(store), Some(config_store)) =
        with_state(|state| (state.profiles.clone(), state.config.clone()))
    else {
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
            set_message(
                format!("{}. Nothing was changed.", theme_problem(&error)),
                true,
            );
            project(ui);
            return;
        }
    };
    let opened = scope::capture(ui);
    let name = profile.name.clone();
    let shown_theme = theme.clone();
    let previous = with_state(|state| state.preview.take());
    // A second preview keeps the original base when it belongs to the same
    // scope.
    let base = match previous {
        Some(previous) if scope::current(ui, &previous.scope) => previous.base,
        _ => base,
    };
    with_state(|state| {
        state.confirm = Confirm::None;
        state.preview = Some(Preview {
            file,
            name: name.clone(),
            profile,
            theme: shown_theme,
            base,
            scope: opened,
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
    if crate::theme_studio::drafting() {
        set_message(DRAFT_FIRST, true);
        project(ui);
        return;
    }
    let (Some(store), Some(config_store)) =
        with_state(|state| (state.profiles.clone(), state.config.clone()))
    else {
        return;
    };
    // The preview bar applies exactly the look it shows, not the file as it
    // is now; the Settings card applies the selected profile's file.
    let profile = if previewed {
        let Some((profile, opened)) = with_state(|state| {
            state
                .preview
                .as_ref()
                .map(|preview| (preview.profile.clone(), preview.scope.clone()))
        }) else {
            return;
        };
        // The preview's own scope, checked here rather than trusting that
        // every change was notified. Nothing has been written yet.
        if !scope::current(ui, &opened) {
            cancel_preview(ui, "");
            notify(
                ui,
                "The preview ended because the account or settings changed. Nothing was applied.",
                false,
            );
            return;
        }
        profile
    } else {
        let Some(file) = selected_file(ui) else {
            return;
        };
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
            set_message(
                format!("{}. Nothing was changed.", theme_problem(&error)),
                true,
            );
            project(ui);
            return;
        }
    }
    // End the preview without restoring it: the save below applies the same
    // look, and the save guard must not treat it as an edit made during it.
    let preview = with_state(|state| state.preview.take());
    // Applying a profile is itself the explicit save of a whole look.
    let saved = crate::write_config_edit(ui, &config_store, |config| {
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

/// Called whenever the active account is published. A preview whose scope
/// ended (another account, A -> B -> A, a detach or a new settings context)
/// is cancelled.
pub(crate) fn check_account(ui: &AppWindow) {
    scope::observe(ui);
    let changed = with_state(|state| state.preview.as_ref().map(|preview| preview.scope.clone()))
        .is_some_and(|opened| !scope::current(ui, &opened));
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
        set_message(DialogPermit::unavailable_message(), true);
        project(ui);
        return;
    };
    // Captured before the dialog opens: its answer belongs to this scope.
    let opened = scope::begin_dialog(ui);
    ui.set_style_profile_busy(true);
    let weak = ui.as_weak();
    let dialog_scope = opened.clone();
    let failure_ui = ui.as_weak();
    let failure_scope = opened.clone();
    let worker_owner = permit.clone();
    let spawned = worker_owner.spawn(
        "morpheus-style-profile-import",
        move || {
            // macOS runs the panel on the main thread for this worker.
            let chosen = rfd::FileDialog::new()
                .set_title("Import a style profile")
                .add_filter("Morpheus style profile", &["toml"])
                .pick_file();
            // Reading the chosen file writes nothing; whether it is added is
            // decided on the UI thread.
            let result = chosen.map(|path| {
                style_profiles::import_profile(&path).map_err(|error| error.to_string())
            });
            drop(permit);
            let _ = weak.upgrade_in_event_loop(move |ui| finish_import(&ui, dialog_scope, result));
        },
        move || {
            let _ = failure_ui.upgrade_in_event_loop(move |ui| {
                failed_import_start(&ui, &failure_scope);
            });
        },
    );
    if spawned.is_err() {
        failed_import_start(ui, &opened);
    }
}
fn failed_import_start(ui: &AppWindow, opened: &Scope) {
    scope::finish_dialog(ui, opened);
    ui.set_style_profile_busy(false);
    set_message("The file dialog could not open.", true);
    project(ui);
}

/// The import dialog opened in `opened` returned. Its answer is added to
/// the profiles only while that scope is current; a stale answer is ignored
/// and says so.
fn finish_import(ui: &AppWindow, opened: Scope, result: Option<Result<StyleProfile, String>>) {
    ui.set_style_profile_busy(false);
    if !scope::finish_dialog(ui, &opened) {
        if result.is_some() {
            set_message(
                "The profile you chose was not imported because the account or settings changed while the file dialog was open. Nothing was added.",
                false,
            );
            project(ui);
        }
        return;
    }
    match result {
        None => {}
        Some(Err(problem)) => {
            set_message(
                format!("Could not import that file: {problem}. Nothing was added."),
                true,
            );
            project(ui);
        }
        // Imported, never applied: preview it to see the look.
        Some(Ok(profile)) => create_or_confirm(ui, profile, "Imported"),
    }
}

fn export(ui: &AppWindow) {
    let Some(file) = selected_file(ui) else {
        return;
    };
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
        set_message(DialogPermit::unavailable_message(), true);
        project(ui);
        return;
    };
    // Captured before the dialog opens; the write is admitted only in this
    // scope (see finish_export).
    let opened = scope::begin_dialog(ui);
    ui.set_style_profile_busy(true);
    let suggested = style_profiles::export_file_name(&profile.name);
    let documents = directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(std::path::Path::to_path_buf));
    let weak = ui.as_weak();
    let dialog_scope = opened.clone();
    let failure_ui = ui.as_weak();
    let failure_scope = opened.clone();
    let worker_owner = permit.clone();
    let spawned = worker_owner.spawn(
        "morpheus-style-profile-export",
        move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Export style profile")
                .add_filter("Morpheus style profile", &["toml"])
                .set_file_name(suggested);
            if let Some(documents) = documents {
                dialog = dialog.set_directory(documents);
            }
            // Only the chosen path leaves this thread; the write is admitted
            // on the UI thread, and the permit goes with it.
            let chosen = dialog.save_file();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                finish_export(&ui, dialog_scope, file, profile, chosen, permit);
            });
        },
        move || {
            let _ = failure_ui.upgrade_in_event_loop(move |ui| {
                failed_export_start(&ui, &failure_scope);
            });
        },
    );
    if spawned.is_err() {
        failed_export_start(ui, &opened);
    }
}
fn failed_export_start(ui: &AppWindow, opened: &Scope) {
    scope::finish_dialog(ui, opened);
    ui.set_style_profile_busy(false);
    set_message("The file dialog could not open.", true);
    project(ui);
}

/// The export dialog opened in `opened` returned with `chosen`: the
/// admission boundary.
///
/// The write is admitted only while that scope is current and `file` still
/// holds exactly `profile`. Otherwise nothing is written, and the message
/// says so. An admitted write runs on a worker with `permit` held and is
/// reported by [`finish_write`]. Whatever the system dialog asked about an
/// existing file, the export never replaces one: it asks for a new name
/// instead.
fn finish_export(
    ui: &AppWindow,
    opened: Scope,
    file: ProfileFile,
    profile: StyleProfile,
    chosen: Option<std::path::PathBuf>,
    permit: DialogPermit,
) {
    let admitted = scope::finish_dialog(ui, &opened);
    let Some(path) = chosen else {
        ui.set_style_profile_busy(false);
        return;
    };
    if !admitted {
        ui.set_style_profile_busy(false);
        set_message(
            "The profile was not exported because the account or settings changed while the file dialog was open. Nothing was written.",
            false,
        );
        project(ui);
        return;
    }
    let unchanged = store().is_some_and(|store| store.read(&file).is_ok_and(|now| now == profile));
    if !unchanged {
        ui.set_style_profile_busy(false);
        set_message(
            "The profile changed while the file dialog was open. Nothing was written; export again.",
            true,
        );
        project(ui);
        return;
    }
    // Admitted. From here the write is under way and will be reported.
    with_state(|state| state.writing = Some(opened.clone()));
    let name = profile.name.clone();
    let weak = ui.as_weak();
    let failure_ui = ui.as_weak();
    let write_scope = opened.clone();
    let failure_scope = opened.clone();
    let worker_owner = permit.clone();
    let spawned = worker_owner.spawn(
        "morpheus-style-profile-write",
        move || {
            let result =
                style_profiles::export_profile(&profile, &path).map_err(|error| error.to_string());
            drop(permit);
            let _ =
                weak.upgrade_in_event_loop(move |ui| finish_write(&ui, write_scope, name, result));
        },
        move || {
            let _ = failure_ui.upgrade_in_event_loop(move |ui| {
                failed_write_start(&ui, &failure_scope);
            });
        },
    );
    if spawned.is_err() {
        failed_write_start(ui, &opened);
    }
}
fn failed_write_start(ui: &AppWindow, _opened: &Scope) {
    with_state(|state| state.writing = None);
    ui.set_style_profile_busy(false);
    set_message(
        "The profile export could not start. Nothing was written.",
        true,
    );
    project(ui);
}

/// The admitted export of `admitted` ended. It is reported once, in place
/// while its scope is current. Otherwise it is reported without naming the
/// profile, stating only what happened to the file.
fn finish_write(ui: &AppWindow, admitted: Scope, name: String, result: Result<(), String>) {
    let ours = with_state(|state| {
        state
            .writing
            .take_if(|writing| *writing == admitted)
            .is_some()
    });
    if !ours {
        return;
    }
    ui.set_style_profile_busy(false);
    let failed = result.is_err();
    let text = match (result, scope::current(ui, &admitted)) {
        (Ok(()), true) => format!("Exported {}.", shown_name(&name)),
        // Every export error is a refusal or a failed no-clobber write that
        // removed its staged file: nothing is at the chosen path.
        (Err(problem), true) => {
            format!("Could not export the profile: {problem}. Nothing was written.")
        }
        (Ok(()), false) => "A profile export started before the account or settings changed has finished; the file was written.".to_owned(),
        (Err(_), false) => "A profile export started before the account or settings changed could not be written. Nothing was written.".to_owned(),
    };
    set_message(text, failed);
    project(ui);
}

/// Deliver an import dialog's answer for `opened`, as the dialog thread does.
#[cfg(test)]
pub(crate) fn finish_import_for_test(ui: &AppWindow, opened: Scope, profile: StyleProfile) {
    finish_import(ui, opened, Some(Ok(profile)));
}

/// Deliver an export dialog's answer for `opened` and the selected profile,
/// as the dialog thread does, holding the permit and busy state as export()
/// does. Returns false when no profile is selected.
#[cfg(test)]
pub(crate) fn finish_export_for_test(
    ui: &AppWindow,
    opened: Scope,
    chosen: std::path::PathBuf,
) -> bool {
    let Some(file) = with_state(|state| state.selected.clone()) else {
        return false;
    };
    let Some(profile) = store().and_then(|store| store.read(&file).ok()) else {
        return false;
    };
    let permit = DialogPermit::acquire().expect("no other file dialog in the test");
    ui.set_style_profile_busy(true);
    finish_export(ui, opened, file, profile, Some(chosen), permit);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HeldTls {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for HeldTls {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            // Bounded even if the test's observation fails; no forgotten
            // assertion can strand a real worker indefinitely.
            let _ = self.release.recv_timeout(std::time::Duration::from_secs(5));
        }
    }
    thread_local! { static HELD_TLS: RefCell<Option<HeldTls>> = const { RefCell::new(None) }; }
    fn drain_test_workers(mut owners: DialogWorkers) -> AppearanceObservation {
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let observation = owners.observe();
            if matches!(observation, AppearanceObservation::Released(_)) {
                return observation;
            }
            if std::time::Instant::now() >= limit {
                // Keep the actual supervisors/operation leases on uncertainty.
                // No Drop of the sole JoinHandle is treated as cleanup.
                std::mem::forget(owners);
                panic!("Appearance test could not confirm actual worker cleanup");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn dialog_body_result_does_not_release_permit_before_actual_tls_join() {
        let busy = Arc::new(AtomicBool::new(false));
        let mut owners = DialogWorkers::default();
        let permit = DialogPermit::acquire_in(&mut owners, busy.clone()).expect("free local lease");
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let spawned = owners.spawn(
            &permit,
            "morpheus-dialog-tls-regression",
            move || {
                HELD_TLS.with(|slot| {
                    *slot.borrow_mut() = Some(HeldTls {
                        entered,
                        release: released,
                    })
                });
            },
            || {},
        );
        drop(permit);
        let entered = entering.recv_timeout(std::time::Duration::from_secs(3));
        let pending = owners.observe();
        let refused = DialogPermit::acquire_in(&mut owners, busy.clone()).is_none();
        owners.closed = true;
        let _ = release.send(());
        let observation = drain_test_workers(owners);
        // Assertions happen only after the actual owned workers/TLS joined.
        assert!(spawned.is_ok());
        assert!(entered.is_ok());
        assert_eq!(pending, AppearanceObservation::Pending);
        assert!(refused);
        assert_eq!(observation, AppearanceObservation::Released(Ok(())));
        assert!(!busy.load(Ordering::Acquire));
    }

    #[test]
    fn transferred_dialog_permit_retains_both_dialog_tail_and_actual_export() {
        let busy = Arc::new(AtomicBool::new(false));
        let mut owners = DialogWorkers::default();
        let permit = DialogPermit::acquire_in(&mut owners, busy.clone()).expect("free local lease");
        let transfer = permit.clone();
        let (returned, callback) = std::sync::mpsc::channel();
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let first = owners.spawn(
            &permit,
            "morpheus-dialog-transfer-regression",
            move || {
                HELD_TLS.with(|slot| {
                    *slot.borrow_mut() = Some(HeldTls {
                        entered,
                        release: released,
                    })
                });
                let _ = returned.send(transfer);
            },
            || {},
        );
        drop(permit);
        let transferred = callback.recv_timeout(std::time::Duration::from_secs(3));
        let tls_entered = entering.recv_timeout(std::time::Duration::from_secs(3));
        let (wrote, written) = std::sync::mpsc::channel();
        let second = match transferred {
            Ok(permit) => {
                let body_permit = permit.clone();
                owners.spawn(
                    &permit,
                    "morpheus-export-transfer-regression",
                    move || {
                        drop(body_permit);
                        let _ = wrote.send(());
                    },
                    || {},
                )
            }
            Err(_) => Err(std::io::Error::other("No original dialog handoff")),
        };
        let body_ended = written.recv_timeout(std::time::Duration::from_secs(3));
        let still_busy = busy.load(Ordering::Acquire);
        let refused = DialogPermit::acquire_in(&mut owners, busy.clone()).is_none();
        owners.closed = true;
        let _ = release.send(());
        let observation = drain_test_workers(owners);
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert!(tls_entered.is_ok());
        assert!(body_ended.is_ok());
        assert!(still_busy);
        assert!(refused);
        assert_eq!(observation, AppearanceObservation::Released(Ok(())));
        assert!(!busy.load(Ordering::Acquire));
    }

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
        // A refused acquire leaves the held permit in force.
        assert!(DialogPermit::acquire().is_none());
        drop(first);
        assert!(DialogPermit::acquire().is_some());
    }
}
