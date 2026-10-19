//! Slint adapter for native Space management. Every asynchronous result is
//! fenced by the panel generation, the revocable account selection and the
//! exact Space on screen; a stale result is dropped, never painted over a
//! newer view. Opening reads only. Writes are sent once and reported only
//! after the homeserver answered and the affected state was read back.

use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};

use morpheus_matrix::{
    AccountSelection, CreatedChild, HierarchyPage, MAX_CHILD_NAME_BYTES, MAX_HIERARCHY_PAGES,
    MatrixLinkTarget, RoomAdminError, RoomMembership, RoomSettingsCancellation, SpaceChildLink,
    SpaceNode, SpacePermissions, SpaceSession, SpaceTarget, SpaceWriteOutcome,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::{runtime::Handle, sync::Notify};

use super::{Change, Filter, Intent, Reading, Row};
use crate::{AppWindow, SpaceManagerView, SpaceRow, live::LiveUi};

const SECURITY_BUSY: &str = "Finish the open account security step before changing the Space.";
const JOIN_FAILED: &str = "Could not join. Check the room's access rules, then try again.";
const NOTHING_CHANGED: &str = "Nothing changed. Edit the suggested flag or order key first.";

pub(crate) const REFRESH: i32 = 0;
pub(crate) const LOAD_MORE: i32 = 1;
pub(crate) const FIND: i32 = 2;
pub(crate) const CLEAR_FOUND: i32 = 3;
pub(crate) const ADD: i32 = 4;
pub(crate) const SAVE: i32 = 5;
pub(crate) const MOVE_UP: i32 = 6;
pub(crate) const MOVE_DOWN: i32 = 7;
pub(crate) const REMOVE: i32 = 8;
pub(crate) const JOIN: i32 = 9;
pub(crate) const OPEN_SPACE: i32 = 10;
pub(crate) const STOP: i32 = 11;
pub(crate) const CREATE: i32 = 12;

struct Panel {
    id: u64,
    selection: AccountSelection,
    space: String,
    space_name: String,
    cancellation: RoomSettingsCancellation,
    wake: Notify,
    /// The read generation the operator stopped; 0 for none.
    stop: AtomicU64,
    state: Mutex<PanelState>,
}

#[derive(Default)]
struct PanelState {
    session: Option<Arc<SpaceSession>>,
    permissions: Option<SpacePermissions>,
    nodes: Vec<SpaceNode>,
    rows: Vec<Row>,
    next_batch: Option<String>,
    pages: usize,
    loaded: bool,
    progress: Progress,
    /// The read in flight, if the busy operation is a read.
    reading: Option<Reading>,
    /// A write finished; the hierarchy is read again before anything else.
    stale: bool,
    selected: Option<String>,
    /// The resolved room waiting for the operator's choices.
    target: Option<SpaceTarget>,
    /// The change waiting for the homeserver.
    pending: Option<Intent>,
    /// The change waiting for the operator's deliberate confirmation.
    confirm: Option<Intent>,
    error: String,
    notice: String,
}

impl PanelState {
    fn direct(&self, space: &str) -> Vec<SpaceChildLink> {
        let root = self.nodes.iter().find(|node| node.room_id == space);
        root.map(|node| node.children.clone()).unwrap_or_default()
    }
    fn selected_row(&self) -> Option<&Row> {
        let key = self.selected.as_deref()?;
        self.rows.iter().find(|row| row.key() == key)
    }
    fn can_manage(&self) -> bool {
        self.permissions
            .is_some_and(|permissions| permissions.manage_children)
    }
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
    fn stop(&self, generation: u64) {
        self.stop.store(generation, Ordering::Release);
        self.wake.notify_waiters();
    }
    /// Completes once the operator stops the read with this generation.
    async fn stopped(&self, generation: u64) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.stop.load(Ordering::Acquire) == generation {
                return;
            }
            notified.await;
        }
    }
}

