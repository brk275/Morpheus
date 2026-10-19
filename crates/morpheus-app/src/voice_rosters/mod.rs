//! Who is in a voice room's call, beside its entry in the room list.
//!
//! Presence is read only from the current account's own sync: the pinned
//! MatrixRTC core's passive observer (`PassiveCallObserver`) validates the
//! encrypted room-wide call slot and cross-checks every listed membership
//! against live, decrypted sticky events. Nothing here joins, rings,
//! captures, publishes a membership or contacts a media server, and no other
//! account is synced.
//!
//! Watching is bounded: the open voice room, one expanded roster and the
//! first voice rooms in the shown room list, at most `MAX_WATCHED` rooms and
//! one watcher per room however many Spaces list it. A watcher re-checks on
//! membership changes (coalesced) and on a timer, so expiry and departure are
//! seen. Every result is fenced by the scope generation, the account selection
//! (A -> B -> A old work stays revoked) and the watcher's own id. An account
//! change, lock, sign-out or detach resets synchronously.

mod model;

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_matrix::{AccountSelection, SyncBackend, SyncStatus};
use morpheus_rtc::{InteropMode, ObservedCall, ObserverError, PassiveCallObserver};
#[cfg(test)]
use slint::Model;
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};

use crate::{AppWindow, RoomItem, VoiceParticipant, VoicePresence, VoiceRosters, live::LiveUi};
use model::{MAX_SHOWN, Person, Presence, Unsupported, interop_mode, label, people, view, watched};

/// How often the open room, the room list's visibility and the account
/// are rechecked.
const TICK: Duration = Duration::from_secs(2);
/// A roster is confirmed again at least this often, so expiry is seen.
const REFRESH: Duration = Duration::from_secs(30);
/// Membership changes arriving together cause one check.
const COALESCE: Duration = Duration::from_millis(750);
/// The core's roster may briefly lag the sticky cache; check again soon.
const CONVERGE_RETRY: Duration = Duration::from_secs(1);
const MAX_CONVERGE_RETRIES: u32 = 5;
const OPEN_LIMIT: Duration = Duration::from_secs(20);
const NAME_LIMIT: Duration = Duration::from_millis(500);
/// An expanded roster is watched this long after it was opened.
const EXPANDED_LIFETIME: Duration = Duration::from_secs(120);

thread_local! {
    // UI thread only: the controller the room list reports to.
    static CONTROLLER: RefCell<Option<Arc<VoiceRosterUi>>> = const { RefCell::new(None) };
}

/// The room list shows these rooms, in display order. Called once per
/// room-list projection; only the first voice rooms are kept and watched.
/// Cheap: no allocation beyond the kept IDs, no model access.
pub(crate) fn listed<'a>(ui: &AppWindow, rooms: impl IntoIterator<Item = &'a RoomItem>) {
    let controller = CONTROLLER.with(|controller| controller.borrow().clone());
    if let Some(controller) = controller {
        let selection = controller
            .live
            .account_selection(ui.get_active_account_local_id().as_str());
        let listed = if selection
            .as_ref()
            .is_some_and(|selection| selection.is_current())
        {
            model::listed(
                rooms
                    .into_iter()
                    .map(|room| (room.id.as_str(), room.voice_space)),
            )
        } else {
            Vec::new()
        };
        let changed = {
            let mut state = controller.state();
            let changed = state.listed != listed
                || state
                    .listed_selection
                    .as_ref()
                    .is_none_or(|selection| !selection.is_current());
            state.listed = listed;
            state.listed_selection = selection;
            changed
        };
        if changed {
            controller.reconcile(ui);
        }
    }
}

/// Drop every roster and watcher now, on the UI thread (account change,
/// lock, sign-out or detach).
pub(crate) fn clear(ui: &AppWindow) {
    let controller = CONTROLLER.with(|controller| controller.borrow().clone());
    if let Some(controller) = controller {
        controller.reset(ui);
    }
}

