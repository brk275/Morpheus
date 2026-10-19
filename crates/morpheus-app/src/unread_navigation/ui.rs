//! UI-thread controller for scroll-aware unread state, and the bounded
//! private-receipt worker.
//!
//! A room view belongs to one installed Slint model object, of the visible
//! account's exact `AccountSelection` and room. A feed generation that the
//! view did not publish (a restarted feed before its first rows, or another
//! feed using the same model) pauses it: nothing is seen, no receipt is
//! requested and nothing scrolls. Reports and results for any other model,
//! account or room are dropped, so an A→B→A switch inherits nothing.
//!
//! Geometry: only Rust bumps `timeline-probe`, and only from a timer
//! callback. Slint runs timers before change handlers, so every live row
//! answers in the same pass with its geometry as last laid out and painted;
//! a batch is used only when its rows cover the whole viewport. Scroll
//! commands go to the timeline ListView one at a time; it acknowledges each,
//! and the rows are measured again only after that.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_matrix::{AccountSelection, MessageInfo, SeenReceipt, UnreadStart};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, Weak};
use tokio::{runtime::Handle, sync::Notify};

use super::{
    Correction, DIVIDER_TEXT, Dwell, EARLIER_TEXT, Geometry, Report, ScrollTarget, anchor,
    average_height, correction, counter_text, covers_latest, index_of, index_of_event, jump_label,
    receipt_candidate, receipt_frontier, receipt_target, unread_below,
};
use crate::{
    AppWindow, MessageItem, UnreadView,
    live::LiveUi,
    timeline_model::{self, Applied},
};

/// Rows are asked for their geometry, and the views evaluated, this often.
const TICK_INTERVAL: Duration = Duration::from_millis(350);
/// A command the ListView never acknowledged (it was not shown) is dropped.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REPORTS: usize = 512;
const MAX_GLIMPSED: usize = 4_096;
const MAX_SETTLE_ROUNDS: u8 = 12;
/// Where the first unread row's top lands when a room opens on it.
const DIVIDER_TOP_PX: f32 = 40.0;
/// Moves smaller than this between two batches are not a scroll.
const MOVE_SLACK_PX: f32 = 1.0;

// Scroll commands, applied by the timeline ListView.
const SHIFT_PIXELS: i32 = 1;
const SEEK_ROW: i32 = 3;
const SEEK_END: i32 = 4;

/// Receipts leave the worker at most this often.
const RECEIPT_GAP: Duration = Duration::from_millis(1_500);
/// Waits before resending a receipt that failed for a transient reason.
const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];
const MAX_PENDING_RECEIPTS: usize = 8;
const MAX_SETTLED_RECEIPTS: usize = 256;

/// One feed publication: the visible account, its exact selection, the room,
/// the thread for a thread feed, and the feed generation that published it.
#[derive(Clone)]
pub(crate) struct FeedScope {
    pub(crate) selection: AccountSelection,
    pub(crate) account_id: String,
    pub(crate) room_id: String,
    pub(crate) thread_root: Option<String>,
    pub(crate) generation: u64,
}

/// The reports answering one probe.
#[derive(Default)]
struct Batch {
    probe: Option<i32>,
    reports: Vec<Report>,
}

impl Batch {
    fn record(&mut self, probe: i32, report: Report) {
        if self.probe != Some(probe) {
            self.probe = Some(probe);
            self.reports.clear();
        }
        let existing = self
            .reports
            .iter_mut()
            .find(|existing| existing.index == report.index);
        match existing {
            Some(existing) => *existing = report,
            None if self.reports.len() < MAX_REPORTS => self.reports.push(report),
            None => {}
        }
    }

    /// The reports for `probe` when they cover the whole viewport: live
    /// rows are contiguous and reach from its top (or the first row) to its
    /// bottom (or the last row). Anything else describes no screen.
    fn complete(&mut self, probe: i32, row_count: usize) -> Option<&[Report]> {
        if self.probe != Some(probe) {
            return None;
        }
        if row_count == 0 {
            return Some(&[]);
        }
        self.reports.sort_by_key(|report| report.index);
        let first = self.reports.first()?;
        let last = self.reports.last()?;
        let contiguous = self
            .reports
            .windows(2)
            .all(|pair| pair[1].index == pair[0].index + 1);
        let top = first.index == 0 || first.geometry.top <= 0.5;
        let viewport = last.geometry.viewport;
        let bottom = last.index + 1 == row_count || last.geometry.bottom() >= viewport - 0.5;
        (contiguous && top && bottom).then_some(self.reports.as_slice())
    }
}

/// Seen state of one feed on screen: the room timeline or an open thread.
struct View {
    scope: FeedScope,
    model: ModelRc<MessageItem>,
    batch: Batch,
    dwell: Dwell,
    /// Keys of rows that were readable at some evaluation with the gate open.
    glimpsed: HashSet<String>,
    /// Key of the newest row seen for the full dwell.
    seen: Option<String>,
    /// Key of the last row known to be read when the view opened.
    read_through: Option<String>,
    /// Key of the latest row when the reader last caught up explicitly
    /// (Jump to latest, sending): rows up to it need not have been glimpsed.
    caught_up: Option<String>,
    /// The latest receipt this view requested.
    requested: Option<String>,
    /// Receipts wait for an explicit catch-up: unread messages may begin
    /// before the loaded history.
    hold_receipts: bool,
}

impl View {
    fn new(scope: FeedScope, model: ModelRc<MessageItem>) -> Self {
        Self {
            scope,
            model,
            batch: Batch::default(),
            dwell: Dwell::default(),
            glimpsed: HashSet::new(),
            seen: None,
            read_through: None,
            caught_up: None,
            requested: None,
            hold_receipts: false,
        }
    }
}

