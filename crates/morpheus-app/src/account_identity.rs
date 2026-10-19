//! The current account's public profile editor: the display name and avatar
//! the homeserver publishes for this Matrix identity.
//!
//! Three states stay apart: the profile the server last confirmed, the
//! user's local draft (a typed name, a chosen image or a staged removal) and
//! the request in flight. Choosing an image only previews it locally; Save
//! sends each changed field as its own request and reports each outcome, so
//! a saved name with a failed avatar says exactly that, and saving again
//! retries only what is still different from the server. Nothing here claims
//! that a request already sent was cancelled or undone.
//!
//! Every read, choice and save captures the exact account selection, a
//! cancellation token and this editor's open, operation and picker
//! generations. Closing the editor, an account switch (also A -> B -> A),
//! lock, removal or detach cancels the token, bumps the generations and
//! clears drafts, previews and errors synchronously, so a late picker,
//! upload or profile response can never update another identity or repaint
//! private rows.
//!
//! Server-confirmed display names are kept in memory only, per local account
//! ID, to label the account switcher; nothing is written to configuration or
//! any other store. The editor never switches, logs out or removes an
//! account and never asks for a password.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, Mutex, atomic::Ordering},
};

use morpheus_matrix::{
    AccountSelection, OwnProfile, ProfileError, RoomSettingsCancellation, validate_display_name,
};
use slint::{ComponentHandle, Model};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

use crate::{
    AppWindow,
    image_previews::avatar::{AvatarPrepareError, PreparedAvatar, prepare_avatar},
    live::LiveUi,
};

const READING: &str = "Reading your public profile from the homeserver…";
const SAVING: &str = "Saving to your public Matrix profile…";
const PREPARING: &str = "Preparing the image…";
const SAVED: &str = "Saved to your public Matrix profile.";
const NOTHING_TO_SAVE: &str = "Nothing differs from the saved profile.";
const PICK_CANCELLED: &str = "This editor is no longer active.";
const CHOSEN: &str = "New avatar preview. It is not public until you Save.";
const REMOVE_STAGED: &str = "The avatar will be removed when you Save.";

/// Server-confirmed display names by local account ID; memory only.
static CONFIRMED_NAMES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

enum AvatarDraft {
    Keep,
    Chosen(PreparedAvatar),
    Remove,
}

struct Editor {
    epoch: u64,
    operation: u64,
    picker: u64,
    account: String,
    selection: Option<AccountSelection>,
    cancellation: Option<RoomSettingsCancellation>,
    server: Option<OwnProfile>,
    avatar: AvatarDraft,
    busy: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            epoch: 0,
            operation: 0,
            picker: 0,
            account: String::new(),
            selection: None,
            cancellation: None,
            server: None,
            avatar: AvatarDraft::Keep,
            busy: false,
        }
    }
}

thread_local! {
    // Slint callbacks run on the UI thread only.
    static EDITOR: RefCell<Editor> = RefCell::new(Editor::default());
}

fn with<R>(f: impl FnOnce(&mut Editor) -> R) -> R {
    EDITOR.with(|editor| f(&mut editor.borrow_mut()))
}

/// The display name the switcher shows for a local account: the last
/// server-confirmed one, if this session confirmed one.
pub(crate) fn confirmed_name(account_id: &str) -> Option<String> {
    CONFIRMED_NAMES
        .lock()
        .expect("confirmed names poisoned")
        .as_ref()
        .and_then(|names| names.get(account_id).cloned())
}

/// Forget confirmed names of accounts no longer saved.
pub(crate) fn retain_confirmed(account_ids: &[&str]) {
    if let Some(names) = CONFIRMED_NAMES
        .lock()
        .expect("confirmed names poisoned")
        .as_mut()
    {
        names.retain(|id, _| account_ids.contains(&id.as_str()));
    }
}

fn record_confirmed(profile: &OwnProfile) {
    let mut names = CONFIRMED_NAMES.lock().expect("confirmed names poisoned");
    let names = names.get_or_insert_with(HashMap::new);
    match &profile.display_name {
        Some(name) => {
            names.insert(profile.account_id.clone(), name.clone());
        }
        None => {
            names.remove(&profile.account_id);
        }
    }
}

