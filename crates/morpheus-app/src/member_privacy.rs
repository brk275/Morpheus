//! A room member's details and this account's Matrix ignore list
//! (`morpheus_matrix::member_privacy`).
//!
//! The inspector opens from a member row of the open room. It belongs to the
//! account selection and room it was opened for, reads the member from this
//! device's store only, and closes when either changes. Ignore and Stop
//! ignoring first read the server's list; the person confirms the exact
//! Matrix ID against that list, and the core hands the write to the SDK only
//! if a fresh read immediately before it still equals the reviewed list.
//! Settings › Security & recovery › Ignored users lists the same account's
//! ignored users, searchable and bounded, with the same reviewed Stop
//! ignoring.
//!
//! Every action takes a new operation number and keeps its view's id and
//! selection; a result is shown only while the same view, operation, account
//! selection and visible account (and room, for the inspector) are current.
//! A write is sent at most once (no HTTP retries, no replay after a token
//! refresh); an unknown result says so, and the next read shows what the
//! server has. Changes are refused while account access or a security step
//! holds the security gate: the gate's generation is captured when the person
//! confirms, and the core checks it again after its lock and fresh read,
//! immediately before the write (SecurityBusy, nothing sent). Closing the
//! inspector cancels its view and aborts its pending read task; its results
//! are published only to the same open view. Names and IDs are sanitized and bounded; error
//! text is fixed and nothing here is logged.

use std::sync::{Arc, Mutex, MutexGuard, atomic::Ordering};

use morpheus_matrix::{
    AccountSelection, IgnoreBaseline, IgnoreChange, IgnoredUsersSnapshot, MemberDetails,
    MemberPrivacyCancellation, MemberPrivacyError,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::{runtime::Handle, task::AbortHandle};

use crate::{
    AppWindow, IgnoredUserRow, IgnoredUsersView, MemberInspectorView, MemberPrivacy, live::LiveUi,
};

/// Search input is cut to this many characters before matching.
const MAX_QUERY_CHARS: usize = 128;
/// Ignored users listed at once; a search narrows the rest.
const MAX_ROWS: usize = 100;
const MAX_NAME_CHARS: usize = 100;
const MAX_ID_CHARS: usize = 260;

const SECURITY_BUSY: &str = "Finish the account or security step in progress, then try again.";
const UNAVAILABLE: &str = "Sign in to see who this account ignores.";
const LOADING: &str = "Reading this account's ignore list…";

/// Stages of the inspector, as `MemberInspectorView::stage`.
const DETAILS: i32 = 0;
const REVIEW_IGNORE: i32 = 1;
const REVIEW_UNIGNORE: i32 = 2;
const READING: i32 = 3;
const SENDING: i32 = 4;

/// One opened inspector.
struct Inspector {
    id: u64,
    selection: AccountSelection,
    room: String,
    user: String,
    cancellation: MemberPrivacyCancellation,
    details: Option<MemberDetails>,
    /// Ignore (true) or stop ignoring, against the list the person reviews.
    review: Option<(bool, IgnoreBaseline)>,
    op: u64,
    /// The pending details or review read, aborted when the view closes.
    reading: Option<AbortHandle>,
}

impl Inspector {
    /// Ends this view: its cancellation, and its pending read task. A write
    /// already handed to the core is not aborted; the cancellation ends its
    /// wait (Unknown once it may have been sent).
    fn close(self) {
        self.cancellation.cancel();
        if let Some(reading) = self.reading {
            reading.abort();
        }
    }
}

/// One opened ignore list in Settings.
struct Panel {
    id: u64,
    selection: AccountSelection,
    cancellation: MemberPrivacyCancellation,
    snapshot: Option<IgnoredUsersSnapshot>,
    loading: bool,
    busy: bool,
    op: u64,
    /// The exact user ID under review for Stop ignoring.
    review: Option<String>,
    error: String,
    notice: String,
}

#[derive(Default)]
struct State {
    inspector: Option<Inspector>,
    panel: Option<Panel>,
    next: u64,
}

impl State {
    fn next_id(&mut self) -> u64 {
        self.next = self.next.wrapping_add(1);
        self.next
    }
}

pub(crate) struct MemberPrivacyUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    state: Mutex<State>,
}

/// Whether the inspector is open; shortcuts pause while it is.
pub(crate) fn is_open(ui: &AppWindow) -> bool {
    ui.global::<MemberPrivacy>().get_inspector().open
}

