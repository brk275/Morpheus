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
    AccountSelection, OwnProfile, ProfileError, RoomSettingsCancellation, WriteReport,
    validate_display_name,
};
use slint::{ComponentHandle, Model};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

use crate::{
    AppWindow,
    image_previews::avatar::{
        AvatarConsumer, AvatarPrepareError, PreparedAvatar, prepare_avatar_for, preparation_owner,
    },
    live::LiveUi,
};

const READING: &str = "Reading your public profile from the homeserver…";
const SAVING: &str = "Saving to your public Matrix profile…";
const PREPARING: &str = "Preparing the image…";
const NOTHING_TO_SAVE: &str = "Nothing differs from the saved profile.";
const PICK_CANCELLED: &str = "This editor is no longer active.";
const CHOSEN: &str = "New avatar preview. It is not public until you Save.";
const REMOVE_STAGED: &str = "The avatar will be removed when you Save.";
const PICKER_IN_USE: &str =
    "The image picker is in use by Room settings. Finish or cancel it there, then choose again.";
const DECODER_IN_USE: &str =
    "The image decoder is busy with Room settings. Try again when it finishes.";

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
    CONTEXT.with(|context| *context.borrow_mut() = Some((live.clone(), runtime.clone())));
    let weak = ui.as_weak();
    ui.on_account_profile_label_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_active_label(&ui, true);
        }
    });
    let weak = ui.as_weak();
    let clear_live = live.clone();
    let clear_runtime = runtime.clone();
    ui.on_account_profile_clear_name(move || {
        if let Some(ui) = weak.upgrade() {
            clear_name(&ui, &clear_live, &clear_runtime);
        }
    });
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
    ui.set_account_profile_confirm_clear(false);
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
    prepare_avatar_for(
        AvatarConsumer::AccountProfile,
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
    // Say who holds the shared chooser instead of waiting silently; Close
    // and every other control stay available.
    if crate::room_settings::chooser_owner() == Some(AvatarConsumer::RoomSettings) {
        ui.set_account_profile_error(PICKER_IN_USE.into());
        return;
    }
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
        let chosen = crate::room_settings::choose_avatar_file(
            AvatarConsumer::AccountProfile,
            "Choose your public avatar",
            active,
        )
        .await;
        let prepared = match chosen {
            Ok(Some(bytes)) => prepare_avatar_for(
                AvatarConsumer::AccountProfile,
                bytes,
                scope.selection.clone(),
                scope.cancellation.clone(),
            )
            .await
            .map(Some)
            .map_err(prepare_error),
            Ok(None) => Ok(None),
            Err(message) => Err(chooser_error(message)),
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
        AvatarPrepareError::Busy
            if preparation_owner() == Some(AvatarConsumer::RoomSettings) =>
        {
            DECODER_IN_USE
        }
        other => other.text(),
    }
}