/// Install the editor callbacks.
pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: Handle) {
    let weak = ui.as_weak();
    let open_live = live.clone();
    let open_runtime = runtime.clone();
    ui.on_open_account_profile(move || {
        if let Some(ui) = weak.upgrade() {
            open(&ui, &open_live, &open_runtime);
        }
    });
    let weak = ui.as_weak();
    ui.on_close_account_profile(move || {
        if let Some(ui) = weak.upgrade() {
            clear_presentation(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_account_profile_name_edited(move || {
        if let Some(ui) = weak.upgrade() {
            show_dirty(&ui);
        }
    });
    let weak = ui.as_weak();
    let refresh_live = live.clone();
    let refresh_runtime = runtime.clone();
    ui.on_account_profile_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            refresh(&ui, &refresh_live, &refresh_runtime);
        }
    });
    let weak = ui.as_weak();
    let choose_live = live.clone();
    let choose_runtime = runtime.clone();
    ui.on_account_profile_choose_avatar(move || {
        if let Some(ui) = weak.upgrade() {
            choose(&ui, &choose_live, &choose_runtime);
        }
    });
    let weak = ui.as_weak();
    ui.on_account_profile_stage_remove_avatar(move || {
        if let Some(ui) = weak.upgrade() {
            stage_remove(&ui);
        }
    });
    let weak = ui.as_weak();
    let keep_live = live.clone();
    let keep_runtime = runtime.clone();
    ui.on_account_profile_keep_avatar(move || {
        if let Some(ui) = weak.upgrade() {
            keep_avatar(&ui, &keep_live, &keep_runtime);
        }
    });
    let weak = ui.as_weak();
    ui.on_account_profile_save(move || {
        if let Some(ui) = weak.upgrade() {
            save(&ui, &live, &runtime);
        }
    });
}

/// Cancel every step and forget drafts, previews, errors and the server
/// copy synchronously. Called on close and with the account's other private
/// presentation on detach, lock, removal and replacement.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    with(|editor| {
        if let Some(cancellation) = editor.cancellation.take() {
            cancellation.cancel();
        }
        let epoch = editor.epoch.wrapping_add(1);
        let operation = editor.operation.wrapping_add(1);
        let picker = editor.picker.wrapping_add(1);
        *editor = Editor {
            epoch,
            operation,
            picker,
            ..Editor::default()
        };
    });
    ui.set_account_profile_open(false);
    ui.set_account_profile_busy(false);
    ui.set_account_profile_loaded(false);
    ui.set_account_profile_user_id("".into());
    ui.set_account_profile_homeserver("".into());
    ui.set_account_profile_server_name("".into());
    ui.set_account_profile_server_has_avatar(false);
    ui.set_account_profile_name_input("".into());
    ui.set_account_profile_name_dirty(false);
    ui.set_account_profile_avatar_state(0);
    ui.set_account_profile_image(slint::Image::default());
    ui.set_account_profile_image_visible(false);
    ui.set_account_profile_status("".into());
    ui.set_account_profile_error("".into());
}

/// No account, security, call or dialog flow is in the way.
fn unobstructed(ui: &AppWindow, live: &LiveUi) -> bool {
    ui.get_live_session()
        && live.is_live.load(Ordering::Acquire)
        && !live.security_gate.blocked()
        && !ui.get_show_sign_in()
        && !ui.get_access_open()
        && !ui.get_account_switch_pending()
        && ui.get_security_recovery_key().is_empty()
        && !ui.get_leave_call_dialog()
        && !ui.get_native_capture_picker_open()
        && !ui.get_member_confirm_open()
        && !ui.get_room_admin_open()
        && !ui.get_room_actions_open()
        && !ui.get_image_preview_open()
        && !ui.get_poll_dialog_open()
}

/// What an operation captured when it started.
#[derive(Clone)]
struct Scope {
    epoch: u64,
    operation: u64,
    account: String,
    selection: AccountSelection,
    cancellation: RoomSettingsCancellation,
}

impl Scope {
    /// Still the open editor, the same operation and the same exact account.
    fn current(&self, ui: &AppWindow) -> bool {
        !self.cancellation.is_cancelled()
            && self.selection.is_current()
            && ui.get_account_profile_open()
            && ui.get_active_account_local_id().as_str() == self.account
            && with(|editor| editor.epoch == self.epoch && editor.operation == self.operation)
    }
}