/// Account change, logout, detach and shutdown: every member detail and
/// ignore-list row goes now, without waiting for a task.
pub(crate) fn clear_presentation(ui: &AppWindow) {
    let privacy = ui.global::<MemberPrivacy>();
    privacy.set_inspector(MemberInspectorView::default());
    privacy.set_ignored(IgnoredUsersView::default());
    privacy.set_ignored_rows(ModelRc::new(VecModel::<IgnoredUserRow>::default()));
    privacy.set_ignored_query("".into());
    privacy.invoke_reset();
}

/// A display label: control and direction characters replaced, bounded.
fn label(value: &str, max_chars: usize) -> String {
    crate::directory_plain_text(value, max_chars)
}

/// The member's role in the room, as text.
fn role_text(details: &MemberDetails) -> String {
    let power = match details.power_level {
        None => "Room creator".to_owned(),
        Some(level) if level >= 100 => format!("Admin (power {level})"),
        Some(level) if level >= 50 => format!("Moderator (power {level})"),
        Some(level) if level > 0 => format!("Power {level}"),
        Some(_) => "Member".to_owned(),
    };
    match details.membership.as_str() {
        "join" => format!("{power} · joined"),
        "invite" => "Invited".to_owned(),
        "ban" => "Banned from this room".to_owned(),
        "knock" => "Asked to join".to_owned(),
        "leave" => "Left this room".to_owned(),
        _ => "Membership not known".to_owned(),
    }
}

/// The listed rows for `query` (case-insensitive, any part of the ID),
/// at most MAX_ROWS, and how many matched in all.
fn visible_rows(users: &[String], query: &str) -> (Vec<IgnoredUserRow>, usize) {
    let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
    let query = query.trim().to_lowercase();
    let matching: Vec<&String> = users
        .iter()
        .filter(|user| query.is_empty() || user.to_lowercase().contains(&query))
        .collect();
    let total = matching.len();
    let rows = matching
        .into_iter()
        .take(MAX_ROWS)
        .map(|user| IgnoredUserRow {
            key: user.as_str().into(),
            user_id: label(user, MAX_ID_CHARS).into(),
        })
        .collect();
    (rows, total)
}

fn summary_text(snapshot: &IgnoredUsersSnapshot) -> String {
    let listed = match snapshot.users.len() {
        0 => "This account ignores no one.".to_owned(),
        1 => "This account ignores 1 person.".to_owned(),
        count => format!("This account ignores {count} people."),
    };
    match snapshot.unrecognized {
        0 => listed,
        kept => format!("{listed} {kept} other entries are kept but cannot be listed."),
    }
}

fn shown_text(rows: usize, total: usize, all: usize, query: &str) -> String {
    if query.trim().is_empty() && rows == all {
        String::new()
    } else {
        format!("Showing {rows} of {total} matching ({all} ignored)")
    }
}

/// Fixed text for a change that did not complete.
fn change_error(error: MemberPrivacyError) -> String {
    error.to_string()
}