/// Permanent stop for one watcher; wakes every waiter.
#[derive(Default)]
struct Stop {
    stopped: AtomicBool,
    wake: Notify,
}
impl Stop {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }
    async fn wait(&self) {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            wake.await;
        }
    }
}

struct Watcher {
    id: u64,
    stop: Arc<Stop>,
    task: JoinHandle<()>,
    presence: Presence,
    started: Instant,
    mode: morpheus_config::RoomCallMode,
    sticky: bool,
}
impl Watcher {
    fn restart_due(&self, mode: morpheus_config::RoomCallMode, sticky: bool, now: Instant) -> bool {
        if self.mode != mode || self.sticky != sticky {
            return true;
        }
        let wait = match self.presence {
            Presence::Unsupported(Unsupported::Dialect) => return false,
            Presence::Unsupported(_) if !sticky => return false,
            Presence::Unsupported(_) => Duration::from_secs(300),
            _ => REFRESH,
        };
        self.task.is_finished() && now.saturating_duration_since(self.started) >= wait
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.stop();
        // Dropping the task drops its observer, which detaches the core's
        // room feed. No request is left running for a released room.
        self.task.abort();
    }
}

/// What the watchers belong to: one account selection.
#[derive(Clone)]
struct Scope {
    selection: AccountSelection,
    generation: u64,
}

/// What a watcher's result must still match to be stored: the scope
/// generation, the watcher and its room.
struct Fence {
    generation: u64,
    id: u64,
    room: String,
}

#[derive(Default)]
struct State {
    scope: Option<Scope>,
    generation: u64,
    next_watcher: u64,
    listed: Vec<String>,
    listed_selection: Option<AccountSelection>,
    list_shown: bool,
    active: Option<String>,
    expanded: Option<(String, Instant)>,
    // One watcher per room: a room listed in several Spaces shares it.
    watchers: HashMap<String, Watcher>,
}

pub(crate) struct VoiceRosterUi {
    live: Arc<LiveUi>,
    runtime: Handle,
    ui: slint::Weak<AppWindow>,
    state: Mutex<State>,
    publishing: AtomicBool,
    tick: Mutex<Option<JoinHandle<()>>>,
}