/// Start an operation: one at a time, under the editor's selection.
fn begin(ui: &AppWindow, live: &LiveUi) -> Option<Scope> {
    if !unobstructed(ui, live) {
        return None;
    }
    let scope = with(|editor| {
        if editor.busy {
            return None;
        }
        let selection = editor.selection.clone()?;
        let cancellation = editor.cancellation.clone()?;
        if !selection.is_current() || editor.account != ui.get_active_account_local_id().as_str()
        {
            return None;
        }
        editor.busy = true;
        editor.operation = editor.operation.wrapping_add(1);
        Some(Scope {
            epoch: editor.epoch,
            operation: editor.operation,
            account: editor.account.clone(),
            selection,
            cancellation,
        })
    })?;
    ui.set_account_profile_busy(true);
    ui.set_account_profile_error("".into());
    Some(scope)
}

fn end(ui: &AppWindow) {
    with(|editor| editor.busy = false);
    ui.set_account_profile_busy(false);
}

fn open(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    if !unobstructed(ui, live) {
        return;
    }
    let account = ui.get_active_account_local_id().to_string();
    if account.is_empty() {
        return;
    }
    let Some(selection) = live.manager.account_selection(&account) else {
        return;
    };
    clear_presentation(ui);
    with(|editor| {
        editor.account = account;
        editor.selection = Some(selection);
        editor.cancellation = Some(RoomSettingsCancellation::new());
    });
    ui.set_account_menu_open(false);
    // The identity being edited, from the switcher, until the server answers.
    ui.set_account_profile_user_id(ui.get_active_account_handle());
    ui.set_account_profile_open(true);
    read(ui, live, runtime, true);
}

fn refresh(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    read(ui, live, runtime, false);
}

/// Read the server profile, then preview its avatar. `reset_name` puts the
/// server name into the field; a refresh keeps a name the user is editing.
fn read(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle, reset_name: bool) {
    let Some(scope) = begin(ui, live) else {
        return;
    };
    ui.set_account_profile_status(READING.into());
    let weak = ui.as_weak();
    let live = live.clone();
    runtime.spawn(async move {
        let profile = live
            .manager
            .own_profile_for(&scope.selection, &scope.cancellation)
            .await;
        let preview = match &profile {
            Ok(profile) => avatar_preview(&live, &scope, profile).await,
            Err(_) => None,
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !scope.current(&ui) {
                return;
            }
            end(&ui);
            match profile {
                Ok(profile) => {
                    let edited = ui.get_account_profile_name_dirty();
                    apply_server(&ui, &profile, reset_name || !edited);
                    with(|editor| editor.server = Some(profile));
                    if with(|editor| matches!(editor.avatar, AvatarDraft::Keep)) {
                        show_image(&ui, preview.as_ref());
                    }
                    ui.set_account_profile_loaded(true);
                    ui.set_account_profile_status("".into());
                }
                Err(error) => {
                    ui.set_account_profile_status("".into());
                    ui.set_account_profile_error(error.to_string().into());
                }
            }
            show_dirty(&ui);
        });
    });
}

/// The server avatar prepared for a local preview, or nothing on any
/// failure (the preview is optional; the profile still loads).
async fn avatar_preview(live: &LiveUi, scope: &Scope, profile: &OwnProfile) -> Option<PreparedAvatar> {
    let bytes = live
        .manager
        .own_avatar_bytes_for(&scope.selection, &scope.cancellation, profile)
        .await
        .ok()??;
    prepare_avatar(
        Zeroizing::new(bytes),
        scope.selection.clone(),
        scope.cancellation.clone(),
    )
    .await
    .ok()
}

/// Show the server-confirmed profile and project it to the switcher.
pub(crate) fn apply_server(ui: &AppWindow, profile: &OwnProfile, reset_name: bool) {
    let name = profile.display_name.clone().unwrap_or_default();
    ui.set_account_profile_user_id(profile.user_id.as_str().into());
    ui.set_account_profile_homeserver(profile.homeserver.as_str().into());
    ui.set_account_profile_server_name(name.as_str().into());
    ui.set_account_profile_server_has_avatar(profile.has_avatar());
    if reset_name {
        ui.set_account_profile_name_input(name.as_str().into());
    }
    record_confirmed(profile);
    project_switcher(ui, profile);
}