impl MemberPrivacyUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            state: Mutex::new(State::default()),
        });
        let privacy = ui.global::<MemberPrivacy>();
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_inspect(move |user| {
            if let Some(ui) = weak.upgrade() {
                this.inspect(&ui, user.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_close_inspector(move || {
            if let Some(ui) = weak.upgrade() {
                this.close_inspector(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_review(move |ignore| {
            if let Some(ui) = weak.upgrade() {
                this.review(&ui, ignore);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_confirm(move || {
            if let Some(ui) = weak.upgrade() {
                this.confirm(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_cancel_review(move || {
            if let Some(ui) = weak.upgrade() {
                this.cancel_review(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_ignored_scope_changed(move || {
            if let Some(ui) = weak.upgrade() {
                this.panel_scope_changed(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_ignored_refresh(move || {
            if let Some(ui) = weak.upgrade() {
                this.refresh(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_ignored_search(move |_| {
            if let Some(ui) = weak.upgrade() {
                this.paint_panel(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_review_unignore(move |key| {
            if let Some(ui) = weak.upgrade() {
                this.review_unignore(&ui, key.as_str());
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_confirm_unignore(move || {
            if let Some(ui) = weak.upgrade() {
                this.confirm_unignore(&ui);
            }
        });
        let this = owner.clone();
        let weak = ui.as_weak();
        privacy.on_cancel_unignore(move || {
            if let Some(ui) = weak.upgrade() {
                this.cancel_unignore(&ui);
            }
        });
        let this = owner.clone();
        privacy.on_reset(move || this.retire());
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        clear_presentation(ui);
        self.retire();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Cancels and forgets both views.
    fn retire(&self) {
        let mut state = self.lock();
        if let Some(inspector) = state.inspector.take() {
            inspector.close();
        }
        if let Some(panel) = state.panel.take() {
            panel.cancellation.cancel();
        }
    }

    fn live_account(&self, ui: &AppWindow) -> Option<AccountSelection> {
        if !ui.get_live_session() || !self.live.is_live.load(Ordering::Acquire) {
            return None;
        }
        let account = ui.get_active_account_local_id();
        self.live.manager.account_selection(account.as_str())
    }

    fn shows(&self, ui: &AppWindow, selection: &AccountSelection) -> bool {
        selection.is_current()
            && ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && ui.get_active_account_local_id().as_str() == selection.account_id()
    }

    fn shows_inspector(&self, ui: &AppWindow, inspector: &Inspector) -> bool {
        self.shows(ui, &inspector.selection) && ui.get_active_room_id().as_str() == inspector.room
    }

    fn inspect(self: &Arc<Self>, ui: &AppWindow, user: &str) {
        let room = ui.get_active_room_id().to_string();
        let Some(selection) = self.live_account(ui) else {
            return;
        };
        if room.is_empty() {
            return;
        }
        let user = user.trim().to_owned();
        let cancellation = MemberPrivacyCancellation::new();
        let id = {
            let mut state = self.lock();
            if let Some(previous) = state.inspector.take() {
                previous.close();
            }
            let id = state.next_id();
            state.inspector = Some(Inspector {
                id,
                selection: selection.clone(),
                room: room.clone(),
                user: user.clone(),
                cancellation: cancellation.clone(),
                details: None,
                review: None,
                op: 0,
                reading: None,
            });
            id
        };
        ui.global::<MemberPrivacy>()
            .set_inspector(MemberInspectorView {
                open: true,
                loading: true,
                user_id: label(&user, MAX_ID_CHARS).into(),
                ..MemberInspectorView::default()
            });
        let this = self.clone();
        let task = self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let details = manager
                .member_details_selected(&selection, &cancellation, &room, &user)
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_details(&ui, id, details);
            });
        });
        self.keep_reading(id, task.abort_handle());
    }

    /// Records `task` as the inspector's pending read, or aborts it when
    /// that inspector is already gone.
    fn keep_reading(&self, id: u64, task: AbortHandle) {
        let mut state = self.lock();
        match state.inspector.as_mut().filter(|shown| shown.id == id) {
            Some(inspector) => {
                if let Some(previous) = inspector.reading.replace(task) {
                    previous.abort();
                }
            }
            None => task.abort(),
        }
    }

    fn finish_details(
        &self,
        ui: &AppWindow,
        id: u64,
        details: Result<MemberDetails, MemberPrivacyError>,
    ) {
        let mut state = self.lock();
        let Some(inspector) = state.inspector.as_mut().filter(|shown| shown.id == id) else {
            return;
        };
        if !self.shows_inspector(ui, inspector) {
            return;
        }
        let mut view = ui.global::<MemberPrivacy>().get_inspector();
        view.loading = false;
        match details {
            Ok(details) => inspector.details = Some(details),
            Err(MemberPrivacyError::Cancelled | MemberPrivacyError::AccountChanged) => return,
            Err(error) => view.error = change_error(error).into(),
        }
        paint_inspector(&mut view, inspector);
        ui.global::<MemberPrivacy>().set_inspector(view);
    }

    fn close_inspector(&self, ui: &AppWindow) {
        if let Some(inspector) = self.lock().inspector.take() {
            inspector.close();
        }
        ui.global::<MemberPrivacy>()
            .set_inspector(MemberInspectorView::default());
    }

    /// Reads the server's list before asking: the confirmation names the
    /// exact Matrix ID and is checked against that list when sent.
    fn review(self: &Arc<Self>, ui: &AppWindow, ignore: bool) {
        let privacy = ui.global::<MemberPrivacy>();
        let mut view = privacy.get_inspector();
        if self.live.security_gate.blocked() {
            view.error = SECURITY_BUSY.into();
            privacy.set_inspector(view);
            return;
        }
        let started = {
            let mut state = self.lock();
            let Some(inspector) = state.inspector.as_mut() else {
                return;
            };
            let known = inspector.details.as_ref();
            if view.stage != DETAILS || known.is_none_or(|details| details.is_account_user) {
                return;
            }
            inspector.op = inspector.op.wrapping_add(1);
            inspector.review = None;
            (
                inspector.id,
                inspector.op,
                inspector.selection.clone(),
                inspector.cancellation.clone(),
            )
        };
        let (id, op, selection, cancellation) = started;
        view.stage = READING;
        view.error = "".into();
        view.notice = "".into();
        privacy.set_inspector(view);
        let this = self.clone();
        let task = self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let listed = manager
                .ignored_users_selected(&selection, &cancellation)
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_review(&ui, (id, op), ignore, listed);
            });
        });
        self.keep_reading(id, task.abort_handle());
    }

    fn finish_review(
        &self,
        ui: &AppWindow,
        (id, op): (u64, u64),
        ignore: bool,
        listed: Result<IgnoredUsersSnapshot, MemberPrivacyError>,
    ) {
        let mut state = self.lock();
        let Some(inspector) = state.inspector.as_mut().filter(|shown| shown.id == id) else {
            return;
        };
        if inspector.op != op || !self.shows_inspector(ui, inspector) {
            return;
        }
        let privacy = ui.global::<MemberPrivacy>();
        let mut view = privacy.get_inspector();
        view.stage = DETAILS;
        match listed {
            Err(MemberPrivacyError::Cancelled | MemberPrivacyError::AccountChanged) => return,
            Err(error) => view.error = change_error(error).into(),
            Ok(listed) => {
                let ignored = listed.contains(&inspector.user);
                if let Some(details) = inspector.details.as_mut() {
                    details.ignored = Some(ignored);
                }
                if ignored == ignore {
                    // Already as asked: nothing to confirm or send.
                    let notice = if ignored {
                        "The homeserver already lists this person as ignored."
                    } else {
                        "The homeserver does not list this person as ignored."
                    };
                    view.notice = notice.into();
                } else {
                    inspector.review = Some((ignore, listed.baseline));
                    view.stage = if ignore {
                        REVIEW_IGNORE
                    } else {
                        REVIEW_UNIGNORE
                    };
                }
            }
        }
        paint_inspector(&mut view, inspector);
        privacy.set_inspector(view);
    }

    fn cancel_review(&self, ui: &AppWindow) {
        if let Some(inspector) = self.lock().inspector.as_mut() {
            inspector.review = None;
            inspector.op = inspector.op.wrapping_add(1);
        }
        let privacy = ui.global::<MemberPrivacy>();
        let mut view = privacy.get_inspector();
        view.stage = DETAILS;
        privacy.set_inspector(view);
    }

    fn confirm(self: &Arc<Self>, ui: &AppWindow) {
        let privacy = ui.global::<MemberPrivacy>();
        let mut view = privacy.get_inspector();
        // The admission fence the core rechecks right before the write.
        let generation = self.live.security_gate.ringing_generation();
        if !self.live.security_gate.ringing_allowed(generation) {
            view.error = SECURITY_BUSY.into();
            privacy.set_inspector(view);
            return;
        }
        let started = {
            let mut state = self.lock();
            let Some(inspector) = state.inspector.as_mut() else {
                return;
            };
            if !self.shows_inspector(ui, inspector) {
                return;
            }
            let Some((ignore, baseline)) = inspector.review.take() else {
                return;
            };
            inspector.op = inspector.op.wrapping_add(1);
            (
                (inspector.id, inspector.op),
                ignore,
                baseline,
                inspector.selection.clone(),
                inspector.cancellation.clone(),
                inspector.user.clone(),
            )
        };
        let (ids, ignore, baseline, selection, cancellation, user) = started;
        view.stage = SENDING;
        view.error = "".into();
        view.notice = "".into();
        privacy.set_inspector(view);
        let this = self.clone();
        let gate = self.live.security_gate.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let admitted = || gate.ringing_allowed(generation);
            let changed = manager
                .set_user_ignored_selected(
                    &selection,
                    &cancellation,
                    &user,
                    ignore,
                    &baseline,
                    admitted,
                )
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_change(&ui, ids, ignore, changed);
            });
        });
    }

    fn finish_change(
        self: &Arc<Self>,
        ui: &AppWindow,
        (id, op): (u64, u64),
        ignore: bool,
        changed: Result<IgnoreChange, MemberPrivacyError>,
    ) {
        let listed = {
            let mut state = self.lock();
            let Some(inspector) = state.inspector.as_mut().filter(|shown| shown.id == id) else {
                return;
            };
            if inspector.op != op || !self.shows_inspector(ui, inspector) {
                return;
            }
            let privacy = ui.global::<MemberPrivacy>();
            let mut view = privacy.get_inspector();
            view.stage = DETAILS;
            let listed = match changed {
                Err(MemberPrivacyError::Cancelled | MemberPrivacyError::AccountChanged) => {
                    return;
                }
                Err(error) => {
                    view.error = change_error(error).into();
                    None
                }
                Ok(IgnoreChange::Confirmed(listed)) => {
                    let notice = if ignore {
                        "Ignored. Your Matrix apps hide this person once they sync."
                    } else {
                        "No longer ignored."
                    };
                    view.notice = notice.into();
                    Some(listed)
                }
                Ok(IgnoreChange::AlreadyDone(listed)) => {
                    view.notice = "The list already was as asked; nothing was sent.".into();
                    Some(listed)
                }
                Ok(IgnoreChange::Unconfirmed) => {
                    view.notice = "The homeserver accepted the change; reading the list back \
                                   did not show it yet. Refresh to check."
                        .into();
                    None
                }
            };
            if let (Some(listed), Some(details)) = (listed.as_ref(), inspector.details.as_mut()) {
                details.ignored = Some(listed.contains(&inspector.user));
            }
            paint_inspector(&mut view, inspector);
            privacy.set_inspector(view);
            listed.map(|listed| (inspector.selection.clone(), listed))
        };
        // The Settings list of the same account shows the same result.
        if let Some((selection, listed)) = listed {
            self.adopt_snapshot(ui, &selection, listed);
        }
    }

    fn panel_scope_changed(self: &Arc<Self>, ui: &AppWindow) {
        let Some(selection) = self.live_account(ui) else {
            if let Some(panel) = self.lock().panel.take() {
                panel.cancellation.cancel();
            }
            paint_unavailable(ui);
            return;
        };
        {
            let mut state = self.lock();
            let same = state.panel.as_ref().is_some_and(|panel| {
                panel.selection.is_current()
                    && panel.selection.account_id() == selection.account_id()
            });
            if !same {
                if let Some(panel) = state.panel.take() {
                    panel.cancellation.cancel();
                }
                let id = state.next_id();
                state.panel = Some(Panel {
                    id,
                    selection,
                    cancellation: MemberPrivacyCancellation::new(),
                    snapshot: None,
                    loading: false,
                    busy: false,
                    op: 0,
                    review: None,
                    error: String::new(),
                    notice: String::new(),
                });
            }
        }
        self.refresh(ui);
    }

    fn refresh(self: &Arc<Self>, ui: &AppWindow) {
        let started = {
            let mut state = self.lock();
            let Some(panel) = state.panel.as_mut() else {
                return;
            };
            if panel.loading || panel.busy || !self.shows(ui, &panel.selection) {
                return;
            }
            panel.loading = true;
            panel.op = panel.op.wrapping_add(1);
            panel.error.clear();
            (
                (panel.id, panel.op),
                panel.selection.clone(),
                panel.cancellation.clone(),
            )
        };
        self.paint_panel(ui);
        let (ids, selection, cancellation) = started;
        let this = self.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let listed = manager
                .ignored_users_selected(&selection, &cancellation)
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| this.finish_load(&ui, ids, listed));
        });
    }

    fn finish_load(
        &self,
        ui: &AppWindow,
        (id, op): (u64, u64),
        listed: Result<IgnoredUsersSnapshot, MemberPrivacyError>,
    ) {
        {
            let mut state = self.lock();
            let Some(panel) = state.panel.as_mut().filter(|shown| shown.id == id) else {
                return;
            };
            if panel.op != op || !self.shows(ui, &panel.selection) {
                return;
            }
            panel.loading = false;
            match listed {
                Err(MemberPrivacyError::Cancelled | MemberPrivacyError::AccountChanged) => return,
                Err(error) => panel.error = error.to_string(),
                Ok(listed) => {
                    // A review whose user is no longer listed has nothing to
                    // change.
                    if panel
                        .review
                        .as_ref()
                        .is_some_and(|user| !listed.contains(user))
                    {
                        panel.review = None;
                    }
                    panel.snapshot = Some(listed);
                }
            }
        }
        self.paint_panel(ui);
    }

    /// A fresh list read elsewhere for the same account replaces the shown
    /// one (never another account's).
    fn adopt_snapshot(
        &self,
        ui: &AppWindow,
        selection: &AccountSelection,
        listed: IgnoredUsersSnapshot,
    ) {
        {
            let mut state = self.lock();
            let Some(panel) = state.panel.as_mut() else {
                return;
            };
            let same = panel.selection.account_id() == selection.account_id();
            if !same || panel.loading || panel.busy || !self.shows(ui, &panel.selection) {
                return;
            }
            panel.snapshot = Some(listed);
        }
        self.paint_panel(ui);
    }

    fn review_unignore(&self, ui: &AppWindow, key: &str) {
        {
            let mut state = self.lock();
            let Some(panel) = state.panel.as_mut() else {
                return;
            };
            if panel.busy || panel.loading {
                return;
            }
            panel.notice.clear();
            if self.live.security_gate.blocked() {
                panel.error = SECURITY_BUSY.to_owned();
            } else if panel
                .snapshot
                .as_ref()
                .is_some_and(|shown| shown.contains(key))
            {
                panel.error.clear();
                panel.review = Some(key.to_owned());
            } else {
                panel.error = "That person is no longer in this list. Refresh and review it again."
                    .to_owned();
            }
        }
        self.paint_panel(ui);
    }

    fn cancel_unignore(&self, ui: &AppWindow) {
        if let Some(panel) = self.lock().panel.as_mut()
            && !panel.busy
        {
            panel.review = None;
        }
        self.paint_panel(ui);
    }

    fn confirm_unignore(self: &Arc<Self>, ui: &AppWindow) {
        // The admission fence the core rechecks right before the write.
        let generation = self.live.security_gate.ringing_generation();
        let started = {
            let mut state = self.lock();
            let Some(panel) = state.panel.as_mut() else {
                return;
            };
            if panel.busy || panel.loading || !self.shows(ui, &panel.selection) {
                return;
            }
            if !self.live.security_gate.ringing_allowed(generation) {
                panel.error = SECURITY_BUSY.to_owned();
                None
            } else {
                let user = panel.review.clone();
                let baseline = panel.snapshot.as_ref().map(|shown| shown.baseline.clone());
                match (user, baseline) {
                    (Some(user), Some(baseline)) => {
                        panel.busy = true;
                        panel.op = panel.op.wrapping_add(1);
                        panel.error.clear();
                        panel.notice.clear();
                        Some((
                            (panel.id, panel.op),
                            user,
                            baseline,
                            panel.selection.clone(),
                            panel.cancellation.clone(),
                        ))
                    }
                    _ => None,
                }
            }
        };
        self.paint_panel(ui);
        let Some((ids, user, baseline, selection, cancellation)) = started else {
            return;
        };
        let this = self.clone();
        let gate = self.live.security_gate.clone();
        self.runtime.spawn(async move {
            let manager = this.live.manager.clone();
            let admitted = || gate.ringing_allowed(generation);
            let changed = manager
                .set_user_ignored_selected(
                    &selection,
                    &cancellation,
                    &user,
                    false,
                    &baseline,
                    admitted,
                )
                .await;
            let weak = this.ui.clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                this.finish_unignore(&ui, ids, &user, changed);
            });
        });
    }

    fn finish_unignore(
        self: &Arc<Self>,
        ui: &AppWindow,
        (id, op): (u64, u64),
        user: &str,
        changed: Result<IgnoreChange, MemberPrivacyError>,
    ) {
        let reconcile = {
            let mut state = self.lock();
            let Some(panel) = state.panel.as_mut().filter(|shown| shown.id == id) else {
                return;
            };
            if panel.op != op || !self.shows(ui, &panel.selection) {
                return;
            }
            panel.busy = false;
            panel.review = None;
            let shown = label(user, MAX_ID_CHARS);
            match changed {
                Err(MemberPrivacyError::Cancelled | MemberPrivacyError::AccountChanged) => return,
                Ok(IgnoreChange::Confirmed(listed)) => {
                    panel.notice = format!("No longer ignored: {shown}");
                    panel.snapshot = Some(listed);
                    false
                }
                Ok(IgnoreChange::AlreadyDone(listed)) => {
                    panel.notice = format!("{shown} was already not ignored; nothing was sent.");
                    panel.snapshot = Some(listed);
                    false
                }
                Ok(IgnoreChange::Unconfirmed) => {
                    panel.notice = "The homeserver accepted the change; reading the list back \
                                    did not show it yet."
                        .to_owned();
                    true
                }
                // Changed elsewhere, no answer, or refused: the next read
                // shows what the server has. Nothing is sent again.
                Err(error) => {
                    panel.error = error.to_string();
                    matches!(
                        error,
                        MemberPrivacyError::Changed | MemberPrivacyError::Unknown
                    )
                }
            }
        };
        self.paint_panel(ui);
        if reconcile {
            self.refresh(ui);
        }
    }

    fn paint_panel(&self, ui: &AppWindow) {
        let privacy = ui.global::<MemberPrivacy>();
        let state = self.lock();
        let Some(panel) = state.panel.as_ref() else {
            paint_unavailable(ui);
            return;
        };
        let query = privacy.get_ignored_query();
        let (rows, total, all) = match panel.snapshot.as_ref() {
            Some(snapshot) => {
                let (rows, total) = visible_rows(&snapshot.users, query.as_str());
                (rows, total, snapshot.users.len())
            }
            None => (Vec::new(), 0, 0),
        };
        let summary = match (&panel.snapshot, panel.loading) {
            (_, true) => LOADING.to_owned(),
            (Some(snapshot), false) => summary_text(snapshot),
            (None, false) => "Refresh to read this account's ignore list.".to_owned(),
        };
        let shown = match panel.snapshot {
            Some(_) => shown_text(rows.len(), total, all, query.as_str()),
            None => String::new(),
        };
        let stage = match (&panel.review, panel.busy) {
            (Some(_), true) => 2,
            (Some(_), false) => 1,
            (None, _) => 0,
        };
        let review_id = panel
            .review
            .as_deref()
            .map(|user| label(user, MAX_ID_CHARS))
            .unwrap_or_default();
        privacy.set_ignored(IgnoredUsersView {
            available: true,
            loading: panel.loading,
            loaded: panel.snapshot.is_some(),
            busy: panel.busy,
            summary: summary.into(),
            shown: shown.into(),
            error: panel.error.as_str().into(),
            notice: panel.notice.as_str().into(),
            review_id: review_id.into(),
            stage,
        });
        privacy.set_ignored_rows(ModelRc::new(VecModel::from(rows)));
    }
}

fn paint_unavailable(ui: &AppWindow) {
    let privacy = ui.global::<MemberPrivacy>();
    privacy.set_ignored(IgnoredUsersView {
        summary: UNAVAILABLE.into(),
        ..IgnoredUsersView::default()
    });
    privacy.set_ignored_rows(ModelRc::new(VecModel::<IgnoredUserRow>::default()));
}

/// The inspector's text from its details; the stage, error and notice are
/// set by the caller.
fn paint_inspector(view: &mut MemberInspectorView, inspector: &Inspector) {
    view.open = true;
    view.user_id = label(&inspector.user, MAX_ID_CHARS).into();
    if let Some(details) = inspector.details.as_ref() {
        fill_details(view, details);
    }
}

/// Name, role, own-account and ignore state of one member, sanitized and
/// bounded: the room's display name is not a verified identity.
fn fill_details(view: &mut MemberInspectorView, details: &MemberDetails) {
    let fallback = details.user_id.trim_start_matches('@');
    let fallback = fallback.split(':').next().unwrap_or("");
    let name = details
        .display_name
        .as_deref()
        .map(|name| label(name, MAX_NAME_CHARS))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| label(fallback, MAX_NAME_CHARS));
    view.name = name.into();
    view.role = role_text(details).into();
    view.you = details.is_account_user;
    view.ignored = match details.ignored {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(power_level: Option<i64>, membership: &str) -> MemberDetails {
        MemberDetails {
            user_id: "@robin:example.org".into(),
            display_name: Some("Robin".into()),
            membership: membership.into(),
            power_level,
            is_account_user: false,
            ignored: Some(false),
        }
    }

    #[test]
    fn roles_never_overstate_membership_or_power() {
        assert_eq!(
            role_text(&details(Some(100), "join")),
            "Admin (power 100) · joined"
        );
        assert_eq!(
            role_text(&details(Some(50), "join")),
            "Moderator (power 50) · joined"
        );
        assert_eq!(role_text(&details(Some(0), "join")), "Member · joined");
        assert_eq!(role_text(&details(None, "join")), "Room creator · joined");
        // Power never shows for someone who is not in the room.
        assert_eq!(
            role_text(&details(Some(100), "ban")),
            "Banned from this room"
        );
        assert_eq!(role_text(&details(Some(100), "leave")), "Left this room");
        assert_eq!(
            role_text(&details(Some(0), "other")),
            "Membership not known"
        );
    }

    #[test]
    fn the_settings_list_is_searchable_bounded_and_sanitized() {
        let mut users: Vec<String> = (0..250)
            .map(|index| format!("@u{index:03}:example.org"))
            .collect();
        users.push("@robin:example.org".into());
        users.sort();
        let (rows, total) = visible_rows(&users, "");
        assert_eq!((rows.len(), total), (MAX_ROWS, 251));
        let (rows, total) = visible_rows(&users, "  ROBIN ");
        assert_eq!((rows.len(), total), (1, 1));
        assert_eq!(rows[0].key, "@robin:example.org");
        // A huge query is cut before matching and finds nothing.
        let (rows, _) = visible_rows(&users, &"x".repeat(100_000));
        assert!(rows.is_empty());
        // Display text never carries control or direction characters; the
        // key stays the exact ID for routing.
        let hostile = vec!["@a\u{202e}b:example.org".to_owned()];
        let (rows, _) = visible_rows(&hostile, "");
        assert!(!rows[0].user_id.contains('\u{202e}'));
        assert_eq!(rows[0].key, "@a\u{202e}b:example.org");
        assert_eq!(
            shown_text(100, 251, 251, ""),
            "Showing 100 of 251 matching (251 ignored)"
        );
        assert_eq!(shown_text(3, 3, 3, ""), "");
    }

    #[test]
    fn a_hostile_member_name_is_shown_bounded_and_plain() {
        let mut hostile = details(Some(0), "join");
        hostile.display_name = Some(format!("{}\u{202e}evil", "n".repeat(500)));
        hostile.ignored = Some(true);
        let mut view = MemberInspectorView::default();
        fill_details(&mut view, &hostile);
        assert!(view.name.chars().count() <= MAX_NAME_CHARS);
        assert!(!view.name.contains('\u{202e}'));
        assert_eq!(view.ignored, 2);
        // No display name: the ID's localpart. An unreadable ignore list is
        // "not known", never "not ignored".
        let mut unnamed = details(Some(0), "join");
        unnamed.display_name = None;
        unnamed.ignored = None;
        let mut view = MemberInspectorView::default();
        fill_details(&mut view, &unnamed);
        assert_eq!(view.name, "robin");
        assert_eq!(view.ignored, 0);
    }

    /// The fence a confirm passes to the core: captured with the gate open,
    /// it closes once account access or a security step begins, and stays
    /// closed after that step ends (the generation moved), so a write the
    /// core had not yet handed to the SDK is never sent under a newer step.
    /// A new confirm captures a new generation.
    #[test]
    fn a_security_step_begun_after_confirm_closes_the_write_admission() {
        let gate = Arc::new(crate::security::SecurityGate::default());
        let generation = gate.ringing_generation();
        let admitted = || gate.ringing_allowed(generation);
        assert!(admitted());
        let access = gate.try_account_access().unwrap();
        assert!(!admitted());
        drop(access);
        assert!(!gate.blocked());
        assert!(!admitted());
        let renewed = gate.ringing_generation();
        assert!(gate.ringing_allowed(renewed));
        // The core's refusal reads exactly like the app's own.
        assert_eq!(MemberPrivacyError::SecurityBusy.to_string(), SECURITY_BUSY);
    }

    /// At a short window and a large text scale the inspector's details
    /// scroll, while Close, Cancel and the confirmation sit in a fixed row
    /// outside the scroll area, so they are always reachable.
    #[test]
    fn the_inspector_actions_stay_outside_its_scrolling_details() {
        let source = include_str!("../ui/member-privacy.slint");
        let inspector = &source[source.find("export component MemberInspector").unwrap()..];
        let body = inspector.find("body := ScrollView").unwrap();
        let actions = inspector.find("actions := HorizontalLayout").unwrap();
        let first = inspector.find("first := TokenButton").unwrap();
        let second = inspector.find("second := TokenButton").unwrap();
        assert!(body < actions && actions < first && first < second);
        // The scroll area closes before the action row opens.
        let scrolled = &inspector[body..actions];
        let opened = scrolled.matches('{').count();
        assert_eq!(opened, scrolled.matches('}').count());
        assert!(inspector[..actions].contains("vertical-stretch: 1;"));
        assert!(inspector[actions..first].contains("vertical-stretch: 0;"));
        // The card never grows past the window.
        assert!(inspector.contains("height: min(parent.height - 32px,"));
    }
}