/// The row a reader keeps looking at while rows change above it.
#[derive(Clone)]
struct Anchor {
    key: String,
    top: f32,
}

/// Where the first unread row is, and the row read before it.
struct Divider {
    event_id: String,
    after: Option<String>,
    /// Unread messages begin before the loaded history.
    earlier: bool,
}

/// On-screen tops and heights of the last accepted batch, to tell a user's
/// scroll (everything moved alike) from rows changing size.
#[derive(Default)]
struct Layout {
    viewport: f32,
    rows: HashMap<String, (f32, f32)>,
}

struct RoomState {
    view: View,
    start: Option<UnreadStart>,
    divider: Option<Divider>,
    /// Key of the newest row when the view opened; later rows are new.
    newest_at_open: Option<String>,
    /// The view has been placed on its first unread or latest message.
    positioned: bool,
    at_latest: bool,
    following: bool,
    target: Option<(ScrollTarget, u8)>,
    /// The scroll command the ListView has not acknowledged yet.
    command: Option<(i32, Instant)>,
    /// The topmost on-screen row at the last accepted batch.
    anchor: Option<Anchor>,
    /// Lowest live row index at the last accepted batch.
    first_live: Option<usize>,
    layout: Layout,
    /// Nothing but the user moved the rows since the last accepted batch.
    quiet: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Probe,
    Evaluate,
}

struct Controller {
    ui: Weak<AppWindow>,
    live: Arc<LiveUi>,
    worker: Arc<ReceiptWorker>,
    room: Option<RoomState>,
    thread: Option<View>,
    phase: Phase,
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
    static TICK: Timer = Timer::default();
}

/// Run `work` on the controller. A nested call (a Slint callback raised
/// while the controller is busy) is skipped rather than re-entered.
fn with_controller(work: impl FnOnce(&mut Controller)) {
    CONTROLLER.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut()
            && let Some(controller) = slot.as_mut()
        {
            work(controller);
        }
    });
}

pub(crate) fn register(ui: &AppWindow, live: Arc<LiveUi>, runtime: &Handle) -> Arc<ReceiptWorker> {
    let worker = Arc::new(ReceiptWorker::default());
    runtime.spawn(worker.clone().run(live.clone()));
    CONTROLLER.with(|cell| {
        *cell.borrow_mut() = Some(Controller {
            ui: ui.as_weak(),
            live,
            worker: worker.clone(),
            room: None,
            thread: None,
            phase: Phase::Idle,
        });
    });
    let weak = ui.as_weak();
    ui.on_timeline_row_report(move |probe, index, event_id, top, height, viewport| {
        if let Some(ui) = weak.upgrade() {
            let geometry = Geometry {
                top,
                height,
                viewport,
            };
            let event_id = event_id.as_str();
            with_controller(|controller| {
                controller.report(&ui, false, probe, index, event_id, geometry);
            });
        }
    });
    let weak = ui.as_weak();
    ui.on_thread_row_report(move |probe, index, event_id, top, height, viewport| {
        if let Some(ui) = weak.upgrade() {
            let geometry = Geometry {
                top,
                height,
                viewport,
            };
            let event_id = event_id.as_str();
            with_controller(|controller| {
                controller.report(&ui, true, probe, index, event_id, geometry);
            });
        }
    });
    let weak = ui.as_weak();
    ui.on_timeline_scroll_applied(move |serial| {
        if weak.upgrade().is_some() {
            with_controller(|controller| controller.scroll_applied(serial));
        }
    });
    let weak = ui.as_weak();
    ui.on_timeline_user_scrolled(move || {
        if weak.upgrade().is_some() {
            with_controller(Controller::user_scrolled);
        }
    });
    let weak = ui.as_weak();
    ui.on_timeline_moved(move || {
        if weak.upgrade().is_some() {
            with_controller(Controller::request_cycle);
        }
    });
    let weak = ui.as_weak();
    ui.on_timeline_view_created(move || {
        if weak.upgrade().is_some() {
            with_controller(Controller::view_created);
        }
    });
    let weak = ui.as_weak();
    ui.on_jump_to_latest(move || {
        if let Some(ui) = weak.upgrade() {
            with_controller(|controller| controller.catch_up(&ui));
        }
    });
    let weak = ui.as_weak();
    TICK.with(|timer| {
        timer.start(TimerMode::Repeated, TICK_INTERVAL, move || {
            if let Some(ui) = weak.upgrade() {
                with_controller(|controller| controller.tick(&ui));
            }
        });
    });
    worker
}

/// Forget every view; used when the room, the account or the session changes.
pub(crate) fn reset(ui: &AppWindow) {
    with_controller(|controller| {
        controller.room = None;
        controller.thread = None;
    });
    ui.set_unread_view(UnreadView::default());
    ui.set_unread_divider_event_id("".into());
    ui.set_unread_divider_text("".into());
}

/// A room feed publication was applied to the window's `messages` model;
/// `start` is where unread messages began when the feed opened, if known.
pub(crate) fn after_publish(ui: &AppWindow, scope: FeedScope, start: Option<UnreadStart>) {
    with_controller(|controller| controller.after_publish(ui, scope, start));
}

/// A thread feed publication was applied to the window's `thread-messages`.
pub(crate) fn after_thread_publish(ui: &AppWindow, scope: FeedScope) {
    with_controller(|controller| {
        let model = ui.get_thread_messages();
        let same = controller.thread.as_ref().is_some_and(|thread| {
            thread.model == model
                && thread.scope.account_id == scope.account_id
                && thread.scope.thread_root == scope.thread_root
        });
        match controller.thread.as_mut() {
            Some(thread) if same => {
                thread.scope = scope;
                thread.batch = Batch::default();
            }
            _ => controller.thread = Some(View::new(scope, model)),
        }
        controller.request_cycle();
    });
}

