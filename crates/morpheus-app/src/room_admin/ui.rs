//! Slint adapter for native room administration. Every asynchronous result
//! is fenced by the panel generation, the revocable account selection and the
//! exact room on screen; a stale result is dropped, never painted over a newer
//! roster. Writes are sent once; their outcome is shown only after the
//! homeserver answered and the affected state was read back.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_matrix::{
    AccessChange, AccountSelection, AdminAction, AdminMember, AdminMembership, AdminOutcome,
    MemberPower, PERMISSION_KEYS, PermissionKey, PermissionSetting, RoomAdminError,
    RoomAdminSession, RoomAdminSnapshot, RoomSettingsCancellation, RosterWatch,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::{runtime::Handle, sync::Notify};

use super::{Filter, Intent};
use crate::{AppWindow, RosterMember, RosterPermission, live::LiveUi};

// Sync delivers bursts of member events; one read follows each quiet period.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(1500);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const SECURITY_BUSY: &str = "Finish the open account security step before changing room members.";

struct Panel {
    id: u64,
    selection: AccountSelection,
    room: String,
    cancellation: RoomSettingsCancellation,
    wake: Notify,
    state: Mutex<PanelState>,
}

#[derive(Default)]
struct PanelState {
    session: Option<Arc<RoomAdminSession>>,
    snapshot: Option<Arc<RoomAdminSnapshot>>,
    progress: Progress,
    /// A sync change arrived, or a write needs reading back, while busy.
    stale: bool,
    watching: bool,
    last_read: Option<Instant>,
    /// The change waiting for the homeserver.
    pending: Option<Intent>,
    /// The change waiting for the operator's deliberate confirmation.
    confirm: Option<Intent>,
    error: String,
    /// `error` describes a failed read, which a later successful read clears.
    read_failed: bool,
    notice: String,
}

/// One read or write at a time. A completion applies only to the generation
/// that started it, and generation overflow fails closed.
#[derive(Default)]
struct Progress {
    generation: u64,
    busy: bool,
}

