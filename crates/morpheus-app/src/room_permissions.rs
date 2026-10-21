//! Roles and permissions of the open room: the third view of the room admin
//! dialog, opened from room settings.
//!
//! One editor exists per account selection and room while the view is
//! shown. It opens its own settings session (its own cancellation) whose
//! admission, asked by the core right before the write is handed over,
//! requires the editor to be the shown one, no security step in progress or
//! begun since it opened, and the navigation it opened in. Reads and the
//! save run on the runtime; every result is shown only while the editor is
//! still current for the visible account and room. Hiding the view, a room
//! or account change, a lock or a detach drops every row synchronously and
//! cancels pending work.
//!
//! Changes are staged per row and saved only from the review, which shows
//! each change's level before and after. Lowering your own level needs an
//! explicit confirmation; nothing is ever changed for anyone the user did
//! not stage. One save is one write (see
//! `morpheus_matrix::room_settings::room_permissions`). A save without an
//! answer keeps that room paused; a read cannot settle it, and it is never
//! resent. An accepted save waits for a later complete read showing its effect.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use morpheus_matrix::{
    AccountManager, AccountSelection, EventClass, MAX_PERMISSION_CHANGES, PermissionChange,
    PermissionKey, PermissionLevel, PermissionProblem, PermissionTarget, PermissionThreshold,
    PermissionUser, PermissionsEdit, PermissionsError, PermissionsReadOnly, PermissionsReview,
    ReviewedChange, RoomPermissionsSnapshot, RoomPermissionsView, RoomSettingsCancellation,
    RoomSettingsError, RoomSettingsSession, review_permissions,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{AppWindow, PermissionRow, RoomPermissions, live::LiveUi};

/// The room admin dialog's mode that shows this view.
pub(crate) const MODE: i32 = 3;
const MAX_QUERY_CHARS: usize = 200;
const TITLE_CHARS: usize = 80;
const DETAIL_CHARS: usize = 255;
const CHANGED: &str = "The room or a security step changed since this view opened. \
     Nothing more is sent from it; close it and open it again.";
const SECURITY_BUSY: &str =
    "Finish the account or security step in progress first. Nothing was sent.";
const NOT_A_NUMBER: &str = "Enter a whole number, for example 50.";
const VIEW_ONLY: &str = "You can see these permissions but not change them.";
const PAUSED_PENDING: &str = "A save of this room's permissions is still being sent. Changes \
     here wait until it ends.";
const PAUSED_UNKNOWN: &str = "A save of this room's permissions got no answer: it may or may not \
     have applied, and it may still arrive. To avoid overwriting it, changes to this room's \
     permissions stay paused here; nothing is sent again.";
const PAUSED_ACCEPTED: &str = "A save of this room's permissions was accepted, but no read since \
     then shows it yet. Refresh until it does; changes here wait until then.";
const RECONCILED: &str = "The earlier save now shows in the permissions read since it was \
     accepted.";
const TOO_MANY_RECEIPTS: &str = "Too many permission saves are still unresolved. Nothing was sent.";
/// Unresolved saves remembered at most; a new save is refused beyond it.
pub(crate) const MAX_RECEIPTS: usize = 32;

/// What the editor needs of the account core. `Selection` is captured on
/// the UI thread; `View` is a read the editor shows and a save is checked
/// against.
pub(crate) trait PermissionsCore: Send + Sync + 'static {
    type Selection: Send + Sync + 'static;
    type Session: Send + Sync + 'static;
    type View: Clone + Send + Sync + 'static;
    fn selection(&self, account: &str) -> Option<Self::Selection>;
    fn current(&self, selection: &Self::Selection) -> bool;
    fn snapshot(view: &Self::View) -> &RoomPermissionsSnapshot;
    fn open<'a>(
        &'a self,
        selection: &'a Self::Selection,
        room: &'a str,
        cancellation: RoomSettingsCancellation,
        admission: Admission,
    ) -> Pending<'a, Self::Session>;
    fn load<'a>(&'a self, session: &'a Self::Session) -> Pending<'a, Self::View>;
    fn save<'a>(
        &'a self,
        session: &'a Self::Session,
        view: &'a Self::View,
        edit: &'a PermissionsEdit,
    ) -> Pending<'a, Saved<Self::View>>;
}

pub(crate) type Admission = Arc<dyn Fn() -> bool + Send + Sync>;
pub(crate) type Pending<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, PermissionsError>> + Send + 'a>>;

/// A save that was read back: the new view and whether each change shows.
pub(crate) struct Saved<V> {
    pub(crate) view: V,
    pub(crate) shown: Vec<(PermissionTarget, bool)>,
}

impl PermissionsCore for AccountManager {
    type Selection = AccountSelection;
    type Session = RoomSettingsSession;
    type View = RoomPermissionsView;

    fn selection(&self, account: &str) -> Option<AccountSelection> {
        self.account_selection(account)
    }

    fn current(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
    }

    fn snapshot(view: &RoomPermissionsView) -> &RoomPermissionsSnapshot {
        &view.snapshot
    }

    fn open<'a>(
        &'a self,
        selection: &'a AccountSelection,
        room: &'a str,
        cancellation: RoomSettingsCancellation,
        admission: Admission,
    ) -> Pending<'a, RoomSettingsSession> {
        Box::pin(async move {
            let session = self
                .open_room_settings_for(selection, room, cancellation)
                .await?;
            Ok(session.with_admission(admission))
        })
    }

    fn load<'a>(&'a self, session: &'a RoomSettingsSession) -> Pending<'a, RoomPermissionsView> {
        Box::pin(session.load_permissions())
    }

    fn save<'a>(
        &'a self,
        session: &'a RoomSettingsSession,
        view: &'a RoomPermissionsView,
        edit: &'a PermissionsEdit,
    ) -> Pending<'a, Saved<RoomPermissionsView>> {
        Box::pin(async move {
            let saved = session.save_permissions(view, edit).await?;
            Ok(Saved {
                view: saved.view,
                shown: saved.shown,
            })
        })
    }
}

/// One staged change, by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Staged {
    pub(crate) row: String,
    pub(crate) change: PermissionChange,
}

struct Inner<C: PermissionsCore> {
    session: Option<Arc<C::Session>>,
    view: Option<C::View>,
    staged: Vec<Staged>,
    reading: bool,
    saving: bool,
    refresh_required: bool,
    query: String,
    section: i32,
    /// The edit under review.
    review: Option<PermissionsEdit>,
}

impl<C: PermissionsCore> Default for Inner<C> {
    fn default() -> Self {
        Self {
            session: None,
            view: None,
            staged: Vec::new(),
            reading: false,
            saving: false,
            refresh_required: false,
            query: String::new(),
            section: 0,
            review: None,
        }
    }
}

struct Editor<C: PermissionsCore> {
    id: u64,
    selection: C::Selection,
    account: String,
    room: String,
    navigation: u64,
    security: u64,
    cancellation: RoomSettingsCancellation,
    alive: Arc<AtomicBool>,
    inner: Mutex<Inner<C>>,
}

impl<C: PermissionsCore> Editor<C> {
    fn inner(&self) -> MutexGuard<'_, Inner<C>> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn end(&self) {
        self.alive.store(false, Ordering::Release);
        self.cancellation.cancel();
    }
}