/// The user sent a message: everything up to the latest message counts as
/// read and the timeline follows the latest message again.
pub(crate) fn caught_up(ui: &AppWindow) {
    with_controller(|controller| controller.catch_up(ui));
}

/// What the conversation on screen shows, from the latest complete batch:
/// the feed-visible-event contract for exact-event navigation
/// (claude/overnight-messaging-20261018), which must read visibility here
/// rather than track it again.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VisibleWindow {
    pub(crate) account_id: String,
    pub(crate) room_id: String,
    pub(crate) generation: u64,
    /// The oldest and newest readable rows on screen, by event ID.
    pub(crate) first_event: Option<String>,
    pub(crate) last_event: Option<String>,
    pub(crate) at_latest: bool,
}

#[allow(dead_code)]
pub(crate) fn visible_window(ui: &AppWindow) -> Option<VisibleWindow> {
    let mut window = None;
    with_controller(|controller| {
        let Some(room) = controller.room.as_mut() else {
            return;
        };
        if !matches_room(ui, &room.view) {
            return;
        }
        let rows = timeline_model::rows(&room.view.model).len();
        let Some(reports) = room.view.batch.complete(ui.get_timeline_probe(), rows) else {
            return;
        };
        let readable = reports
            .iter()
            .filter(|report| report.geometry.readable())
            .collect::<Vec<_>>();
        let id = |report: Option<&&Report>| report.and_then(|report| report.row.event_id.clone());
        window = Some(VisibleWindow {
            account_id: room.view.scope.account_id.clone(),
            room_id: room.view.scope.room_id.clone(),
            generation: room.view.scope.generation,
            first_event: id(readable.first()),
            last_event: id(readable.last()),
            at_latest: room.at_latest,
        });
    });
    window
}

/// The window is shown, not minimized and the active window.
pub(crate) fn foreground(ui: &AppWindow) -> bool {
    let window = ui.window();
    window.is_visible() && !window.is_minimized() && window_active(window)
}

/// Slint 1.18.1 tracks whether its window is the active one (from the
/// windowing system's focus events) but has no public getter; the pinned
/// version's internal accessor is read here and only here.
fn window_active(window: &slint::Window) -> bool {
    slint::private_unstable_api::re_exports::WindowInner::from_pub(window).active()
}

/// The live conversation is what the active window shows, uncovered.
fn room_gate(ui: &AppWindow) -> bool {
    ui.get_live_session()
        && ui.get_page() == 0
        && ui.get_timeline_reading_allowed()
        && foreground(ui)
}

fn matches_room(ui: &AppWindow, view: &View) -> bool {
    view.scope.selection.is_current()
        && ui.get_active_account_local_id().as_str() == view.scope.account_id
        && ui.get_active_room_id().as_str() == view.scope.room_id
        && ui.get_messages() == view.model
}

fn matches_thread(ui: &AppWindow, view: &View) -> bool {
    view.scope.selection.is_current()
        && ui.get_active_account_local_id().as_str() == view.scope.account_id
        && ui.get_active_room_id().as_str() == view.scope.room_id
        && view.scope.thread_root.as_deref() == Some(ui.get_thread_root_id().as_str())
        && ui.get_thread_messages() == view.model
}

/// Record the readable rows of a complete batch, move the newest seen row
/// forwards, and return its index in `rows`.
fn observe(
    view: &mut View,
    rows: &[Arc<MessageInfo>],
    reports: &[Report],
    now: Instant,
) -> Option<usize> {
    let readable = reports
        .iter()
        .filter(|report| report.geometry.readable())
        .map(|report| report.row.key.as_str())
        .collect::<Vec<_>>();
    if view.glimpsed.len() + readable.len() > MAX_GLIMPSED {
        // Keep the bound: what was seen is already behind `seen`.
        view.glimpsed.clear();
        view.read_through = view.seen.clone().or(view.read_through.take());
    }
    view.glimpsed
        .extend(readable.iter().map(|key| (*key).to_owned()));
    let seen = view.dwell.observe(now, &readable);
    let mut seen_index = view.seen.as_deref().and_then(|key| index_of(rows, key));
    let newest = reports
        .iter()
        .filter(|report| seen.contains(&report.row.key))
        .max_by_key(|report| report.index);
    if let Some(newest) = newest
        && seen_index.is_none_or(|index| newest.index > index)
    {
        seen_index = Some(newest.index);
        view.seen = Some(newest.row.key.clone());
    }
    seen_index
}

/// Request a private receipt for the newest message from someone else that
/// the reader reached: no further than the newest seen row, and only through
/// messages that were all on screen since the last known read row.
fn request_receipt(
    worker: &ReceiptWorker,
    view: &mut View,
    rows: &[Arc<MessageInfo>],
    seen: Option<usize>,
    newest_seen: bool,
) {
    if view.hold_receipts {
        return;
    }
    let read_through = view.read_through.as_deref();
    let read_through = read_through.and_then(|key| index_of(rows, key));
    let requested = view
        .requested
        .as_deref()
        .and_then(|event_id| index_of_event(rows, event_id));
    let floor = read_through.max(requested);
    let caught_up = view.caught_up.as_deref();
    let caught_up = caught_up.and_then(|key| index_of(rows, key));
    let Some(seen) = seen else {
        return;
    };
    let Some(frontier) = receipt_frontier(rows, floor, caught_up, seen, &view.glimpsed) else {
        return;
    };
    let Some(index) = receipt_candidate(rows, frontier) else {
        return;
    };
    if floor.is_some_and(|floor| index <= floor) {
        return;
    }
    let Some(event_id) = receipt_target(&rows[index]) else {
        return;
    };
    worker.request(ReceiptRequest {
        key: ReceiptKey {
            account_id: view.scope.account_id.clone(),
            room_id: view.scope.room_id.clone(),
            thread_root: view.scope.thread_root.clone(),
        },
        selection: view.scope.selection.clone(),
        event_id: event_id.to_owned(),
        through_latest: newest_seen && covers_latest(rows, index),
        attempt: 0,
        not_before: None,
    });
    view.requested = Some(event_id.to_owned());
}