/// A chooser refusal, naming Room settings when it holds the chooser.
fn chooser_error(message: &'static str) -> &'static str {
    if crate::room_settings::chooser_owner() == Some(AvatarConsumer::RoomSettings) {
        PICKER_IN_USE
    } else {
        message
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

/// How one field's change ended, as far as the server shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldOutcome {
    /// The read-back shows exactly the change.
    Saved,
    /// Refused before anything was sent; the server is unchanged by it.
    NotSent(ProfileError),
    /// Sent (or possibly sent) but the read-back does not show it. It may
    /// or may not have reached the server; it is never resent automatically.
    Unknown,
}

impl FieldOutcome {
    fn from_report(result: &Result<WriteReport, ProfileError>) -> Self {
        match result {
            Ok(report) if report.confirmed() => Self::Saved,
            Ok(_) => Self::Unknown,
            Err(error) => Self::NotSent(*error),
        }
    }

    fn error(self) -> Option<ProfileError> {
        match self {
            Self::NotSent(error) => Some(error),
            Self::Saved | Self::Unknown => None,
        }
    }
}

/// What one Save (or Remove display name) did, field by field. `None` for a
/// field means nothing was attempted; `profile` is the last profile the
/// server returned.
struct Outcome {
    name: Option<FieldOutcome>,
    avatar: Option<FieldOutcome>,
    profile: Option<OwnProfile>,
    /// The first send error a field reported, for the error line.
    send_error: Option<ProfileError>,
    /// The chosen image, returned so a failed upload can be retried.
    image: Option<PreparedAvatar>,
}

impl Outcome {
    fn new() -> Self {
        Self {
            name: None,
            avatar: None,
            profile: None,
            send_error: None,
            image: None,
        }
    }

    /// Record one field's report; keep the newest profile the server showed.
    fn record(&mut self, result: &Result<WriteReport, ProfileError>) -> FieldOutcome {
        if let Ok(report) = result {
            if report.profile.is_some() {
                self.profile = report.profile.clone();
            }
            if self.send_error.is_none() {
                self.send_error = report.send_error;
            }
        }
        FieldOutcome::from_report(result)
    }
}

enum AvatarSave {
    Keep,
    Set(PreparedAvatar),
    Remove,
}

enum NameSave {
    Keep,
    Set(String),
    Clear,
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
    let name = name.map_or(NameSave::Keep, NameSave::Set);
    start_save(ui, live, runtime, name, true);
}

/// Remove the display name after the user confirmed it for this account.
/// The confirmation names the identity; the request is its own change.
fn clear_name(ui: &AppWindow, live: &Arc<LiveUi>, runtime: &Handle) {
    ui.set_account_profile_confirm_clear(false);
    let has_name = with(|editor| {
        editor
            .server
            .as_ref()
            .is_some_and(|server| server.display_name.is_some())
    });
    if !has_name {
        return;
    }
    start_save(ui, live, runtime, NameSave::Clear, false);
}

/// Run one save: the name change (if any) and, when `with_avatar`, the
/// drafted avatar change, each as its own single-send request.
fn start_save(
    ui: &AppWindow,
    live: &Arc<LiveUi>,
    runtime: &Handle,
    name: NameSave,
    with_avatar: bool,
) {
    let Some(scope) = begin(ui, live) else {
        return;
    };
    // The chosen image moves into the save and comes back with the outcome.
    let avatar = if with_avatar {
        with(|editor| {
            match std::mem::replace(&mut editor.avatar, AvatarDraft::Keep) {
                AvatarDraft::Keep => AvatarSave::Keep,
                AvatarDraft::Chosen(image) => AvatarSave::Set(image),
                AvatarDraft::Remove => AvatarSave::Remove,
            }
        })
    } else {
        AvatarSave::Keep
    };
    let removing = matches!(avatar, AvatarSave::Remove);
    let clearing = matches!(name, NameSave::Clear);
    ui.set_account_profile_status(SAVING.into());
    let weak = ui.as_weak();
    let live = live.clone();
    runtime.spawn(async move {
        let outcome = save_fields(&live, &scope, name, avatar).await;
        let preview = match (&outcome.avatar, &outcome.profile) {
            (Some(FieldOutcome::Saved), Some(profile)) => {
                avatar_preview(&live, &scope, profile).await
            }
            _ => None,
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if !scope.current(&ui) {
                return;
            }
            end(&ui);
            finish_save(&ui, outcome, preview, removing, clearing);
        });
    });
}

/// Send each changed field as its own single-send request. A later field is
/// skipped once the editor or account is gone; an earlier field's result is
/// kept and reported.
async fn save_fields(
    live: &LiveUi,
    scope: &Scope,
    name: NameSave,
    avatar: AvatarSave,
) -> Outcome {
    let manager = &live.manager;
    let mut outcome = Outcome::new();
    let name_result = match name {
        NameSave::Keep => None,
        NameSave::Set(name) => Some(
            manager
                .set_own_display_name_for(&scope.selection, &scope.cancellation, &name)
                .await,
        ),
        NameSave::Clear => Some(
            manager
                .clear_own_display_name_for(&scope.selection, &scope.cancellation)
                .await,
        ),
    };
    if let Some(result) = &name_result {
        outcome.name = Some(outcome.record(result));
    }
    let gone = !scope.selection.is_current() || scope.cancellation.is_cancelled();
    match avatar {
        AvatarSave::Keep => {}
        AvatarSave::Set(image) if gone => outcome.image = Some(image),
        AvatarSave::Set(image) => {
            let result = match image.upload_copy() {
                Ok(png) => {
                    manager
                        .set_own_avatar_for(&scope.selection, &scope.cancellation, png)
                        .await
                }
                Err(_) => Err(ProfileError::InvalidAvatar),
            };
            let field = outcome.record(&result);
            if field != FieldOutcome::Saved {
                outcome.image = Some(image);
            }
            outcome.avatar = Some(field);
        }
        AvatarSave::Remove if gone => {}
        AvatarSave::Remove => {
            let result = manager
                .remove_own_avatar_for(&scope.selection, &scope.cancellation)
                .await;
            outcome.avatar = Some(outcome.record(&result));
        }
    }
    outcome
}

fn finish_save(
    ui: &AppWindow,
    outcome: Outcome,
    preview: Option<PreparedAvatar>,
    removing: bool,
    clearing: bool,
) {
    let Outcome {
        name,
        avatar,
        profile,
        send_error,
        image,
    } = outcome;
    if let Some(profile) = &profile {
        // What the server shows now replaces the confirmed copy. The name
        // field keeps what the user typed unless the name was cleared.
        apply_server(ui, profile, clearing && name == Some(FieldOutcome::Saved));
        with(|editor| editor.server = Some(profile.clone()));
    }
    // A failed, unknown or unsent avatar change stays drafted for a retry.
    if avatar == Some(FieldOutcome::Saved) {
        ui.set_account_profile_avatar_state(0);
        show_image(ui, preview.as_ref());
    } else if let Some(image) = image {
        with(|editor| editor.avatar = AvatarDraft::Chosen(image));
    } else if removing {
        with(|editor| editor.avatar = AvatarDraft::Remove);
    }
    show_dirty(ui);
    ui.set_account_profile_status(summary(name, avatar, profile.as_ref()).into());
    let failure = name
        .and_then(FieldOutcome::error)
        .or_else(|| avatar.and_then(FieldOutcome::error))
        .or(send_error);
    ui.set_account_profile_error(failure.map(|error| error.to_string()).unwrap_or_default().into());
}

fn field_words(label: &str, outcome: FieldOutcome) -> String {
    match outcome {
        FieldOutcome::Saved => format!("{label} saved."),
        FieldOutcome::NotSent(_) => format!("{label} not sent."),
        FieldOutcome::Unknown => {
            format!("{label} not confirmed; the server may or may not have it.")
        }
    }
}

/// The exact per-field outcome and what the server shows now, in plain
/// words. Never "saved" for a field the read-back did not confirm.
fn summary(
    name: Option<FieldOutcome>,
    avatar: Option<FieldOutcome>,
    shown: Option<&OwnProfile>,
) -> String {
    let mut parts = Vec::new();
    if let Some(name) = name {
        parts.push(field_words("Display name", name));
    }
    if let Some(avatar) = avatar {
        parts.push(field_words("Avatar", avatar));
    }
    if parts.is_empty() {
        return NOTHING_TO_SAVE.to_owned();
    }
    let all_saved = [name, avatar]
        .into_iter()
        .flatten()
        .all(|field| field == FieldOutcome::Saved);
    match shown {
        Some(profile) => parts.push(format!(
            "The server now shows {} and {}.",
            profile
                .display_name
                .as_deref()
                .map_or_else(|| "no display name".to_owned(), |name| format!("the name \u{201c}{name}\u{201d}")),
            if profile.has_avatar() { "an avatar" } else { "no avatar" }
        )),
        None => parts.push(
            "The profile could not be read back; Refresh to see what the server holds.".to_owned(),
        ),
    }
    if !all_saved {
        parts.push("Save again to retry only what still differs.".to_owned());
    }
    parts.join(" ")
}

/// The active account's switcher label: one bounded read of its own public
/// profile per account selection (after an activation or restore makes it
/// the visible account), and one more each time Manage accounts opens. Never
/// for an inactive account and never in the background otherwise.
#[derive(Default)]
struct LabelRefresh {
    selection: Option<AccountSelection>,
    cancellation: Option<RoomSettingsCancellation>,
    in_flight: bool,
    generation: u64,
}

thread_local! {
    static CONTEXT: RefCell<Option<(Arc<LiveUi>, Handle)>> = const { RefCell::new(None) };
    static LABEL: RefCell<LabelRefresh> = RefCell::new(LabelRefresh::default());
}

/// Whether a label read should start for `account` now: not while one is in
/// flight, and once per selection unless `force` (Manage accounts opened).
fn label_read_needed(
    refresh: &LabelRefresh,
    account: &str,
    force: bool,
) -> bool {
    if refresh.in_flight {
        return false;
    }
    force
        || !refresh
            .selection
            .as_ref()
            .is_some_and(|done| done.is_current() && done.account_id() == account)
}

/// Read the visible account's profile once for its switcher label. Called
/// after an accepted account snapshot (`force` false) and when Manage
/// accounts opens (`force` true).
pub(crate) fn refresh_active_label(ui: &AppWindow, force: bool) {
    let Some((live, runtime)) = CONTEXT.with(|context| context.borrow().clone()) else {
        return;
    };
    if !ui.get_live_session() || !live.is_live.load(Ordering::Acquire) {
        return;
    }
    let account = ui.get_active_account_local_id().to_string();
    if account.is_empty() {
        return;
    }
    let Some(selection) = live.manager.account_selection(&account) else {
        return;
    };
    let started = LABEL.with(|label| {
        let mut label = label.borrow_mut();
        if !label_read_needed(&label, &account, force) {
            return None;
        }
        if let Some(previous) = label.cancellation.take() {
            previous.cancel();
        }
        let cancellation = RoomSettingsCancellation::new();
        label.selection = Some(selection.clone());
        label.cancellation = Some(cancellation.clone());
        label.in_flight = true;
        label.generation = label.generation.wrapping_add(1);
        Some((label.generation, cancellation))
    });
    let Some((generation, cancellation)) = started else {
        return;
    };
    let weak = ui.as_weak();
    runtime.spawn(async move {
        let profile = live.manager.own_profile_for(&selection, &cancellation).await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let current = LABEL.with(|label| {
                let mut label = label.borrow_mut();
                if label.generation != generation {
                    return false;
                }
                label.in_flight = false;
                true
            });
            // Only the same visible account under the same selection.
            if !current
                || !selection.is_current()
                || ui.get_active_account_local_id().as_str() != selection.account_id()
            {
                return;
            }
            if let Ok(profile) = profile
                && profile.account_id == selection.account_id()
            {
                record_confirmed(&profile);
                project_switcher(&ui, &profile);
            }
        });
    });
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
                AvatarConsumer::AccountProfile,
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

    fn profile(name: Option<&str>) -> OwnProfile {
        OwnProfile::synthetic(
            "synthetic-a",
            "@synthetic-a:example.org",
            "https://example.org",
            name,
        )
    }

    #[test]
    fn partial_outcomes_are_reported_per_field_with_what_the_server_shows() {
        use FieldOutcome::*;
        let shown = profile(Some("Synthetic"));
        let both = summary(Some(Saved), Some(Saved), Some(&shown));
        assert!(both.starts_with("Display name saved. Avatar saved."));
        assert!(both.contains("the name \u{201c}Synthetic\u{201d}"));
        assert!(!both.contains("retry"));
        let partial = summary(Some(Saved), Some(Unknown), Some(&shown));
        assert!(partial.starts_with("Display name saved. Avatar not confirmed"));
        assert!(partial.contains("may or may not"));
        assert!(partial.ends_with("Save again to retry only what still differs."));
        let refused = summary(Some(NotSent(ProfileError::AccountChanged)), None, None);
        assert!(refused.starts_with("Display name not sent."));
        assert!(refused.contains("could not be read back"));
        let cleared = summary(Some(Saved), None, Some(&profile(None)));
        assert!(cleared.contains("no display name"));
        assert_eq!(summary(None, None, None), NOTHING_TO_SAVE);
    }

    #[test]
    fn a_send_is_saved_only_when_the_read_back_shows_it() {
        let shown = Some(profile(Some("Synthetic")));
        let confirmed: Result<WriteReport, ProfileError> = Ok(WriteReport {
            profile: shown.clone(),
            applied: true,
            send_error: Some(ProfileError::TimedOut),
        });
        assert_eq!(FieldOutcome::from_report(&confirmed), FieldOutcome::Saved);
        let unknown: Result<WriteReport, ProfileError> = Ok(WriteReport {
            profile: shown,
            applied: false,
            send_error: Some(ProfileError::TimedOut),
        });
        assert_eq!(FieldOutcome::from_report(&unknown), FieldOutcome::Unknown);
        let refused: Result<WriteReport, ProfileError> = Err(ProfileError::Cancelled);
        assert_eq!(
            FieldOutcome::from_report(&refused),
            FieldOutcome::NotSent(ProfileError::Cancelled)
        );
        // The outcome keeps the server's latest profile and first send error.
        let mut outcome = Outcome::new();
        outcome.record(&unknown);
        assert_eq!(outcome.send_error, Some(ProfileError::TimedOut));
        assert!(outcome.profile.is_some());
    }

    #[test]
    fn label_reads_run_once_per_selection_and_never_concurrently() {
        let idle = LabelRefresh::default();
        // Nothing read yet: read the visible account once.
        assert!(label_read_needed(&idle, "synthetic-a", false));
        let busy = LabelRefresh {
            in_flight: true,
            ..LabelRefresh::default()
        };
        assert!(!label_read_needed(&busy, "synthetic-a", true));
        // Opening Manage accounts forces one more read.
        assert!(label_read_needed(&idle, "synthetic-a", true));
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