pub(crate) struct RoomPermissionsUi<C: PermissionsCore> {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    core: Arc<C>,
    next: AtomicU64,
    current: Mutex<Option<Arc<Editor<C>>>>,
}

impl<C: PermissionsCore> RoomPermissionsUi<C> {
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: &Handle,
        core: Arc<C>,
    ) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            core,
            next: AtomicU64::new(0),
            current: Mutex::new(None),
        });
        let global = ui.global::<RoomPermissions>();
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_shown(move |shown| {
            if let Some(ui) = weak.upgrade() {
                this.context_changed(&ui, shown);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_context_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.context_changed(&ui, Self::visible(&ui));
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_reset(move || {
            if let Some(ui) = weak.upgrade() {
                this.reset(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_refresh(move || {
            if let Some(ui) = weak.upgrade() {
                this.refresh(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_filter(move |query, section| {
            if let Some(ui) = weak.upgrade() {
                this.filter(&ui, query.as_str(), section);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_stage(move |row, level| {
            if let Some(ui) = weak.upgrade() {
                this.stage(&ui, row.as_str(), level.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_unstage(move |row| {
            if let Some(ui) = weak.upgrade() {
                this.unstage(&ui, row.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_discard(move || {
            if let Some(ui) = weak.upgrade() {
                this.discard(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_review(move || {
            if let Some(ui) = weak.upgrade() {
                this.review(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_cancel_review(move || {
            if let Some(ui) = weak.upgrade() {
                this.cancel_review(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        global.on_save(move || {
            if let Some(ui) = weak.upgrade() {
                this.save(&ui);
            }
        });
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.reset(ui);
    }

    fn visible(ui: &AppWindow) -> bool {
        ui.get_room_admin_open() && ui.get_room_admin_mode() == MODE
    }

    /// The editor is the one on screen: current here, alive, its selection
    /// current, the session live, its account and room visible and the view
    /// shown.
    fn on_screen(&self, editor: &Editor<C>, ui: &AppWindow) -> bool {
        self.current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|current| current.id == editor.id)
            && editor.alive.load(Ordering::Acquire)
            && self.core.current(&editor.selection)
            && self.live.is_live.load(Ordering::Acquire)
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == editor.account
            && ui.get_active_room_id().as_str() == editor.room
            && Self::visible(ui)
    }

    /// No room was left and come back to, and no security step began (even
    /// one already ended), since the editor opened.
    fn epochs_hold(&self, editor: &Editor<C>) -> bool {
        self.live.navigation_is_current(editor.navigation)
            && self.live.security_gate.ringing_allowed(editor.security)
    }

    fn current_editor(&self, ui: &AppWindow) -> Option<Arc<Editor<C>>> {
        let editor = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()?;
        let shown = self.on_screen(&editor, ui);
        (shown && self.live.navigation_is_current(editor.navigation)).then_some(editor)
    }

    /// Synchronous: no row, level, staged change or notice survives this
    /// call, and the editor's pending work is cancelled. Close, Back and
    /// Escape call it before the view is hidden, so a held read or save can
    /// no longer act for it. A save's receipt is not the view's: it stays
    /// in the receipt table and is settled by the save task itself.
    fn reset(&self, ui: &AppWindow) {
        let previous = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(editor) = previous {
            editor.end();
            *editor.inner() = Inner::default();
        }
        clear_presentation(ui);
    }

    /// After a read or a save: a read started after an accepted save's
    /// answer that shows its exact effect settles it; any receipt still
    /// open for this account and room keeps the editor paused, whatever the
    /// view did or reads.
    fn apply_receipts(
        &self,
        editor: &Editor<C>,
        inner: &mut Inner<C>,
        ui: &AppWindow,
        read: Option<u64>,
    ) {
        let global = ui.global::<RoomPermissions>();
        let mut table = receipts();
        let mut reconciled = false;
        if let (Some(started), Some(view)) = (read, inner.view.as_ref()) {
            let snapshot = C::snapshot(view);
            reconciled = table.observe_read(&editor.account, &editor.room, started, snapshot);
        }
        match table.blocking(&editor.account, &editor.room) {
            Some(state) => {
                inner.refresh_required = true;
                global.set_notice(paused_text(state).into());
            }
            None if reconciled => global.set_notice(RECONCILED.into()),
            None => {}
        }
    }

    fn context_changed(self: &Arc<Self>, ui: &AppWindow, shown: bool) {
        let account = ui.get_active_account_local_id().to_string();
        let room = ui.get_active_room_id().to_string();
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        let wanted = shown && live && !account.is_empty() && !room.is_empty();
        let kept = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|editor| {
                editor.alive.load(Ordering::Acquire)
                    && self.core.current(&editor.selection)
                    && editor.account == account
                    && editor.room == room
                    && self.live.navigation_is_current(editor.navigation)
            });
        if wanted && kept {
            return;
        }
        self.reset(ui);
        if wanted {
            self.open(ui, account, room);
        }
    }

    fn open(self: &Arc<Self>, ui: &AppWindow, account: String, room: String) {
        if crate::room_leave::is_open(ui) {
            return;
        }
        // The exact selection of the account shown, captured here.
        let Some(selection) = self.core.selection(&account) else {
            return;
        };
        let Ok(previous) = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
        else {
            return;
        };
        let editor = Arc::new(Editor {
            id: previous + 1,
            selection,
            account,
            room,
            navigation: self.live.current_navigation_epoch(),
            security: self.live.security_gate.ringing_generation(),
            cancellation: RoomSettingsCancellation::new(),
            alive: Arc::new(AtomicBool::new(true)),
            inner: Mutex::new(Inner::default()),
        });
        *self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(editor.clone());
        let global = ui.global::<RoomPermissions>();
        let room_name = crate::directory_plain_text(&ui.get_active_room_name(), 120);
        global.set_room_name(room_name.into());
        if self.live.security_gate.blocked() {
            global.set_state(3);
            global.set_error(SECURITY_BUSY.into());
            return;
        }
        editor.inner().reading = true;
        global.set_state(1);
        let admission = self.admission(&editor);
        // Taken before the read starts: only a read started after a save's
        // answer can settle it.
        let started = receipts().read_started();
        let this = self.clone();
        self.runtime.spawn(async move {
            let core = this.core.clone();
            let room = editor.room.clone();
            let cancellation = editor.cancellation.clone();
            let opened = core
                .open(&editor.selection, &room, cancellation, admission)
                .await;
            let read = match opened {
                Ok(session) => {
                    let session = Arc::new(session);
                    let view = core.load(&session).await;
                    view.map(|view| (session, view))
                }
                Err(error) => Err(error),
            };
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| this.opened(&ui, &editor, read, started));
        });
    }

    /// What the core asks right before handing the write over.
    fn admission(&self, editor: &Editor<C>) -> Admission {
        let gate = self.live.security_gate.clone();
        let live = self.live.clone();
        let alive = editor.alive.clone();
        let (navigation, security) = (editor.navigation, editor.security);
        Arc::new(move || {
            alive.load(Ordering::Acquire)
                && !gate.blocked()
                && gate.ringing_allowed(security)
                && live.navigation_is_current(navigation)
        })
    }

    fn opened(
        &self,
        ui: &AppWindow,
        editor: &Arc<Editor<C>>,
        read: Result<(Arc<C::Session>, C::View), PermissionsError>,
        started: u64,
    ) {
        if !self.on_screen(editor, ui) {
            // Its editor ended: a reset already cleared what it showed.
            return;
        }
        let global = ui.global::<RoomPermissions>();
        let mut inner = editor.inner();
        inner.reading = false;
        if !self.epochs_hold(editor) {
            global.set_state(3);
            global.set_error(CHANGED.into());
            return;
        }
        match read {
            Ok((session, view)) => {
                inner.session = Some(session);
                inner.view = Some(view);
                global.set_state(2);
                global.set_error("".into());
                self.apply_receipts(editor, &mut inner, ui, Some(started));
                publish::<C>(ui, &inner);
            }
            Err(error) => {
                global.set_state(3);
                global.set_error(error_text(error).into());
            }
        }
    }

    fn refresh(self: &Arc<Self>, ui: &AppWindow) {
        let Some(editor) = self.current_editor(ui) else {
            return;
        };
        let session = {
            let mut inner = editor.inner();
            if inner.reading || inner.saving {
                return;
            }
            let Some(session) = inner.session.clone() else {
                drop(inner);
                // The first read failed: open again from the start.
                self.reset(ui);
                self.context_changed(ui, Self::visible(ui));
                return;
            };
            inner.reading = true;
            session
        };
        let global = ui.global::<RoomPermissions>();
        global.set_state(1);
        global.set_notice("".into());
        let started = receipts().read_started();
        let this = self.clone();
        self.runtime.spawn(async move {
            let read = this.core.load(&session).await;
            let weak = this.ui.clone();
            let _ =
                weak.upgrade_in_event_loop(move |ui| this.reloaded(&ui, &editor, read, started));
        });
    }

    fn reloaded(
        &self,
        ui: &AppWindow,
        editor: &Arc<Editor<C>>,
        read: Result<C::View, PermissionsError>,
        started: u64,
    ) {
        if !self.on_screen(editor, ui) {
            return;
        }
        let global = ui.global::<RoomPermissions>();
        let mut inner = editor.inner();
        inner.reading = false;
        if !self.epochs_hold(editor) {
            global.set_state(3);
            global.set_error(CHANGED.into());
            return;
        }
        let fresh = read.is_ok();
        match read {
            Ok(view) => {
                // Staged changes stay staged; the review shows their effect
                // against what was just read.
                inner.view = Some(view);
                inner.refresh_required = false;
                global.set_state(2);
                global.set_error("".into());
            }
            Err(error) => {
                global.set_state(2);
                global.set_error(error_text(error).into());
            }
        }
        // A refresh never clears what an open receipt requires; only a
        // read that succeeded may settle one.
        let read = fresh.then_some(started);
        self.apply_receipts(editor, &mut inner, ui, read);
        publish::<C>(ui, &inner);
    }

    fn filter(&self, ui: &AppWindow, query: &str, section: i32) {
        let Some(editor) = self.current_editor(ui) else {
            return;
        };
        let mut inner = editor.inner();
        inner.query = query.chars().take(MAX_QUERY_CHARS).collect();
        inner.section = section.clamp(0, 1);
        publish::<C>(ui, &inner);
        let global = ui.global::<RoomPermissions>();
        global.set_generation(global.get_generation().wrapping_add(1));
    }

    fn stage(&self, ui: &AppWindow, row: &str, level: &str) {
        let Some(editor) = self.current_editor(ui) else {
            return;
        };
        let global = ui.global::<RoomPermissions>();
        let mut inner = editor.inner();
        if inner.saving || inner.review.is_some() {
            return;
        }
        let Some(view) = inner.view.clone() else {
            return;
        };
        let snapshot = C::snapshot(&view);
        let rows = rows(snapshot, &inner.staged, &inner.query, inner.section);
        // Only a row on screen, and only one this account may change.
        let Some(shown) = rows.iter().find(|shown| shown.id.as_str() == row) else {
            return;
        };
        if !shown.editable {
            global.set_error(shown.reason.clone());
            return;
        }
        let level = match parse_level(level, shown.removable) {
            Ok(level) => level,
            Err(text) => {
                global.set_error(text.into());
                return;
            }
        };
        let Some(target) = target(row) else {
            return;
        };
        let change = PermissionChange { target, level };
        match stage_change(&mut inner.staged, snapshot, row, change) {
            Ok(()) => global.set_error("".into()),
            Err(problem) => global.set_error(problem.text().into()),
        }
        global.set_notice("".into());
        publish::<C>(ui, &inner);
    }

    fn unstage(&self, ui: &AppWindow, row: &str) {
        let Some(editor) = self.current_editor(ui) else {
            return;
        };
        let mut inner = editor.inner();
        if inner.saving || inner.review.is_some() {
            return;
        }
        inner.staged.retain(|staged| staged.row != row);
        publish::<C>(ui, &inner);
    }

    fn discard(&self, ui: &AppWindow) {
        let Some(editor) = self.current_editor(ui) else {
            return;
        };
        let mut inner = editor.inner();
        if inner.saving {
            return;
        }
        inner.staged.clear();
        inner.review = None;
        let global = ui.global::<RoomPermissions>();
        global.set_review_open(false);
        global.set_error("".into());
        publish::<C>(ui, &inner);
    }

    fn review(&self, ui: &AppWindow) {
        let Some(editor) = self.current_editor(ui) else {
            return;
        };
        let global = ui.global::<RoomPermissions>();
        let mut inner = editor.inner();
        if inner.saving || inner.reading || inner.refresh_required || inner.staged.is_empty() {
            return;
        }
        let Some(view) = inner.view.clone() else {
            return;
        };
        let snapshot = C::snapshot(&view);
        let mut edit = PermissionsEdit {
            changes: inner
                .staged
                .iter()
                .map(|staged| staged.change.clone())
                .collect(),
            // Reviewed as confirmed to show the effect; the save carries
            // only what the user confirms in the review.
            own_demotion_confirmed: true,
        };
        let review = match review_permissions(snapshot, &edit) {
            Ok(review) => review,
            Err(problem) => {
                global.set_error(problem.text().into());
                return;
            }
        };
        edit.own_demotion_confirmed = false;
        let lines: Vec<SharedString> = review_lines(snapshot, &review)
            .into_iter()
            .map(SharedString::from)
            .collect();
        global.set_review_lines(ModelRc::new(VecModel::from(lines)));
        global.set_review_warning(review_warning(snapshot, &review).into());
        global.set_review_needs_confirmation(review.own_demotion.is_some());
        global.set_review_confirmed(false);
        global.set_review_open(true);
        global.set_error("".into());
        inner.review = Some(edit);
    }

    fn cancel_review(&self, ui: &AppWindow) {
        let global = ui.global::<RoomPermissions>();
        global.set_review_open(false);
        global.set_review_confirmed(false);
        if let Some(editor) = self.current_editor(ui) {
            editor.inner().review = None;
        }
    }

    fn save(self: &Arc<Self>, ui: &AppWindow) {
        let Some(editor) = self.current_editor(ui) else {
            return;
        };
        let global = ui.global::<RoomPermissions>();
        let (session, view, edit, receipt) = {
            let mut inner = editor.inner();
            if inner.saving {
                return;
            }
            // Lowering your own level is sent only once confirmed.
            let confirmed = global.get_review_confirmed();
            if global.get_review_needs_confirmation() && !confirmed {
                return;
            }
            if inner.refresh_required {
                return;
            }
            let (Some(session), Some(view), Some(changes)) = (
                inner.session.clone(),
                inner.view.clone(),
                inner.review.as_ref().map(|edit| edit.changes.clone()),
            ) else {
                return;
            };
            // The receipt is reserved before anything can be handed over;
            // the table refuses a second save for the room and any save
            // beyond its bound.
            let reserved = receipts().reserve(&editor.account, &editor.room, changes);
            let receipt = match reserved {
                Ok(receipt) => receipt,
                Err(Refusal::Busy(state)) => {
                    inner.refresh_required = true;
                    global.set_error(paused_text(state).into());
                    return;
                }
                Err(Refusal::Full) => {
                    global.set_error(TOO_MANY_RECEIPTS.into());
                    return;
                }
            };
            let Some(mut edit) = inner.review.take() else {
                receipts().complete(receipt, SaveOutcome::Unsent);
                return;
            };
            edit.own_demotion_confirmed = confirmed;
            inner.saving = true;
            (session, view, edit, receipt)
        };
        global.set_review_open(false);
        global.set_review_confirmed(false);
        global.set_busy(true);
        global.set_error("".into());
        global.set_notice("".into());
        let this = self.clone();
        self.runtime.spawn(async move {
            let saved = this.core.save(&session, &view, &edit).await;
            // Settled here, whatever any view does: only what the answer
            // proves. A dropped task leaves the receipt pending (paused).
            receipts().complete(receipt, outcome_of(&saved));
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| this.saved(&ui, &editor, &edit, saved));
        });
    }

    fn saved(
        &self,
        ui: &AppWindow,
        editor: &Arc<Editor<C>>,
        edit: &PermissionsEdit,
        saved: Result<Saved<C::View>, PermissionsError>,
    ) {
        if !self.on_screen(editor, ui) {
            return;
        }
        let global = ui.global::<RoomPermissions>();
        let mut inner = editor.inner();
        inner.saving = false;
        global.set_busy(false);
        match saved {
            Ok(saved) => {
                inner.view = Some(saved.view);
                inner.staged.clear();
                inner.refresh_required = false;
                global.set_notice(saved_notice(edit.changes.len(), &saved.shown).into());
            }
            Err(error) => {
                let (mut text, refresh, applied) = save_failure(error);
                // Refused at the hand-off for a step that already ended: this
                // editor stays refused, so only reopening helps.
                let busy = PermissionsError::Settings(RoomSettingsError::SecurityBusy);
                if error == busy && !self.epochs_hold(editor) {
                    text = CHANGED;
                }
                global.set_error(text.into());
                inner.refresh_required |= refresh;
                if applied {
                    inner.staged.clear();
                }
            }
        }
        // The save's own read-back settled it, or its receipt keeps the
        // editor paused.
        self.apply_receipts(editor, &mut inner, ui, None);
        publish::<C>(ui, &inner);
    }
}

/// What one save's answer proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveOutcome {
    /// Refused before anything was handed over: nothing was sent.
    Unsent,
    /// The homeserver answered with a refusal: nothing was applied.
    Refused,
    /// Accepted, and the read-back after the answer shows every change.
    AcceptedShown,
    /// Accepted (or saved and not read back), not yet shown by a read.
    Accepted,
    /// No answer after the hand-off: it may or may not have applied, and
    /// it may still arrive.
    Unknown,
}

/// The outcome a save's result proves (see the core's write contract: after
/// the hand-off only Unknown, SavedNotRead or a fixed refusal).
pub(crate) fn outcome_of<V>(saved: &Result<Saved<V>, PermissionsError>) -> SaveOutcome {
    match saved {
        Ok(saved) if saved.shown.iter().all(|(_, shown)| *shown) => SaveOutcome::AcceptedShown,
        Ok(_) => SaveOutcome::Accepted,
        Err(PermissionsError::Problem(_)) => SaveOutcome::Unsent,
        Err(PermissionsError::Settings(error)) => match error {
            RoomSettingsError::Unknown => SaveOutcome::Unknown,
            RoomSettingsError::SavedNotRead => SaveOutcome::Accepted,
            RoomSettingsError::PermissionDenied
            | RoomSettingsError::Refused
            | RoomSettingsError::RateLimited
            | RoomSettingsError::SignInExpired => SaveOutcome::Refused,
            _ => SaveOutcome::Unsent,
        },
    }
}

/// An open receipt's state: what keeps its room paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptState {
    /// Reserved and maybe handed over; its save has not answered.
    Pending,
    /// Accepted at this tick of the table's clock; a read started later
    /// that shows its exact effect settles it.
    Accepted { answered: u64 },
    /// No answer: never settled by a read, never evicted.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// This account and room already have an open receipt.
    Busy(ReceiptState),
    /// The table is at its bound with open receipts only.
    Full,
}

struct Receipt {
    id: u64,
    account: String,
    room: String,
    changes: Vec<PermissionChange>,
    state: ReceiptState,
}

/// Every save's receipt, per account and exact room, kept apart from any
/// view so closing, reopening or switching accounts never loses one. It
/// holds identifiers and the staged changes only, never a client or store.
/// Only open receipts are kept (a settled one is removed), so the bound is
/// enforced where receipts are made: `reserve` refuses beyond it.
#[derive(Default)]
pub(crate) struct Receipts {
    entries: Vec<Receipt>,
    next_id: u64,
    clock: u64,
}

impl Receipts {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 0,
            clock: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Reserve a receipt before a save is dispatched.
    pub(crate) fn reserve(
        &mut self,
        account: &str,
        room: &str,
        changes: Vec<PermissionChange>,
    ) -> Result<u64, Refusal> {
        if let Some(state) = self.blocking(account, room) {
            return Err(Refusal::Busy(state));
        }
        if self.entries.len() >= MAX_RECEIPTS {
            return Err(Refusal::Full);
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push(Receipt {
            id: self.next_id,
            account: account.to_owned(),
            room: room.to_owned(),
            changes,
            state: ReceiptState::Pending,
        });
        Ok(self.next_id)
    }

    /// Settle a receipt with what its save's answer proves. A receipt that
    /// is gone (its account was removed) is never made again.
    pub(crate) fn complete(&mut self, id: u64, outcome: SaveOutcome) {
        let answered = self.tick();
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            return;
        };
        match outcome {
            SaveOutcome::Unsent | SaveOutcome::Refused | SaveOutcome::AcceptedShown => {
                self.entries.remove(index);
            }
            SaveOutcome::Accepted => {
                self.entries[index].state = ReceiptState::Accepted { answered };
            }
            SaveOutcome::Unknown => self.entries[index].state = ReceiptState::Unknown,
        }
    }

    /// A tick taken when a read starts.
    pub(crate) fn read_started(&mut self) -> u64 {
        self.tick()
    }

    /// A read started at `started` settles an accepted receipt of the same
    /// account and room only if it started after the answer and shows the
    /// exact effect of every change. Pending and Unknown receipts stay.
    pub(crate) fn observe_read(
        &mut self,
        account: &str,
        room: &str,
        started: u64,
        snapshot: &RoomPermissionsSnapshot,
    ) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| {
            let settled = entry.account == account
                && entry.room == room
                && matches!(entry.state, ReceiptState::Accepted { answered } if started > answered)
                && entry
                    .changes
                    .iter()
                    .all(|change| snapshot_shows(snapshot, change));
            !settled
        });
        self.entries.len() != before
    }

    pub(crate) fn blocking(&self, account: &str, room: &str) -> Option<ReceiptState> {
        self.entries
            .iter()
            .find(|entry| entry.account == account && entry.room == room)
            .map(|entry| entry.state)
    }

    /// The account was removed: its receipts go, and a late completion
    /// cannot bring them back.
    pub(crate) fn forget_account(&mut self, account: &str) {
        self.entries.retain(|entry| entry.account != account);
    }
}

static RECEIPTS: Mutex<Receipts> = Mutex::new(Receipts::new());

fn receipts() -> MutexGuard<'static, Receipts> {
    RECEIPTS.lock().unwrap_or_else(|error| error.into_inner())
}

/// Account removal or logout: forget its permission receipts.
pub(crate) fn forget_account(account: &str) {
    receipts().forget_account(account);
}

fn paused_text(state: ReceiptState) -> &'static str {
    match state {
        ReceiptState::Pending => PAUSED_PENDING,
        ReceiptState::Accepted { .. } => PAUSED_ACCEPTED,
        ReceiptState::Unknown => PAUSED_UNKNOWN,
    }
}

/// Whether a read shows one change's exact effect: a user's exact stored
/// level (or absence), a permission set to exactly that level (or no longer
/// set), an event entry beyond the listed ones only when none were left out.
pub(crate) fn snapshot_shows(
    snapshot: &RoomPermissionsSnapshot,
    change: &PermissionChange,
) -> bool {
    if !snapshot.has_complete_state || snapshot.read_only.is_some() {
        return false;
    }
    match &change.target {
        PermissionTarget::User(user) => snapshot.exact_users.get(user).copied() == change.level,
        PermissionTarget::Threshold(key) => {
            let row = snapshot.thresholds.iter().find(|row| row.key == *key);
            match (row, change.level) {
                (Some(row), Some(level)) => row.explicit && row.level == level,
                (Some(row), None) => !row.explicit,
                (None, None) => {
                    matches!(key, PermissionKey::Event(_)) && snapshot.omitted_events == 0
                }
                (None, Some(_)) => false,
            }
        }
    }
}

/// The view's own reset, for a room or account change, a lock or a detach:
/// its editor ends and its rows go before this returns.
pub(crate) fn reset_window(ui: &AppWindow) {
    ui.global::<RoomPermissions>().invoke_reset();
}

/// Every row, level, staged change and notice cleared. UI thread only.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    let global = ui.global::<RoomPermissions>();
    global.set_state(0);
    global.set_summary("".into());
    global.set_error("".into());
    global.set_notice("".into());
    global.set_busy(false);
    global.set_refresh_required(false);
    global.set_query("".into());
    global.set_section(0);
    global.set_rows(ModelRc::new(VecModel::<PermissionRow>::default()));
    global.set_staged(0);
    global.set_review_open(false);
    global.set_review_lines(ModelRc::new(VecModel::<SharedString>::default()));
    global.set_review_warning("".into());
    global.set_review_needs_confirmation(false);
    global.set_review_confirmed(false);
    global.set_generation(global.get_generation().wrapping_add(1));
}

/// The rows, summary and counts, built on the UI thread.
fn publish<C: PermissionsCore>(ui: &AppWindow, inner: &Inner<C>) {
    let global = ui.global::<RoomPermissions>();
    let Some(view) = inner.view.as_ref() else {
        return;
    };
    let snapshot = C::snapshot(view);
    let shown = rows(snapshot, &inner.staged, &inner.query, inner.section);
    global.set_rows(ModelRc::new(VecModel::from(shown)));
    global.set_summary(summary(snapshot).into());
    global.set_staged(i32::try_from(inner.staged.len()).unwrap_or(i32::MAX));
    global.set_refresh_required(inner.refresh_required);
}

/// A typed level: a whole number in Matrix's range, or empty for the
/// default where the entry can be removed.
pub(crate) fn parse_level(text: &str, removable: bool) -> Result<Option<i64>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return if removable {
            Ok(None)
        } else {
            Err(PermissionProblem::NotRemovable.text())
        };
    }
    let level = text.parse::<i64>().map_err(|_| NOT_A_NUMBER)?;
    if level.unsigned_abs() > 9_007_199_254_740_991 {
        return Err(PermissionProblem::OutOfRange.text());
    }
    Ok(Some(level))
}

/// Stage `change` for `row`, replacing an earlier one for it. A change back
/// to what the room already has is no change and unstages the row.
pub(crate) fn stage_change(
    staged: &mut Vec<Staged>,
    snapshot: &RoomPermissionsSnapshot,
    row: &str,
    change: PermissionChange,
) -> Result<(), PermissionProblem> {
    staged.retain(|staged| staged.row != row);
    if unchanged(snapshot, &change) {
        return Ok(());
    }
    if staged.len() >= MAX_PERMISSION_CHANGES {
        return Err(PermissionProblem::TooManyChanges);
    }
    staged.push(Staged {
        row: row.to_owned(),
        change,
    });
    Ok(())
}

fn unchanged(snapshot: &RoomPermissionsSnapshot, change: &PermissionChange) -> bool {
    match &change.target {
        // From the full stored levels, never the display-capped list.
        PermissionTarget::User(user) => snapshot.exact_users.get(user).copied() == change.level,
        PermissionTarget::Threshold(key) => {
            let row = snapshot.thresholds.iter().find(|row| row.key == *key);
            match (row, change.level) {
                (Some(row), Some(level)) => row.explicit && row.level == level,
                (Some(row), None) => !row.explicit,
                (None, None) => true,
                (None, Some(_)) => false,
            }
        }
    }
}

fn row_id(target: &PermissionTarget) -> String {
    match target {
        PermissionTarget::User(user) => format!("u:{user}"),
        PermissionTarget::Threshold(PermissionKey::Event(kind)) => format!("e:{kind}"),
        PermissionTarget::Threshold(key) => format!("k:{}", key_name(key)),
    }
}

fn key_name(key: &PermissionKey) -> &'static str {
    match key {
        PermissionKey::UsersDefault => "users_default",
        PermissionKey::EventsDefault => "events_default",
        PermissionKey::StateDefault => "state_default",
        PermissionKey::Invite => "invite",
        PermissionKey::Kick => "kick",
        PermissionKey::Ban => "ban",
        PermissionKey::Redact => "redact",
        PermissionKey::NotifyRoom => "notify_room",
        PermissionKey::Event(_) => "event",
    }
}

/// The target a row stands for.
pub(crate) fn target(row: &str) -> Option<PermissionTarget> {
    if let Some(user) = row.strip_prefix("u:") {
        return Some(PermissionTarget::User(user.to_owned()));
    }
    if let Some(kind) = row.strip_prefix("e:") {
        return Some(PermissionTarget::Threshold(PermissionKey::Event(
            kind.to_owned(),
        )));
    }
    let key = match row.strip_prefix("k:")? {
        "users_default" => PermissionKey::UsersDefault,
        "events_default" => PermissionKey::EventsDefault,
        "state_default" => PermissionKey::StateDefault,
        "invite" => PermissionKey::Invite,
        "kick" => PermissionKey::Kick,
        "ban" => PermissionKey::Ban,
        "redact" => PermissionKey::Redact,
        "notify_room" => PermissionKey::NotifyRoom,
        _ => return None,
    };
    Some(PermissionTarget::Threshold(key))
}

/// What a permission allows, in plain words.
pub(crate) fn key_label(key: &PermissionKey) -> String {
    let label = match key {
        PermissionKey::UsersDefault => "Default level of members",
        PermissionKey::EventsDefault => "Send messages",
        PermissionKey::StateDefault => "Change other room settings",
        PermissionKey::Invite => "Invite people",
        PermissionKey::Kick => "Remove people",
        PermissionKey::Ban => "Ban people",
        PermissionKey::Redact => "Remove other people's messages",
        PermissionKey::NotifyRoom => "Notify the whole room (@room)",
        PermissionKey::Event(kind) => match kind.as_str() {
            "m.room.name" => "Change the room name",
            "m.room.topic" => "Change the topic",
            "m.room.avatar" => "Change the room avatar",
            "m.room.canonical_alias" => "Change the main address",
            "m.room.join_rules" => "Change who can join",
            "m.room.history_visibility" => "Change who can read history",
            "m.room.power_levels" => "Change roles and permissions",
            "m.room.pinned_events" => "Pin messages",
            "m.room.encryption" => "Turn on encryption",
            "m.room.server_acl" => "Change which servers may take part",
            "m.room.tombstone" => "Upgrade the room",
            "m.reaction" => "Send reactions",
            other => {
                let other = crate::directory_plain_text(other, DETAIL_CHARS);
                return format!("Send {other} events");
            }
        },
    };
    label.to_owned()
}

/// A user's level in words: "Administrator · 100", "Creator · unlimited".
pub(crate) fn user_level_text(level: PermissionLevel, users_default: i64) -> String {
    match level {
        PermissionLevel::Creator => "Creator · unlimited".to_owned(),
        PermissionLevel::Level(level) => {
            let role = match level {
                100 => "Administrator",
                50 => "Moderator",
                _ if level == users_default => "Member",
                _ => "Custom",
            };
            format!("{role} · {level}")
        }
    }
}

/// Who a permission's level lets act: "Moderators and above · 50".
pub(crate) fn threshold_text(level: i64, users_default: i64) -> String {
    if level <= users_default {
        format!("Everyone · {level}")
    } else if level == 50 {
        "Moderators and above · 50".to_owned()
    } else if level == 100 {
        "Administrators · 100".to_owned()
    } else {
        format!("Level {level} and above (custom)")
    }
}

fn readonly_text(reason: PermissionsReadOnly) -> &'static str {
    PermissionProblem::ReadOnly(reason).text()
}

/// Why a row cannot be changed, from what was read.
fn user_reason(snapshot: &RoomPermissionsSnapshot, user: &PermissionUser) -> &'static str {
    if user.editable {
        ""
    } else if let Some(reason) = snapshot.read_only {
        readonly_text(reason)
    } else if user.level == PermissionLevel::Creator {
        PermissionProblem::CreatorFixed.text()
    } else if !snapshot.can_change {
        PermissionProblem::CannotChange.text()
    } else {
        PermissionProblem::NotBelowYou.text()
    }
}

fn threshold_reason(snapshot: &RoomPermissionsSnapshot, row: &PermissionThreshold) -> &'static str {
    if row.editable {
        ""
    } else if let Some(reason) = snapshot.read_only {
        readonly_text(reason)
    } else if !snapshot.can_change {
        PermissionProblem::CannotChange.text()
    } else {
        PermissionProblem::NeedsMoreThanYou.text()
    }
}

fn staged_text(staged: &[Staged], target: &PermissionTarget, default: &str) -> String {
    match staged.iter().find(|staged| staged.change.target == *target) {
        Some(Staged {
            change: PermissionChange {
                level: Some(level), ..
            },
            ..
        }) => format!("→ {level}"),
        Some(_) => format!("→ default, {default}"),
        None => String::new(),
    }
}

/// The default an event entry falls back to, by its class; a custom type's
/// depends on how it is sent.
fn class_default(snapshot: &RoomPermissionsSnapshot, class: EventClass) -> String {
    match class {
        EventClass::State => state_default(snapshot).to_string(),
        EventClass::Unknown => format!(
            "{} as a message, {} as state",
            events_default(snapshot),
            state_default(snapshot)
        ),
        EventClass::Message | EventClass::NotEvent => events_default(snapshot).to_string(),
    }
}

/// The rows of one section, filtered by the loaded text only. In People a
/// full Matrix ID that is not listed becomes a row to give them a level.
pub(crate) fn rows(
    snapshot: &RoomPermissionsSnapshot,
    staged: &[Staged],
    query: &str,
    section: i32,
) -> Vec<PermissionRow> {
    let needle = query.trim().to_lowercase();
    let matches = |fields: &[&str]| {
        let found = |field: &&str| field.to_lowercase().contains(&needle);
        needle.is_empty() || fields.iter().any(found)
    };
    let mut rows = Vec::new();
    if section == 0 {
        let users_default = snapshot.users_default;
        for user in &snapshot.users {
            if !matches(&[&user.display_name, &user.user_id]) {
                continue;
            }
            let target = PermissionTarget::User(user.user_id.clone());
            let title = if user.own {
                format!("{} (you)", user.display_name)
            } else {
                user.display_name.clone()
            };
            rows.push(PermissionRow {
                id: row_id(&target).into(),
                title: crate::directory_plain_text(&title, TITLE_CHARS).into(),
                detail: crate::directory_plain_text(&user.user_id, DETAIL_CHARS).into(),
                level: user_level_text(user.level, users_default).into(),
                change: staged_text(staged, &target, &users_default.to_string()).into(),
                editable: user.editable,
                removable: user.editable,
                reason: user_reason(snapshot, user).into(),
            });
        }
        // Staged and typed users who are not listed: their level is the
        // default unless the room sets one beyond the listed users.
        let listed = |id: &str| snapshot.users.iter().any(|user| user.user_id == id);
        let mut extra: Vec<String> = staged
            .iter()
            .filter_map(|staged| match &staged.change.target {
                PermissionTarget::User(user) if !listed(user) => Some(user.clone()),
                _ => None,
            })
            .collect();
        let typed = query.trim();
        if matrix_user_id(typed) && !listed(typed) && !extra.iter().any(|user| user == typed) {
            extra.push(typed.to_owned());
        }
        for user in extra {
            let target = PermissionTarget::User(user.clone());
            // Their real level from the full stored levels: one past the
            // listed rows may still have a level of their own.
            let stored = snapshot.exact_users.get(&user).copied();
            let own = user == snapshot.own_user;
            let below = stored.is_none_or(|level| own || snapshot.own_level.above(level));
            let editable = snapshot.can_change && snapshot.read_only.is_none() && below;
            let reason = if editable {
                ""
            } else if let Some(reason) = snapshot.read_only {
                readonly_text(reason)
            } else if !snapshot.can_change {
                PermissionProblem::CannotChange.text()
            } else {
                PermissionProblem::NotBelowYou.text()
            };
            let (title, detail, level) = match stored {
                Some(level) => (
                    format!("Change {user}'s level"),
                    "Not listed here, but has a level of their own",
                    level,
                ),
                None => (
                    format!("Give {user} a level"),
                    "No level of their own: the default level applies",
                    users_default,
                ),
            };
            rows.push(PermissionRow {
                id: row_id(&target).into(),
                title: crate::directory_plain_text(&title, TITLE_CHARS).into(),
                detail: detail.into(),
                level: user_level_text(PermissionLevel::Level(level), users_default).into(),
                change: staged_text(staged, &target, &users_default.to_string()).into(),
                editable,
                removable: editable && stored.is_some(),
                reason: reason.into(),
            });
        }
    } else {
        for row in &snapshot.thresholds {
            let label = key_label(&row.key);
            let kind = match &row.key {
                PermissionKey::Event(kind) => kind.as_str(),
                _ => "",
            };
            if !matches(&[&label, kind]) {
                continue;
            }
            let target = PermissionTarget::Threshold(row.key.clone());
            let mut level = match row.key {
                PermissionKey::UsersDefault => format!("Level {}", row.level),
                _ => threshold_text(row.level, snapshot.users_default),
            };
            if !row.explicit {
                level.push_str(" (default)");
            }
            let removable = matches!(row.key, PermissionKey::Event(_) | PermissionKey::NotifyRoom)
                && row.editable;
            let default = match &row.key {
                PermissionKey::NotifyRoom => "50".to_owned(),
                PermissionKey::Event(_) => class_default(snapshot, row.class),
                _ => events_default(snapshot).to_string(),
            };
            rows.push(PermissionRow {
                id: row_id(&target).into(),
                title: label.into(),
                detail: crate::directory_plain_text(kind, DETAIL_CHARS).into(),
                level: level.into(),
                change: staged_text(staged, &target, &default).into(),
                editable: row.editable,
                removable,
                reason: threshold_reason(snapshot, row).into(),
            });
        }
    }
    rows
}

fn default_of(snapshot: &RoomPermissionsSnapshot, key: PermissionKey) -> i64 {
    snapshot
        .thresholds
        .iter()
        .find(|row| row.key == key)
        .map_or(0, |row| row.level)
}

fn state_default(snapshot: &RoomPermissionsSnapshot) -> i64 {
    default_of(snapshot, PermissionKey::StateDefault)
}

fn events_default(snapshot: &RoomPermissionsSnapshot) -> i64 {
    default_of(snapshot, PermissionKey::EventsDefault)
}

/// Shaped like a full Matrix user ID, "@localpart:server"; the save checks
/// it exactly.
fn matrix_user_id(text: &str) -> bool {
    text.len() <= 255
        && !text.chars().any(|c| c.is_control() || c.is_whitespace())
        && text
            .strip_prefix('@')
            .and_then(|rest| rest.split_once(':'))
            .is_some_and(|(local, server)| !local.is_empty() && !server.is_empty())
}

/// Your level and what changing permissions needs, why nothing can be
/// changed when that is so, and what the lists leave out.
pub(crate) fn summary(snapshot: &RoomPermissionsSnapshot) -> String {
    let mut parts = Vec::new();
    let own = user_level_text(snapshot.own_level, snapshot.users_default);
    parts.push(format!(
        "Your level: {own}. Changing roles and permissions needs level {}.",
        snapshot.change_level
    ));
    if let Some(reason) = snapshot.read_only {
        parts.push(readonly_text(reason).to_owned());
    } else if !snapshot.can_change {
        parts.push(VIEW_ONLY.to_owned());
    }
    if snapshot.omitted_users > 0 {
        let more = snapshot.omitted_users;
        parts.push(format!(
            "{more} more people with their own level are not listed."
        ));
    }
    if snapshot.omitted_events > 0 {
        let more = snapshot.omitted_events;
        parts.push(format!("{more} more event permissions are not listed."));
    }
    parts.join(" ")
}

fn target_title(snapshot: &RoomPermissionsSnapshot, target: &PermissionTarget) -> String {
    match target {
        PermissionTarget::User(user) => {
            let listed = snapshot.users.iter().find(|row| row.user_id == *user);
            let user = crate::directory_plain_text(user, DETAIL_CHARS);
            match listed {
                Some(row) if row.display_name != row.user_id => {
                    let name = crate::directory_plain_text(&row.display_name, TITLE_CHARS);
                    format!("{name} ({user})")
                }
                _ => user,
            }
        }
        PermissionTarget::Threshold(key) => key_label(key),
    }
}

/// One line per change: "Dana (@dana:example.org): Moderator · 50 → Custom · 75".
pub(crate) fn review_lines(
    snapshot: &RoomPermissionsSnapshot,
    review: &PermissionsReview,
) -> Vec<String> {
    let users_default = snapshot.users_default;
    review
        .changes
        .iter()
        .map(|change| {
            let title = target_title(snapshot, &change.target);
            let (before, after) = change_texts(change, users_default);
            format!("{title}: {before} → {after}")
        })
        .collect()
}

fn change_texts(change: &ReviewedChange, users_default: i64) -> (String, String) {
    let user = matches!(change.target, PermissionTarget::User(_));
    let default = PermissionTarget::Threshold(PermissionKey::UsersDefault);
    if change.target == default {
        let before = match change.before {
            PermissionLevel::Level(level) => format!("Level {level}"),
            PermissionLevel::Creator => "unlimited".to_owned(),
        };
        return (before, format!("Level {}", change.after));
    }
    let before = if user {
        user_level_text(change.before, users_default)
    } else {
        match change.before {
            PermissionLevel::Level(level) => threshold_text(level, users_default),
            PermissionLevel::Creator => "unlimited".to_owned(),
        }
    };
    let before = if change.before_explicit {
        before
    } else {
        format!("{before} (default)")
    };
    let after = if user {
        user_level_text(PermissionLevel::Level(change.after), users_default)
    } else {
        threshold_text(change.after, users_default)
    };
    let after = match (change.after_explicit, change.after_if_state) {
        (true, _) => after,
        // A custom event type: its default depends on how it is sent.
        (false, Some(if_state)) => format!(
            "the default: {} as a message, {} as state",
            threshold_text(change.after, users_default),
            threshold_text(if_state, users_default)
        ),
        (false, None) => format!("the default, {after}"),
    };
    (before, after)
}

/// The review's warnings: your own demotion, nobody left to manage
/// permissions, and a changed default level for everyone.
pub(crate) fn review_warning(
    snapshot: &RoomPermissionsSnapshot,
    review: &PermissionsReview,
) -> String {
    let mut parts = Vec::new();
    if let Some((before, after)) = review.own_demotion {
        let before = match before {
            PermissionLevel::Level(level) => level.to_string(),
            PermissionLevel::Creator => "unlimited".to_owned(),
        };
        parts.push(format!(
            "You are lowering your own level from {before} to {after}. You will not be able \
             to raise it again yourself; only someone with a higher level can."
        ));
    }
    if review.no_other_manager {
        parts.push("After this, no one listed here could change roles and permissions.".to_owned());
    }
    let default = PermissionTarget::Threshold(PermissionKey::UsersDefault);
    if let Some(change) = review
        .changes
        .iter()
        .find(|change| change.target == default)
    {
        let before = match change.before {
            PermissionLevel::Level(level) => level,
            PermissionLevel::Creator => snapshot.users_default,
        };
        parts.push(format!(
            "Everyone without a level of their own changes from {before} to {}.",
            change.after
        ));
    }
    parts.join(" ")
}

fn saved_notice(count: usize, shown: &[(PermissionTarget, bool)]) -> String {
    let missing = shown.iter().filter(|(_, shown)| !shown).count();
    let changes = if count == 1 { "change" } else { "changes" };
    if missing == 0 {
        format!("Saved {count} {changes}. The read-back shows them.")
    } else {
        format!(
            "Saved {count} {changes}, but {missing} of them do not show in the permissions read \
             back. Refresh in a moment to see what the room has."
        )
    }
}

/// A failed save in words, whether to require a refresh before the next
/// one, and whether the change was applied (so nothing stays staged).
pub(crate) fn save_failure(error: PermissionsError) -> (&'static str, bool, bool) {
    let settings = match error {
        PermissionsError::Problem(problem) => {
            let refresh = matches!(problem, PermissionProblem::ReadOnly(_));
            return (problem.text(), refresh, false);
        }
        PermissionsError::Settings(error) => error,
    };
    match settings {
        RoomSettingsError::Unknown => (
            "No answer came back. The permissions may or may not have changed, and the change \
             is not sent again. Refresh before another change.",
            true,
            false,
        ),
        RoomSettingsError::SavedNotRead => (
            "Saved, but reading the permissions back failed. Refresh to see them.",
            true,
            true,
        ),
        RoomSettingsError::BaselineChanged => (
            "The permissions changed since they were read. Nothing was sent. Refresh, then \
             review your changes again.",
            true,
            false,
        ),
        RoomSettingsError::SecurityBusy => (SECURITY_BUSY, false, false),
        RoomSettingsError::PermissionDenied => (
            "Your homeserver did not allow this change. Nothing was changed. Refresh to see \
             your current level.",
            true,
            false,
        ),
        RoomSettingsError::Refused => (
            "Your homeserver refused this change. Nothing was changed.",
            true,
            false,
        ),
        RoomSettingsError::RateLimited => (
            "Your homeserver is limiting requests. Nothing was changed. Wait, then refresh.",
            true,
            false,
        ),
        RoomSettingsError::SignInExpired => (
            "This sign-in needs renewing. Your homeserver did not apply the change.",
            true,
            false,
        ),
        other => (error_text(PermissionsError::Settings(other)), true, false),
    }
}

/// A read that did not complete, in words. Server text is never shown.
pub(crate) fn error_text(error: PermissionsError) -> &'static str {
    match error {
        PermissionsError::Problem(problem) => problem.text(),
        PermissionsError::Settings(error) => match error {
            RoomSettingsError::Cancelled => "This view was closed.",
            RoomSettingsError::AccountChanged => "The account changed. Close and open this view.",
            RoomSettingsError::NotJoined => "Join this room to see its permissions.",
            RoomSettingsError::TimedOut => "Reading the permissions timed out. Nothing was sent.",
            RoomSettingsError::RateLimited => "Your homeserver is busy. Try again shortly.",
            RoomSettingsError::SecurityBusy => SECURITY_BUSY,
            RoomSettingsError::InvalidState => {
                "The room returned permissions Morpheus cannot read. Nothing was sent."
            }
            _ => "The room's permissions could not be read. Nothing was sent.",
        },
    }
}

/// A core stand-in for the production-callback regression in
/// `account_access_ui_tests`: its reads and saves wait until released, and a
/// save is "sent" only while the admission the editor gave it allows, as
/// the core's write is at its hand-off.
#[cfg(test)]
pub(crate) struct HeldCore {
    held: tokio::sync::Semaphore,
    /// Saves wait on their own permits, so a held save never takes a
    /// permit meant for a read.
    held_save: tokio::sync::Semaphore,
    snapshot: Mutex<RoomPermissionsSnapshot>,
    admission: Mutex<Option<Admission>>,
    calls: std::sync::atomic::AtomicUsize,
    answered: std::sync::atomic::AtomicUsize,
    pub(crate) sent: Mutex<Vec<PermissionsEdit>>,
    /// The next admitted save gets this answer instead of a shown success.
    pub(crate) answer: Mutex<Option<HeldAnswer>>,
}

/// A save answer the held core gives on request.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeldAnswer {
    /// Handed over, no answer: Unknown.
    Unknown,
    /// Accepted, but the read-back does not show it.
    AcceptedNotShown,
}

#[cfg(test)]
impl HeldCore {
    pub(crate) fn new(snapshot: RoomPermissionsSnapshot) -> Self {
        Self {
            held: tokio::sync::Semaphore::new(0),
            held_save: tokio::sync::Semaphore::new(0),
            snapshot: Mutex::new(snapshot),
            admission: Mutex::new(None),
            calls: std::sync::atomic::AtomicUsize::new(0),
            answered: std::sync::atomic::AtomicUsize::new(0),
            sent: Mutex::new(Vec::new()),
            answer: Mutex::new(None),
        }
    }

    pub(crate) fn set_snapshot(&self, snapshot: RoomPermissionsSnapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
    }

    pub(crate) fn release(&self) {
        self.held.add_permits(1);
    }

    pub(crate) fn release_save(&self) {
        self.held_save.add_permits(1);
    }

    pub(crate) async fn wait_calls(&self, count: usize) {
        while self.calls.load(Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    pub(crate) async fn wait_answered(&self, count: usize) {
        while self.answered.load(Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    async fn held(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(permit) = self.held.acquire().await {
            permit.forget();
        }
    }

    async fn held_save(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(permit) = self.held_save.acquire().await {
            permit.forget();
        }
    }
}

#[cfg(test)]
impl PermissionsCore for HeldCore {
    type Selection = ();
    type Session = ();
    type View = RoomPermissionsSnapshot;

    fn selection(&self, _: &str) -> Option<()> {
        Some(())
    }

    fn current(&self, _: &()) -> bool {
        true
    }

    fn snapshot(view: &RoomPermissionsSnapshot) -> &RoomPermissionsSnapshot {
        view
    }

    fn open<'a>(
        &'a self,
        _: &'a (),
        _: &'a str,
        _: RoomSettingsCancellation,
        admission: Admission,
    ) -> Pending<'a, ()> {
        *self.admission.lock().unwrap() = Some(admission);
        Box::pin(async { Ok(()) })
    }

    fn load<'a>(&'a self, _: &'a ()) -> Pending<'a, RoomPermissionsSnapshot> {
        Box::pin(async move {
            self.held().await;
            let snapshot = self.snapshot.lock().unwrap().clone();
            self.answered.fetch_add(1, Ordering::SeqCst);
            Ok(snapshot)
        })
    }

    fn save<'a>(
        &'a self,
        _: &'a (),
        view: &'a RoomPermissionsSnapshot,
        edit: &'a PermissionsEdit,
    ) -> Pending<'a, Saved<RoomPermissionsSnapshot>> {
        Box::pin(async move {
            self.held_save().await;
            let admitted = self
                .admission
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|admission| admission());
            let result = if admitted {
                self.sent.lock().unwrap().push(edit.clone());
                let answer = self.answer.lock().unwrap().take();
                let shown = edit
                    .changes
                    .iter()
                    .map(|change| (change.target.clone(), answer.is_none()))
                    .collect();
                match answer {
                    Some(HeldAnswer::Unknown) => {
                        Err(PermissionsError::Settings(RoomSettingsError::Unknown))
                    }
                    _ => Ok(Saved {
                        view: view.clone(),
                        shown,
                    }),
                }
            } else {
                Err(PermissionsError::Settings(RoomSettingsError::SecurityBusy))
            };
            self.answered.fetch_add(1, Ordering::SeqCst);
            result
        })
    }
}

#[cfg(test)]
mod tests;