impl Controller {
    fn report(
        &mut self,
        ui: &AppWindow,
        thread: bool,
        probe: i32,
        index: i32,
        event_id: &str,
        geometry: Geometry,
    ) {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let (view, model) = if thread {
            let Some(view) = self.thread.as_mut() else {
                return;
            };
            (view, ui.get_thread_messages())
        } else {
            let Some(room) = self.room.as_mut() else {
                return;
            };
            (&mut room.view, ui.get_messages())
        };
        if view.model != model {
            return;
        }
        // The Rust row must be the one the delegate showed.
        let Some(row) = timeline_model::row(&model, index) else {
            return;
        };
        if row.event_id.as_deref().unwrap_or("") != event_id {
            return;
        }
        view.batch.record(
            probe,
            Report {
                index,
                row,
                geometry,
            },
        );
    }

    /// Measure and evaluate again soon: bump the probe from a timer in a
    /// later event-loop turn (after the pending layout was painted), then
    /// evaluate the answers from another timer.
    fn request_cycle(&mut self) {
        if self.phase != Phase::Idle {
            return;
        }
        self.phase = Phase::Probe;
        self.schedule_phase();
    }

    fn schedule_phase(&self) {
        let weak = self.ui.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                with_controller(|controller| controller.run_phase(&ui));
            }
        });
    }

    fn run_phase(&mut self, ui: &AppWindow) {
        match self.phase {
            Phase::Idle => {}
            Phase::Probe => {
                self.probe(ui);
                self.phase = Phase::Evaluate;
                self.schedule_phase();
            }
            Phase::Evaluate => {
                self.phase = Phase::Idle;
                self.evaluate(ui);
            }
        }
    }

    /// Ask every live row for its geometry. Called only from timers.
    fn probe(&mut self, ui: &AppWindow) {
        let conversation =
            ui.get_live_session() && ui.get_page() == 0 && !ui.get_active_room_id().is_empty();
        if !conversation {
            return;
        }
        ui.set_timeline_probe(ui.get_timeline_probe().wrapping_add(1));
        let thread_open = self.thread.is_some()
            && ui.get_show_side_panel()
            && ui.get_side_panel_tab() == 1;
        if thread_open {
            ui.set_thread_probe(ui.get_thread_probe().wrapping_add(1));
        }
    }

    fn tick(&mut self, ui: &AppWindow) {
        if self.phase != Phase::Idle {
            return;
        }
        // Evaluate the answers to the previous probe, then ask again.
        self.evaluate(ui);
        if self.phase == Phase::Idle {
            self.probe(ui);
        }
    }

    fn evaluate(&mut self, ui: &AppWindow) {
        let now = Instant::now();
        self.evaluate_room(ui, now);
        self.evaluate_thread(ui, now);
    }

    fn evaluate_room(&mut self, ui: &AppWindow, now: Instant) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        if !matches_room(ui, &room.view) {
            self.room = None;
            clear_unread(ui);
            return;
        }
        // A command the ListView never applied (it was not on screen).
        if room
            .command
            .is_some_and(|(_, sent)| now.saturating_duration_since(sent) >= COMMAND_TIMEOUT)
        {
            room.command = None;
        }
        // A feed generation this view did not publish shows other rows in
        // the same model (a restarted feed, or a focused history view).
        let live_feed = self.live.feed_generation() == room.view.scope.generation;
        let gate = live_feed && room_gate(ui);
        let rows = timeline_model::rows(&room.view.model);
        let probe = ui.get_timeline_probe();
        let complete = room
            .view
            .batch
            .complete(probe, rows.len())
            .map(<[Report]>::to_vec);
        if let Some(reports) = &complete
            && room.command.is_none()
        {
            room.at_latest = rows.is_empty()
                || reports.iter().any(|report| {
                    report.index + 1 == rows.len() && report.geometry.ends_on_screen()
                });
            if room.quiet && moved_alike(&room.layout, reports) {
                // The rows moved together with no command or publication:
                // the user scrolled (scrollbar track, arrows, focus reveal).
                room.following = false;
                room.target = None;
                room.positioned = true;
            }
            room.layout = Layout {
                viewport: reports.first().map_or(0.0, |row| row.geometry.viewport),
                rows: reports
                    .iter()
                    .map(|report| {
                        let geometry = report.geometry;
                        (report.row.key.clone(), (geometry.top, geometry.height))
                    })
                    .collect(),
            };
            room.quiet = true;
            room.first_live = reports.first().map(|report| report.index);
            if let Some((key, top)) = anchor(reports) {
                room.anchor = Some(Anchor { key, top });
            }
            if room.at_latest && room.target.is_none() {
                room.following = true;
            }
            let seen = if gate {
                Some(observe(&mut room.view, &rows, reports, now))
            } else {
                room.view.dwell.clear();
                None
            };
            if let Some(seen) = seen {
                let newest_seen = seen.is_some_and(|index| index + 1 == rows.len());
                request_receipt(&self.worker, &mut room.view, &rows, seen, newest_seen);
            }
            // Follow the latest message while it is being read here.
            if gate && room.following && !room.at_latest && room.target.is_none() {
                room.target = Some((ScrollTarget::End, 0));
            }
            settle(room, ui, &rows, reports);
        } else if !gate {
            room.view.dwell.clear();
        }
        update_divider(room, &rows);
        let seen_index = room
            .view
            .seen
            .as_deref()
            .and_then(|key| index_of(&rows, key));
        let last_readable = complete.as_ref().and_then(|reports| {
            reports
                .iter()
                .filter(|report| report.geometry.readable())
                .map(|report| report.index)
                .max()
        });
        let divider_index = room
            .divider
            .as_ref()
            .and_then(|divider| index_of_event(&rows, &divider.event_id));
        let fresh_from = match &room.newest_at_open {
            Some(key) => index_of(&rows, key).map(|index| index + 1),
            None => Some(0),
        };
        let reached = last_readable.max(seen_index);
        let count = unread_below(&rows, reached, divider_index, fresh_from);
        let show_jump = live_feed && !room.at_latest && !rows.is_empty();
        publish_unread(ui, room, show_jump, count);
    }

    fn evaluate_thread(&mut self, ui: &AppWindow, now: Instant) {
        let Some(view) = self.thread.as_mut() else {
            return;
        };
        if !view.scope.selection.is_current() {
            self.thread = None;
            return;
        }
        let shown = self.live.thread_request_is_current(view.scope.generation)
            && matches_thread(ui, view)
            && ui.get_show_side_panel()
            && ui.get_side_panel_tab() == 1;
        let gate = shown
            && ui.get_live_session()
            && ui.get_page() == 0
            && ui.get_thread_reading_allowed()
            && foreground(ui);
        if !gate {
            view.dwell.clear();
            return;
        }
        let rows = timeline_model::rows(&view.model);
        let probe = ui.get_thread_probe();
        // Thread replies are not virtualized: every reply on screen answers.
        let reports = match &view.batch {
            batch if batch.probe == Some(probe) => batch.reports.clone(),
            _ => Vec::new(),
        };
        let seen = observe(view, &rows, &reports, now);
        let newest_seen = seen.is_some_and(|index| index + 1 == rows.len());
        request_receipt(&self.worker, view, &rows, seen, newest_seen);
    }

    fn after_publish(&mut self, ui: &AppWindow, scope: FeedScope, start: Option<UnreadStart>) {
        let model = ui.get_messages();
        let rows = timeline_model::rows(&model);
        let same = self.room.as_ref().is_some_and(|room| {
            room.view.model == model
                && room.view.scope.account_id == scope.account_id
                && room.view.scope.room_id == scope.room_id
                && room.view.scope.selection.is_current()
        });
        if !same {
            self.room = Some(RoomState {
                view: View::new(scope.clone(), model),
                start: None,
                divider: None,
                newest_at_open: None,
                positioned: false,
                at_latest: true,
                following: false,
                target: None,
                command: None,
                anchor: None,
                first_live: None,
                layout: Layout::default(),
                quiet: false,
            });
            clear_unread(ui);
        }
        let Some(room) = self.room.as_mut() else {
            return;
        };
        room.view.scope = scope;
        room.quiet = false;
        if !room.positioned {
            if !rows.is_empty() {
                open(room, ui, &rows, start);
            }
        } else {
            keep_in_place(room, ui, &rows);
        }
        // The reports described the rows before this publication.
        room.view.batch = Batch::default();
        self.request_cycle();
    }

    fn scroll_applied(&mut self, serial: i32) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        if room.command.is_some_and(|(sent, _)| sent == serial) {
            room.command = None;
            room.quiet = false;
            self.request_cycle();
        }
    }

    fn user_scrolled(&mut self) {
        if let Some(room) = self.room.as_mut() {
            room.positioned = true;
            room.following = false;
            room.target = None;
            room.quiet = false;
        }
        self.request_cycle();
    }

    /// A new timeline ListView was created (the conversation page is shown
    /// again). It starts at the top: put it back where the reader was.
    fn view_created(&mut self) {
        if let Some(room) = self.room.as_mut() {
            room.command = None;
            room.view.batch = Batch::default();
            room.view.dwell.clear();
            room.quiet = false;
            if let Some((_, rounds)) = room.target.as_mut() {
                // A move that was waiting for the ListView starts over.
                *rounds = 0;
            } else if room.positioned {
                room.target = match (&room.anchor, room.following) {
                    (_, true) | (None, false) => Some((ScrollTarget::End, 0)),
                    (Some(anchor), false) => Some((
                        ScrollTarget::Row {
                            key: anchor.key.clone(),
                            top: anchor.top,
                        },
                        0,
                    )),
                };
            }
        }
        // Not from inside the ListView's init: its change trackers do not
        // exist yet, so a command sent now would be lost.
        self.request_cycle();
    }

    /// Jump to latest, or a sent message: the reader explicitly accepts
    /// everything up to the latest row, which is read once it is on screen,
    /// and the timeline follows the latest message again.
    fn catch_up(&mut self, ui: &AppWindow) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        if !matches_room(ui, &room.view) {
            return;
        }
        let rows = timeline_model::rows(&room.view.model);
        room.view.caught_up = rows.last().map(|row| row.key.clone());
        room.view.hold_receipts = false;
        room.positioned = true;
        room.following = true;
        room.target = Some((ScrollTarget::End, 0));
        self.request_cycle();
    }
}

