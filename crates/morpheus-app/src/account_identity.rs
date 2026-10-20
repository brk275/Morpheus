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
        AvatarConsumer, AvatarPrepareError, PreparedAvatar, preparation_owner, prepare_avatar_for,
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

/// Serializes tests that touch the process-wide name and picture caches
/// (`retain_confirmed` prunes both), so one test never prunes another's.
#[cfg(test)]
pub(crate) static SHARED_CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn shared_cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
    SHARED_CACHE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    /// A display-name send ended Unknown and the profile could not be read
    /// back: the server's name may differ from the copy above. Avatar
    /// outcomes never set this; they keep their own draft for a retry.
    name_uncertain: bool,
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
            name_uncertain: false,
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
    // A removed or logged-out account's picture goes with it.
    if let Some(avatars) = AVATARS.lock().expect("account avatars poisoned").as_mut() {
        avatars.retain(|id, _| account_ids.contains(&id.as_str()));
    }
}

/// Longest side of an account picture held for the switcher, in pixels.
pub(crate) const MAX_ACCOUNT_AVATAR_SIDE: u32 = 64;

/// One account's own public avatar, decoded and reduced to at most 64 px,
/// with the key of the profile avatar it came from. Memory only.
struct Thumb {
    key: String,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Account pictures by exact local account ID. Filled only from the active
/// account's own profile, never for an inactive account; kept for the other
/// signed-in accounts until they are removed, logged out or the app locks.
static AVATARS: Mutex<Option<HashMap<String, Thumb>>> = Mutex::new(None);

/// The picture held for this exact local account, as a Slint image.
pub(crate) fn avatar_image(account_id: &str) -> Option<slint::Image> {
    let avatars = AVATARS.lock().expect("account avatars poisoned");
    let thumb = avatars.as_ref()?.get(account_id)?;
    let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &thumb.rgba,
        thumb.width,
        thumb.height,
    );
    Some(slint::Image::from_rgba8(pixels))
}

fn held_avatar_key(account_id: &str) -> Option<String> {
    AVATARS
        .lock()
        .expect("account avatars poisoned")
        .as_ref()?
        .get(account_id)
        .map(|thumb| thumb.key.clone())
}

/// A Manage accounts row with its fallback initials and color, and the
/// picture held for exactly its local account, if any.
pub(crate) fn with_saved_avatar(mut row: crate::SavedAccountItem) -> crate::SavedAccountItem {
    let name = confirmed_name(row.id.as_str())
        .unwrap_or_else(|| crate::live::display_user(row.handle.as_str()));
    row.initials = crate::live::initials(&name).into();
    row.accent = crate::live::accent(row.id.as_str());
    match avatar_image(row.id.as_str()) {
        Some(image) => {
            row.avatar = image;
            row.has_avatar = true;
        }
        None => {
            row.avatar = slint::Image::default();
            row.has_avatar = false;
        }
    }
    row
}