/// Relabel the account switcher rows for this account with the confirmed
/// name (or the user ID's localpart when the server holds none).
fn project_switcher(ui: &AppWindow, profile: &OwnProfile) {
    let name = profile
        .display_name
        .clone()
        .unwrap_or_else(|| crate::live::display_user(&profile.user_id));
    let initials = crate::live::initials(&name);
    if ui.get_active_account_local_id().as_str() == profile.account_id {
        ui.set_active_account_name(name.as_str().into());
        ui.set_active_account_initials(initials.as_str().into());
        let accounts = ui.get_accounts();
        if let Ok(index) = usize::try_from(ui.get_active_account_index())
            && let Some(mut item) = accounts.row_data(index)
        {
            item.name = name.as_str().into();
            item.initials = initials.as_str().into();
            accounts.set_row_data(index, item);
        }
    }
    let options = ui.get_account_options();
    for index in 0..options.row_count() {
        if let Some(mut option) = options.row_data(index)
            && option.local_id.as_str() == profile.account_id
        {
            option.name = name.as_str().into();
            option.initials = initials.as_str().into();
            options.set_row_data(index, option);
        }
    }
}

fn show_image(ui: &AppWindow, image: Option<&PreparedAvatar>) {
    match image {
        Some(prepared) => {
            let (width, height) = prepared.dimensions();
            let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                prepared.rgba(),
                width,
                height,
            );
            ui.set_account_profile_image(slint::Image::from_rgba8(pixels));
            ui.set_account_profile_image_visible(true);
        }
        None => {
            ui.set_account_profile_image(slint::Image::default());
            ui.set_account_profile_image_visible(false);
        }
    }
}

/// The name to save, if it differs from the server's: `Ok(None)` when it is
/// unchanged, `Err` when it cannot be saved.
fn name_change(input: &str, server: Option<&str>) -> Result<Option<String>, ProfileError> {
    if input.trim() == server.unwrap_or("").trim() {
        return Ok(None);
    }
    validate_display_name(input).map(Some)
}

fn show_dirty(ui: &AppWindow) {
    let server = ui.get_account_profile_server_name();
    let dirty = ui.get_account_profile_name_input().trim() != server.trim();
    ui.set_account_profile_name_dirty(dirty);
}

fn choose(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    let Some(scope) = begin(ui, live) else {
        return;
    };
    let picker = with(|editor| {
        editor.picker = editor.picker.wrapping_add(1);
        editor.picker
    });
    ui.set_account_profile_status(PREPARING.into());
    let weak = ui.as_weak();
    runtime.spawn(async move {
        let selection = scope.selection.clone();
        let cancellation = scope.cancellation.clone();
        let active = Arc::new(move || selection.is_current() && !cancellation.is_cancelled());
        let chosen =
            crate::room_settings::choose_avatar_file("Choose your public avatar", active).await;
        let prepared = match chosen {
            Ok(Some(bytes)) => prepare_avatar(
                bytes,
                scope.selection.clone(),
                scope.cancellation.clone(),
            )
            .await
            .map(Some)
            .map_err(prepare_error),
            Ok(None) => Ok(None),
            Err(message) => Err(message),
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !scope.current(&ui) || with(|editor| editor.picker != picker) {
                return;
            }
            end(&ui);
            ui.set_account_profile_status("".into());
            match prepared {
                Ok(Some(image)) => {
                    show_image(&ui, Some(&image));
                    with(|editor| editor.avatar = AvatarDraft::Chosen(image));
                    ui.set_account_profile_avatar_state(1);
                    ui.set_account_profile_status(CHOSEN.into());
                }
                // Cancelled in the chooser: the previous choice stays.
                Ok(None) => {}
                Err(message) => ui.set_account_profile_error(message.into()),
            }
        });
    });
}

fn prepare_error(error: AvatarPrepareError) -> &'static str {
    match error {
        AvatarPrepareError::Cancelled => PICK_CANCELLED,
        other => other.text(),
    }
}

fn stage_remove(ui: &AppWindow) {
    let staged = with(|editor| {
        if editor.busy || !editor.server.as_ref().is_some_and(OwnProfile::has_avatar) {
            return false;
        }
        editor.avatar = AvatarDraft::Remove;
        true
    });
    if staged {
        show_image(ui, None);
        ui.set_account_profile_avatar_state(2);
        ui.set_account_profile_status(REMOVE_STAGED.into());
    }
}

/// Drop the local avatar choice and show the server's avatar again.
fn keep_avatar(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    let changed = with(|editor| {
        if editor.busy {
            return false;
        }
        editor.avatar = AvatarDraft::Keep;
        true
    });
    if changed {
        ui.set_account_profile_avatar_state(0);
        ui.set_account_profile_status("".into());
        show_image(ui, None);
        refresh(ui, live, runtime);
    }
}