/// The rows of `reports` that were also in `layout` all moved by the same
/// amount, with unchanged heights and viewport: a scroll, not a resize.
fn moved_alike(layout: &Layout, reports: &[Report]) -> bool {
    let Some(first) = reports.first() else {
        return false;
    };
    if (first.geometry.viewport - layout.viewport).abs() > MOVE_SLACK_PX {
        return false;
    }
    let mut shift = None;
    for report in reports {
        let Some(&(top, height)) = layout.rows.get(&report.row.key) else {
            continue;
        };
        if (report.geometry.height - height).abs() > MOVE_SLACK_PX {
            return false;
        }
        let moved = report.geometry.top - top;
        match shift {
            None => shift = Some(moved),
            Some(shift) if (shift - moved).abs() > MOVE_SLACK_PX => return false,
            Some(_) => {}
        }
    }
    shift.is_some_and(|shift| shift.abs() > MOVE_SLACK_PX)
}

/// Place a newly opened room: on its first unread message, at the top of the
/// loaded history when unread messages begin before it, else on the latest
/// message. Applied in the same frame as the rows' installation.
fn open(
    room: &mut RoomState,
    ui: &AppWindow,
    rows: &[Arc<MessageInfo>],
    start: Option<UnreadStart>,
) {
    room.positioned = true;
    room.newest_at_open = rows.last().map(|row| row.key.clone());
    let start = start.unwrap_or(UnreadStart::Unknown);
    let found = match &start {
        UnreadStart::Event(event_id) => index_of_event(rows, event_id),
        _ => None,
    };
    match (&start, found) {
        (UnreadStart::Event(event_id), Some(index)) => {
            room.divider = Some(Divider {
                event_id: event_id.clone(),
                after: index.checked_sub(1).map(|before| rows[before].key.clone()),
                earlier: false,
            });
            room.view.read_through = index.checked_sub(1).map(|before| rows[before].key.clone());
            let goal = ScrollTarget::Row {
                key: rows[index].key.clone(),
                top: DIVIDER_TOP_PX,
            };
            aim(room, ui, goal, index);
        }
        (UnreadStart::BeforeLoaded(_), _) => {
            room.divider = rows
                .first()
                .and_then(|row| row.event_id.clone())
                .map(|event_id| Divider {
                    event_id,
                    after: None,
                    earlier: true,
                });
            // A receipt on any loaded row would also read the unread
            // messages before it that were never loaded or seen.
            room.view.hold_receipts = true;
            let goal = ScrollTarget::Row {
                key: rows[0].key.clone(),
                top: 0.0,
            };
            aim(room, ui, goal, 0);
        }
        _ => {
            // Everything loaded was read already (or no read position is
            // known): open on the latest message.
            room.view.read_through = room.newest_at_open.clone();
            room.following = true;
            aim(room, ui, ScrollTarget::End, rows.len() - 1);
        }
    }
    room.start = Some(start);
}