/// Reduce RGBA pixels to at most `max` pixels per side by averaging boxes of
/// source pixels, alpha-weighted so transparent pixels lend no color.
/// Returns the input unchanged when it already fits.
pub(crate) fn thumbnail(rgba: &[u8], width: u32, height: u32, max: u32) -> (Vec<u8>, u32, u32) {
    if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
        return (Vec::new(), 0, 0);
    }
    if width <= max && height <= max {
        return (rgba.to_vec(), width, height);
    }
    let scale = f64::from(width.max(height)) / f64::from(max);
    let out_w = ((f64::from(width) / scale).round() as u32).clamp(1, max);
    let out_h = ((f64::from(height) / scale).round() as u32).clamp(1, max);
    let mut out = vec![0u8; out_w as usize * out_h as usize * 4];
    for y in 0..out_h {
        let y0 = (u64::from(y) * u64::from(height) / u64::from(out_h)) as u32;
        let y1 = ((u64::from(y + 1) * u64::from(height)).div_ceil(u64::from(out_h)) as u32)
            .clamp(y0 + 1, height);
        for x in 0..out_w {
            let x0 = (u64::from(x) * u64::from(width) / u64::from(out_w)) as u32;
            let x1 = ((u64::from(x + 1) * u64::from(width)).div_ceil(u64::from(out_w)) as u32)
                .clamp(x0 + 1, width);
            let (mut r, mut g, mut b, mut a, mut count) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = (sy as usize * width as usize + sx as usize) * 4;
                    let alpha = u64::from(rgba[i + 3]);
                    r += u64::from(rgba[i]) * alpha;
                    g += u64::from(rgba[i + 1]) * alpha;
                    b += u64::from(rgba[i + 2]) * alpha;
                    a += alpha;
                    count += 1;
                }
            }
            let o = (y as usize * out_w as usize + x as usize) * 4;
            if a > 0 {
                out[o] = (r / a) as u8;
                out[o + 1] = (g / a) as u8;
                out[o + 2] = (b / a) as u8;
            }
            out[o + 3] = (a / count.max(1)) as u8;
        }
    }
    (out, out_w, out_h)
}

/// Hold `prepared` (the account's own avatar for `key`) for `account_id`.
fn store_avatar(account_id: &str, key: String, prepared: &PreparedAvatar) {
    let (width, height) = prepared.dimensions();
    let (rgba, width, height) = thumbnail(prepared.rgba(), width, height, MAX_ACCOUNT_AVATAR_SIDE);
    if rgba.is_empty() {
        return;
    }
    AVATARS
        .lock()
        .expect("account avatars poisoned")
        .get_or_insert_with(HashMap::new)
        .insert(
            account_id.to_owned(),
            Thumb {
                key,
                width,
                height,
                rgba,
            },
        );
}

fn forget_avatar(account_id: &str) {
    if let Some(avatars) = AVATARS.lock().expect("account avatars poisoned").as_mut() {
        avatars.remove(account_id);
    }
}

/// Drop every account picture from memory and from view, synchronously.
/// Called when the app locks or the account detaches (logout, removal).
pub(crate) fn forget_all_avatars(ui: &AppWindow) {
    AVATARS.lock().expect("account avatars poisoned").take();
    LABEL.with(|label| {
        let mut label = label.borrow_mut();
        if let Some(cancellation) = label.cancellation.take() {
            cancellation.cancel();
        }
        label.selection = None;
        label.in_flight = false;
        label.generation = label.generation.wrapping_add(1);
    });
    ui.set_active_account_avatar(slint::Image::default());
    ui.set_active_account_has_avatar(false);
    let accounts = ui.get_accounts();
    for index in 0..accounts.row_count() {
        if let Some(mut item) = accounts.row_data(index)
            && item.has_avatar
        {
            item.avatar = slint::Image::default();
            item.has_avatar = false;
            accounts.set_row_data(index, item);
        }
    }
    let options = ui.get_account_options();
    for index in 0..options.row_count() {
        if let Some(mut option) = options.row_data(index)
            && option.has_avatar
        {
            option.avatar = slint::Image::default();
            option.has_avatar = false;
            options.set_row_data(index, option);
        }
    }
    let saved = ui.get_saved_accounts();
    for index in 0..saved.row_count() {
        if let Some(mut row) = saved.row_data(index)
            && row.has_avatar
        {
            row.avatar = slint::Image::default();
            row.has_avatar = false;
            saved.set_row_data(index, row);
        }
    }
}

/// Hold a synthetic picture for a local account and show it, for UI tests.
#[cfg(test)]
pub(crate) fn hold_test_avatar(ui: &AppWindow, account_id: &str, side: u32) {
    let rgba = vec![200u8; side as usize * side as usize * 4];
    let (rgba, width, height) = thumbnail(&rgba, side, side, MAX_ACCOUNT_AVATAR_SIDE);
    AVATARS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(
            account_id.to_owned(),
            Thumb {
                key: "mxc://example.org/synthetic".to_owned(),
                width,
                height,
                rgba,
            },
        );
    project_avatar(ui, account_id);
}