enum Done {
    Loaded {
        session: Arc<SpaceSession>,
        permissions: Option<SpacePermissions>,
        page: HierarchyPage,
        fresh: bool,
    },
    Resolved(SpaceTarget),
    Written(SpaceWriteOutcome),
    Created(CreatedChild),
    Joined,
}

enum Failure {
    Admin(RoomAdminError),
    Join,
    Stopped,
}

impl From<RoomAdminError> for Failure {
    fn from(error: RoomAdminError) -> Self {
        Self::Admin(error)
    }
}

pub(crate) struct SpaceManagerUi {
    current: Mutex<Option<Arc<Panel>>>,
    next: AtomicU64,
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
}

impl SpaceManagerUi {
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
        ui.on_space_manager_scope_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.scope_changed(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_space_manager_action(move |action| {
            if let Some(ui) = weak.upgrade() {
                this.action(&ui, action);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_space_manager_select(move |key| {
            if let Some(ui) = weak.upgrade() {
                this.select(&ui, key.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_space_manager_search_edited(move |_| {
            if let Some(ui) = weak.upgrade() {
                this.repaint(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_space_manager_choose_filter(move |index| {
            if let Some(ui) = weak.upgrade() {
                let mut view = ui.get_space_manager_view();
                view.filter = index.clamp(0, 3);
                ui.set_space_manager_view(view);
                this.repaint(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_space_manager_confirm(move || {
            if let Some(ui) = weak.upgrade() {
                this.confirm(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        ui.on_space_manager_cancel_confirm(move || {
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
                &panel.space,
                ui.get_space_manager_view().panel_id.as_str(),
                ui.get_active_account_local_id().as_str(),
                ui.get_active_space_id().as_str(),
            )
    }

    fn current_for(&self, ui: &AppWindow) -> Option<Arc<Panel>> {
        let panel = self.current()?;
        self.matches_ui(&panel, ui).then_some(panel)
    }

    /// The page opened, or the visible account or Space changed. Reopening
    /// the same Space re-reads it; nothing is written by opening.
    fn scope_changed(self: &Arc<Self>, ui: &AppWindow) {
        if let Some(panel) = self.current_for(ui) {
            self.load(ui, panel, true);
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
        let space = ui.get_active_space_id().to_string();
        let live = ui.get_live_session() && self.live.is_live.load(Ordering::Acquire);
        if !live || space.is_empty() {
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
        let name = crate::directory_plain_text(ui.get_active_space_name().as_str(), 100);
        let space_name = if name.is_empty() {
            "this Space".to_owned()
        } else {
            name
        };
        let panel = Arc::new(Panel {
            id: previous + 1,
            selection,
            space,
            space_name,
            cancellation: RoomSettingsCancellation::new(),
            wake: Notify::new(),
            stop: AtomicU64::new(0),
            state: Mutex::new(PanelState::default()),
        });
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(panel.clone());
        let view = SpaceManagerView {
            panel_id: panel.id.to_string().into(),
            space_name: panel.space_name.as_str().into(),
            ..SpaceManagerView::default()
        };
        ui.set_space_manager_view(view);
        let this = self.clone();
        let watched = panel.clone();
        self.runtime.spawn(async move {
            watched.cancelled().await;
            watched.cancel();
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| this.retire(&ui, &watched));
        });
        self.load(ui, panel, true);
    }

    /// The panel's account selection was revoked. A coalesced A→B→A switch
    /// keeps the same scope visible; reopen it under the new selection.
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
            if ui.get_page() == 4 {
                self.open(ui);
            }
        }
    }

    fn begin(&self, ui: &AppWindow, panel: &Panel) -> Option<u64> {
        if !self.matches_ui(panel, ui) {
            return None;
        }
        panel.lock().progress.begin()
    }

    fn action(self: &Arc<Self>, ui: &AppWindow, action: i32) {
        let Some(panel) = self.current_for(ui) else {
            return;
        };
        match action {
            REFRESH => self.load(ui, panel, true),
            LOAD_MORE => self.load(ui, panel, false),
            FIND => self.resolve(ui, panel),
            CLEAR_FOUND => {
                panel.lock().target = None;
                ui.set_space_manager_target_input("".into());
                self.repaint_panel(ui, &panel);
            }
            ADD => self.review_add(ui, panel),
            SAVE => self.review_save(ui, panel),
            MOVE_UP | MOVE_DOWN => self.review_move(ui, panel, action == MOVE_UP),
            REMOVE => self.review_remove(ui, panel),
            JOIN => self.review_join(ui, panel),
            OPEN_SPACE => self.open_selected_space(ui, &panel),
            STOP => self.stop_read(&panel),
            CREATE => self.review_create(ui, panel),
            _ => {}
        }
    }

    /// Stop the read in flight. Nothing was written, so nothing is left
    /// half-done; the generation still fences the late completion.
    fn stop_read(&self, panel: &Panel) {
        let generation = {
            let state = panel.lock();
            if !state.progress.busy || state.reading.is_none() {
                return;
            }
            state.progress.generation
        };
        panel.stop(generation);
    }

    /// Show a joined sub-Space here so its own links can be managed. This
    /// only navigates: the sidebar selects that Space as if it was picked there.
    fn open_selected_space(&self, ui: &AppWindow, panel: &Panel) {
        let space = {
            let state = panel.lock();
            if state.progress.busy || state.confirm.is_some() {
                return;
            }
            let row = state.selected_row();
            row.and_then(|row| super::openable_space(row, &state.nodes, &panel.space))
        };
        let Some((space_id, name)) = space else {
            return;
        };
        ui.set_active_space_id(space_id.as_str().into());
        ui.set_active_space_name(name.into());
        ui.invoke_select_space(space_id.into());
    }

    /// Read the first page and permissions again, or the next page.
    fn load(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>, fresh: bool) {
        let from = {
            let state = panel.lock();
            if !fresh && (state.next_batch.is_none() || state.pages >= MAX_HIERARCHY_PAGES) {
                return;
            }
            if fresh {
                None
            } else {
                state.next_batch.clone()
            }
        };
        let Some(generation) = self.begin(ui, &panel) else {
            return;
        };
        {
            let mut state = panel.lock();
            state.reading = Some(match fresh {
                true if state.loaded => Reading::Refresh,
                true => Reading::First,
                false => Reading::More,
            });
        }
        self.repaint_panel(ui, &panel);
        let this = self.clone();
        self.runtime.spawn(async move {
            let session = panel.lock().session.clone();
            let result = tokio::select! { biased;
                _ = panel.cancelled() => return,
                _ = panel.stopped(generation) => Err(Failure::Stopped),
                result = async {
                    let session = match session {
                        Some(session) => session,
                        None => Arc::new(this.live.manager.open_space_management_for(&panel.selection, &panel.space, panel.cancellation.clone()).await?),
                    };
                    let permissions = match fresh {
                        true => Some(session.permissions().await?),
                        false => None,
                    };
                    let page = session.hierarchy(from.as_deref()).await?;
                    Ok::<_, Failure>(Done::Loaded { session, permissions, page, fresh })
                } => result,
            };
            this.finish(panel, generation, None, result);
        });
    }

    /// Resolve the entered room. This reads only; nothing is joined.
    fn resolve(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        let input = ui.get_space_manager_target_input().to_string();
        if input.trim().is_empty() {
            return;
        }
        let Some(generation) = self.begin(ui, &panel) else {
            return;
        };
        {
            let mut state = panel.lock();
            state.reading = Some(Reading::Find);
            state.target = None;
            state.error.clear();
            state.notice.clear();
        }
        self.repaint_panel(ui, &panel);
        let this = self.clone();
        self.runtime.spawn(async move {
            let session = panel.lock().session.clone();
            let result = tokio::select! { biased;
                _ = panel.cancelled() => return,
                _ = panel.stopped(generation) => Err(Failure::Stopped),
                result = async {
                    let session = session.ok_or(RoomAdminError::Unavailable)?;
                    let target = session.resolve_target(&input).await?;
                    Ok::<_, Failure>(Done::Resolved(target))
                } => result,
            };
            this.finish(panel, generation, None, result);
        });
    }

    fn idle(&self, panel: &Panel) -> bool {
        let state = panel.lock();
        !state.progress.busy && state.confirm.is_none() && state.loaded
    }

    fn review_add(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        if !self.idle(&panel) {
            return;
        }
        let order = match super::parse_order(ui.get_space_manager_target_order().as_str()) {
            Ok(order) => order,
            Err(message) => return self.reject(ui, &panel, message),
        };
        let target = panel.lock().target.clone();
        let Some(target) = target else {
            return;
        };
        let name = super::target_text(&target);
        let name = name.lines().next().unwrap_or_default().to_owned();
        let change = Change::Link {
            target,
            suggested: ui.get_space_manager_target_suggested(),
            order,
        };
        self.review(ui, panel, change, name);
    }

    fn review_create(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        if !self.idle(&panel) || !panel.lock().can_manage() {
            return;
        }
        let name = ui.get_space_manager_create_name().trim().to_owned();
        let valid = !name.is_empty()
            && name.len() <= MAX_CHILD_NAME_BYTES
            && !name.chars().any(char::is_control);
        if !valid {
            let message = super::error_text(RoomAdminError::InvalidName);
            return self.reject(ui, &panel, message);
        }
        let shown = crate::directory_plain_text(&name, 100);
        let change = Change::Create {
            name,
            members_only: ui.get_space_manager_create_members_only(),
            suggested: ui.get_space_manager_create_suggested(),
        };
        self.review(ui, panel, change, shown);
    }

    fn review_save(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        if !self.idle(&panel) {
            return;
        }
        let Some((row, name)) = self.selected(&panel) else {
            return;
        };
        let order = match super::parse_order(ui.get_space_manager_order().as_str()) {
            Ok(order) => order,
            Err(message) => return self.reject(ui, &panel, message),
        };
        let suggested = ui.get_space_manager_suggested();
        if suggested == row.link.suggested && order == row.link.order {
            return self.reject(ui, &panel, NOTHING_CHANGED.to_owned());
        }
        let change = Change::Update {
            link: row.link,
            suggested,
            order,
        };
        self.review(ui, panel, change, name);
    }

    fn review_move(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>, up: bool) {
        if !self.idle(&panel) {
            return;
        }
        let Some((row, name)) = self.selected(&panel) else {
            return;
        };
        let direct = panel.lock().direct(&panel.space);
        match super::move_change(&direct, &row.link.room_id, up) {
            Ok(change) => self.review(ui, panel, change, name),
            Err(message) => self.reject(ui, &panel, message),
        }
    }

    fn review_remove(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        if !self.idle(&panel) {
            return;
        }
        if let Some((row, name)) = self.selected(&panel) {
            let change = Change::Unlink { link: row.link };
            self.review(ui, panel, change, name);
        }
    }

    fn review_join(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>) {
        if !self.idle(&panel) {
            return;
        }
        let node = {
            let state = panel.lock();
            let row = state.selected_row();
            let node = row
                .and_then(|row| row.node)
                .map(|index| state.nodes[index].clone());
            row.zip(node).map(|(row, node)| (row.clone(), node))
        };
        let Some((row, node)) = node else {
            return;
        };
        if node.membership == Some(RoomMembership::Joined) {
            return;
        }
        let name = super::title(&node.room_id, Some(&node));
        let change = Change::Join {
            room_id: node.room_id,
            via: row.link.via,
            is_space: node.is_space,
        };
        self.review(ui, panel, change, name);
    }

    /// The selected row for a change of this Space's own links, with its title.
    fn selected(&self, panel: &Panel) -> Option<(Row, String)> {
        let state = panel.lock();
        let row = state.selected_row()?;
        if !row.direct() || !row.link.editable || !state.can_manage() {
            return None;
        }
        let node = row.node.map(|index| &state.nodes[index]);
        Some((row.clone(), super::title(&row.link.room_id, node)))
    }

    fn reject(&self, ui: &AppWindow, panel: &Panel, message: String) {
        {
            let mut state = panel.lock();
            state.error = message;
            state.notice.clear();
        }
        self.repaint_panel(ui, panel);
    }

    /// Every change waits for a deliberate confirmation with its effects.
    fn review(&self, ui: &AppWindow, panel: Arc<Panel>, change: Change, name: String) {
        {
            let mut state = panel.lock();
            state.confirm = Some(Intent {
                change,
                name,
                space: panel.space_name.clone(),
            });
            state.error.clear();
            state.notice.clear();
        }
        self.repaint_panel(ui, &panel);
    }

    fn confirm(self: &Arc<Self>, ui: &AppWindow) {
        let Some(panel) = self.current_for(ui) else {
            let mut view = ui.get_space_manager_view();
            view.confirm_open = false;
            ui.set_space_manager_view(view);
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
        if let Some(intent) = intent {
            self.dispatch(ui, panel, intent);
        } else {
            self.repaint_panel(ui, &panel);
        }
    }

    fn cancel_confirm(&self, ui: &AppWindow) {
        if let Some(panel) = self.current() {
            panel.lock().confirm = None;
        }
        let mut view = ui.get_space_manager_view();
        view.confirm_open = false;
        ui.set_space_manager_view(view);
        self.repaint(ui);
    }

    fn dispatch(self: &Arc<Self>, ui: &AppWindow, panel: Arc<Panel>, intent: Intent) {
        if self.live.security_gate.blocked() {
            return self.reject(ui, &panel, SECURITY_BUSY.to_owned());
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
        let weak = self.ui.clone();
        self.runtime.spawn(async move {
            let change = intent.change.clone();
            let name = intent.name.clone();
            let result = tokio::select! { biased;
                _ = panel.cancelled() => return,
                result = async {
                    let session = session.ok_or(RoomAdminError::Unavailable)?;
                    let outcome = match change {
                        Change::Link { target, suggested, order } => session.link(&target, suggested, order.as_deref()).await?,
                        Change::Update { link, suggested, order } => session.update(&link, suggested, order.as_deref()).await?,
                        Change::Unlink { link } => session.unlink(&link).await?,
                        Change::Reorder { plan, .. } => session.reorder(&plan).await?,
                        Change::Create { name: room_name, members_only, suggested } => {
                            let created = session.create_child(&room_name, members_only, suggested).await?;
                            return Ok(Done::Created(created));
                        }
                        Change::Join { room_id, via, is_space } => {
                            // The normal explicit join flow for one room only.
                            let target = MatrixLinkTarget::Room { room_or_alias_id: room_id, event_id: None, via_servers: via };
                            let account = panel.selection.account_id();
                            this.live.join_matrix_target(account, &target, Some(is_space), &name, weak).await.map_err(|_| Failure::Join)?;
                            return Ok(Done::Joined);
                        }
                    };
                    Ok::<_, Failure>(Done::Written(outcome))
                } => result,
            };
            this.finish(panel, generation, Some(intent), result);
        });
    }

    /// Apply one asynchronous result, then repaint and run a waiting read.
    fn finish(
        self: &Arc<Self>,
        panel: Arc<Panel>,
        generation: u64,
        intent: Option<Intent>,
        result: Result<Done, Failure>,
    ) {
        // Release the generation even for a retired panel; paint only an active one.
        let active = self.active(&panel);
        let mut joined = false;
        let succeeded = result.is_ok();
        {
            let mut state = panel.lock();
            if !state.progress.finish(generation) {
                return;
            }
            state.pending = None;
            let reading = state.reading.take();
            if !active {
                return;
            }
            match result {
                Ok(Done::Loaded {
                    session,
                    permissions,
                    page,
                    fresh,
                }) => apply_page(&mut state, &panel.space, session, permissions, page, fresh),
                Ok(Done::Resolved(target)) => state.target = Some(target),
                Ok(Done::Written(outcome)) => {
                    if let Some(intent) = &intent {
                        state.notice = super::outcome_text(intent, outcome);
                        settle(&mut state, intent, outcome);
                    }
                    // Show the authoritative hierarchy after every write.
                    state.stale = true;
                }
                Ok(Done::Created(created)) => {
                    if let Some(intent) = &intent {
                        let (notice, error) = super::created_text(intent, &created);
                        state.notice = notice;
                        state.error = error;
                    }
                    // Show the authoritative hierarchy with the new room.
                    state.stale = true;
                }
                Ok(Done::Joined) => {
                    let name = intent.as_ref().map_or("the room", |i| i.name.as_str());
                    state.notice = format!("Joined {name}.");
                    joined = true;
                }
                Err(Failure::Admin(error)) => {
                    state.error = super::error_text(error);
                    state.stale |= intent.is_some() && super::reread_after(error);
                }
                Err(Failure::Join) => state.error = JOIN_FAILED.to_owned(),
                Err(Failure::Stopped) => {
                    let stopped = reading.map_or("Stopped.", super::stopped_text);
                    state.notice = stopped.to_owned();
                }
            }
        }
        let this = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if joined && ui.get_page() == 4 {
                // The join flow already selected the joined room, which may
                // also have closed this Space's page; show the room.
                ui.set_page(0);
            }
            if !this.matches_ui(&panel, &ui) {
                return;
            }
            if let Some(intent) = &intent
                && succeeded
            {
                reset_inputs(&ui, intent);
            }
            this.repaint_panel(&ui, &panel);
            let rerun = {
                let mut state = panel.lock();
                !state.progress.busy && std::mem::take(&mut state.stale)
            };
            if rerun {
                this.load(&ui, panel, true);
            }
        });
    }

    fn select(&self, ui: &AppWindow, key: &str) {
        let Some(panel) = self.current_for(ui) else {
            return;
        };
        let link = {
            let mut state = panel.lock();
            let link = state
                .rows
                .iter()
                .find(|row| row.key() == key)
                .map(|row| row.link.clone());
            state.selected = link.as_ref().map(|_| key.to_owned());
            if !state.progress.busy {
                state.error.clear();
                state.notice.clear();
            }
            link
        };
        let order = link.as_ref().and_then(|link| link.order.clone());
        ui.set_space_manager_suggested(link.is_some_and(|link| link.suggested));
        ui.set_space_manager_order(order.unwrap_or_default().into());
        self.repaint_panel(ui, &panel);
    }

    fn repaint(&self, ui: &AppWindow) {
        if let Some(panel) = self.current_for(ui) {
            self.repaint_panel(ui, &panel);
        }
    }

    fn repaint_panel(&self, ui: &AppWindow, panel: &Panel) {
        let previous = ui.get_space_manager_view();
        let query = ui.get_space_manager_query();
        let filter = Filter::from_index(previous.filter);
        let (view, rows) = {
            let state = panel.lock();
            render(&state, panel, previous, query.as_str(), filter)
        };
        ui.set_space_manager_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_space_manager_view(view);
    }
}

fn apply_page(
    state: &mut PanelState,
    space: &str,
    session: Arc<SpaceSession>,
    permissions: Option<SpacePermissions>,
    page: HierarchyPage,
    fresh: bool,
) {
    state.session = Some(session);
    if let Some(permissions) = permissions {
        state.permissions = Some(permissions);
    }
    if fresh {
        state.nodes = page.nodes;
        state.pages = 1;
    } else {
        state.nodes.extend(page.nodes);
        state.pages += 1;
    }
    state.next_batch = page
        .next_batch
        .filter(|_| state.pages < MAX_HIERARCHY_PAGES);
    state.rows = super::rows(space, &state.nodes);
    state.loaded = true;
    // Withdraw a confirmation whose reviewed links changed underneath it.
    let direct = state.direct(space);
    let changed = state
        .confirm
        .as_ref()
        .filter(|intent| !intent.still_matches(&direct))
        .map(|intent| intent.name.clone());
    if let Some(name) = changed {
        state.confirm = None;
        state.notice = format!("{name} changed on the server. Review it again.");
    }
    let selected = state.selected.clone();
    if selected.is_some_and(|key| !state.rows.iter().any(|row| row.key() == key)) {
        state.selected = None;
    }
}

/// Keep the found room until it is added; clear it once the link is confirmed.
fn settle(state: &mut PanelState, intent: &Intent, outcome: SpaceWriteOutcome) {
    match intent.change {
        Change::Link { .. } if outcome.confirmed => state.target = None,
        Change::Unlink { .. } if outcome.confirmed => state.selected = None,
        _ => {}
    }
}

/// Clear the inputs a successful change used; a failed one keeps them.
fn reset_inputs(ui: &AppWindow, intent: &Intent) {
    match intent.change {
        Change::Link { .. } => {
            ui.set_space_manager_target_input("".into());
            ui.set_space_manager_target_order("".into());
            ui.set_space_manager_target_suggested(false);
        }
        Change::Create { .. } => {
            ui.set_space_manager_create_name("".into());
            ui.set_space_manager_create_suggested(false);
        }
        _ => {}
    }
}

fn render(
    state: &PanelState,
    panel: &Panel,
    previous: SpaceManagerView,
    query: &str,
    filter: Filter,
) -> (SpaceManagerView, Vec<SpaceRow>) {
    let pending_room = state.pending.as_ref().map(Intent::room_id);
    let (shown, total) = super::visible(&state.rows, &state.nodes, query, filter);
    let rows = shown
        .iter()
        .map(|row| space_row(row, &state.nodes, pending_room))
        .collect::<Vec<_>>();
    let can_manage = state.can_manage();
    let permission = state.permissions.map_or_else(String::new, |permissions| {
        let own = crate::room_admin::power_text(permissions.own_power);
        super::permission_text(can_manage, permissions.required_level, &own)
    });
    let selected = state.selected_row();
    let direct = state.direct(&panel.space);
    let position = selected.and_then(|row| {
        let direct_row = row.direct().then_some(&row.link.room_id)?;
        direct.iter().position(|link| &link.room_id == direct_row)
    });
    let editable = selected.is_some_and(|row| row.direct() && row.link.editable) && can_manage;
    let joinable = selected
        .and_then(|row| row.node)
        .is_some_and(|index| state.nodes[index].membership != Some(RoomMembership::Joined));
    let openable = selected
        .and_then(|row| super::openable_space(row, &state.nodes, &panel.space))
        .is_some();
    let confirmation = state.confirm.as_ref().map(super::confirmation);
    let mut view = SpaceManagerView {
        panel_id: previous.panel_id,
        space_name: panel.space_name.as_str().into(),
        loaded: state.loaded,
        loading_text: state.reading.map_or("", super::reading_text).into(),
        busy: state.progress.busy,
        can_manage,
        has_more: state.next_batch.is_some(),
        filter: previous.filter,
        permission: permission.into(),
        summary: super::summary(&state.rows, rows.len(), total, state.next_batch.is_some()).into(),
        pending_text: state
            .pending
            .as_ref()
            .map(super::pending_text)
            .unwrap_or_default()
            .into(),
        error: state.error.as_str().into(),
        notice: state.notice.as_str().into(),
        selected_key: state.selected.clone().unwrap_or_default().into(),
        can_join: joinable,
        can_open: openable,
        can_edit: editable,
        can_move_up: editable && position.is_some_and(|index| index > 0),
        can_move_down: editable && position.is_some_and(|index| index + 1 < direct.len()),
        target: state
            .target
            .as_ref()
            .map(super::target_text)
            .unwrap_or_default()
            .into(),
        target_ready: state.target.is_some() && can_manage,
        ..SpaceManagerView::default()
    };
    if let Some(row) = selected {
        view.selected = space_row(row, &state.nodes, pending_room);
        let detail = super::selected_detail(row, &state.nodes, can_manage);
        view.selected_detail = detail.into();
    }
    if let Some(confirmation) = confirmation {
        view.confirm_open = true;
        view.confirm_title = confirmation.title.into();
        view.confirm_body = confirmation.body.into();
        view.confirm_action = confirmation.action.into();
        view.confirm_destructive = confirmation.destructive;
    }
    (view, rows)
}

fn space_row(row: &Row, nodes: &[SpaceNode], pending_room: Option<&str>) -> SpaceRow {
    let node = row.node.map(|index| &nodes[index]);
    let name = super::title(&row.link.room_id, node);
    SpaceRow {
        key: row.key().into(),
        room_id: crate::directory_plain_text(&row.link.room_id, 260).into(),
        initials: crate::live::initials(&name).into(),
        accent: crate::live::accent(&row.link.room_id),
        detail: super::row_detail(row, nodes).into(),
        depth: i32::try_from(row.depth).unwrap_or(i32::MAX),
        is_space: node.is_some_and(|node| node.is_space),
        joined: node.is_some_and(|node| node.membership == Some(RoomMembership::Joined)),
        direct: row.direct(),
        suggested: row.link.suggested,
        repeated: row.repeated,
        pending: pending_room == Some(row.link.room_id.as_str()),
        name: name.into(),
    }
}

/// Remove every Space-management value from the window: names, inputs,
/// found rooms and an open confirmation. Used on account and session changes;
/// queued results for the old panel are already fenced.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    ui.set_space_manager_view(SpaceManagerView::default());
    ui.set_space_manager_rows(ModelRc::new(VecModel::<SpaceRow>::default()));
    ui.set_space_manager_query("".into());
    ui.set_space_manager_target_input("".into());
    ui.set_space_manager_target_order("".into());
    ui.set_space_manager_target_suggested(false);
    ui.set_space_manager_order("".into());
    ui.set_space_manager_suggested(false);
    ui.set_space_manager_create_name("".into());
    ui.set_space_manager_create_members_only(true);
    ui.set_space_manager_create_suggested(false);
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
    space: &str,
    shown_id: &str,
    shown_account: &str,
    shown_space: &str,
) -> bool {
    shown_id == id.to_string() && account == shown_account && space == shown_space
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_pages_never_match_a_returning_account_or_space() {
        let space = "!space:example.org";
        assert!(panel_matches(1, "account-a", space, "1", "account-a", space));
        assert!(!panel_matches(1, "account-a", space, "2", "account-a", space));
        assert!(!panel_matches(1, "account-a", space, "1", "account-b", space));
        assert!(!panel_matches(1, "account-a", space, "1", "account-a", "!b"));
        assert!(!panel_matches(1, "account-a", space, "", "account-a", space));
    }

    #[test]
    fn account_aba_closed_pages_and_offline_sessions_deny_owned_work() {
        assert!(scope_active(Some(3), 3, true, false, true));
        assert!(!scope_active(Some(3), 3, false, false, true));
        assert!(!scope_active(Some(3), 3, true, true, true));
        assert!(!scope_active(Some(4), 3, true, false, true));
        assert!(!scope_active(None, 3, true, false, true));
        assert!(!scope_active(Some(3), 3, true, false, false));
    }

    #[test]
    fn operations_serialize_and_stale_completions_cannot_unlock_newer_work() {
        let mut progress = Progress::default();
        let first = progress.begin().unwrap();
        assert!(progress.begin().is_none());
        assert!(progress.finish(first));
        let second = progress.begin().unwrap();
        assert!(!progress.finish(first));
        assert!(progress.finish(second));
        let mut exhausted = Progress {
            generation: u64::MAX,
            busy: false,
        };
        assert!(exhausted.begin().is_none());
    }

    #[test]
    fn controller_and_private_worker_state_are_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<SpaceManagerUi>();
        check::<Panel>();
    }
}