/// After a publication: follow the latest message, or keep the row the
/// reader looks at in place when rows changed above it.
fn keep_in_place(room: &mut RoomState, ui: &AppWindow, rows: &[Arc<MessageInfo>]) {
    let applied = timeline_model::last_applied(&room.view.model);
    let (prepended, appended) = match applied {
        Applied::Nothing => return,
        Applied::Aligned {
            prepended,
            appended,
        } => (prepended, appended),
        Applied::Spliced => (0, 0),
    };
    if room.following {
        if room_gate(ui) {
            room.target = Some((ScrollTarget::End, 0));
        } else if appended > 0 {
            // New messages while nobody reads here: stop following, so
            // they wait below a divider instead of scrolling past unseen.
            room.following = false;
            let first_new = rows.len() - appended;
            if room.divider.is_none()
                && let Some(event_id) = rows[first_new..].iter().find_map(|row| receipt_target(row))
            {
                room.divider = Some(Divider {
                    event_id: event_id.to_owned(),
                    after: first_new.checked_sub(1).map(|at| rows[at].key.clone()),
                    earlier: false,
                });
            }
        }
        return;
    }
    if prepended > 0
        && let Some(UnreadStart::BeforeLoaded(read)) = &room.start
        && let Some(index) = index_of_event(rows, read)
    {
        // Older history brought the read position in: unread messages now
        // begin at a loaded row.
        upgrade_divider(room, rows, index);
    }
    let Some(anchor) = room.anchor.clone() else {
        return;
    };
    if index_of(rows, &anchor.key).is_none() {
        return;
    }
    if let Applied::Aligned { prepended, .. } = applied {
        // Appends and in-place changes keep the rows on screen where they
        // are, and so does Slint for inserts above its first live row.
        if prepended == 0 || room.first_live.is_some_and(|first| first >= prepended) {
            return;
        }
        // The ListView now shows the inserted rows where the old ones were:
        // move by their real average height at once, then exactly.
        let average = average_height(&room.view.batch.reports);
        if let Some(average) = average
            && room.command.is_none()
        {
            send_command(room, ui, SHIFT_PIXELS, prepended as f32 * average);
        }
    }
    let goal = ScrollTarget::Row {
        key: anchor.key,
        top: anchor.top,
    };
    room.target = Some((goal, 0));
}

/// Older history made the read position loaded: the divider moves to the
/// first message after it, never forwards past one already shown.
fn upgrade_divider(room: &mut RoomState, rows: &[Arc<MessageInfo>], read: usize) {
    let first = rows
        .iter()
        .enumerate()
        .skip(read + 1)
        .find_map(|(index, row)| receipt_target(row).map(|event_id| (index, event_id)));
    room.divider = first.map(|(_, event_id)| Divider {
        event_id: event_id.to_owned(),
        after: Some(rows[read].key.clone()),
        earlier: false,
    });
    room.view.read_through = Some(rows[read].key.clone());
    room.view.hold_receipts = false;
    room.start = Some(match first {
        Some((_, event_id)) => UnreadStart::Event(event_id.to_owned()),
        None => UnreadStart::AllRead,
    });
}

