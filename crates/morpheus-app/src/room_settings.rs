//! Account- and form-scoped native room settings. Images remain in memory.
//! A cancelled request already handed to a homeserver may still apply. Failed
//! writes require a fresh authoritative read; this controller never retries them.
use crate::{
    AppWindow,
    image_previews::avatar::{PreparedAvatar, prepare_avatar},
    live::LiveUi,
};
use morpheus_matrix::{
    AccountSelection, CurrentJoinRule, EditableJoinRule, JoinRuleKind, MAX_IMAGE_PREVIEW_BYTES,
    MAX_RESTRICTED_ALLOW_ROOMS, RoomSettingsCancellation, RoomSettingsError, RoomSettingsSession,
    RoomSettingsSnapshot,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{runtime::Handle, sync::Notify};
use zeroize::Zeroizing;

static FILE_WORKER: AtomicBool = AtomicBool::new(false);
const REFRESH_FIRST: &str = "The change may have reached your homeserver. Refresh room details before making another change.";
const UNAVAILABLE: &str = "Room settings are unavailable. Close this form and reopen it.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirmation {
    Public,
    Remove,
    Refresh,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Topic,
    Avatar,
    Join,
}
impl Field {
    fn bit(self) -> u8 {
        match self {
            Self::Name => 1,
            Self::Topic => 2,
            Self::Avatar => 4,
            Self::Join => 8,
        }
    }
}
#[derive(Clone, Copy)]
enum Reset {
    None,
    All,
    Field(Field),
}
impl Reset {
    fn includes(self, field: Field) -> bool {
        matches!(self, Self::All) || matches!(self, Self::Field(current) if current == field)
    }
}
#[derive(Default)]
struct Progress {
    generation: u64,
    busy: bool,
    refresh_required: bool,
}
impl Progress {
    fn begin(&mut self, refresh: bool) -> Option<u64> {
        if self.busy || (self.refresh_required && !refresh) {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        self.busy = true;
        Some(self.generation)
    }
    fn finish(&mut self, generation: u64, write_failed: bool) -> bool {
        if !self.busy || self.generation != generation {
            return false;
        }
        self.busy = false;
        self.refresh_required |= write_failed;
        true
    }
}
struct Content {
    dirty: u8,
    progress: Progress,
    session: Option<Arc<RoomSettingsSession>>,
    snapshot: Option<RoomSettingsSnapshot>,
    image: Option<PreparedAvatar>,
    staged: bool,
    confirmation: Option<Confirmation>,
    error: &'static str,
    notice: &'static str,
}
impl Default for Content {
    fn default() -> Self {
        Self {
            dirty: 0,
            progress: Progress::default(),
            session: None,
            snapshot: None,
            image: None,
            staged: false,
            confirmation: None,
            error: "",
            notice: "",
        }
    }
}
struct Form {
    id: u64,
    selection: AccountSelection,
    room: String,
    navigation: u64,
    cancellation: RoomSettingsCancellation,
    wake: Notify,
    content: Mutex<Content>,
}
impl Form {
    fn cancel(&self) {
        self.cancellation.cancel();
        self.wake.notify_waiters();
        let mut state = self.content.lock().expect("Room settings state poisoned");
        state.image = None;
        state.staged = false;
        state.confirmation = None;
    }
    async fn cancelled(&self) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancellation.is_cancelled() {
                return;
            }
            tokio::select! { biased; _ = self.selection.revoked() => return, _ = notified => {} }
        }
    }
}