impl Progress {
    fn begin(&mut self) -> Option<u64> {
        if self.busy {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        self.busy = true;
        Some(self.generation)
    }
    fn finish(&mut self, generation: u64) -> bool {
        if !self.busy || self.generation != generation {
            return false;
        }
        self.busy = false;
        true
    }
}

impl Panel {
    fn lock(&self) -> MutexGuard<'_, PanelState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn cancel(&self) {
        self.cancellation.cancel();
        self.wake.notify_waiters();
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

/// What the window shows, copied out of the panel lock before any property
/// is set, so a property callback can never re-enter a held lock.
struct View {
    snapshot: Option<Arc<RoomAdminSnapshot>>,
    busy: bool,
    pending: Option<Intent>,
    confirm: Option<Intent>,
    error: String,
    notice: String,
}

pub(crate) struct RoomAdminUi {
    current: Mutex<Option<Arc<Panel>>>,
    next: AtomicU64,
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
}

impl RoomAdminUi {
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
        ui.on_roster_scope_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.scope_changed(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_refresh(move || {
            if let Some(ui) = weak.upgrade() {
                this.refresh_requested(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_search_edited(move |_| {
            if let Some(ui) = weak.upgrade() {
                this.repaint(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_choose_filter(move |index| {
            if let Some(ui) = weak.upgrade() {
                ui.set_roster_filter(index.clamp(0, 4));
                this.repaint(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_select(move |key| {
            if let Some(ui) = weak.upgrade() {
                this.select(&ui, key.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_invite(move || {
            if let Some(ui) = weak.upgrade() {
                this.invite(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_request(move |kind| {
            if let Some(ui) = weak.upgrade() {
                this.request(&ui, kind);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_request_permission(move |index, level| {
            if let Some(ui) = weak.upgrade() {
                this.request_permission(&ui, index, level.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_request_access(move |kind, choice| {
            if let Some(ui) = weak.upgrade() {
                this.request_access(&ui, kind, choice);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_confirm(move || {
            if let Some(ui) = weak.upgrade() {
                this.confirm(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_roster_cancel_confirm(move || {
            if let Some(ui) = weak.upgrade() {
                this.cancel_confirm(&ui);
            }
        });
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        self.close(ui);
    }

    fn current(&self) -> Option<Arc<Panel>> {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn active(&self, panel: &Panel) -> bool {
        let current = self.current().map(|current| current.id);
        scope_active(
            current,
            panel.id,
            panel.selection.is_current(),
            panel.cancellation.is_cancelled(),
            self.live.is_live.load(Ordering::Acquire),
        )
    }

    fn matches_ui(&self, panel: &Panel, ui: &AppWindow) -> bool {
        self.active(panel)
            && panel_matches(
                panel.id,
                panel.selection.account_id(),
                &panel.room,
                ui.get_roster_panel_id().as_str(),
                ui.get_active_account_local_id().as_str(),
                ui.get_active_room_id().as_str(),
            )
    }

    fn current_for(&self, ui: &AppWindow) -> Option<Arc<Panel>> {
        let panel = self.current()?;
        self.matches_ui(&panel, ui).then_some(panel)
    }

    /// The visible account or room changed, or the roster became visible.
    fn scope_changed(self: &Arc<Self>, ui: &AppWindow) {
        if self.current_for(ui).is_some() {
            return;
        }
        self.close(ui);
        self.open(ui);
    }

    fn close(&self, ui: &AppWindow) {
        let previous = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(panel) = previous {
            panel.cancel();
        }
        clear_presentation(ui);
    }

    fn open(self: &Arc<Self>, ui: &AppWindow) {
        let room = ui.get_active_room_id().to_string();
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        if !live || room.is_empty() {
            return;
        }
        let account = ui.get_active_account_local_id();
        let Some(selection) = self.live.manager.account_selection(account.as_str()) else {
            return;
        };
        let Ok(previous) = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
        else {
            return;
        };
        let panel = Arc::new(Panel {
            id: previous + 1,
            selection,
            room,
            cancellation: RoomSettingsCancellation::new(),
            wake: Notify::new(),
            state: Mutex::new(PanelState::default()),
        });
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(panel.clone());
        ui.set_roster_panel_id(panel.id.to_string().into());
        let this = self.clone();
        let watched = panel.clone();
        self.runtime.spawn(async move {
            watched.cancelled().await;
            watched.cancel();
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| this.retire(&ui, &watched));
        });
        self.load(ui, panel);
    }

    /// The panel's account selection was revoked or the panel was closed.
    /// A coalesced A→B→A switch leaves the same scope visible; reopen it under
    /// the new selection instead of reviving the old one.
    fn retire(self: &Arc<Self>, ui: &AppWindow, panel: &Panel) {
        let retired = {
            let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
            let retired = current.as_ref().is_some_and(|open| open.id == panel.id);
            if retired {
                *current = None;
            }
            retired
        };
        if retired {
            clear_presentation(ui);
            self.open(ui);
        }
    }

    fn begin(&self, ui: &AppWindow, panel: &Panel) -> Option<u64> {
        if !self.matches_ui(panel, ui) {
            return None;
        }
        panel.lock().progress.begin()
    }

    fn refresh_requested(self: &Arc<Self>, ui: &AppWindow) {
        match self.current_for(ui) {
            Some(panel) => self.load(ui, panel),
            None => self.scope_changed(ui),
        }
    }

    /// Open the session on first use, otherwise re-read the roles and roster.
    fn load(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        let Some(generation) = self.begin(ui, &panel) else {
            panel.lock().stale = true;
            return;
        };
        self.repaint_panel(ui, &panel);
        let this = self.clone();
        self.runtime.spawn(async move {
            let session = panel.lock().session.clone();
            let result = tokio::select! { biased;
                _ = panel.cancelled() => return,
                result = async {
                    match session {
                        Some(session) => session.refresh().await.map(|snapshot| (session, snapshot)),
                        None => {
                            let opened = this.live.manager.open_room_admin_for(&panel.selection, &panel.room, panel.cancellation.clone()).await?;
                            let session = Arc::new(opened);
                            let snapshot = session.snapshot();
                            Ok((session, snapshot))
                        }
                    }
                } => result,
            };
            this.finish_load(panel, generation, result);
        });
    }

    fn finish_load(
        self: &Arc<Self>,
        panel: Arc<Panel>,
        generation: u64,
        result: Result<(Arc<RoomAdminSession>, Arc<RoomAdminSnapshot>), RoomAdminError>,
    ) {
        // Release the generation even for a retired panel; paint only an active one.
        let active = self.active(&panel);
        let watch = {
            let mut state = panel.lock();
            if !state.progress.finish(generation) || !active {
                return;
            }
            state.last_read = Some(Instant::now());
            match result {
                Ok((session, snapshot)) => {
                    if std::mem::take(&mut state.read_failed) {
                        state.error.clear();
                    }
                    reconcile_confirmation(&mut state, &snapshot);
                    state.snapshot = Some(snapshot);
                    state.session = Some(session.clone());
                    let first = !state.watching;
                    state.watching = true;
                    first.then(|| session.watch())
                }
                Err(error) => {
                    state.error = super::load_error_text(error);
                    state.read_failed = true;
                    None
                }
            }
        };
        if let Some(watch) = watch {
            self.spawn_watch(panel.clone(), watch);
        }
        self.publish(panel, None);
    }

    fn spawn_watch(self: &Arc<Self>, panel: Arc<Panel>, mut watch: RosterWatch) {
        let this = self.clone();
        self.runtime.spawn(async move {
            loop {
                let changed = tokio::select! { biased;
                    _ = panel.cancelled() => return,
                    changed = watch.changed() => changed,
                };
                if !changed {
                    return;
                }
                let since = panel.lock().last_read.map(|at| at.elapsed());
                tokio::select! { biased;
                    _ = panel.cancelled() => return,
                    _ = tokio::time::sleep(settle_delay(since)) => {}
                }
                let owner = this.clone();
                let watched = panel.clone();
                let _ = this.ui.upgrade_in_event_loop(move |ui| {
                    if owner.matches_ui(&watched, &ui) {
                        owner.load(&ui, watched);
                    }
                });
            }
        });
    }

    /// Repaint after an asynchronous result, then run a read that waited.
    /// `written` carries a completed change so its inputs can be reset.
    fn publish(self: &Arc<Self>, panel: Arc<Panel>, written: Option<(Intent, bool)>) {
        let this = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if !this.matches_ui(&panel, &ui) {
                return;
            }
            if let Some((intent, confirmed)) = &written {
                reset_inputs(&ui, intent, *confirmed);
            }
            this.repaint_panel(&ui, &panel);
            let rerun = {
                let mut state = panel.lock();
                !state.progress.busy && std::mem::take(&mut state.stale)
            };
            if rerun {
                this.load(&ui, panel);
            }
        });
    }

    fn repaint(&self, ui: &AppWindow) {
        if let Some(panel) = self.current_for(ui) {
            self.repaint_panel(ui, &panel);
        }
    }

    fn repaint_panel(&self, ui: &AppWindow, panel: &Panel) {
        let view = {
            let state = panel.lock();
            View {
                snapshot: state.snapshot.clone(),
                busy: state.progress.busy,
                pending: state.pending.clone(),
                confirm: state.confirm.clone(),
                error: state.error.clone(),
                notice: state.notice.clone(),
            }
        };
        render(ui, &view);
    }

    fn select(self: &Arc<Self>, ui: &AppWindow, key: &str) {
        let Some(panel) = self.current_for(ui) else {
            clear_selection(ui);
            return;
        };
        let snapshot = {
            let mut state = panel.lock();
            if !state.progress.busy {
                state.error.clear();
                state.notice.clear();
            }
            state.snapshot.clone()
        };
        match snapshot.as_deref().and_then(|shown| shown.member(key)) {
            Some(member) => {
                ui.set_roster_selected_key(key.into());
                ui.set_roster_level_input(level_input(member.power).into());
                ui.set_roster_reason_input("".into());
            }
            None => clear_selection(ui),
        }
        self.repaint_panel(ui, &panel);
    }

    /// Admit the panel only when it is idle, loaded and not confirming.
    fn idle_snapshot(&self, ui: &AppWindow) -> Option<(Arc<Panel>, Arc<RoomAdminSnapshot>)> {
        let panel = self.current_for(ui)?;
        let snapshot = {
            let state = panel.lock();
            if state.progress.busy || state.confirm.is_some() {
                return None;
            }
            state.snapshot.clone()?
        };
        Some((panel, snapshot))
    }

    fn request(self: &Arc<Self>, ui: &AppWindow, kind: i32) {
        let Some((panel, snapshot)) = self.idle_snapshot(ui) else {
            return;
        };
        let key = ui.get_roster_selected_key();
        let Some(member) = snapshot.member(key.as_str()) else {
            clear_selection(ui);
            return;
        };
        let level = ui.get_roster_level_input();
        let reason = ui.get_roster_reason_input();
        match super::member_intent(kind, member, &level, &reason, &snapshot) {
            Ok(intent) => self.review(ui, panel, intent, &snapshot),
            Err(message) => self.reject(ui, &panel, message),
        }
    }

    fn request_permission(self: &Arc<Self>, ui: &AppWindow, index: i32, level: &str) {
        let Some((panel, snapshot)) = self.idle_snapshot(ui) else {
            return;
        };
        match super::permission_intent(index, level, &snapshot) {
            Ok(intent) => self.review(ui, panel, intent, &snapshot),
            Err(message) => self.reject(ui, &panel, message),
        }
    }

    fn request_access(self: &Arc<Self>, ui: &AppWindow, kind: i32, choice: i32) {
        let Some((panel, snapshot)) = self.idle_snapshot(ui) else {
            return;
        };
        match super::access_intent(kind, choice, &snapshot) {
            Ok(intent) => self.review(ui, panel, intent, &snapshot),
            Err(message) => self.reject(ui, &panel, message),
        }
    }

    fn invite(self: &Arc<Self>, ui: &AppWindow) {
        let Some((panel, snapshot)) = self.idle_snapshot(ui) else {
            return;
        };
        match super::invite_intent(ui.get_roster_invite_input().as_str(), &snapshot) {
            Ok(intent) => self.review(ui, panel, intent, &snapshot),
            Err(message) => self.reject(ui, &panel, message),
        }
    }

    fn reject(&self, ui: &AppWindow, panel: &Panel, message: String) {
        {
            let mut state = panel.lock();
            state.error = message;
            state.notice.clear();
        }
        self.repaint_panel(ui, panel);
    }

    /// Destructive, role and permission changes wait for a deliberate
    /// confirmation.
    fn review(
        self: &Arc<Self>,
        ui: &AppWindow,
        panel: Arc<Panel>,
        intent: Intent,
        snapshot: &RoomAdminSnapshot,
    ) {
        if super::confirmation(&intent, snapshot).is_none() {
            self.dispatch(ui, panel, intent);
            return;
        }
        {
            let mut state = panel.lock();
            state.confirm = Some(intent);
            state.error.clear();
            state.notice.clear();
        }
        self.repaint_panel(ui, &panel);
    }

    fn confirm(self: &Arc<Self>, ui: &AppWindow) {
        let Some(panel) = self.current_for(ui) else {
            ui.set_roster_confirm_open(false);
            return;
        };
        let intent = {
            let mut state = panel.lock();
            if state.progress.busy {
                // A read is finishing; the dialog stays open until it can act.
                return;
            }
            state.confirm.take()
        };
        ui.set_roster_confirm_open(false);
        if let Some(intent) = intent {
            self.dispatch(ui, panel, intent);
        }
    }

    fn cancel_confirm(&self, ui: &AppWindow) {
        if let Some(panel) = self.current() {
            panel.lock().confirm = None;
        }
        ui.set_roster_confirm_open(false);
        self.repaint(ui);
    }

    fn dispatch(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>, intent: Intent) {
        if self.live.security_gate.blocked() {
            self.reject(ui, &panel, SECURITY_BUSY.to_owned());
            return;
        }
        let Some(generation) = self.begin(ui, &panel) else {
            return;
        };
        let session = {
            let mut state = panel.lock();
            state.pending = Some(intent.clone());
            state.error.clear();
            state.notice.clear();
            state.session.clone()
        };
        self.repaint_panel(ui, &panel);
        let this = self.clone();
        self.runtime.spawn(async move {
            let request = intent.request();
            let result = tokio::select! { biased;
                _ = panel.cancelled() => return,
                result = async {
                    let session = session.ok_or(RoomAdminError::Unavailable)?;
                    session.apply(request).await
                } => result,
            };
            this.finish_write(panel, generation, intent, result);
        });
    }

    fn finish_write(
        self: &Arc<Self>,
        panel: Arc<Panel>,
        generation: u64,
        intent: Intent,
        result: Result<AdminOutcome, RoomAdminError>,
    ) {
        let active = self.active(&panel);
        let confirmed = {
            let mut state = panel.lock();
            if !state.progress.finish(generation) {
                return;
            }
            state.pending = None;
            if !active {
                return;
            }
            state.read_failed = false;
            match result {
                Ok(outcome) => {
                    state.notice = super::outcome_text(&intent, &outcome);
                    state.last_read = Some(Instant::now());
                    // An unconfirmed result is re-read, never resent.
                    state.stale |= !outcome.confirmed;
                    state.snapshot = Some(outcome.snapshot);
                    Some(outcome.confirmed)
                }
                Err(error) => {
                    state.error = super::error_text(error, &intent, state.snapshot.as_deref());
                    state.stale |= super::reread_after(error);
                    None
                }
            }
        };
        let written = confirmed.map(|confirmed| (intent, confirmed));
        self.publish(panel, written);
    }
}

/// A read replaced the snapshot under an open confirmation. If the reviewed
/// member changed, withdraw the confirmation rather than send a stale choice.
fn reconcile_confirmation(state: &mut PanelState, snapshot: &RoomAdminSnapshot) {
    let changed = state
        .confirm
        .as_ref()
        .filter(|intent| !intent.still_matches(snapshot))
        .map(|intent| intent.name.clone());
    if let Some(name) = changed {
        state.confirm = None;
        state.notice = format!("{name} changed on the server. Review them before trying again.");
    }
}

fn reset_inputs(ui: &AppWindow, intent: &Intent, confirmed: bool) {
    if matches!(
        intent.action,
        AdminAction::SetPermission(..) | AdminAction::SetAccess(_)
    ) {
        return;
    }
    if intent.action == AdminAction::Invite
        && ui.get_roster_invite_input().trim() == intent.user_id
    {
        ui.set_roster_invite_input("".into());
    }
    if ui.get_roster_selected_key().as_str() != intent.user_id {
        return;
    }
    ui.set_roster_reason_input("".into());
    if let (AdminAction::SetPowerLevel(level), true) = (intent.action, confirmed) {
        ui.set_roster_level_input(level.to_string().into());
    }
}

fn render(ui: &AppWindow, view: &View) {
    ui.set_roster_busy(view.busy);
    ui.set_roster_loading(view.busy && view.pending.is_none());
    let pending = view.pending.as_ref().map(super::pending_text);
    ui.set_roster_pending_text(pending.unwrap_or_default().into());
    ui.set_roster_error(view.error.as_str().into());
    ui.set_roster_notice(view.notice.as_str().into());
    let Some(snapshot) = view.snapshot.as_deref() else {
        ui.set_roster_loaded(false);
        ui.set_roster_confirm_open(false);
        return;
    };
    ui.set_roster_loaded(true);
    render_confirmation(ui, view.confirm.as_ref(), snapshot);
    let pending_user = view.pending.as_ref().map(|intent| intent.user_id.as_str());
    let pending_key = pending_permission(view.pending.as_ref());
    let mut permissions = snapshot
        .permissions
        .iter()
        .map(|setting| permission_row(*setting, snapshot, pending_key))
        .collect::<Vec<_>>();
    let pending_change = pending_access(view.pending.as_ref());
    let access = super::access_rows(snapshot).into_iter();
    permissions.extend(access.map(|row| access_row(row, pending_change)));
    ui.set_roster_permissions(ModelRc::new(VecModel::from(permissions)));
    let query = ui.get_roster_query();
    let filter = Filter::from_index(ui.get_roster_filter());
    let (rows, total) = super::visible(snapshot, query.as_str(), filter);
    let shown = rows.len();
    let rows = rows
        .into_iter()
        .map(|member| roster_row(member, snapshot, pending_user))
        .collect::<Vec<_>>();
    ui.set_roster_members(ModelRc::new(VecModel::from(rows)));
    ui.set_roster_summary(super::summary(snapshot.counts).into());
    ui.set_roster_shown_text(super::shown_text(shown, total, snapshot.truncated).into());
    ui.set_roster_own_role(super::own_role_text(snapshot).into());
    ui.set_roster_can_invite(snapshot.own.invite);
    ui.set_roster_own_level(ui_level(snapshot.own_power));
    let thresholds = snapshot.thresholds;
    let default = thresholds.map_or(0, |levels| levels.users_default);
    ui.set_roster_member_level(ui_level(MemberPower::Level(default)));
    let selected = ui.get_roster_selected_key();
    match snapshot.member(selected.as_str()) {
        Some(member) => {
            ui.set_roster_selected(roster_row(member, snapshot, pending_user));
            let thresholds = snapshot.thresholds.as_ref();
            ui.set_roster_selected_status(super::status_text(member, thresholds).into());
            let restriction = super::restriction(member, snapshot);
            ui.set_roster_selected_restriction(restriction.into());
        }
        None if !selected.is_empty() => clear_selection(ui),
        None => {}
    }
}

fn render_confirmation(ui: &AppWindow, intent: Option<&Intent>, snapshot: &RoomAdminSnapshot) {
    let Some(intent) = intent else {
        ui.set_roster_confirm_open(false);
        return;
    };
    let Some(confirmation) = super::confirmation(intent, snapshot) else {
        ui.set_roster_confirm_open(false);
        return;
    };
    ui.set_roster_confirm_title(confirmation.title.into());
    ui.set_roster_confirm_body(confirmation.body.into());
    ui.set_roster_confirm_action(confirmation.action.into());
    ui.set_roster_confirm_destructive(confirmation.destructive);
    ui.set_roster_confirm_open(true);
}

fn roster_row(
    member: &AdminMember,
    snapshot: &RoomAdminSnapshot,
    pending_user: Option<&str>,
) -> RosterMember {
    let name = super::display_name(member);
    let actions = member.actions;
    RosterMember {
        key: member.user_id.as_str().into(),
        user_id: crate::directory_plain_text(&member.user_id, 260).into(),
        initials: crate::live::initials(&name).into(),
        accent: crate::live::accent(&member.user_id),
        membership: match member.membership {
            AdminMembership::Join => 0,
            AdminMembership::Invite => 1,
            AdminMembership::Knock => 2,
            AdminMembership::Ban => 3,
        },
        role: super::role(member.power, snapshot.thresholds.as_ref()).into(),
        detail: super::row_detail(member, snapshot.thresholds.as_ref()).into(),
        level: level_input(member.power).into(),
        is_self: member.is_self,
        can_kick: actions.kick,
        can_ban: actions.ban,
        can_unban: actions.unban,
        can_change_level: actions.change_power,
        can_approve: actions.approve,
        pending: pending_user == Some(member.user_id.as_str()),
        name: name.into(),
    }
}

fn permission_row(
    setting: PermissionSetting,
    snapshot: &RoomAdminSnapshot,
    pending: Option<PermissionKey>,
) -> RosterPermission {
    let index = PERMISSION_KEYS.iter().position(|key| *key == setting.key);
    let key = index.and_then(|index| i32::try_from(index).ok());
    let restriction = if setting.editable {
        String::new()
    } else {
        super::permission_restriction(setting.key, snapshot)
    };
    let thresholds = snapshot.thresholds.as_ref();
    RosterPermission {
        key: key.unwrap_or(-1),
        kind: 0,
        choice: -1,
        label: super::permission_label(setting.key).into(),
        description: super::permission_description(setting.key).into(),
        level: setting.level.to_string().into(),
        summary: super::permission_summary(setting, thresholds).into(),
        editable: setting.editable,
        restriction: restriction.into(),
        pending: pending == Some(setting.key),
    }
}

fn access_row(row: super::AccessRow, pending: Option<AccessChange>) -> RosterPermission {
    let pending = match pending {
        Some(AccessChange::History { .. }) => row.kind == super::ACCESS_HISTORY,
        Some(AccessChange::Guests { .. }) => row.kind == super::ACCESS_GUESTS,
        Some(AccessChange::Encrypt) => row.kind == super::ACCESS_ENCRYPTION,
        None => false,
    };
    RosterPermission {
        key: -1,
        kind: row.kind,
        choice: row.choice,
        label: row.label.into(),
        description: row.description.into(),
        level: "".into(),
        summary: row.summary.into(),
        editable: row.editable,
        restriction: row.restriction.into(),
        pending,
    }
}

fn pending_access(pending: Option<&Intent>) -> Option<AccessChange> {
    match pending?.action {
        AdminAction::SetAccess(change) => Some(change),
        _ => None,
    }
}

fn pending_permission(pending: Option<&Intent>) -> Option<PermissionKey> {
    match pending?.action {
        AdminAction::SetPermission(key, _) => Some(key),
        _ => None,
    }
}

fn level_input(power: MemberPower) -> String {
    match power {
        MemberPower::Creator => String::new(),
        MemberPower::Level(level) => level.to_string(),
    }
}

/// Slint integers are 32-bit; levels outside that range only gate presets.
fn ui_level(power: MemberPower) -> i32 {
    match power {
        MemberPower::Creator => i32::MAX,
        MemberPower::Level(level) => {
            i32::try_from(level).unwrap_or(if level < 0 { i32::MIN } else { i32::MAX })
        }
    }
}

fn clear_selection(ui: &AppWindow) {
    ui.set_roster_selected_key("".into());
    ui.set_roster_selected(RosterMember::default());
    ui.set_roster_selected_status("".into());
    ui.set_roster_selected_restriction("".into());
    ui.set_roster_level_input("".into());
    ui.set_roster_reason_input("".into());
}

/// Remove every roster value from the window, including private member
/// names, inputs and an open confirmation. Used on room, account and
/// session changes; queued results for the old panel are already fenced.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    ui.set_roster_panel_id("".into());
    ui.set_roster_members(ModelRc::new(VecModel::<RosterMember>::default()));
    ui.set_roster_permissions(ModelRc::new(VecModel::<RosterPermission>::default()));
    ui.set_roster_summary("".into());
    ui.set_roster_shown_text("".into());
    ui.set_roster_own_role("".into());
    ui.set_roster_loaded(false);
    ui.set_roster_loading(false);
    ui.set_roster_busy(false);
    ui.set_roster_pending_text("".into());
    ui.set_roster_error("".into());
    ui.set_roster_notice("".into());
    ui.set_roster_can_invite(false);
    ui.set_roster_own_level(0);
    ui.set_roster_member_level(0);
    ui.set_roster_query("".into());
    ui.set_roster_filter(0);
    ui.set_roster_invite_input("".into());
    clear_selection(ui);
    ui.set_roster_confirm_open(false);
    ui.set_roster_confirm_title("".into());
    ui.set_roster_confirm_body("".into());
    ui.set_roster_confirm_action("".into());
    ui.set_roster_confirm_destructive(false);
}

fn settle_delay(since_last_read: Option<Duration>) -> Duration {
    let since = since_last_read.unwrap_or(MIN_REFRESH_INTERVAL);
    let wait = MIN_REFRESH_INTERVAL.saturating_sub(since);
    wait.max(WATCH_DEBOUNCE)
}

fn scope_active(
    current: Option<u64>,
    id: u64,
    selected: bool,
    cancelled: bool,
    live: bool,
) -> bool {
    current == Some(id) && selected && !cancelled && live
}

fn panel_matches(
    id: u64,
    account: &str,
    room: &str,
    shown_id: &str,
    shown_account: &str,
    shown_room: &str,
) -> bool {
    shown_id == id.to_string() && account == shown_account && room == shown_room
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_panels_never_match_a_returning_account_or_room() {
        let room = "!room:example.org";
        assert!(panel_matches(1, "account-a", room, "1", "account-a", room));
        assert!(!panel_matches(1, "account-a", room, "2", "account-a", room));
        assert!(!panel_matches(1, "account-a", room, "1", "account-b", room));
        assert!(!panel_matches(1, "account-a", room, "1", "account-a", "!b"));
        assert!(!panel_matches(1, "account-a", room, "", "account-a", room));
    }

    #[test]
    fn account_aba_closed_panels_and_offline_sessions_deny_owned_work() {
        assert!(scope_active(Some(7), 7, true, false, true));
        assert!(!scope_active(Some(7), 7, false, false, true));
        assert!(!scope_active(Some(7), 7, true, true, true));
        assert!(!scope_active(Some(8), 7, true, false, true));
        assert!(!scope_active(None, 7, true, false, true));
        assert!(!scope_active(Some(7), 7, true, false, false));
    }

    #[test]
    fn operations_serialize_and_stale_completions_cannot_unlock_newer_work() {
        let mut progress = Progress::default();
        let first = progress.begin().unwrap();
        assert!(progress.begin().is_none());
        assert!(progress.finish(first));
        let second = progress.begin().unwrap();
        assert!(!progress.finish(first));
        assert!(progress.busy);
        assert!(progress.finish(second));
        let mut exhausted = Progress {
            generation: u64::MAX,
            busy: false,
        };
        assert!(exhausted.begin().is_none());
        assert!(!exhausted.busy);
    }

    #[test]
    fn sync_bursts_wait_for_a_quiet_period_and_a_minimum_interval() {
        assert_eq!(settle_delay(None), WATCH_DEBOUNCE);
        assert_eq!(settle_delay(Some(Duration::ZERO)), MIN_REFRESH_INTERVAL);
        assert_eq!(settle_delay(Some(Duration::from_secs(60))), WATCH_DEBOUNCE);
    }

    #[test]
    fn slint_levels_saturate_without_wrapping() {
        assert_eq!(ui_level(MemberPower::Creator), i32::MAX);
        assert_eq!(ui_level(MemberPower::Level(50)), 50);
        assert_eq!(ui_level(MemberPower::Level(i64::MAX)), i32::MAX);
        assert_eq!(ui_level(MemberPower::Level(i64::MIN)), i32::MIN);
    }

    #[test]
    fn controller_and_private_worker_state_are_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<RoomAdminUi>();
        check::<Panel>();
    }
}