/// A divider whose row was removed (redacted) moves to the next message
/// from someone else after the row read before it.
fn update_divider(room: &mut RoomState, rows: &[Arc<MessageInfo>]) {
    let Some(divider) = room.divider.as_mut() else {
        return;
    };
    if divider.earlier || index_of_event(rows, &divider.event_id).is_some() {
        return;
    }
    let next = divider
        .after
        .as_deref()
        .and_then(|key| index_of(rows, key))
        .and_then(|after| rows[after + 1..].iter().find_map(|row| receipt_target(row)));
    match next {
        Some(event_id) => divider.event_id = event_id.to_owned(),
        None => room.divider = None,
    }
}

/// Start moving towards `goal` (the row at `index`): one coarse move now,
/// exact corrections from the next complete batches.
fn aim(room: &mut RoomState, ui: &AppWindow, goal: ScrollTarget, index: usize) {
    match goal {
        ScrollTarget::End => send_command(room, ui, SEEK_END, 0.0),
        ScrollTarget::Row { .. } => send_command(room, ui, SEEK_ROW, index as f32),
    }
    room.target = Some((goal, 0));
}

/// Hand one scroll command to the timeline ListView. It acknowledges it
/// through `timeline-scroll-applied` once applied.
fn send_command(room: &mut RoomState, ui: &AppWindow, kind: i32, amount: f32) {
    let serial = ui.get_timeline_scroll_serial().wrapping_add(1);
    ui.set_timeline_scroll_kind(kind);
    ui.set_timeline_scroll_amount(amount);
    ui.set_timeline_scroll_serial(serial);
    // Tracker-only properties schedule no frame by themselves.
    ui.window().request_redraw();
    room.command = Some((serial, Instant::now()));
    room.quiet = false;
}

/// One correction step towards the room's scroll target from a complete
/// batch. A command's acknowledgement measures again.
fn settle(room: &mut RoomState, ui: &AppWindow, rows: &[Arc<MessageInfo>], reports: &[Report]) {
    let Some((goal, rounds)) = room.target.as_mut() else {
        return;
    };
    let index = match goal {
        ScrollTarget::End => rows.len().checked_sub(1),
        ScrollTarget::Row { key, .. } => index_of(rows, key),
    };
    let Some(index) = index else {
        room.target = None;
        return;
    };
    if *rounds >= MAX_SETTLE_ROUNDS {
        room.target = None;
        return;
    }
    *rounds += 1;
    match correction(goal, index, rows.len(), reports) {
        Correction::Done => {
            if *goal == ScrollTarget::End {
                room.following = true;
                room.at_latest = true;
            }
            room.target = None;
        }
        Correction::Shift(delta) => send_command(room, ui, SHIFT_PIXELS, delta),
        Correction::Unknown => {
            let (kind, amount) = match goal {
                ScrollTarget::End => (SEEK_END, 0.0),
                ScrollTarget::Row { .. } => (SEEK_ROW, index as f32),
            };
            send_command(room, ui, kind, amount);
        }
    }
}

fn clear_unread(ui: &AppWindow) {
    ui.set_unread_view(UnreadView::default());
    ui.set_unread_divider_event_id("".into());
    ui.set_unread_divider_text("".into());
}