/// Show the picture held for `account_id` (or its initials) in every row
/// that names exactly that local account, and in the active profile when it
/// is the visible account.
fn project_avatar(ui: &AppWindow, account_id: &str) {
    let image = avatar_image(account_id);
    let has = image.is_some();
    let image = image.unwrap_or_default();
    if ui.get_active_account_local_id().as_str() == account_id {
        ui.set_active_account_avatar(image.clone());
        ui.set_active_account_has_avatar(has);
        let accounts = ui.get_accounts();
        if let Ok(index) = usize::try_from(ui.get_active_account_index())
            && let Some(mut item) = accounts.row_data(index)
        {
            item.avatar = image.clone();
            item.has_avatar = has;
            accounts.set_row_data(index, item);
        }
    }
    let options = ui.get_account_options();
    for index in 0..options.row_count() {
        if let Some(mut option) = options.row_data(index)
            && option.local_id.as_str() == account_id
        {
            option.avatar = image.clone();
            option.has_avatar = has;
            options.set_row_data(index, option);
        }
    }
    let saved = ui.get_saved_accounts();
    for index in 0..saved.row_count() {
        if let Some(mut row) = saved.row_data(index)
            && row.id.as_str() == account_id
        {
            row.avatar = image.clone();
            row.has_avatar = has;
            saved.set_row_data(index, row);
        }
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
    ui.set_account_profile_server_has_name(false);
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
    !crate::room_addresses::is_open(ui)
        && !crate::room_leave::is_open(ui)
        && ui.get_live_session()
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
        && !ui.get_people_open()
        && !crate::member_privacy::is_open(ui)
        && !crate::space_management::is_open(ui)
        && !crate::identity_warnings::is_open(ui)
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
        if !selection.is_current() || editor.account != ui.get_active_account_local_id().as_str() {
            return None;
        }
        editor.busy = true;
        editor.operation = editor.operation.wrapping_add(1);
        // A label read started before this editor step must not publish
        // over whatever this step reads or saves.
        retire_label_job();
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
    crate::matrix_links::clear(ui);
    crate::thread_browser::withdraw(ui);
    crate::withdraw_call_bar(ui);
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
                    // The switcher and active profile show the same picture.
                    hold_profile_avatar(&ui, &profile, preview.as_ref());
                    with(|editor| {
                        editor.server = Some(profile);
                        editor.name_uncertain = false;
                    });
                    publish_remove_flag(&ui);
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
async fn avatar_preview(
    live: &LiveUi,
    scope: &Scope,
    profile: &OwnProfile,
) -> Option<PreparedAvatar> {
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
    retire_label_job();
    published();
    let name = profile.display_name.clone().unwrap_or_default();
    ui.set_account_profile_user_id(profile.user_id.as_str().into());
    ui.set_account_profile_homeserver(profile.homeserver.as_str().into());
    ui.set_account_profile_server_name(name.as_str().into());
    ui.set_account_profile_server_has_avatar(profile.has_avatar());
    // Offered on the raw value: a name that displays as nothing can still
    // be removed.
    ui.set_account_profile_server_has_name(!profile.name_is_absent());
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
/// The name to save, decided against the server's exact raw value (never
/// the shortened, normalized display copy): `Ok(None)` when the server holds
/// exactly the validated draft (or no name and the field is empty), `Err`
/// when it cannot be saved. While the server's state is uncertain (a send
/// ended Unknown and nothing could be read back), nothing counts as
/// unchanged, so the user can always send again deliberately.
fn name_change(
    input: &str,
    server: Option<&OwnProfile>,
    uncertain: bool,
) -> Result<Option<String>, ProfileError> {
    if input.trim().is_empty() {
        // An empty field never sends anything: removing a name is its own
        // confirmed action. It is "unchanged" when the server holds no name,
        // or when the name's state is uncertain (Remove stays available).
        if uncertain || server.is_none_or(OwnProfile::name_is_absent) {
            return Ok(None);
        }
        return Err(ProfileError::InvalidName);
    }
    let draft = validate_display_name(input)?;
    if !uncertain && server.is_some_and(|server| server.name_is_exactly(&draft)) {
        return Ok(None);
    }
    Ok(Some(draft))
}

/// Whether Remove display name is offered: the server holds any raw name
/// at all (a whitespace-only or otherwise unshowable one included), or a
/// name send's outcome is uncertain.
fn can_remove_name(server: Option<&OwnProfile>, name_uncertain: bool) -> bool {
    name_uncertain || server.is_some_and(|server| !server.name_is_absent())
}

/// The name's uncertainty after a save with no read-back: set only when the
/// name field itself ended Unknown; any other outcome keeps the previous.
fn name_uncertain_after(name: Option<FieldOutcome>, previous: bool) -> bool {
    previous || name == Some(FieldOutcome::Unknown)
}

/// Whether the editor offers Remove display name now.
fn remove_offered(editor: &Editor) -> bool {
    can_remove_name(editor.server.as_ref(), editor.name_uncertain)
}

/// Publish the Remove flag from the editor's state (server copy plus name
/// uncertainty), after every read and save outcome.
fn publish_remove_flag(ui: &AppWindow) {
    let offered = with(|editor| remove_offered(editor));
    ui.set_account_profile_server_has_name(offered);
}

fn show_dirty(ui: &AppWindow) {
    let input = ui.get_account_profile_name_input();
    let dirty = with(|editor| {
        !matches!(
            name_change(
                input.as_str(),
                editor.server.as_ref(),
                editor.name_uncertain
            ),
            Ok(None)
        )
    });
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
        AvatarPrepareError::Busy if preparation_owner() == Some(AvatarConsumer::RoomSettings) => {
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
    let input = ui.get_account_profile_name_input();
    let decided = with(|editor| {
        editor
            .server
            .as_ref()
            .map(|server| name_change(input.as_str(), Some(server), editor.name_uncertain))
    });
    let Some(decided) = decided else {
        return;
    };
    let name = match decided {
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
    // A whitespace-only or otherwise unshowable raw name can be removed too:
    // only a server holding no name at all has nothing to remove.
    let has_name = with(|editor| can_remove_name(editor.server.as_ref(), editor.name_uncertain));
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
        with(
            |editor| match std::mem::replace(&mut editor.avatar, AvatarDraft::Keep) {
                AvatarDraft::Keep => AvatarSave::Keep,
                AvatarDraft::Chosen(image) => AvatarSave::Set(image),
                AvatarDraft::Remove => AvatarSave::Remove,
            },
        )
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
async fn save_fields(live: &LiveUi, scope: &Scope, name: NameSave, avatar: AvatarSave) -> Outcome {
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
        // A saved picture replaces the held one; a removed one drops it.
        let picture = (avatar == Some(FieldOutcome::Saved))
            .then_some(preview.as_ref())
            .flatten();
        hold_profile_avatar(ui, profile, picture);
        with(|editor| {
            editor.server = Some(profile.clone());
            editor.name_uncertain = false;
        });
    } else {
        // Sent, but nothing could be read back: only a name outcome makes
        // the held name uncertain; an avatar outcome never touches it.
        with(|editor| editor.name_uncertain = name_uncertain_after(name, editor.name_uncertain));
    }
    // Whatever the outcome, Remove follows the editor's own state: an
    // uncertain name keeps Remove reachable even when the last known name
    // was absent.
    publish_remove_flag(ui);
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
    ui.set_account_profile_error(
        failure
            .map(|error| error.to_string())
            .unwrap_or_default()
            .into(),
    );
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
            profile.display_name.as_deref().map_or_else(
                || "no display name".to_owned(),
                |name| format!("the name \u{201c}{name}\u{201d}")
            ),
            if profile.has_avatar() {
                "an avatar"
            } else {
                "no avatar"
            }
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

/// Whether a label read should start for `account` now. A read already in
/// flight for this same current selection is left to finish; one for a
/// revoked selection or another account (A -> B -> A) is retired by the
/// caller and never starves the new selection. Otherwise once per selection,
/// unless `force` (Manage accounts opened).
fn label_read_needed(refresh: &LabelRefresh, account: &str, force: bool) -> bool {
    let same_current = refresh
        .selection
        .as_ref()
        .is_some_and(|done| done.is_current() && done.account_id() == account);
    label_decision(refresh.in_flight, same_current, force)
}

fn label_decision(in_flight: bool, same_current: bool, force: bool) -> bool {
    if in_flight {
        return !same_current;
    }
    force || !same_current
}

thread_local! {
    // Bumped on every publication of a confirmed profile to the account
    // views (editor or label read). A label read applies only if nothing was
    // published since it started, so an older read can never revert a newer
    // save.
    static PUBLICATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn publication() -> u64 {
    PUBLICATION.with(std::cell::Cell::get)
}

fn published() {
    PUBLICATION.with(|publication| publication.set(publication.get().wrapping_add(1)));
}

/// Retire any label read in flight: cancel it and make its result stale.
/// Called before every editor read, write and publication, so a slower
/// label read started earlier can never overwrite what the editor shows.
fn retire_label_job() {
    LABEL.with(|label| {
        let mut label = label.borrow_mut();
        if let Some(cancellation) = label.cancellation.take() {
            cancellation.cancel();
        }
        if label.in_flight {
            // Read again on the next snapshot or Manage accounts open.
            label.selection = None;
        }
        label.in_flight = false;
        label.generation = label.generation.wrapping_add(1);
    });
}

/// Whether a finished label read may publish: still the newest label job,
/// nothing published since it started, and the same visible account under
/// a still-current selection.
fn label_result_applies(
    job: u64,
    current_job: u64,
    started_publication: u64,
    current_publication: u64,
    selection_current: bool,
    same_account: bool,
) -> bool {
    job == current_job
        && started_publication == current_publication
        && selection_current
        && same_account
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
    let started_publication = publication();
    let held_key = held_avatar_key(&account);
    let weak = ui.as_weak();
    runtime.spawn(async move {
        let profile = live
            .manager
            .own_profile_for(&selection, &cancellation)
            .await;
        // The picture is read only for this active account, only when its
        // profile names one that is not already held, and only while the
        // same selection and read stay current.
        let picture = match &profile {
            Ok(profile) if profile.avatar_key().is_some() && profile.avatar_key() != held_key => {
                match live
                    .manager
                    .own_avatar_bytes_for(&selection, &cancellation, profile)
                    .await
                {
                    Ok(Some(bytes)) => prepare_avatar_for(
                        AvatarConsumer::AccountProfile,
                        Zeroizing::new(bytes),
                        selection.clone(),
                        cancellation.clone(),
                    )
                    .await
                    .ok(),
                    _ => None,
                }
            }
            _ => None,
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let current_job = LABEL.with(|label| {
                let mut label = label.borrow_mut();
                if label.generation == generation {
                    label.in_flight = false;
                }
                label.generation
            });
            if !label_result_applies(
                generation,
                current_job,
                started_publication,
                publication(),
                selection.is_current(),
                ui.get_active_account_local_id().as_str() == selection.account_id(),
            ) {
                return;
            }
            if let Ok(profile) = profile
                && profile.account_id == selection.account_id()
            {
                published();
                record_confirmed(&profile);
                project_switcher(&ui, &profile);
                hold_profile_avatar(&ui, &profile, picture.as_ref());
            }
        });
    });
}

/// Keep the held picture in step with a server-confirmed profile of the
/// visible account: a new picture replaces it, a profile without an avatar
/// drops it, and an unchanged key keeps it. A picture that failed to load
/// leaves the initials (never another account's picture).
fn hold_profile_avatar(ui: &AppWindow, profile: &OwnProfile, picture: Option<&PreparedAvatar>) {
    if ui.get_active_account_local_id().as_str() != profile.account_id {
        return;
    }
    match (profile.avatar_key(), picture) {
        (None, _) => forget_avatar(&profile.account_id),
        (Some(key), Some(picture)) => store_avatar(&profile.account_id, key, picture),
        (Some(key), None) => {
            if held_avatar_key(&profile.account_id).as_deref() != Some(key.as_str()) {
                forget_avatar(&profile.account_id);
            }
        }
    }
    project_avatar(ui, &profile.account_id);
}

#[cfg(test)]
mod tests {
    use morpheus_matrix::MAX_DISPLAY_NAME_CHARS;

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

    fn raw(name: Option<&str>) -> OwnProfile {
        // `synthetic` treats the name as the server's raw value.
        OwnProfile::synthetic(
            "synthetic-a",
            "@synthetic-a:example.org",
            "https://example.org",
            name,
        )
    }

    #[test]
    fn unchanged_names_are_decided_on_the_exact_raw_value() {
        let same = raw(Some("Synthetic"));
        assert_eq!(name_change("Synthetic", Some(&same), false), Ok(None));
        // The validated draft (trimmed) still equals the raw value.
        assert_eq!(name_change("  Synthetic ", Some(&same), false), Ok(None));
        assert_eq!(name_change("", Some(&raw(None)), false), Ok(None));
        assert_eq!(
            name_change("New Synthetic", Some(&same), false),
            Ok(Some("New Synthetic".to_owned()))
        );
        // A raw value that only displays like the draft is not unchanged.
        let spaced = raw(Some("Synthetic  Name"));
        assert_eq!(spaced.display_name.as_deref(), Some("Synthetic Name"));
        assert_eq!(
            name_change("Synthetic Name", Some(&spaced), false),
            Ok(Some("Synthetic Name".to_owned()))
        );
        let long = raw(Some(&format!("{}tail", "n".repeat(MAX_DISPLAY_NAME_CHARS))));
        let prefix = "n".repeat(MAX_DISPLAY_NAME_CHARS);
        assert_eq!(
            name_change(&prefix, Some(&long), false),
            Ok(Some(prefix.clone()))
        );
        // Emptying the field is not a removal.
        assert_eq!(
            name_change("", Some(&same), false),
            Err(ProfileError::InvalidName)
        );
        assert_eq!(
            name_change("two\nlines", Some(&same), false),
            Err(ProfileError::InvalidName)
        );
    }

    #[test]
    fn remove_is_offered_on_the_raw_name_and_avatar_unknowns_never_block_the_name() {
        // A whitespace-only raw name displays as nothing but can be removed.
        let blank = raw(Some("   "));
        assert_eq!(blank.display_name, None);
        assert!(can_remove_name(Some(&blank), false));
        assert!(!can_remove_name(Some(&raw(None)), false));
        assert!(!can_remove_name(Some(&raw(Some(""))), false));
        assert!(can_remove_name(Some(&raw(None)), true));
        // An avatar Unknown with no read-back leaves the name certain...
        assert!(!name_uncertain_after(None, false));
        assert!(!name_uncertain_after(Some(FieldOutcome::Saved), false));
        assert!(name_uncertain_after(Some(FieldOutcome::Unknown), false));
        // ...so an absent name with an empty field stays unchanged and Save
        // never fails with InvalidName for it.
        assert_eq!(name_change("", Some(&raw(None)), false), Ok(None));
        // And an uncertain name with an empty field sends nothing either.
        assert_eq!(
            name_change("", Some(&raw(Some("Synthetic"))), true),
            Ok(None)
        );
    }

    #[test]
    fn an_uncertain_name_keeps_remove_offered_after_an_absent_name() {
        // Last read: no name. A name send then ended Unknown with no
        // read-back: Remove must be offered from the editor's own state.
        let editor = Editor {
            server: Some(raw(None)),
            name_uncertain: true,
            ..Editor::default()
        };
        assert!(remove_offered(&editor));
        let certain = Editor {
            server: Some(raw(None)),
            ..Editor::default()
        };
        assert!(!remove_offered(&certain));
        let named = Editor {
            server: Some(raw(Some("   "))),
            ..Editor::default()
        };
        assert!(remove_offered(&named));
    }

    #[test]
    fn an_unknown_outcome_never_reads_as_unchanged() {
        let same = raw(Some("Synthetic"));
        // After a send ended Unknown with no read-back, an equal draft can
        // still be sent deliberately.
        assert_eq!(
            name_change("Synthetic", Some(&same), true),
            Ok(Some("Synthetic".to_owned()))
        );
        assert_eq!(name_change("", Some(&raw(None)), true), Ok(None));
    }

    #[test]
    fn a_whitespace_only_raw_name_counts_as_a_name_that_can_be_removed() {
        let blank = raw(Some("   "));
        assert_eq!(blank.display_name, None);
        assert!(!blank.name_is_absent());
        assert!(raw(None).name_is_absent());
        assert!(raw(Some("")).name_is_absent());
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
    fn account_pictures_are_reduced_to_64_pixels_without_bleeding_transparency() {
        // 256 x 128 opaque red with a fully transparent green right half.
        let (width, height) = (256u32, 128u32);
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _y in 0..height {
            for x in 0..width {
                if x < width / 2 {
                    rgba.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    rgba.extend_from_slice(&[0, 255, 0, 0]);
                }
            }
        }
        let (out, out_w, out_h) = thumbnail(&rgba, width, height, MAX_ACCOUNT_AVATAR_SIDE);
        assert_eq!((out_w, out_h), (64, 32));
        assert_eq!(out.len(), 64 * 32 * 4);
        // Left: opaque red. Right: transparent, its hidden green never shows.
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
        let right = ((64 - 1) * 4) as usize;
        assert_eq!(out[right + 3], 0);
        assert_eq!(&out[right..right + 3], &[0, 0, 0]);
        // Small pictures pass through; malformed input yields nothing.
        assert_eq!(thumbnail(&[1, 2, 3, 4], 1, 1, 64).1, 1);
        assert_eq!(thumbnail(&[1, 2, 3], 1, 1, 64).1, 0);
        assert_eq!(thumbnail(&[], 0, 0, 64).1, 0);
    }

    #[test]
    fn pictures_map_only_to_their_exact_local_account_and_leave_with_it() {
        let _shared = shared_cache_test_guard();
        let thumb = |key: &str| Thumb {
            key: key.to_owned(),
            width: 1,
            height: 1,
            rgba: vec![9, 9, 9, 255],
        };
        {
            let mut avatars = AVATARS.lock().unwrap();
            let avatars = avatars.get_or_insert_with(HashMap::new);
            avatars.insert("synthetic-pic-a".to_owned(), thumb("mxc://example.org/a"));
            avatars.insert("synthetic-pic-b".to_owned(), thumb("mxc://example.org/b"));
        }
        assert!(avatar_image("synthetic-pic-a").is_some());
        assert!(avatar_image("synthetic-pic-a2").is_none());
        assert!(avatar_image("synthetic-pic").is_none());
        assert_eq!(
            held_avatar_key("synthetic-pic-b").as_deref(),
            Some("mxc://example.org/b")
        );
        // A Manage accounts row gets exactly its own account's picture.
        let row = with_saved_avatar(crate::SavedAccountItem {
            id: "synthetic-pic-b".into(),
            handle: "@synthetic-pic-b:example.org".into(),
            ..Default::default()
        });
        assert!(row.has_avatar);
        assert!(!row.initials.is_empty());
        let other = with_saved_avatar(crate::SavedAccountItem {
            id: "synthetic-unknown".into(),
            handle: "@synthetic-unknown:example.org".into(),
            ..Default::default()
        });
        assert!(!other.has_avatar);
        // Forgetting one account's picture leaves the other's. (Pruning to
        // the saved accounts uses the same removal; it is not exercised
        // here because the cache is shared with concurrently running tests.)
        forget_avatar("synthetic-pic-a");
        assert!(avatar_image("synthetic-pic-a").is_none());
        assert!(avatar_image("synthetic-pic-b").is_some());
        forget_avatar("synthetic-pic-b");
    }

    #[test]
    fn label_reads_run_once_per_selection_and_a_stale_job_never_starves_a_new_one() {
        // Nothing read yet for this selection: read once.
        assert!(label_decision(false, false, false));
        // Already read for this current selection: not again on a snapshot,
        // but once more when Manage accounts opens.
        assert!(!label_decision(false, true, false));
        assert!(label_decision(false, true, true));
        // In flight for this same current selection: let it finish.
        assert!(!label_decision(true, true, false));
        assert!(!label_decision(true, true, true));
        // In flight for a revoked selection or another account (A -> B -> A):
        // the new selection starts its own read; the old job is retired.
        assert!(label_decision(true, false, false));
        let stale = LabelRefresh {
            in_flight: true,
            ..LabelRefresh::default()
        };
        assert!(label_read_needed(&stale, "synthetic-a", false));
    }

    #[test]
    fn a_late_label_read_never_reverts_a_newer_editor_save() {
        // Controlled completion order, as the UI thread sees it:
        // 1. a label read (P0) starts: job 1, nothing published yet;
        let (job, started) = (1, 0);
        // 2. the editor starts a save, retiring label job 1 (job 2 is now
        //    current), and publishes its confirmed profile (P1);
        let (current_job, current_publication) = (2, 1);
        // 3. the slow label read (P0) finishes last: it must not publish.
        assert!(!label_result_applies(
            job,
            current_job,
            started,
            current_publication,
            true,
            true
        ));
        // Even if the job number still matched, a publication since it
        // started blocks it.
        assert!(!label_result_applies(1, 1, 0, 1, true, true));
        // Without any newer publication or job, the same read applies...
        assert!(label_result_applies(1, 1, 0, 0, true, true));
        // ...but never under a revoked selection or for another account.
        assert!(!label_result_applies(1, 1, 0, 0, false, true));
        assert!(!label_result_applies(1, 1, 0, 0, true, false));

        // The real counters: an editor publication bumps the shared
        // generation and retires the label job.
        let before = (publication(), LABEL.with(|label| label.borrow().generation));
        LABEL.with(|label| label.borrow_mut().in_flight = true);
        retire_label_job();
        published();
        let after = (publication(), LABEL.with(|label| label.borrow().generation));
        assert_ne!(before.0, after.0);
        assert_ne!(before.1, after.1);
        assert!(!LABEL.with(|label| label.borrow().in_flight));
    }

    #[test]
    fn confirmed_names_are_memory_only_and_bounded_to_saved_accounts() {
        let _shared = shared_cache_test_guard();
        // Synthetic identities only.
        CONFIRMED_NAMES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert("synthetic-a".to_owned(), "Synthetic A".to_owned());
        assert_eq!(
            confirmed_name("synthetic-a").as_deref(),
            Some("Synthetic A")
        );
        assert_eq!(confirmed_name("synthetic-b"), None);
        // A removed account's confirmed name is forgotten.
        retain_confirmed(&["synthetic-b"]);
        assert_eq!(confirmed_name("synthetic-a"), None);
    }
}