pub(crate) struct RoomSettingsUi {
    current: Mutex<Option<Arc<Form>>>,
    next: AtomicU64,
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
}
impl RoomSettingsUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            current: Mutex::new(None),
            next: AtomicU64::new(0),
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_open_room_settings(move || {
            if let Some(ui) = weak.upgrade() {
                this.open(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_cancel_room_settings(move || {
            if let Some(ui) = weak.upgrade() {
                this.close(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_room_settings_action(move |action| {
            if let Some(ui) = weak.upgrade() {
                this.action(&ui, action);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_room_settings_edit(move |field| {
            if let Some(ui) = weak.upgrade() {
                this.edited(&ui, field);
            }
        });
        owner
    }
    fn edited(&self, ui: &AppWindow, field: i32) {
        let field = match field {
            1 => Field::Name,
            2 => Field::Topic,
            4 => Field::Join,
            _ => return,
        };
        let Some(form) = self.current_for(ui) else {
            return;
        };
        let mut state = form.content.lock().expect("Room settings state poisoned");
        if state.progress.busy || state.progress.refresh_required || state.confirmation.is_some() {
            return;
        }
        state.dirty |= field.bit();
        ui.set_room_settings_dirty(true);
    }
    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.close(ui);
    }
    fn close(&self, ui: &AppWindow) {
        let previous = self
            .current
            .lock()
            .expect("Room settings owner poisoned")
            .take();
        if let Some(form) = &previous {
            form.cancel();
        }
        if previous.is_some() {
            ui.set_room_admin_busy(false);
        }
        clear_image(ui);
        ui.set_room_settings_confirmation(0);
        ui.set_room_settings_dirty(false);
        ui.set_room_settings_access_description("".into());
        ui.set_room_settings_avatar_present(false);
        ui.set_room_settings_form_id("".into());
        // These fields belong only to settings, even while the shared admin
        // modal is showing a redaction. Never retain another form's text while
        // the next room's authoritative snapshot is still loading.
        ui.set_room_admin_name_input("".into());
        ui.set_room_admin_topic_input("".into());
        ui.set_room_settings_allow_rooms("".into());
        if ui.get_room_admin_mode() == 0 {
            ui.set_room_admin_open(false);
            ui.set_room_admin_busy(false);
        }
    }
    fn active(&self, form: &Form) -> bool {
        let current = self
            .current
            .lock()
            .expect("Room settings owner poisoned")
            .as_ref()
            .map(|v| v.id);
        scope_active(
            current,
            form.id,
            form.selection.is_current(),
            form.cancellation.is_cancelled(),
            self.live.navigation_is_current(form.navigation),
            self.live.is_live.load(Ordering::Acquire),
        )
    }
    fn matches_ui(&self, form: &Form, ui: &AppWindow) -> bool {
        self.active(form)
            && ui.get_room_admin_open()
            && ui.get_room_admin_mode() == 0
            && form_matches(
                form.id,
                form.selection.account_id(),
                &form.room,
                ui.get_room_settings_form_id().as_str(),
                ui.get_active_account_local_id().as_str(),
                ui.get_active_room_id().as_str(),
            )
            && ui.get_room_admin_account_id().as_str() == form.selection.account_id()
            && ui.get_room_admin_room_id().as_str() == form.room
    }
    fn current_for(&self, ui: &AppWindow) -> Option<Arc<Form>> {
        let form = self
            .current
            .lock()
            .expect("Room settings owner poisoned")
            .clone()?;
        self.matches_ui(&form, ui).then_some(form)
    }
    fn open(self: &Arc<Self>, ui: &AppWindow) {
        self.close(ui);
        if !ui.get_live_session() || !self.live.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(selection) = self
            .live
            .manager
            .account_selection(ui.get_active_account_local_id().as_str())
        else {
            return;
        };
        let room = ui.get_active_room_id().to_string();
        if room.is_empty() {
            return;
        }
        let Ok(id) = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
        else {
            return;
        };
        let form = Arc::new(Form {
            id: id + 1,
            selection,
            room,
            navigation: self.live.current_navigation_epoch(),
            cancellation: RoomSettingsCancellation::new(),
            wake: Notify::new(),
            content: Mutex::new(Content::default()),
        });
        *self.current.lock().expect("Room settings owner poisoned") = Some(form.clone());
        ui.set_room_admin_mode(0);
        ui.set_room_admin_account_id(form.selection.account_id().into());
        ui.set_room_admin_room_id(form.room.clone().into());
        ui.set_room_settings_form_id(form.id.to_string().into());
        ui.set_room_settings_loaded(false);
        ui.set_room_settings_name_allowed(false);
        ui.set_room_settings_topic_allowed(false);
        ui.set_room_settings_avatar_allowed(false);
        ui.set_room_settings_join_allowed(false);
        ui.set_room_settings_join_options(ModelRc::new(VecModel::from(
            Vec::<slint::SharedString>::new(),
        )));
        ui.set_room_settings_join_index(-1);
        ui.set_room_settings_restricted(false);
        ui.set_room_settings_refresh_required(false);
        ui.set_room_admin_error("".into());
        ui.set_room_admin_notice("".into());
        ui.set_room_admin_open(true);
        let this = self.clone();
        let watched = form.clone();
        self.runtime.spawn(async move {
            watched.cancelled().await;
            watched.cancel();
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                // Never close a newer form opened for the same room/account.
                if ui.get_room_settings_form_id().as_str() == watched.id.to_string() {
                    this.close(&ui);
                }
            });
        });
        self.refresh(ui, form);
    }
    fn begin(&self, ui: &AppWindow, form: &Form, refresh: bool) -> Option<u64> {
        if !self.matches_ui(form, ui) {
            return None;
        }
        let mut state = form.content.lock().expect("Room settings state poisoned");
        let generation = state.progress.begin(refresh)?;
        state.confirmation = None;
        state.error = "";
        state.notice = "";
        ui.set_room_admin_busy(true);
        ui.set_room_admin_error("".into());
        ui.set_room_admin_notice("".into());
        ui.set_room_settings_confirmation(0);
        Some(generation)
    }
    fn refresh(self: &Arc<Self>, ui: &AppWindow, form: Arc<Form>) {
        let Some(generation) = self.begin(ui, &form, true) else {
            return;
        };
        {
            let mut state = form.content.lock().expect("Room settings state poisoned");
            state.dirty = 0;
            state.image = None;
            state.staged = false;
        }
        clear_image(ui);
        let this = self.clone();
        self.runtime.spawn(async move {
            let session = form.content.lock().expect("Room settings state poisoned").session.clone();
            let result = tokio::select! { biased;
                _ = form.cancelled() => return,
                result = async {
                    if !this.active(&form) { return Err(RoomSettingsError::Cancelled); }
                    match session {
                        Some(session) => { let snapshot = session.refresh().await?; Ok((session, snapshot)) },
                        None => { let session = Arc::new(this.live.manager.open_room_settings_for(&form.selection, &form.room, form.cancellation.clone()).await?);
                            let snapshot = session.snapshot(); Ok((session, snapshot)) },
                    }
                } => result,
            };
            if !this.active(&form) { return; }
            let mut state = form.content.lock().expect("Room settings state poisoned");
            if !state.progress.finish(generation, false) { return; }
            match result {
                Ok((session, snapshot)) => { state.session = Some(session); state.snapshot = Some(snapshot);
                    state.progress.refresh_required = false; state.notice = "Current room settings loaded."; }
                Err(error) => { state.progress.refresh_required = true; state.error = settings_error(error); }
            }
            drop(state);
            this.publish(form, generation, Reset::All);
        });
    }
    fn publish(self: &Arc<Self>, form: Arc<Form>, generation: u64, reset_inputs: Reset) {
        let this = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if this.matches_ui(&form, &ui) {
                this.render(&form, generation, &ui, reset_inputs);
            }
        });
    }
    fn render(&self, form: &Form, generation: u64, ui: &AppWindow, reset_inputs: Reset) {
        let (snapshot, busy, refresh, staged, image, error, notice, confirmation, dirty) = {
            let state = form.content.lock().expect("Room settings state poisoned");
            if state.progress.generation != generation {
                return;
            }
            let image = state.image.as_ref().map(|prepared| {
                let (width, height) = prepared.dimensions();
                let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    prepared.rgba(),
                    width,
                    height,
                );
                slint::Image::from_rgba8(pixels)
            });
            (
                state.snapshot.clone(),
                state.progress.busy,
                state.progress.refresh_required,
                state.staged,
                image,
                state.error,
                state.notice,
                state.confirmation,
                state.dirty,
            )
        };
        ui.set_room_admin_busy(busy);
        ui.set_room_settings_refresh_required(refresh);
        ui.set_room_admin_error(error.into());
        ui.set_room_admin_notice(notice.into());
        ui.set_room_settings_dirty(dirty != 0);
        ui.set_room_settings_staged(staged);
        ui.set_room_settings_image_visible(image.is_some());
        ui.set_room_settings_image(image.unwrap_or_default());
        ui.set_room_settings_confirmation(match confirmation {
            None => 0,
            Some(Confirmation::Public) => 1,
            Some(Confirmation::Remove) => 2,
            Some(Confirmation::Refresh) => 3,
        });
        ui.set_room_settings_loaded(snapshot.is_some());
        if let Some(snapshot) = snapshot {
            ui.set_room_settings_name_allowed(snapshot.permissions.name);
            ui.set_room_settings_topic_allowed(snapshot.permissions.topic);
            ui.set_room_settings_avatar_allowed(snapshot.permissions.avatar);
            ui.set_room_settings_join_allowed(
                snapshot.permissions.join_rule
                    && matches!(snapshot.join_rule, CurrentJoinRule::Editable(_)),
            );
            ui.set_room_settings_avatar_present(snapshot.avatar.present);
            ui.set_room_settings_preview_available(
                snapshot.avatar.preview_available
                    && (!snapshot.encrypted || ui.get_image_previews_encrypted_enabled()),
            );
            ui.set_room_settings_access_description(rule_description(&snapshot.join_rule).into());
            if reset_inputs.includes(Field::Name) {
                ui.set_room_admin_name_input(snapshot.name.clone().into());
            }
            if reset_inputs.includes(Field::Topic) {
                ui.set_room_admin_topic_input(snapshot.topic.clone().into());
            }
            if reset_inputs.includes(Field::Join) {
                let (selected, allow) = current_rule(&snapshot.join_rule);
                let index = selected
                    .and_then(|kind| {
                        snapshot
                            .available_join_rules
                            .iter()
                            .position(|v| *v == kind)
                    })
                    .map(|v| v as i32)
                    .unwrap_or(-1);
                let labels = snapshot
                    .available_join_rules
                    .iter()
                    .map(|kind| rule_label(*kind).into())
                    .collect::<Vec<slint::SharedString>>();
                ui.set_room_settings_join_options(ModelRc::new(VecModel::from(labels)));
                ui.set_room_settings_join_index(index);
                ui.set_room_settings_restricted(selected.is_some_and(restricted));
                ui.set_room_settings_allow_rooms(allow.join("\n").into());
            }
        }
    }
    fn action(self: &Arc<Self>, ui: &AppWindow, action: i32) {
        let Some(form) = self.current_for(ui) else {
            self.close(ui);
            return;
        };
        if action == 0 {
            let mut state = form.content.lock().expect("Room settings state poisoned");
            if state.progress.busy {
                return;
            }
            if state.dirty != 0 {
                state.confirmation = Some(Confirmation::Refresh);
                drop(state);
                ui.set_room_settings_confirmation(3);
            } else {
                drop(state);
                self.refresh(ui, form);
            }
            return;
        }
        if action == 7 {
            // Confirmation cancelled; no mutation was dispatched.
            let mut state = form.content.lock().expect("Room settings state poisoned");
            state.confirmation = None;
            ui.set_room_settings_confirmation(0);
            return;
        }
        if action == 4 {
            let mut state = form.content.lock().expect("Room settings state poisoned");
            if state.progress.busy || state.confirmation.is_some() {
                return;
            }
            state.image = None;
            state.staged = false;
            state.dirty &= !Field::Avatar.bit();
            ui.set_room_settings_dirty(state.dirty != 0);
            clear_image(ui);
            return;
        }
        let (snapshot, confirmation) = {
            let state = form.content.lock().expect("Room settings state poisoned");
            if state.progress.busy
                || (state.progress.refresh_required
                    && state.confirmation != Some(Confirmation::Refresh))
            {
                return;
            }
            let Some(snapshot) = state.snapshot.clone() else {
                return;
            };
            (snapshot, state.confirmation)
        };
        if confirmation.is_some() && action != 6 {
            return;
        }
        match action {
            1 if snapshot.permissions.avatar && !ui.get_room_settings_staged() => {
                self.choose(ui, form)
            }
            2 if snapshot.avatar.preview_available && !ui.get_room_settings_staged() => {
                self.preview(ui, form, ui.get_image_previews_encrypted_enabled())
            }
            3 if snapshot.permissions.avatar => self.write(ui, form, Write::Avatar),
            5 if snapshot.permissions.avatar && snapshot.avatar.present => {
                form.content
                    .lock()
                    .expect("Room settings state poisoned")
                    .confirmation = Some(Confirmation::Remove);
                ui.set_room_settings_confirmation(2);
            }
            6 => match confirmation {
                Some(Confirmation::Remove) => self.write(ui, form, Write::Remove),
                Some(Confirmation::Public) => {
                    self.write(ui, form, Write::Rule(EditableJoinRule::Public))
                }
                Some(Confirmation::Refresh) => self.refresh(ui, form),
                None => {}
            },
            8 if snapshot.permissions.join_rule => {
                match rule_from_input(
                    &snapshot,
                    ui.get_room_settings_join_index(),
                    ui.get_room_settings_allow_rooms().as_str(),
                ) {
                    Ok(EditableJoinRule::Public) => {
                        form.content
                            .lock()
                            .expect("Room settings state poisoned")
                            .confirmation = Some(Confirmation::Public);
                        ui.set_room_settings_confirmation(1);
                    }
                    Ok(rule) => self.write(ui, form, Write::Rule(rule)),
                    Err(message) => ui.set_room_admin_error(message.into()),
                }
            }
            9 => {
                // Selection changes only update presentation; the write resolves from a fresh immutable snapshot.
                let selected = usize::try_from(ui.get_room_settings_join_index())
                    .ok()
                    .and_then(|v| snapshot.available_join_rules.get(v))
                    .copied();
                ui.set_room_settings_restricted(selected.is_some_and(restricted));
                self.edited(ui, 4);
            }
            _ => {}
        }
    }
    fn choose(self: &Arc<Self>, ui: &AppWindow, form: Arc<Form>) {
        let Some(generation) = self.begin(ui, &form, false) else {
            return;
        };
        {
            let mut state = form.content.lock().expect("Room settings state poisoned");
            state.image = None;
            state.staged = false;
        }
        clear_image(ui);
        let this = self.clone();
        self.runtime.spawn(async move {
            let result = tokio::select! { biased; _ = form.cancelled() => return,
                result = choose_file(form.clone()) => result };
            let result = match result {
                Ok(Some(bytes)) if this.active(&form) => tokio::select! { biased;
                    _ = form.cancelled() => return,
                    result = prepare_avatar(bytes, form.selection.clone(), form.cancellation.clone()) => result.map(Some).map_err(|e| e.text()),
                },
                Ok(_) => Ok(None), Err(error) => Err(error),
            };
            this.finish_image(form, generation, result, true);
        });
    }
    fn preview(self: &Arc<Self>, ui: &AppWindow, form: Arc<Form>, allow_encrypted: bool) {
        let Some(generation) = self.begin(ui, &form, false) else {
            return;
        };
        {
            let mut state = form.content.lock().expect("Room settings state poisoned");
            state.image = None;
            state.staged = false;
        }
        clear_image(ui);
        let this = self.clone();
        self.runtime.spawn(async move {
            let session = form.content.lock().expect("Room settings state poisoned").session.clone();
            let result = tokio::select! { biased; _ = form.cancelled() => return, result = async {
                let session = session.ok_or(UNAVAILABLE)?;
                let bytes = session.download_avatar_preview(allow_encrypted).await.map_err(settings_error)?.map(|v| Zeroizing::new(v.bytes));
                if !this.active(&form) { return Err(UNAVAILABLE); }
                match bytes {
                    None => Ok(None),
                    Some(bytes) => prepare_avatar(bytes, form.selection.clone(), form.cancellation.clone()).await.map(Some).map_err(|e| e.text()),
                }
            } => result };
            this.finish_image(form, generation, result, false);
        });
    }
    fn finish_image(
        self: &Arc<Self>,
        form: Arc<Form>,
        generation: u64,
        result: Result<Option<PreparedAvatar>, &'static str>,
        staged: bool,
    ) {
        if !self.active(&form) {
            return;
        }
        let mut state = form.content.lock().expect("Room settings state poisoned");
        if !state.progress.finish(generation, false) {
            return;
        }
        match result {
            Ok(image) => {
                state.staged = staged && image.is_some();
                if state.staged {
                    state.dirty |= Field::Avatar.bit();
                }
                state.image = image;
                state.notice = if state.staged {
                    "Preview staged. Save avatar uploads this image as shared room metadata."
                } else if state.image.is_some() {
                    "Current avatar preview. Nothing has been uploaded."
                } else {
                    "No image selected or available."
                };
            }
            Err(message) => state.error = message,
        }
        drop(state);
        self.publish(form, generation, Reset::None);
    }
    fn write(self: &Arc<Self>, ui: &AppWindow, form: Arc<Form>, write: Write) {
        let Some(generation) = self.begin(ui, &form, false) else {
            return;
        };
        let field = match &write {
            Write::Avatar | Write::Remove => Field::Avatar,
            Write::Rule(_) => Field::Join,
        };
        let (session, prepared) = {
            let mut state = form.content.lock().expect("Room settings state poisoned");
            let prepared = if matches!(write, Write::Avatar) && state.staged {
                state.image.take()
            } else {
                None
            };
            if field == Field::Avatar {
                state.staged = false;
                state.image = None;
            }
            (state.session.clone(), prepared)
        };
        if field == Field::Avatar {
            clear_image(ui);
        }
        let this = self.clone();
        self.runtime.spawn(async move {
            let result = tokio::select! { biased; _ = form.cancelled() => return, result = async {
                if !this.active(&form) { return Err(RoomSettingsError::Cancelled); }
                let session = session.ok_or(RoomSettingsError::Unavailable)?;
                match write {
                    Write::Avatar => session.set_avatar(prepared.ok_or(RoomSettingsError::InvalidAvatar)?.into_upload().map_err(|_|RoomSettingsError::InvalidAvatar)?).await,
                    Write::Remove => session.remove_avatar().await,
                    Write::Rule(rule) => session.set_join_rule(rule).await,
                }
            } => result };
            this.finish_write(form, generation, field, result);
        });
    }
    fn finish_write(
        self: &Arc<Self>,
        form: Arc<Form>,
        generation: u64,
        field: Field,
        result: Result<RoomSettingsSnapshot, RoomSettingsError>,
    ) {
        if !self.active(&form) {
            return;
        }
        let mut state = form.content.lock().expect("Room settings state poisoned");
        if !state.progress.finish(generation, result.is_err()) {
            return;
        }
        if field == Field::Avatar {
            state.image = None;
            state.staged = false;
        }
        state.confirmation = None;
        let reset = if result.is_ok() {
            Reset::Field(field)
        } else {
            Reset::None
        };
        match result {
            Ok(snapshot) => {
                state.dirty &= !field.bit();
                state.snapshot = Some(snapshot);
                state.notice = "Room settings saved.";
            }
            Err(error) => {
                state.error = settings_error(error);
                state.notice = REFRESH_FIRST;
            }
        }
        drop(state);
        self.publish(form.clone(), generation, reset);
        // Room list refresh is independent of displaying this form's response.
        let live = self.live.clone();
        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            if form.selection.is_current() && live.navigation_is_current(form.navigation) {
                let _ = live.refresh(weak).await;
            }
        });
    }
    /// Text writes share the same session's authoritative baseline and permission
    /// checks as avatar and access changes; the old admin callback is redaction-only.
    pub(crate) fn save_text(self: &Arc<Self>, ui: &AppWindow, kind: i32, value: String) {
        let Some(form) = self.current_for(ui) else {
            return;
        };
        if !valid_text(kind, &value) {
            ui.set_room_admin_error(
                "Use a nonempty name up to 1024 bytes, or a topic up to 16 KiB.".into(),
            );
            return;
        }
        let Some(generation) = self.begin(ui, &form, false) else {
            return;
        };
        let session = form
            .content
            .lock()
            .expect("Room settings state poisoned")
            .session
            .clone();
        let this = self.clone();
        self.runtime.spawn(async move {
            let result = tokio::select! { biased; _ = form.cancelled() => return,
                result = async {
                    if !this.active(&form) { return Err(RoomSettingsError::Cancelled); }
                    let session = session.ok_or(RoomSettingsError::Unavailable)?;
                    if kind == 0 { session.set_name(&value).await } else { session.set_topic(&value).await }
                } => result,
            };
            this.finish_write(form, generation, if kind == 0 { Field::Name } else { Field::Topic }, result);
        });
    }
}
enum Write {
    Avatar,
    Remove,
    Rule(EditableJoinRule),
}
fn valid_text(kind: i32, value: &str) -> bool {
    match kind {
        0 => !value.trim().is_empty() && value.trim().len() <= 1024,
        1 => value.len() <= 16 * 1024,
        _ => false,
    }
}
fn clear_image(ui: &AppWindow) {
    ui.set_room_settings_image(slint::Image::default());
    ui.set_room_settings_image_visible(false);
    ui.set_room_settings_staged(false);
}
fn scope_active(
    current: Option<u64>,
    id: u64,
    selected: bool,
    cancelled: bool,
    navigation: bool,
    live: bool,
) -> bool {
    current == Some(id) && selected && !cancelled && navigation && live
}
fn form_matches(
    id: u64,
    account: &str,
    room: &str,
    shown_id: &str,
    shown_account: &str,
    shown_room: &str,
) -> bool {
    shown_id == id.to_string() && account == shown_account && room == shown_room
}
fn rule_label(kind: JoinRuleKind) -> &'static str {
    match kind {
        JoinRuleKind::Invite => "Invite only",
        JoinRuleKind::Public => "Public",
        JoinRuleKind::Knock => "Knock",
        JoinRuleKind::Restricted => "Restricted",
        JoinRuleKind::KnockRestricted => "Knock or restricted",
    }
}
fn restricted(kind: JoinRuleKind) -> bool {
    matches!(
        kind,
        JoinRuleKind::Restricted | JoinRuleKind::KnockRestricted
    )
}
fn current_rule(rule: &CurrentJoinRule) -> (Option<JoinRuleKind>, Vec<String>) {
    match rule {
        CurrentJoinRule::ReadOnly(_) => (None, Vec::new()),
        CurrentJoinRule::Editable(rule) => match rule {
            EditableJoinRule::Invite => (Some(JoinRuleKind::Invite), Vec::new()),
            EditableJoinRule::Public => (Some(JoinRuleKind::Public), Vec::new()),
            EditableJoinRule::Knock => (Some(JoinRuleKind::Knock), Vec::new()),
            EditableJoinRule::Restricted { allow_rooms } => {
                (Some(JoinRuleKind::Restricted), allow_rooms.clone())
            }
            EditableJoinRule::KnockRestricted { allow_rooms } => {
                (Some(JoinRuleKind::KnockRestricted), allow_rooms.clone())
            }
        },
    }
}
fn rule_description(rule: &CurrentJoinRule) -> &'static str {
    match current_rule(rule).0 {
        None => "This join rule cannot be edited safely by this version of Morpheus.",
        Some(JoinRuleKind::Invite) => "Only invited people can join.",
        Some(JoinRuleKind::Public) => "Anyone can join this room.",
        Some(JoinRuleKind::Knock) => "People can request an invitation by knocking.",
        Some(JoinRuleKind::Restricted) => {
            "Members of the allowed rooms can join; other people need an invitation."
        }
        Some(JoinRuleKind::KnockRestricted) => {
            "Members of allowed rooms can join; other people can knock."
        }
    }
}
fn rule_from_input(
    snapshot: &RoomSettingsSnapshot,
    index: i32,
    allow: &str,
) -> Result<EditableJoinRule, &'static str> {
    if !snapshot.permissions.join_rule
        || !matches!(snapshot.join_rule, CurrentJoinRule::Editable(_))
    {
        return Err("This room's join rule is read-only.");
    }
    let kind = usize::try_from(index)
        .ok()
        .and_then(|index| snapshot.available_join_rules.get(index))
        .copied()
        .ok_or("Choose an available join rule.")?;
    let rooms = if restricted(kind) {
        allow_rooms(allow)?
    } else {
        Vec::new()
    };
    Ok(match kind {
        JoinRuleKind::Invite => EditableJoinRule::Invite,
        JoinRuleKind::Public => EditableJoinRule::Public,
        JoinRuleKind::Knock => EditableJoinRule::Knock,
        JoinRuleKind::Restricted => EditableJoinRule::Restricted { allow_rooms: rooms },
        JoinRuleKind::KnockRestricted => EditableJoinRule::KnockRestricted { allow_rooms: rooms },
    })
}
fn allow_rooms(input: &str) -> Result<Vec<String>, &'static str> {
    const ERROR: &str = "Use at most 32 distinct Matrix room IDs, one per line (1024 bytes each).";
    if input.len() > MAX_RESTRICTED_ALLOW_ROOMS * 1025 {
        return Err(ERROR);
    }
    let mut rooms = Vec::new();
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.len() > 1024
            || rooms.len() == MAX_RESTRICTED_ALLOW_ROOMS
            || rooms.iter().any(|room| room == line)
        {
            return Err(ERROR);
        }
        // The core uses the pinned Ruma validator; no second Matrix ID grammar.
        rooms.push(line.to_owned());
    }
    Ok(rooms)
}
fn settings_error(error: RoomSettingsError) -> &'static str {
    match error {
        RoomSettingsError::Cancelled | RoomSettingsError::AccountChanged => {
            "This room-settings form is no longer active."
        }
        RoomSettingsError::NotJoined => "Join this room before editing its settings.",
        RoomSettingsError::Unavailable => {
            "The homeserver could not complete this room-settings request."
        }
        RoomSettingsError::RateLimited => {
            "The homeserver is limiting requests. Wait, then refresh before saving again."
        }
        RoomSettingsError::TimedOut => "The room-settings request timed out.",
        RoomSettingsError::InvalidState => "The room returned unsupported or invalid settings.",
        RoomSettingsError::PermissionDenied => {
            "Your current room permissions do not allow this change."
        }
        RoomSettingsError::ReadOnly => {
            "These room settings are read-only in this version of Morpheus."
        }
        RoomSettingsError::BaselineChanged => {
            "This setting changed since it was loaded. Refresh before editing it."
        }
        RoomSettingsError::InvalidText => {
            "Use a nonempty name up to 1024 bytes, or a topic up to 16 KiB."
        }
        RoomSettingsError::InvalidAvatar => {
            "Choose a supported image and inspect its staged preview before saving."
        }
        RoomSettingsError::InvalidJoinRule => {
            "The join rule or allowed room IDs are not valid for this room version."
        }
        RoomSettingsError::EncryptedPreviewDisabled => {
            "Enable previews in encrypted rooms in Appearance before viewing this avatar."
        }
    }
}
struct FilePermit<'a>(&'a AtomicBool);
impl<'a> FilePermit<'a> {
    fn acquire(gate: &'a AtomicBool) -> Option<Self> {
        gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(gate))
    }
}
impl Drop for FilePermit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
async fn choose_file(form: Arc<Form>) -> Result<Option<Zeroizing<Vec<u8>>>, &'static str> {
    let active = Arc::new(move || form.selection.is_current() && !form.cancellation.is_cancelled());
    choose_avatar_file("Choose room avatar", active).await
}
/// The one guarded native image chooser and bounded file read, shared by the
/// room-avatar form and the account's own public-profile editor. One chooser
/// or read runs process-wide; `active` is checked before and after the dialog
/// and between reads, so a closed form or changed account reads nothing more.
pub(crate) async fn choose_avatar_file(
    title: &'static str,
    active: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Option<Zeroizing<Vec<u8>>>, &'static str> {
    let permit = FilePermit::acquire(&FILE_WORKER).ok_or("A native image chooser or file read is still active. Finish it before choosing another image.")?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("morpheus-avatar-file".into())
        .spawn(move || {
            let _permit = permit;
            let active = || active();
            let result = (|| {
                if !active() {
                    return Ok(None);
                }
                let Some(path) = rfd::FileDialog::new()
                    .set_title(title)
                    .add_filter("PNG or baseline JPEG", &["png", "jpg", "jpeg"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                if !active() {
                    return Ok(None);
                }
                let metadata = std::fs::symlink_metadata(&path)
                    .map_err(|_| "The selected file could not be read.")?;
                if !metadata.file_type().is_file()
                    || metadata.len() > MAX_IMAGE_PREVIEW_BYTES as u64
                {
                    return Err("Choose a regular image file of at most 8 MiB.");
                }
                let mut file = std::fs::File::open(path)
                    .map_err(|_| "The selected file could not be read.")?;
                let metadata = file
                    .metadata()
                    .map_err(|_| "The selected file could not be read.")?;
                if !metadata.is_file() || metadata.len() > MAX_IMAGE_PREVIEW_BYTES as u64 {
                    return Err("Choose a regular image file of at most 8 MiB.");
                }
                read_bounded(&mut file, MAX_IMAGE_PREVIEW_BYTES, active).map(Some)
            })();
            let _ = tx.send(result);
        })
        .map_err(|_| "The native image chooser could not start.")?;
    rx.await
        .map_err(|_| "The selected file could not be read.")?
}
fn read_bounded(
    reader: &mut impl Read,
    limit: usize,
    active: impl Fn() -> bool,
) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit));
    let mut chunk = Zeroizing::new([0u8; 8192]);
    loop {
        if !active() {
            return Err("The image selection was cancelled.");
        }
        let maximum = chunk
            .len()
            .min(limit.saturating_sub(bytes.len()).saturating_add(1));
        let length = reader
            .read(&mut chunk[..maximum])
            .map_err(|_| "The selected file could not be read.")?;
        if length == 0 {
            return Ok(bytes);
        }
        if length > limit.saturating_sub(bytes.len()) {
            return Err("Choose an image file of at most 8 MiB.");
        }
        bytes.extend_from_slice(&chunk[..length]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_forms_do_not_match_returning_to_same_account_and_room() {
        assert!(form_matches(
            1,
            "account-a",
            "!room:example.org",
            "1",
            "account-a",
            "!room:example.org"
        ));
        assert!(!form_matches(
            1,
            "account-a",
            "!room:example.org",
            "2",
            "account-a",
            "!room:example.org"
        ));
        assert!(!form_matches(
            1,
            "account-a",
            "!room:example.org",
            "1",
            "account-b",
            "!room:example.org"
        ));
        assert!(!form_matches(
            1,
            "account-a",
            "!room:example.org",
            "1",
            "account-a",
            "!other:example.org"
        ));
    }
    #[test]
    fn writes_serialize_and_stale_completion_cannot_unlock_a_new_operation() {
        let mut state = Progress::default();
        let first = state.begin(false).unwrap();
        assert!(state.begin(false).is_none());
        assert!(state.finish(first, false));
        let second = state.begin(false).unwrap();
        assert!(!state.finish(first, false));
        assert!(state.busy);
        assert!(state.finish(second, false));
    }
    #[test]
    fn uncertain_write_requires_refresh_and_never_automatic_retry() {
        let mut state = Progress::default();
        let first = state.begin(false).unwrap();
        assert!(state.finish(first, true));
        assert!(state.begin(false).is_none());
        let refresh = state.begin(true).unwrap();
        assert!(state.finish(refresh, false));
        assert!(state.refresh_required); // Only a successful authoritative snapshot clears the latch.
    }
    #[test]
    fn restricted_room_entries_are_bounded_distinct_and_never_silently_truncated() {
        assert_eq!(
            allow_rooms(" !a:example.org\n\n!b\n").unwrap(),
            vec!["!a:example.org", "!b"]
        );
        assert!(allow_rooms("!a\n!a").is_err());
        assert!(allow_rooms(&"x".repeat(1025)).is_err());
        assert!(allow_rooms(&(0..33).map(|i| format!("!{i}\n")).collect::<String>()).is_err());
        assert!(allow_rooms("").unwrap().is_empty());
    }
    #[test]
    fn file_bytes_are_capped_during_read_even_if_reported_size_was_small() {
        let mut input = &b"12345"[..];
        assert!(read_bounded(&mut input, 4, || true).is_err());
        let mut exact = &b"1234"[..];
        assert_eq!(
            read_bounded(&mut exact, 4, || true).unwrap().as_slice(),
            b"1234"
        );
        let mut cancelled = &b"1234"[..];
        assert!(read_bounded(&mut cancelled, 4, || false).is_err());
    }
    #[test]
    fn generation_overflow_fails_closed_without_admitting_a_write() {
        let mut state = Progress {
            generation: u64::MAX,
            busy: false,
            refresh_required: false,
        };
        assert!(state.begin(false).is_none());
        assert!(!state.busy);
    }
    #[test]
    fn account_aba_navigation_and_cancelled_form_deny_owned_work() {
        assert!(scope_active(Some(7), 7, true, false, true, true));
        assert!(!scope_active(Some(7), 7, false, false, true, true));
        assert!(!scope_active(Some(7), 7, true, true, true, true));
        assert!(!scope_active(Some(7), 7, true, false, false, true));
        assert!(!scope_active(Some(8), 7, true, false, true, true));
        assert!(!scope_active(Some(7), 7, true, false, true, false));
    }
    #[test]
    fn saved_field_never_replaces_other_dirty_inputs() {
        for field in [Field::Name, Field::Topic, Field::Avatar, Field::Join] {
            let reset = Reset::Field(field);
            for other in [Field::Name, Field::Topic, Field::Avatar, Field::Join] {
                assert_eq!(reset.includes(other), field == other);
                assert_eq!((15 & !field.bit()) & other.bit() != 0, field != other);
            }
        }
        assert!(!Reset::None.includes(Field::Name));
        assert!(Reset::All.includes(Field::Topic));
    }
    #[test]
    fn text_bounds_match_typed_core_contract() {
        assert!(valid_text(0, "  Workshop  "));
        assert!(!valid_text(0, "  "));
        assert!(valid_text(0, &"x".repeat(1024)));
        assert!(!valid_text(0, &"é".repeat(513)));
        assert!(valid_text(1, ""));
        assert!(valid_text(1, &"x".repeat(16 * 1024)));
        assert!(!valid_text(1, &"x".repeat(16 * 1024 + 1)));
        assert!(!valid_text(2, "redaction"));
    }
    #[test]
    fn cancellation_does_not_release_owned_native_worker_permit() {
        let gate = AtomicBool::new(false);
        let permit = FilePermit::acquire(&gate).unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        drop(receiver);
        assert!(sender.send(()).is_err());
        assert!(FilePermit::acquire(&gate).is_none());
        drop(permit);
        assert!(FilePermit::acquire(&gate).is_some());
    }
    #[test]
    fn only_server_available_rules_are_selectable_and_unknown_rules_stay_read_only() {
        use morpheus_matrix::{ReadOnlyJoinRule, RoomAvatarSummary, RoomSettingsPermissions};
        let mut snapshot = RoomSettingsSnapshot {
            name: String::new(),
            topic: String::new(),
            avatar: RoomAvatarSummary {
                present: false,
                preview_available: false,
                width: None,
                height: None,
            },
            join_rule: CurrentJoinRule::Editable(EditableJoinRule::Invite),
            available_join_rules: vec![JoinRuleKind::Invite, JoinRuleKind::Restricted],
            permissions: RoomSettingsPermissions {
                name: false,
                topic: false,
                avatar: false,
                join_rule: true,
            },
            room_version: "12".into(),
            encrypted: false,
        };
        assert_eq!(
            rule_from_input(&snapshot, 1, "!allow").unwrap(),
            EditableJoinRule::Restricted {
                allow_rooms: vec!["!allow".into()]
            }
        );
        assert!(rule_from_input(&snapshot, -1, "").is_err());
        assert!(rule_from_input(&snapshot, 2, "").is_err());
        snapshot.join_rule = CurrentJoinRule::ReadOnly(ReadOnlyJoinRule::Unknown);
        assert!(rule_from_input(&snapshot, 0, "").is_err());
        snapshot.join_rule = CurrentJoinRule::Editable(EditableJoinRule::Invite);
        snapshot.permissions.join_rule = false;
        assert!(rule_from_input(&snapshot, 0, "").is_err());
    }
    #[test]
    fn controller_and_private_worker_state_are_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<RoomSettingsUi>();
        check::<Form>();
        check::<PreparedAvatar>();
    }
}