/// Set the divider, the counter and Jump to latest; only changed values are
/// written, so an idle tick draws nothing.
fn publish_unread(ui: &AppWindow, room: &RoomState, show_jump: bool, count: usize) {
    let (event_id, text) = match &room.divider {
        Some(divider) if divider.earlier => (divider.event_id.as_str(), EARLIER_TEXT),
        Some(divider) => (divider.event_id.as_str(), DIVIDER_TEXT),
        None => ("", ""),
    };
    if ui.get_unread_divider_event_id().as_str() != event_id {
        ui.set_unread_divider_event_id(event_id.into());
    }
    if ui.get_unread_divider_text().as_str() != text {
        ui.set_unread_divider_text(text.into());
    }
    let next = UnreadView {
        show_jump,
        new_count: i32::try_from(count).unwrap_or(i32::MAX),
        jump_text: counter_text(count).into(),
        jump_label: jump_label(count).into(),
    };
    if ui.get_unread_view() != next {
        ui.set_unread_view(next);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ReceiptKey {
    account_id: String,
    room_id: String,
    thread_root: Option<String>,
}

struct ReceiptRequest {
    key: ReceiptKey,
    selection: AccountSelection,
    event_id: String,
    /// The newest row was seen too: hidden events after it count as read.
    through_latest: bool,
    attempt: usize,
    /// A retry waits until then.
    not_before: Option<Instant>,
}

#[derive(Default)]
struct WorkerState {
    pending: Vec<ReceiptRequest>,
    /// The last receipt per room or thread that the homeserver confirmed,
    /// skipped as not newer, or refused for good.
    settled: HashMap<ReceiptKey, String>,
}

/// Sends private receipts one at a time, at most every `RECEIPT_GAP`, with
/// the newest request per room or thread replacing older ones. A transient
/// failure is retried a few times with growing waits; a refusal is not. A
/// request whose selection is no longer current is dropped, nothing is sent
/// while the session is not live, and nothing at all after shutdown.
#[derive(Default)]
pub(crate) struct ReceiptWorker {
    state: Mutex<WorkerState>,
    wake: Notify,
    stop: Notify,
    stopped: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

impl ReceiptWorker {
    fn request(&self, request: ReceiptRequest) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        {
            let mut state = lock(&self.state);
            let pending = &mut state.pending;
            pending.retain(|queued| queued.key != request.key);
            if pending.len() >= MAX_PENDING_RECEIPTS {
                pending.remove(0);
            }
            pending.push(request);
        }
        self.wake.notify_one();
    }

    /// Stop at quit: pending receipts are dropped, a send in progress is
    /// abandoned, and none is sent after.
    pub(crate) fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        lock(&self.state).pending.clear();
        self.stop.notify_waiters();
        self.wake.notify_one();
    }

    /// The oldest request that may leave now, or how long until one may.
    fn next(&self, now: Instant) -> Result<ReceiptRequest, Option<Duration>> {
        let mut state = lock(&self.state);
        let ready = state
            .pending
            .iter()
            .position(|request| request.not_before.is_none_or(|at| at <= now));
        match ready {
            Some(index) => Ok(state.pending.remove(index)),
            None => Err(state
                .pending
                .iter()
                .filter_map(|request| request.not_before)
                .min()
                .map(|at| at.saturating_duration_since(now))),
        }
    }

    fn after(&self, key: &ReceiptKey) -> Option<String> {
        lock(&self.state).settled.get(key).cloned()
    }

    fn settle(&self, request: &ReceiptRequest) {
        let mut state = lock(&self.state);
        let full = state.settled.len() >= MAX_SETTLED_RECEIPTS;
        if full && !state.settled.contains_key(&request.key) {
            state.settled.clear();
        }
        state
            .settled
            .insert(request.key.clone(), request.event_id.clone());
    }

    /// Retry a transient failure later unless a newer request replaced it.
    fn retry(&self, mut request: ReceiptRequest) {
        let Some(delay) = RETRY_DELAYS.get(request.attempt) else {
            return;
        };
        request.attempt += 1;
        request.not_before = Some(Instant::now() + *delay);
        let mut state = lock(&self.state);
        if state.pending.iter().any(|queued| queued.key == request.key) {
            return;
        }
        state.pending.push(request);
    }

    async fn stopped(&self) {
        loop {
            let notified = self.stop.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    async fn run(self: Arc<Self>, live: Arc<LiveUi>) {
        let mut last_sent: Option<Instant> = None;
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            let elapsed = last_sent.map_or(RECEIPT_GAP, |sent| sent.elapsed());
            let pause = RECEIPT_GAP.saturating_sub(elapsed);
            if !pause.is_zero() {
                tokio::select! {
                    _ = self.stopped() => return,
                    _ = tokio::time::sleep(pause) => {}
                }
                continue;
            }
            let request = match self.next(Instant::now()) {
                Ok(request) => request,
                Err(wait) => {
                    let wait = wait.unwrap_or(Duration::from_secs(3_600));
                    tokio::select! {
                        _ = self.stopped() => return,
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(wait) => {}
                    }
                    continue;
                }
            };
            if !request.selection.is_current() || !live.is_live.load(Ordering::Acquire) {
                continue;
            }
            let after = self.after(&request.key);
            if after.as_deref() == Some(request.event_id.as_str()) {
                continue;
            }
            let receipt = SeenReceipt {
                room_id: &request.key.room_id,
                thread_root: request.key.thread_root.as_deref(),
                event_id: &request.event_id,
                after: after.as_deref(),
                through_latest: request.through_latest,
            };
            let send = live
                .manager
                .send_seen_receipt_selected(&request.selection, receipt);
            let sent = tokio::select! {
                _ = self.stopped() => return,
                sent = send => sent,
            };
            last_sent = Some(Instant::now());
            match sent {
                // Sent, skipped as not newer, or refused for good.
                Ok(_) => self.settle(&request),
                Err(_) if request.selection.is_current() => self.retry(request),
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::MessageSendState;

    fn row(index: usize) -> Arc<MessageInfo> {
        Arc::new(MessageInfo {
            key: format!("key-{index}"),
            event_id: Some(format!("$event-{index}")),
            sender: "@casey:example.org".to_owned(),
            body: "hello".to_owned(),
            rich_markdown: None,
            rich_accessible: None,
            attachment_name: None,
            attachment_is_image: false,
            reply_preview: None,
            reaction_summary: String::new(),
            timestamp_ms: 0,
            is_own: false,
            is_local_echo: false,
            send_state: MessageSendState::Sent,
        })
    }
    fn report(index: usize, top: f32) -> Report {
        let geometry = Geometry {
            top,
            height: 100.0,
            viewport: 280.0,
        };
        Report {
            index,
            row: row(index),
            geometry,
        }
    }

    #[test]
    fn only_complete_batches_for_the_current_probe_describe_the_screen() {
        let mut batch = Batch::default();
        for (index, top) in [(5, -10.0), (6, 90.0), (7, 190.0)] {
            batch.record(3, report(index, top));
        }
        assert_eq!(batch.complete(3, 20).map(<[Report]>::len), Some(3));
        assert!(batch.complete(4, 20).is_none());
        // A gap, or rows that stop short of the viewport's bottom.
        let mut gap = Batch::default();
        gap.record(1, report(5, -10.0));
        gap.record(1, report(7, 190.0));
        assert!(gap.complete(1, 20).is_none());
        let mut short = Batch::default();
        short.record(1, report(5, 0.0));
        assert!(short.complete(1, 20).is_none());
        // The last row may end above the viewport's bottom.
        assert!(short.complete(1, 6).is_some());
        // A newer probe starts over.
        batch.record(4, report(9, 0.0));
        assert!(batch.complete(3, 20).is_none());
    }

    #[test]
    fn a_scroll_moves_every_row_alike_while_a_resize_does_not() {
        let rows = [
            ("key-5".to_owned(), (-10.0, 100.0)),
            ("key-6".to_owned(), (90.0, 100.0)),
        ];
        let layout = Layout {
            viewport: 280.0,
            rows: rows.into_iter().collect(),
        };
        assert!(moved_alike(&layout, &[report(5, -40.0), report(6, 60.0)]));
        assert!(!moved_alike(&layout, &[report(5, -10.0), report(6, 90.0)]));
        assert!(!moved_alike(&layout, &[report(5, -10.0), report(6, 120.0)]));
    }

    #[test]
    fn the_receipt_worker_is_shared_with_the_runtime() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<ReceiptWorker>();
        send_sync::<FeedScope>();
    }
}