/// What one Save did, field by field. `None` for a field means it was not
/// sent; `profile` is the last profile the server returned.
struct Outcome {
    name: Option<Result<(), ProfileError>>,
    avatar: Option<Result<(), ProfileError>>,
    profile: Option<OwnProfile>,
    /// The chosen image, returned so a failed upload can be retried.
    image: Option<PreparedAvatar>,
}

enum AvatarSave {
    Keep,
    Set(PreparedAvatar),
    Remove,
}

fn save(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    let server_name = with(|editor| editor.server.as_ref().map(|server| server.display_name.clone()));
    let Some(server_name) = server_name else {
        return;
    };
    let name = match name_change(
        ui.get_account_profile_name_input().as_str(),
        server_name.as_deref(),
    ) {
        Ok(name) => name,
        Err(error) => {
            ui.set_account_profile_error(error.to_string().into());
            return;
        }
    };
    let avatar_dirty = with(|editor| !matches!(editor.avatar, AvatarDraft::Keep));
    if name.is_none() && !avatar_dirty {
        ui.set_account_profile_status(NOTHING_TO_SAVE.into());
        return;
    }
    let Some(scope) = begin(ui, live) else {
        return;
    };
    // The chosen image moves into the save and comes back with the outcome.
    let avatar = with(|editor| {
        match std::mem::replace(&mut editor.avatar, AvatarDraft::Keep) {
            AvatarDraft::Keep => AvatarSave::Keep,
            AvatarDraft::Chosen(image) => AvatarSave::Set(image),
            AvatarDraft::Remove => AvatarSave::Remove,
        }
    });
    let removing = matches!(avatar, AvatarSave::Remove);
    ui.set_account_profile_status(SAVING.into());
    let weak = ui.as_weak();
    let live = live.clone();
    runtime.spawn(async move {
        let outcome = save_fields(&live, &scope, name, avatar).await;
        let preview = match (&outcome.avatar, &outcome.profile) {
            (Some(Ok(())), Some(profile)) => avatar_preview(&live, &scope, profile).await,
            _ => None,
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !scope.current(&ui) {
                return;
            }
            end(&ui);
            finish_save(&ui, outcome, preview, removing);
        });
    });
}

/// Send each changed field as its own request. A field is skipped once the
/// editor or account is gone; an earlier field's success is kept.
async fn save_fields(
    live: &LiveUi,
    scope: &Scope,
    name: Option<String>,
    avatar: AvatarSave,
) -> Outcome {
    let manager = &live.manager;
    let mut outcome = Outcome {
        name: None,
        avatar: None,
        profile: None,
        image: None,
    };
    if let Some(name) = name {
        match manager
            .set_own_display_name_for(&scope.selection, &scope.cancellation, &name)
            .await
        {
            Ok(profile) => {
                outcome.name = Some(Ok(()));
                outcome.profile = Some(profile);
            }
            Err(error) => outcome.name = Some(Err(error)),
        }
    }
    let gone = matches!(
        outcome.name,
        Some(Err(ProfileError::Cancelled | ProfileError::AccountChanged))
    );
    match avatar {
        AvatarSave::Keep => {}
        AvatarSave::Set(image) => {
            if gone {
                outcome.image = Some(image);
            } else {
                let result = match image.upload_copy() {
                    Ok(png) => {
                        manager
                            .set_own_avatar_for(&scope.selection, &scope.cancellation, png)
                            .await
                    }
                    Err(_) => Err(ProfileError::InvalidAvatar),
                };
                match result {
                    Ok(profile) => {
                        outcome.avatar = Some(Ok(()));
                        outcome.profile = Some(profile);
                    }
                    Err(error) => {
                        outcome.avatar = Some(Err(error));
                        outcome.image = Some(image);
                    }
                }
            }
        }
        AvatarSave::Remove if !gone => {
            match manager
                .remove_own_avatar_for(&scope.selection, &scope.cancellation)
                .await
            {
                Ok(profile) => {
                    outcome.avatar = Some(Ok(()));
                    outcome.profile = Some(profile);
                }
                Err(error) => outcome.avatar = Some(Err(error)),
            }
        }
        AvatarSave::Remove => {}
    }
    outcome
}