impl VoiceRosterUi {
    pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<Self> {
        let owner = Arc::new(Self {
            live,
            runtime: runtime.clone(),
            ui: ui.as_weak(),
            state: Mutex::new(State::default()),
            publishing: AtomicBool::new(false),
            tick: Mutex::new(None),
        });
        CONTROLLER.with(|controller| *controller.borrow_mut() = Some(owner.clone()));
        let rosters = ui.global::<VoiceRosters>();
        let (this, weak) = (owner.clone(), ui.as_weak());
        rosters.on_presence(move |room, _| {
            weak.upgrade().map_or_else(VoicePresence::default, |ui| {
                this.presence(&ui, room.as_str())
            })
        });
        let (this, weak) = (owner.clone(), ui.as_weak());
        rosters.on_expand(move |room| {
            if let Some(ui) = weak.upgrade() {
                this.expand(&ui, room.as_str());
            }
        });
        let (this, weak) = (owner.clone(), ui.as_weak());
        rosters.on_collapse(move || {
            if let Some(ui) = weak.upgrade() {
                this.collapse(&ui);
            }
        });
        let (this, weak) = (owner.clone(), ui.as_weak());
        rosters.on_join(move |room| {
            if let Some(ui) = weak.upgrade() {
                this.join(&ui, room.as_str());
            }
        });
        let (this, weak) = (owner.clone(), ui.as_weak());
        rosters.on_reset(move || {
            if let Some(ui) = weak.upgrade() {
                this.reset(&ui);
            }
        });
        let this = owner.clone();
        let task = runtime.spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let next = this.clone();
                if this
                    .ui
                    .upgrade_in_event_loop(move |ui| next.reconcile(&ui))
                    .is_err()
                {
                    return;
                }
            }
        });
        *owner.tick.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        owner
    }

    pub(crate) fn shutdown(&self, ui: &AppWindow) {
        if let Some(task) = self.tick.lock().unwrap_or_else(|e| e.into_inner()).take() {
            task.abort();
        }
        self.reset(ui);
        CONTROLLER.with(|controller| controller.borrow_mut().take());
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Synchronous: no watcher, roster or open list survives, and results
    /// of the old generation are dropped when they arrive.
    fn reset(&self, ui: &AppWindow) {
        let released = {
            let mut state = self.state();
            state.scope = None;
            state.generation = state.generation.wrapping_add(1);
            state.expanded = None;
            state.active = None;
            state.listed.clear();
            state.listed_selection = None;
            state.list_shown = false;
            std::mem::take(&mut state.watchers)
        };
        drop(released);
        let rosters = ui.global::<VoiceRosters>();
        rosters.set_context(rosters.get_context().wrapping_add(1));
        rosters.set_expanded_room("".into());
        rosters.set_participants(ModelRc::new(VecModel::<VoiceParticipant>::default()));
        rosters.set_unnamed(0);
        rosters.set_revision(rosters.get_revision().wrapping_add(1));
    }

    /// Bring the watchers in line with what is shown now.
    fn reconcile(self: &Arc<Self>, ui: &AppWindow) {
        let live = ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && !ui.get_access_open()
            && !ui.get_account_switch_pending();
        let account = ui.get_active_account_local_id().to_string();
        let scope_ok = {
            let state = self.state();
            state.scope.as_ref().is_some_and(|scope| {
                scope.selection.is_current() && scope.selection.account_id() == account
            })
        };
        if !live || account.is_empty() {
            if self.state().scope.is_some() {
                self.reset(ui);
            }
            return;
        }
        if !scope_ok {
            // A projection may already have delivered the new account's rows.
            // Preserve only that exact current selection, never the old list.
            let pending_list = {
                let state = self.state();
                state
                    .listed_selection
                    .as_ref()
                    .filter(|selection| selection.is_current() && selection.account_id() == account)
                    .map(|selection| (selection.clone(), state.listed.clone()))
            };
            self.reset(ui);
            let Some(selection) = self.live.manager.account_selection(&account) else {
                return;
            };
            let mut state = self.state();
            let generation = state.generation;
            state.scope = Some(Scope {
                selection,
                generation,
            });
            if let Some((selection, rooms)) = pending_list {
                state.listed_selection = Some(selection);
                state.listed = rooms;
            }
        }
        let room = ui.get_active_room_id().to_string();
        let voice = ui.get_active_voice_local() || ui.get_active_voice_shared();
        let list_shown = !ui.get_pane_rooms_hidden()
            && !ui.get_pane_rooms_collapsed()
            && ui.get_pane_rooms_w() > crate::workspace_layout::STRIP;
        let sticky = matches!(
            *self.live.manager.subscribe_sync_status().borrow(),
            SyncStatus::Running {
                backend: SyncBackend::SlidingSticky
            }
        );
        let (start, released, changed) = {
            let mut state = self.state();
            state.active = (voice && !room.is_empty()).then_some(room);
            state.list_shown = list_shown;
            if !state.listed_selection.as_ref().is_some_and(|selection| {
                selection.is_current() && selection.account_id() == account
            }) {
                state.listed.clear();
                state.listed_selection = None;
            }
            if state
                .expanded
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() > EXPANDED_LIFETIME)
            {
                state.expanded = None;
            }
            let expanded = state.expanded.as_ref().map(|(room, _)| room.as_str());
            let wanted = watched(
                state.active.as_deref(),
                expanded,
                &state.listed,
                state.list_shown,
            );
            let released: Vec<Watcher> = {
                let gone: Vec<String> = state
                    .watchers
                    .keys()
                    .filter(|room| {
                        let watcher = &state.watchers[*room];
                        !wanted.contains(room)
                            || watcher.restart_due(
                                self.live.voice_spaces.room_call_mode(&account, room),
                                sticky,
                                Instant::now(),
                            )
                    })
                    .cloned()
                    .collect();
                gone.iter()
                    .filter_map(|room| state.watchers.remove(room))
                    .collect()
            };
            let start: Vec<String> = wanted
                .into_iter()
                .filter(|room| !state.watchers.contains_key(room))
                .collect();
            let changed = !released.is_empty() || !start.is_empty();
            (start, released, changed)
        };
        drop(released);
        if self.state().expanded.is_none()
            && !ui.global::<VoiceRosters>().get_expanded_room().is_empty()
        {
            let rosters = ui.global::<VoiceRosters>();
            rosters.set_expanded_room("".into());
            rosters.set_participants(ModelRc::new(VecModel::<VoiceParticipant>::default()));
            rosters.set_unnamed(0);
        }
        let Some(scope) = self.state().scope.clone() else {
            return;
        };
        for room in start {
            self.start(&account, scope.clone(), room);
        }
        if changed {
            self.publish_now(ui);
        }
    }

    fn start(self: &Arc<Self>, account: &str, scope: Scope, room: String) {
        let mode = self.live.voice_spaces.room_call_mode(account, &room);
        let stop = Arc::new(Stop::default());
        let mut state = self.state();
        state.next_watcher = state.next_watcher.wrapping_add(1);
        let id = state.next_watcher;
        let task = self.runtime.spawn(self.clone().watch(
            scope,
            id,
            room.clone(),
            interop_mode(mode),
            stop.clone(),
        ));
        state.watchers.insert(
            room,
            Watcher {
                id,
                stop,
                task,
                presence: Presence::Loading,
                started: Instant::now(),
                mode,
                sticky: matches!(
                    *self.live.manager.subscribe_sync_status().borrow(),
                    SyncStatus::Running {
                        backend: SyncBackend::SlidingSticky
                    }
                ),
            },
        );
    }

    /// One room's roster until it is released, its account changes, or it
    /// becomes unsupported or unavailable.
    async fn watch(
        self: Arc<Self>,
        scope: Scope,
        id: u64,
        room: String,
        mode: Option<InteropMode>,
        stop: Arc<Stop>,
    ) {
        let selection = scope.selection.clone();
        let fence = Fence {
            generation: scope.generation,
            id,
            room: room.clone(),
        };
        let Some(mode) = mode else {
            self.record(
                &fence,
                &selection,
                Presence::Unsupported(Unsupported::Dialect),
            );
            return;
        };
        let manager = self.live.manager.clone();
        let sticky = matches!(
            *manager.subscribe_sync_status().borrow(),
            SyncStatus::Running {
                backend: SyncBackend::SlidingSticky
            }
        );
        if !sticky {
            self.record(
                &fence,
                &selection,
                Presence::Unsupported(Unsupported::Server),
            );
            return;
        }
        let opened = async {
            let client = manager.active_client().await.map_err(|_| None)?;
            if !selection.is_current() {
                return Err(None);
            }
            let observer = PassiveCallObserver::open(client.clone(), &room, mode).await;
            observer.map(|observer| (client, observer)).map_err(Some)
        };
        let opened = tokio::select! {
            biased;
            _ = stop.wait() => return,
            _ = selection.revoked() => return,
            opened = tokio::time::timeout(OPEN_LIMIT, opened) => opened,
        };
        let (client, mut observer) = match opened {
            Ok(Ok(opened)) => opened,
            Ok(Err(Some(error))) => {
                self.record(&fence, &selection, opening_failed(error));
                return;
            }
            Ok(Err(None)) | Err(_) => {
                self.record(&fence, &selection, Presence::Unavailable);
                return;
            }
        };
        let matrix_room = matrix_sdk::ruma::RoomId::parse(&room)
            .ok()
            .and_then(|id| client.get_room(&id));
        let mut names: HashMap<String, Option<String>> = HashMap::new();
        let mut converging = 0;
        loop {
            let snapshot = tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = selection.revoked() => return,
                snapshot = observer.snapshot() => snapshot,
            };
            let mut retry_soon = false;
            match snapshot {
                Ok(call) => {
                    let confirmed_at = Instant::now();
                    converging = 0;
                    let people = tokio::select! {
                        biased;
                        _ = stop.wait() => return,
                        people = named(matrix_room.as_ref(), &call, &mut names) => people,
                    };
                    let live = Presence::Live {
                        people,
                        at: confirmed_at,
                        refresh_failed: false,
                    };
                    self.record(&fence, &selection, live);
                }
                Err(ObserverError::RosterNotCurrent) if converging < MAX_CONVERGE_RETRIES => {
                    converging += 1;
                    retry_soon = true;
                }
                Err(ObserverError::SlotUnavailable) => {
                    self.record(&fence, &selection, Presence::NoCall);
                }
                Err(ObserverError::NotJoined) => {
                    self.record(&fence, &selection, Presence::Unavailable);
                    return;
                }
                Err(error) if unsupported(error) => {
                    self.record(&fence, &selection, opening_failed(error));
                    return;
                }
                Err(_) => self.refresh_failed(&fence, &selection),
            }
            let wait = if retry_soon { CONVERGE_RETRY } else { REFRESH };
            tokio::select! {
                biased;
                _ = stop.wait() => return,
                _ = selection.revoked() => return,
                changed = observer.changed() => {
                    if changed.is_err() {
                        self.refresh_failed(&fence, &selection);
                        return;
                    }
                    // Changes arriving together cause one check.
                    tokio::time::sleep(COALESCE).await;
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Store a watcher's result only if the same scope, selection and
    /// watcher still own that room.
    fn record(self: &Arc<Self>, fence: &Fence, selection: &AccountSelection, next: Presence) {
        {
            let mut state = self.state();
            let generation = state.scope.as_ref().map(|scope| scope.generation);
            let watcher = state.watchers.get(&fence.room).map(|watcher| watcher.id);
            if !owns(generation, selection.is_current(), watcher, fence) {
                return;
            }
            let Some(watcher) = state.watchers.get_mut(&fence.room) else {
                return;
            };
            if watcher.presence == next {
                return;
            }
            watcher.presence = next;
        }
        self.schedule_publish();
    }

    /// A later check failed: keep the last people, shown as stale.
    fn refresh_failed(self: &Arc<Self>, fence: &Fence, selection: &AccountSelection) {
        let current = {
            let state = self.state();
            let watcher = state.watchers.get(&fence.room);
            watcher.map(|watcher| watcher.presence.clone())
        };
        let next = match current {
            Some(Presence::Live { people, at, .. }) => Presence::Live {
                people,
                at,
                refresh_failed: true,
            },
            // Nothing confirmed yet: never shown as known.
            Some(Presence::Loading) => Presence::Unavailable,
            Some(other) => other,
            None => return,
        };
        self.record(fence, selection, next);
    }

    fn schedule_publish(self: &Arc<Self>) {
        if self.publishing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        let queued = self.ui.upgrade_in_event_loop(move |ui| {
            this.publishing.store(false, Ordering::Release);
            this.publish_now(&ui);
        });
        if queued.is_err() {
            self.publishing.store(false, Ordering::Release);
        }
    }

    fn context_current(&self, ui: &AppWindow) -> bool {
        ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && !ui.get_access_open()
            && !ui.get_account_switch_pending()
            && self.state().scope.as_ref().is_some_and(|scope| {
                scope.selection.is_current()
                    && scope.selection.account_id() == ui.get_active_account_local_id().as_str()
            })
    }

    /// Rows re-read presence; an open roster gets its people.
    fn publish_now(&self, ui: &AppWindow) {
        if !self.context_current(ui) {
            self.reset(ui);
            return;
        }
        let rosters = ui.global::<VoiceRosters>();
        let expanded = rosters.get_expanded_room().to_string();
        if !expanded.is_empty() {
            let people = {
                let state = self.state();
                let watcher = state.watchers.get(&expanded);
                match watcher.map(|watcher| &watcher.presence) {
                    Some(Presence::Live { people, .. }) => people.clone(),
                    _ => Vec::new(),
                }
            };
            let unnamed = people.len().saturating_sub(MAX_SHOWN);
            let rows: Vec<VoiceParticipant> =
                people.iter().take(MAX_SHOWN).map(participant).collect();
            rosters.set_participants(ModelRc::new(VecModel::from(rows)));
            rosters.set_unnamed(i32::try_from(unnamed).unwrap_or(i32::MAX));
        }
        rosters.set_revision(rosters.get_revision().wrapping_add(1));
    }

    /// What a room entry shows. UI thread; a lookup only.
    fn presence(&self, ui: &AppWindow, room: &str) -> VoicePresence {
        if !self.context_current(ui) {
            return VoicePresence::default();
        }
        let state = self.state();
        let Some(watcher) = state.watchers.get(room) else {
            return VoicePresence::default();
        };
        let shown = view(&watcher.presence, Instant::now());
        VoicePresence {
            state: shown.state,
            count: i32::try_from(shown.count).unwrap_or(i32::MAX),
            label: shown.label.into(),
            preview: shown.preview.into(),
            description: shown.description.into(),
        }
    }

    fn expand(self: &Arc<Self>, ui: &AppWindow, room: &str) {
        if room.is_empty() || !ui.get_live_session() {
            return;
        }
        self.state().expanded = Some((room.to_owned(), Instant::now()));
        let rosters = ui.global::<VoiceRosters>();
        rosters.set_expanded_room(room.into());
        rosters.set_participants(ModelRc::new(VecModel::<VoiceParticipant>::default()));
        rosters.set_unnamed(0);
        self.reconcile(ui);
        self.publish_now(ui);
    }

    fn collapse(self: &Arc<Self>, ui: &AppWindow) {
        self.state().expanded = None;
        let rosters = ui.global::<VoiceRosters>();
        rosters.set_expanded_room("".into());
        rosters.set_participants(ModelRc::new(VecModel::<VoiceParticipant>::default()));
        rosters.set_unnamed(0);
        self.reconcile(ui);
    }

    /// Quiet Join: only for a room this scope watches, still listed or open,
    /// for the same live account selection, and not known to be unsupported.
    /// The existing voice-space start then checks the call gate (a call
    /// elsewhere must be left first), the service and the room again, and
    /// joins the room's call slot without ringing anyone.
    fn join(self: &Arc<Self>, ui: &AppWindow, room: &str) {
        let live = ui.get_live_session()
            && self.live.is_live.load(Ordering::Acquire)
            && !ui.get_access_open()
            && !ui.get_account_switch_pending();
        let account = ui.get_active_account_local_id().to_string();
        let allowed = {
            let state = self.state();
            let scope = state.scope.as_ref().is_some_and(|scope| {
                scope.selection.is_current() && scope.selection.account_id() == account
            });
            let shown = state.listed.iter().any(|listed| listed == room)
                || state.active.as_deref() == Some(room);
            let usable = state.watchers.get(room).is_some_and(|watcher| {
                !matches!(
                    watcher.presence,
                    Presence::Unsupported(_) | Presence::Unavailable
                )
            });
            scope && shown && usable
        };
        if !live || account.is_empty() || !allowed {
            return;
        }
        self.collapse(ui);
        ui.invoke_open_voice_space(room.into());
    }
}

/// A result belongs to the current scope generation, a still-current
/// account selection (A -> B -> A old work stays revoked) and the watcher
/// that now owns the room (a released and restarted room has a new one).
fn owns(
    generation: Option<u64>,
    selection_current: bool,
    watcher: Option<u64>,
    fence: &Fence,
) -> bool {
    generation == Some(fence.generation) && selection_current && watcher == Some(fence.id)
}

/// The room or homeserver cannot carry a passive roster at all.
fn unsupported(error: ObserverError) -> bool {
    matches!(
        error,
        ObserverError::NotEncrypted
            | ObserverError::UnsupportedServer
            | ObserverError::UnsupportedDialect
    )
}

fn opening_failed(error: ObserverError) -> Presence {
    match error {
        ObserverError::UnsupportedDialect => Presence::Unsupported(Unsupported::Dialect),
        ObserverError::NotEncrypted => Presence::Unsupported(Unsupported::Unencrypted),
        ObserverError::UnsupportedServer => Presence::Unsupported(Unsupported::Server),
        ObserverError::SlotUnavailable => Presence::NoCall,
        _ => Presence::Unavailable,
    }
}

/// People of an observed call with local display names. Names come from
/// this device's store only, each read bounded, and are cached per watcher.
async fn named(
    room: Option<&matrix_sdk::Room>,
    call: &ObservedCall,
    names: &mut HashMap<String, Option<String>>,
) -> Vec<Person> {
    for participant in call.participants.iter().take(MAX_SHOWN) {
        if names.contains_key(&participant.user_id) || names.len() >= 2 * MAX_SHOWN {
            continue;
        }
        let read = async {
            let user = matrix_sdk::ruma::UserId::parse(&participant.user_id).ok()?;
            let member = room?.get_member_no_sync(&user).await.ok()??;
            member.display_name().map(str::to_owned)
        };
        let name = tokio::time::timeout(NAME_LIMIT, read).await.ok().flatten();
        names.insert(participant.user_id.clone(), name);
    }
    people(&call.participants, |user| {
        names.get(user).cloned().flatten()
    })
}

fn participant(person: &Person) -> VoiceParticipant {
    VoiceParticipant {
        name: person.name.clone().into(),
        user_id: person.user_id.clone().into(),
        initials: label(&person.initials).into(),
        devices: i32::try_from(person.devices).unwrap_or(i32::MAX),
    }
}

#[cfg(test)]
mod tests;

/// Real controller/read/queued-publication regression with only synthetic
/// rows. No account, credential vault, capture, media server or SDK sync.
#[cfg(test)]
pub(crate) fn assert_unowned_presentation_is_cleared(
    ui: &AppWindow,
    live: Arc<LiveUi>,
    runtime: &Handle,
) {
    let owner = VoiceRosterUi::register(ui, live, runtime);
    let room = "!synthetic-voice:example.invalid";
    let person = Person {
        user_id: "@sample:example.invalid".into(),
        name: "Sample person".into(),
        initials: "SP".into(),
        devices: 1,
    };
    {
        let mut state = owner.state();
        state.listed = vec![room.into()];
        state.watchers.insert(
            room.into(),
            Watcher {
                id: 1,
                stop: Arc::new(Stop::default()),
                task: runtime.spawn(std::future::pending()),
                presence: Presence::Live {
                    people: vec![person.clone()],
                    at: Instant::now(),
                    refresh_failed: false,
                },
                started: Instant::now(),
                mode: morpheus_config::RoomCallMode::Standard,
                sticky: true,
            },
        );
    }
    let rosters = ui.global::<VoiceRosters>();
    rosters.set_expanded_room(room.into());
    rosters.set_participants(ModelRc::new(VecModel::from(vec![participant(&person)])));
    // A stale queued read cannot disclose rows just because they remain in
    // memory; a missing/revoked scope is independently rejected.
    assert_eq!(owner.presence(ui, room).state, 0);
    let context = rosters.get_context();
    owner.publish_now(ui);
    assert_ne!(rosters.get_context(), context);
    assert!(rosters.get_expanded_room().is_empty());
    assert_eq!(rosters.get_participants().row_count(), 0);
    assert!(owner.state().listed.is_empty());
    assert!(owner.state().watchers.is_empty());
    owner.shutdown(ui);
}