fn finish_save(ui: &AppWindow, outcome: Outcome, preview: Option<PreparedAvatar>, removing: bool) {
    let Outcome {
        name,
        avatar,
        profile,
        image,
    } = outcome;
    if let Some(profile) = &profile {
        // The field keeps what the user typed; it now matches the server
        // when the name saved.
        apply_server(ui, profile, false);
        with(|editor| editor.server = Some(profile.clone()));
    }
    // A failed or unsent avatar change stays drafted for a retry.
    let avatar_saved = matches!(avatar, Some(Ok(())));
    if avatar_saved {
        ui.set_account_profile_avatar_state(0);
        show_image(ui, preview.as_ref());
    } else if let Some(image) = image {
        with(|editor| editor.avatar = AvatarDraft::Chosen(image));
    } else if removing {
        with(|editor| editor.avatar = AvatarDraft::Remove);
    }
    show_dirty(ui);
    ui.set_account_profile_status(summary(&name, &avatar).into());
    let failure = [&name, &avatar]
        .into_iter()
        .find_map(|field| field.as_ref().and_then(|result| result.err()));
    ui.set_account_profile_error(failure.map(|error| error.to_string()).unwrap_or_default().into());
}

/// The exact per-field outcome in plain words. Never "saved" for a field
/// that was not confirmed.
fn summary(
    name: &Option<Result<(), ProfileError>>,
    avatar: &Option<Result<(), ProfileError>>,
) -> String {
    match (name, avatar) {
        (Some(Ok(())), Some(Ok(()))) | (Some(Ok(())), None) | (None, Some(Ok(()))) => {
            SAVED.to_owned()
        }
        (Some(Ok(())), Some(Err(_))) => {
            "Display name saved. The avatar was not confirmed; Save again to retry only the avatar."
                .to_owned()
        }
        (Some(Err(_)), Some(Ok(()))) => {
            "Avatar saved. The display name was not confirmed; Save again to retry only the name."
                .to_owned()
        }
        (Some(Err(_)), _) | (None, Some(Err(_))) => {
            "Nothing was confirmed saved. Refresh to see what the server holds.".to_owned()
        }
        (None, None) => NOTHING_TO_SAVE.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_choosers_and_preparations_read_nothing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // A closed editor or changed account never opens the dialog.
            let chosen = crate::room_settings::choose_avatar_file(
                "Choose your public avatar",
                Arc::new(|| false),
            )
            .await;
            assert!(matches!(chosen, Ok(None)));
        });
    }

    #[test]
    fn unchanged_names_are_not_sent_and_invalid_names_are_refused() {
        assert_eq!(name_change("Synthetic", Some("Synthetic")), Ok(None));
        assert_eq!(name_change("  Synthetic ", Some("Synthetic")), Ok(None));
        assert_eq!(name_change("", None), Ok(None));
        assert_eq!(
            name_change("New Synthetic", Some("Synthetic")),
            Ok(Some("New Synthetic".to_owned()))
        );
        // Clearing a name is not a supported edit; the server copy stays.
        assert_eq!(name_change("", Some("Synthetic")), Err(ProfileError::InvalidName));
        assert_eq!(
            name_change("two\nlines", Some("Synthetic")),
            Err(ProfileError::InvalidName)
        );
    }

    #[test]
    fn partial_outcomes_are_reported_per_field() {
        let failed = Some(Err(ProfileError::TimedOut));
        let saved = Some(Ok(()));
        assert_eq!(summary(&saved, &saved), SAVED);
        assert_eq!(summary(&saved, &None), SAVED);
        assert!(summary(&saved, &failed).starts_with("Display name saved."));
        assert!(summary(&saved, &failed).contains("retry only the avatar"));
        assert!(summary(&failed, &saved).starts_with("Avatar saved."));
        assert!(summary(&failed, &failed).starts_with("Nothing was confirmed saved"));
        assert!(summary(&None, &failed).starts_with("Nothing was confirmed saved"));
        assert!(summary(&failed, &None).starts_with("Nothing was confirmed saved"));
    }

    #[test]
    fn confirmed_names_are_memory_only_and_bounded_to_saved_accounts() {
        // Synthetic identities only.
        CONFIRMED_NAMES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert("synthetic-a".to_owned(), "Synthetic A".to_owned());
        assert_eq!(confirmed_name("synthetic-a").as_deref(), Some("Synthetic A"));
        assert_eq!(confirmed_name("synthetic-b"), None);
        // A removed account's confirmed name is forgotten.
        retain_confirmed(&["synthetic-b"]);
        assert_eq!(confirmed_name("synthetic-a"), None);
    }
}
