//! Opt-in live coverage for the desktop room and thread timeline controllers.
//!
//! Slint's event-loop proxy is process-wide, so this scenario never creates a
//! backend of its own: the single real-window harness in the account access UI
//! tests calls `run_if_configured` on its event loop after its own scenario.
//! Without MORPHEUS_MATRIX_TEST_ACCOUNTS (a disposable local fixture from
//! `scripts/local_matrix_env.py`) it returns at once, so ordinary CI runs are
//! unchanged. With a fixture it needs real local homeservers; it is not part of
//! any hosted selector yet. Opt in with:
//!
//! ```text
//! MORPHEUS_MATRIX_TEST_ACCOUNTS=<fixture.json> cargo test --locked -p morpheus-app \
//!   --features native-calls --bin morpheus-app \
//!   account_access::ui_tests::real_slint_account_dialogs_and_detach_clear_private_presentation \
//!   -- --nocapture --test-threads=1
//! ```
//!
//! Real SDK room and thread feeds publish into a real generated `AppWindow`
//! through the controller's actual `open_room` (reached through the production
//! `select_room_for`) and `open_thread`. Account switches deliberately go
//! through the core manager directly: the controller's own switch paths clear
//! its feeds first, which would mask the exact selection lease checked here.
//! The Space scan that `account_activated` spawns is also avoided, because its
//! closing refresh would legitimately reopen the room during the stale checks.
//!
//! Two revocation cases are covered. In the idle case both feeds are waiting
//! for updates when A -> B ends them. In the queued case the UI thread is held
//! in a bounded synchronous wait while a Tokio worker sends new events, waits
//! for the controller's test-build observation that callbacks carrying them
//! are queued on the event loop, and performs A -> B -> A; the held loop then
//! runs those callbacks, which must publish nothing.
//!
//! A joined message link's continuation is then driven through its actual
//! completion (matrix_links' finish_join), the owned continuation and its
//! Slint retry timers on the harness's mock clock: a same-room navigation
//! before a retry, and a security step, Settings, a newer link, or the quick
//! switcher or account menu opened and closed again while the focused view
//! is held before its commit, show and scroll nothing; an unchanged owner
//! still scrolls and focuses. The join's own open is the
//! real `open_joined` of the already joined fixture room; no join is sent.
//!
//! Each fixed `MORPHEUS_LIVE_TIMELINE_CHECK name=true` record is printed once,
//! only after its assertions passed. Records carry no IDs, values or text.

use std::future::Future;

use slint::ComponentHandle;

use super::*;

// Fixed tokens only: no account IDs, room IDs or message text.
macro_rules! stage {
    ($name:ident) => {
        eprintln!("\nMORPHEUS_LIVE_TIMELINE_STAGE {}", stringify!($name));
    };
}

// Printed once per name, only after the corresponding assertions passed.
macro_rules! check {
    ($name:ident) => {
        eprintln!("\nMORPHEUS_LIVE_TIMELINE_CHECK {}=true", stringify!($name));
    };
}

struct FixtureAccount {
    homeserver_url: String,
    username: String,
    password: String,
}

fn fixture() -> Option<(FixtureAccount, FixtureAccount, String)> {
    let path = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS")?;
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("Cannot read local fixture"))
            .expect("Invalid local fixture");
    let account = |label: &str| {
        let entry = fixture["accounts"]
            .as_array()
            .expect("fixture accounts")
            .iter()
            .find(|entry| entry["label"].as_str() == Some(label))
            .expect("fixture account");
        FixtureAccount {
            homeserver_url: entry["homeserver_url"].as_str().unwrap().to_owned(),
            username: entry["username"].as_str().unwrap().to_owned(),
            password: entry["password"].as_str().unwrap().to_owned(),
        }
    };
    let room = fixture["rooms"]["a_room"]
        .as_str()
        .expect("fixture room")
        .to_owned();
    Some((account("a_primary"), account("b_primary"), room))
}

/// Run the scenario on the harness's event loop when a local fixture is
/// configured. Called from inside `slint::spawn_local`; never creates UI
/// infrastructure of its own.
pub(crate) async fn run_if_configured(runtime: Handle) {
    let Some((alice, carol, room)) = fixture() else {
        eprintln!(
            "Skipped live timeline controller scenario; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
        );
        return;
    };
    let scenario = exercise(runtime, alice, carol, room);
    tokio::time::timeout(Duration::from_secs(420), scenario)
        .await
        .expect("live timeline controller scenario must finish within its bound");
}

/// Owns one task spawned on the runtime. Dropping it, for example when the
/// outer timeout or the UI scenario is cancelled mid-await, requests abort so
/// the work is not left detached. Abort is only a request: it does not wait
/// for the task to finish; the runtime drops the task's future when it next
/// processes the cancellation. Aborting a task that already finished is a
/// no-op, so the guard is also dropped after a normal `join`.
struct OwnedTask<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for OwnedTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> OwnedTask<T> {
    fn spawn<F>(runtime: &Handle, work: F) -> Self
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        Self(runtime.spawn(work))
    }

    /// Await the result. A panic in the work is re-raised here so the harness
    /// test fails with it.
    async fn join(&mut self) -> T {
        match (&mut self.0).await {
            Ok(value) => value,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => panic!("Live timeline runtime work was cancelled"),
        }
    }
}

/// Run core work on the Tokio runtime and await it from the Slint event loop.
/// Cancelling the returned future aborts the work (see `OwnedTask`).
async fn on_runtime<T, F>(runtime: &Handle, work: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let mut task = OwnedTask::spawn(runtime, work);
    task.join().await
}

/// Explicit FIFO event-loop barrier: every UI callback the controller queued
/// before this one has run when it returns. This is the only "pump": the
/// scenario itself is a task on the real Slint event loop, so awaiting here
/// lets the loop deliver queued callbacks in order.
async fn settle(ui: &AppWindow) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let weak = ui.as_weak();
    let queued = weak.upgrade_in_event_loop(move |_| {
        let _ = sender.send(());
    });
    queued.expect("The harness event loop must accept callbacks");
    receiver.await.expect("The barrier callback must run");
}

/// Wait, bounded, until the generated window shows a condition.
async fn wait_for_ui(ui: &AppWindow, what: &'static str, ready: impl Fn(&AppWindow) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        settle(ui).await;
        if ready(ui) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn rows(model: &ModelRc<MessageItem>) -> Vec<MessageItem> {
    let count = model.row_count();
    (0..count).filter_map(|row| model.row_data(row)).collect()
}

/// Any row with this text, local echo or server copy.
fn shows_body(model: &ModelRc<MessageItem>, body: &str) -> bool {
    rows(model).iter().any(|row| row.body.as_str() == body)
}

fn has_event_id(model: &ModelRc<MessageItem>, id: &str) -> bool {
    rows(model).iter().any(|row| row.event_id.as_str() == id)
}

/// The event ID of the server copy of a message the window shows.
fn event_id_of(model: &ModelRc<MessageItem>, body: &str) -> Option<String> {
    let items = rows(model);
    let item = items.iter().find(|item| is_server_row(item, body))?;
    Some(item.event_id.to_string())
}

fn is_server_row(item: &MessageItem, body: &str) -> bool {
    item.body.as_str() == body && !item.event_id.is_empty()
}

fn delivered(model: &ModelRc<MessageItem>, body: &str) -> bool {
    event_id_of(model, body).is_some()
}

fn is_server_copy(message: &MessageInfo, body: &str) -> bool {
    message.body == body && message.event_id.is_some() && !message.is_local_echo
}

fn holds(snapshot: &[MessageInfo], body: &str) -> bool {
    snapshot.iter().any(|m| is_server_copy(m, body))
}

fn marker(prefix: &str) -> String {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{unique}")
}

/// Which armed callbacks have been observed queued with their marker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Queued {
    room: bool,
    thread: bool,
}

/// The one scope and pair of unique server-copy markers the queued case
/// waits for. Nothing else is recorded.
struct QueuedWatch {
    account: String,
    room: String,
    root: String,
    room_body: String,
    reply_body: String,
    seen: tokio::sync::watch::Sender<Queued>,
}

static QUEUED_WATCH: std::sync::Mutex<Option<QueuedWatch>> = std::sync::Mutex::new(None);

/// Disarms the watch when dropped, including on a failed assertion.
struct ArmedWatch;

impl Drop for ArmedWatch {
    fn drop(&mut self) {
        *QUEUED_WATCH
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

fn arm_queued_watch(watch: QueuedWatch) -> ArmedWatch {
    let mut armed = QUEUED_WATCH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert!(armed.is_none(), "Only one queued watch may be armed");
    *armed = Some(watch);
    ArmedWatch
}

/// Called by the controller's room and thread feed loops in test builds only,
/// after a successful `upgrade_in_event_loop`: the snapshot is now held by a
/// UI callback queued on the event loop (queued in that iteration, or still
/// pending in one queued earlier that has not taken its payload yet). Only the
/// armed account, room and thread root, and only the server copy of the armed
/// unique markers, are recorded; every other call returns without effect.
pub(crate) fn callback_queued(
    account: &str,
    room: &str,
    thread: Option<&str>,
    snapshot: &[Arc<MessageInfo>],
) {
    let armed = QUEUED_WATCH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(watch) = armed.as_ref() else {
        return;
    };
    if account != watch.account || room != watch.room {
        return;
    }
    let holds_marker = |body: &str| snapshot.iter().any(|m| is_server_copy(m, body));
    match thread {
        None if holds_marker(&watch.room_body) => {
            watch.seen.send_modify(|seen| seen.room = true);
        }
        Some(root) if root == watch.root && holds_marker(&watch.reply_body) => {
            watch.seen.send_modify(|seen| seen.thread = true);
        }
        _ => {}
    }
}

/// Wait until the room is visible to the active account, then scan it the
/// way the app does when a joined room is opened (fresh profile: no drafts).
async fn wait_for_room(manager: &AccountManager, room_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let rooms = manager.rooms_snapshot().await.expect("Cannot read rooms");
        if rooms.iter().any(|room| room.room_id == room_id) {
            super::opened_room::scan(manager, room_id)
                .await
                .expect("Opened fixture room must be scanned");
            return;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for room");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Whether the controller's room and thread feed tasks are still running.
async fn feeds_running(live: &LiveUi) -> (bool, bool) {
    let room = {
        let feed = live.feed.lock().await;
        feed.as_ref().is_some_and(|task| !task.is_finished())
    };
    let thread = {
        let feed = live.thread_feed.lock().await;
        feed.as_ref().is_some_and(|task| !task.is_finished())
    };
    (room, thread)
}

/// What arrives while a joined link's focused view is held before its
/// commit (`Scenario::link_continuation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    SecurityStep,
    Settings,
    NewerLink,
    /// The quick switcher opened through its real callback, then closed.
    QuickSwitcher,
    /// The account menu opened through its real entry, then closed.
    AccountMenu,
}

/// Move Slint's mock clock past every scroll retry a continuation may still
/// have pending, one retry period at a time, so each timer that fires can
/// schedule the next and the last one opens the focused view.
async fn advance_retries(ui: &AppWindow) {
    for _ in 0..=message_search::ARRIVAL_ATTEMPTS {
        i_slint_backend_testing::mock_elapsed_time(message_search::JUMP_RETRY);
        settle(ui).await;
    }
}

async fn exercise(runtime: Handle, alice: FixtureAccount, carol: FixtureAccount, room: String) {
    stage!(setup);
    let directory = tempfile::tempdir().unwrap();
    let manager = Arc::new(AccountManager::open(directory.path().join("profile")).unwrap());
    let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
        directory.path().join("config.toml"),
    )));
    // A second generated window on the harness's single event loop. Feed
    // callbacks publish to its properties and models, which is what the
    // desktop renders from; it need not be shown for that.
    let ui = AppWindow::new().unwrap();
    let live = LiveUi::new(manager.clone(), runtime.clone(), ui.as_weak(), voices);
    let account = {
        let manager = manager.clone();
        let room = room.clone();
        on_runtime(&runtime, async move {
            let added = manager
                .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
                .await;
            let account = added.expect("First login failed");
            wait_for_room(&manager, &room).await;
            account.id
        })
        .await
    };
    let scenario = Scenario {
        runtime,
        manager,
        live,
        ui,
        account,
        room,
    };
    let other = scenario.run(carol).await;
    scenario.close(directory, other).await;
}

struct Scenario {
    runtime: Handle,
    manager: Arc<AccountManager>,
    live: Arc<LiveUi>,
    ui: AppWindow,
    account: String,
    room: String,
}

impl Scenario {
    async fn run(&self, other: FixtureAccount) -> String {
        stage!(activation);
        self.activate().await;

        // Room: the production selection path reaches the actual open_room,
        // which subscribes to the real SDK timeline under the current lease.
        stage!(room_open);
        self.select_room().await;
        let scoped = |ui: &AppWindow| self.shows_scope(ui);
        wait_for_ui(&self.ui, "The selected room must become visible", scoped).await;
        let expected_key = Some((self.account.clone(), self.room.clone()));
        assert!(
            *self.live.feed_key.lock().await == expected_key,
            "open_room must own the feed"
        );

        stage!(room_delivery);
        let root_text = marker("live-timeline-root");
        self.send_room(&root_text).await;
        let published = |ui: &AppWindow| delivered(&ui.get_messages(), &root_text);
        wait_for_ui(&self.ui, "The room feed did not publish", published).await;
        let root = event_id_of(&self.ui.get_messages(), &root_text).unwrap();
        check!(room_delivery);

        // Thread: the Slint handler names the root and clears the list, then
        // the actual open_thread subscribes to the SDK thread timeline.
        stage!(thread_delivery);
        self.open_thread(&root).await;
        let reply_text = marker("live-timeline-reply");
        self.send_reply(&root, &reply_text).await;
        let replied = |ui: &AppWindow| delivered(&ui.get_thread_messages(), &reply_text);
        wait_for_ui(&self.ui, "The thread feed did not publish", replied).await;
        check!(thread_delivery);
        assert!(
            !has_event_id(&self.ui.get_thread_messages(), &root),
            "The thread list must exclude its root"
        );
        check!(thread_root_excluded);

        // Both subscriptions published into a live window and keep waiting
        // for the next update: from here only their lease can end them.
        assert_eq!(
            feeds_running(&self.live).await,
            (true, true),
            "Feeds end too early"
        );
        check!(feeds_running);
        let lease = self.manager.account_selection(&self.account).unwrap();
        assert!(
            lease.is_current(),
            "The lease must be current before revocation"
        );

        // Idle case, A -> B without the controller: another account signs in
        // on this profile and becomes active. Both waiting feeds must end.
        stage!(revocation);
        let other_id = self.switch_away(other).await;
        assert!(
            !lease.is_current(),
            "Signing in another account must end the lease"
        );
        check!(old_lease_revoked);
        self.wait_until_stopped().await;
        settle(&self.ui).await;
        assert!(
            self.shows_scope(&self.ui),
            "The controller must not have been told"
        );
        assert!(
            self.ui.get_thread_root_id().as_str() == root,
            "The thread must stay named"
        );
        check!(idle_feeds_stopped);

        // A -> B -> A, without the controller: the same account and room are
        // visible again under a new lease. New server events reach the same
        // cached SDK timelines the ended feeds subscribed to, so a feed that
        // outlived its lease would publish them into this matching scope.
        stage!(renewal);
        self.switch_back().await;
        let renewed = self.manager.account_selection(&self.account).unwrap();
        assert!(
            renewed.is_current() && !lease.is_current(),
            "Renewal is a new lease"
        );
        let late_room = marker("live-timeline-late-room");
        let late_reply = marker("live-timeline-late-reply");
        self.send_room(&late_room).await;
        self.send_reply(&root, &late_reply).await;
        self.wait_for_core_room_event(&late_room).await;
        self.wait_for_core_thread_event(&root, &late_reply).await;
        check!(late_events_in_core);
        // Bounded window for any surviving subscriber to queue a callback,
        // then drain the event loop in order before checking.
        tokio::time::sleep(Duration::from_secs(2)).await;
        settle(&self.ui).await;
        settle(&self.ui).await;
        assert!(self.shows_scope(&self.ui), "The scope must still match");
        assert!(
            self.ui.get_thread_root_id().as_str() == root,
            "The thread must still match"
        );
        assert!(
            !shows_body(&self.ui.get_messages(), &late_room),
            "A stale room feed published after A -> B -> A"
        );
        check!(stale_room_rejected);
        assert!(
            !shows_body(&self.ui.get_thread_messages(), &late_reply),
            "A stale thread feed published after A -> B -> A"
        );
        check!(stale_thread_rejected);
        assert_eq!(
            feeds_running(&self.live).await,
            (false, false),
            "Ended feeds restarted"
        );

        // Positive controls under the renewed lease: the same window and the
        // actual open paths deliver the late events, so the negative checks
        // above were not caused by a dead window or a broken feed. The room is
        // reopened first because reselecting it clears the thread feed.
        stage!(reopen);
        self.select_room().await;
        let room_back = |ui: &AppWindow| delivered(&ui.get_messages(), &late_room);
        wait_for_ui(&self.ui, "The reselected room did not publish", room_back).await;
        check!(renewed_room_delivery);
        self.open_thread(&root).await;
        let reply_back = |ui: &AppWindow| delivered(&ui.get_thread_messages(), &late_reply);
        wait_for_ui(&self.ui, "The reopened thread did not publish", reply_back).await;
        check!(renewed_thread_delivery);

        self.queued_across_revocation(&root, &other_id).await;
        self.link_continuation(&root).await;
        other_id
    }

    /// Queued case: callbacks carrying new server events are already queued
    /// on the UI loop, but have not run, when A -> B -> A happens.
    async fn queued_across_revocation(&self, root: &str, other: &str) {
        stage!(queued_revocation);
        assert_eq!(
            feeds_running(&self.live).await,
            (true, true),
            "Both feeds must be open"
        );
        let lease = self.manager.account_selection(&self.account).unwrap();
        assert!(lease.is_current(), "The renewed lease must be current");
        // Drain earlier callbacks so only this case's payloads are pending.
        settle(&self.ui).await;
        let room_text = marker("live-timeline-queued-room");
        let reply_text = marker("live-timeline-queued-reply");
        let (seen, mut queued) = tokio::sync::watch::channel(Queued::default());
        let armed = arm_queued_watch(QueuedWatch {
            account: self.account.clone(),
            room: self.room.clone(),
            root: root.to_owned(),
            room_body: room_text.clone(),
            reply_body: reply_text.clone(),
            seen,
        });
        let manager = self.manager.clone();
        let (account, room) = (self.account.clone(), self.room.clone());
        let (root_id, other) = (root.to_owned(), other.to_owned());
        let (room_body, reply_body) = (room_text.clone(), reply_text.clone());
        let (done, finished) = std::sync::mpsc::channel();
        // Async work on the runtime only: it awaits the real feeds and the
        // core, and never waits for the UI thread.
        let mut worker = OwnedTask::spawn(&self.runtime, async move {
            let sent = manager.send_text_for(&account, &room, room_body).await;
            sent.expect("Cannot send the queued room marker");
            let replied = manager
                .send_thread_text_for(&account, &room, &root_id, reply_body)
                .await;
            replied.expect("Cannot send the queued thread marker");
            let both = |seen: &Queued| seen.room && seen.thread;
            // Copy the state out and release the watch borrow before any
            // later await; the feed loops' observations must never wait on it.
            let observed = {
                let waited =
                    tokio::time::timeout(Duration::from_secs(60), queued.wait_for(both)).await;
                match waited {
                    Ok(Ok(seen)) => *seen,
                    _ => Queued::default(),
                }
            };
            // Revoke and renew only once both callbacks are known queued.
            if observed.room && observed.thread {
                let away = manager.switch_to(&other).await;
                away.expect("Cannot switch away while callbacks are queued");
                let back = manager.switch_to(&account).await;
                back.expect("Cannot switch back while callbacks are queued");
            }
            let _ = done.send(());
            observed
        });
        // Bounded synchronous wait on the UI thread itself: the event loop
        // cannot run the queued callbacks until the worker reports.
        let reported = finished.recv_timeout(Duration::from_secs(120));
        drop(armed);
        let timed_out = matches!(reported, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        assert!(
            !timed_out,
            "The queued A -> B -> A must finish within its bound"
        );
        let observed = worker.join().await;
        assert!(
            observed.room && observed.thread,
            "Both callbacks must be queued with the events"
        );
        assert!(
            !lease.is_current(),
            "The queued callbacks' lease must be revoked"
        );
        let renewed = self.manager.account_selection(&self.account).unwrap();
        assert!(
            renewed.is_current(),
            "The account must be selected again under a new lease"
        );
        check!(queued_callbacks_observed);

        // Resume the real loop: the queued callbacks run now, in order, before
        // this barrier. The window still names the matching scope, and the
        // controller's generation and navigation checks still pass.
        stage!(queued_drain);
        settle(&self.ui).await;
        settle(&self.ui).await;
        assert!(self.shows_scope(&self.ui), "The scope must still match");
        assert!(
            self.ui.get_thread_root_id().as_str() == root,
            "The thread must still match"
        );
        assert!(
            !shows_body(&self.ui.get_messages(), &room_text),
            "A queued room callback published after A -> B -> A"
        );
        check!(queued_room_rejected);
        assert!(
            !shows_body(&self.ui.get_thread_messages(), &reply_text),
            "A queued thread callback published after A -> B -> A"
        );
        check!(queued_thread_rejected);
        self.wait_until_stopped().await;

        // Positive control under the newest lease.
        stage!(queued_reopen);
        self.select_room().await;
        let room_back = |ui: &AppWindow| delivered(&ui.get_messages(), &room_text);
        wait_for_ui(
            &self.ui,
            "The room did not deliver after reopening",
            room_back,
        )
        .await;
        self.open_thread(root).await;
        let reply_back = |ui: &AppWindow| delivered(&ui.get_thread_messages(), &reply_text);
        wait_for_ui(
            &self.ui,
            "The thread did not deliver after reopening",
            reply_back,
        )
        .await;
    }

    /// A joined message link's message, through the actual completion
    /// (matrix_links' finish_join), the owned continuation and its Slint
    /// timers, with this real window, selection and join receipt. The
    /// harness runs Slint on mock time, so a retry fires only when
    /// `advance_retries` moves the clock. The message is `root`, a row of
    /// the live conversation, taken out of the window before each follow-up
    /// so the continuation has to wait for it.
    async fn link_continuation(&self, root: &str) {
        stage!(link_continuation);
        let ui = &self.ui;
        crate::matrix_links::register(ui, self.live.clone(), self.runtime.clone());
        // The quick switcher's real open callback (shortcuts::open_picker).
        let shortcuts = tempfile::tempdir().unwrap();
        let store = morpheus_config::ConfigStore::at(shortcuts.path().join("shortcuts.toml"));
        crate::shortcuts::register(ui, store);
        ui.set_show_sign_in(false);
        ui.set_page(0);
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        settle(ui).await;
        assert!(ui.get_live_session(), "The session must be live");
        assert!(
            !ui.get_matrix_link_protected(),
            "No protected surface may be up"
        );
        let selection = self.manager.account_selection(&self.account).unwrap();
        let row = |ui: &AppWindow| has_event_id(&ui.get_messages(), root);

        // An ordinary same-room navigation (a room-list click) lands between
        // the absent row and its retry, and the reopened room delivers the
        // row: the old continuation neither scrolls nor focuses, and the
        // newer epoch is untouched.
        let asked = self.joined_open(root).await;
        let jumps = ui.get_timeline_jump_request();
        self.follow_link(&selection, root, &asked);
        assert_eq!(ui.get_timeline_jump_request(), jumps, "The row is absent");
        self.select_room().await;
        let newer = self.live.current_navigation_epoch();
        wait_for_ui(ui, "The reselected room did not deliver the row", row).await;
        let thread = ui.get_thread_root_id();
        advance_retries(ui).await;
        assert_eq!(
            ui.get_timeline_jump_request(),
            jumps,
            "A stale retry scrolled"
        );
        assert!(
            self.live.navigation_is_current(newer),
            "A stale retry touched the newer navigation"
        );
        assert!(
            !ui.get_conversation_focused() && self.live.focused_view(&self.room).is_none(),
            "A stale retry focused the conversation"
        );
        assert!(
            ui.get_thread_root_id() == thread,
            "A stale retry changed the thread"
        );
        check!(link_follow_up_stale_retry_rejected);

        // The same continuation with nothing navigated since: the row arrives
        // before the retry, which scrolls to it under the receipt's epoch.
        let asked = self.joined_open(root).await;
        let receipt = self.live.current_navigation_epoch();
        let rows = ui.get_messages();
        let jumps = ui.get_timeline_jump_request();
        self.follow_link(&selection, root, &asked);
        ui.set_messages(rows);
        advance_retries(ui).await;
        assert_eq!(
            ui.get_timeline_jump_request(),
            jumps.wrapping_add(1),
            "The owned retry must scroll once"
        );
        assert!(ui.get_timeline_jump_event_id().as_str() == root);
        assert!(
            self.live.navigation_is_current(receipt),
            "The continuation took no navigation"
        );
        check!(link_follow_up_owned_retry);

        // The focused view held after its load, before its commit, while the
        // person's newer intent arrives: a security step begun and ended, a
        // protected surface (Settings), a newer link replacing this one, or a
        // modal opened and closed again before the release (the quick
        // switcher, the account menu). Nothing of the view is installed,
        // shown or scrolled to, and the room's live feed and the newer state
        // stay: a modal's entry ended the message for good.
        let intents = [
            Intent::SecurityStep,
            Intent::Settings,
            Intent::NewerLink,
            Intent::QuickSwitcher,
            Intent::AccountMenu,
        ];
        for intent in intents {
            let asked = self.joined_open(root).await;
            let held = self.focus_held(&selection, root, &asked).await;
            let generation = self.live.generation.load(Ordering::Acquire);
            let jumps = ui.get_timeline_jump_request();
            match intent {
                Intent::SecurityStep => {
                    let step = self.live.security_gate.try_account_access();
                    assert!(step.is_some(), "An idle gate must begin a step");
                    drop(step);
                }
                Intent::Settings => {
                    ui.set_page(1);
                    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
                    settle(ui).await;
                }
                Intent::NewerLink => crate::matrix_links::clear(ui),
                Intent::QuickSwitcher => {
                    ui.invoke_open_quick_switcher();
                    assert!(ui.get_quick_switcher_open(), "The real entry must open it");
                    ui.set_quick_switcher_open(false);
                    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
                    settle(ui).await;
                }
                Intent::AccountMenu => {
                    ui.invoke_open_account_menu();
                    assert!(ui.get_account_menu_open(), "The real entry must open it");
                    ui.set_account_menu_open(false);
                    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
                    settle(ui).await;
                }
            }
            held.proceed();
            // A bounded window for the released load to reach its commit and
            // queue anything, then drain the loop in order.
            tokio::time::sleep(Duration::from_secs(2)).await;
            settle(ui).await;
            advance_retries(ui).await;
            assert!(
                self.live.focused_view(&self.room).is_none(),
                "A refused focused view was installed"
            );
            assert_eq!(
                self.live.generation.load(Ordering::Acquire),
                generation,
                "A refused focused view replaced the live feed"
            );
            let key = Some((self.account.clone(), self.room.clone()));
            assert!(
                *self.live.feed_key.lock().await == key,
                "The room's live feed must stay"
            );
            assert!(
                !ui.get_conversation_focused(),
                "Refused focused rows were shown"
            );
            assert_eq!(
                ui.get_timeline_jump_request(),
                jumps,
                "A refused focused view scrolled"
            );
            if intent == Intent::Settings {
                assert_eq!(ui.get_page(), 1, "The person must stay in Settings");
                ui.set_page(0);
                i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
                settle(ui).await;
            }
        }
        check!(link_follow_up_held_focus_rejected);

        // Unchanged since its click, the held view commits, shows its rows,
        // and its retry scrolls to the message.
        let asked = self.joined_open(root).await;
        let held = self.focus_held(&selection, root, &asked).await;
        let jumps = ui.get_timeline_jump_request();
        held.proceed();
        let focused = |ui: &AppWindow| ui.get_conversation_focused() && row(ui);
        wait_for_ui(ui, "The owned focused view did not show its rows", focused).await;
        settle(ui).await;
        assert!(self.live.focused_view(&self.room).is_some());
        advance_retries(ui).await;
        assert_eq!(
            ui.get_timeline_jump_request(),
            jumps.wrapping_add(1),
            "The owned focused view must scroll once"
        );
        assert!(ui.get_timeline_jump_event_id().as_str() == root);
        check!(link_follow_up_owned_focus);

        // Back to the room's live conversation.
        self.select_room().await;
        let live_rows = |ui: &AppWindow| !ui.get_conversation_focused() && row(ui);
        wait_for_ui(ui, "The live conversation did not return", live_rows).await;
    }

    /// The join's own open of this (already joined) room, as join_target_for
    /// runs it once sync shows a confirmed join: under the join's click-time
    /// capture it takes the next navigation, shows the room and records its
    /// receipt. No join request is sent. Returns once the reopened feed shows
    /// `root` and has had a moment to settle.
    async fn joined_open(&self, root: &str) -> JoinAsked {
        let asked = self.live.join_asked();
        let selection = self.manager.account_selection(&self.account).unwrap();
        let (live, weak) = (self.live.clone(), self.ui.as_weak());
        let (room, joining) = (self.room.clone(), asked.clone());
        let opened = on_runtime(&self.runtime, async move {
            let open = JoinedNavigation::Room;
            live.open_joined(selection, room, open, joining, weak).await
        })
        .await;
        assert!(
            matches!(opened, Ok(true)),
            "The join's own open must show the room"
        );
        assert!(
            asked.owned_open(&self.live).is_some(),
            "Its receipt must own the screen"
        );
        let row = |ui: &AppWindow| has_event_id(&ui.get_messages(), root);
        wait_for_ui(&self.ui, "The joined room did not show the row", row).await;
        // Let the reopened feed's first publications land before the row
        // is taken out of the window.
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle(&self.ui).await;
        asked
    }

    /// The link's confirmed join completes with the message's row not (yet)
    /// in the window, as right after the join's own open.
    fn follow_link(&self, selection: &AccountSelection, event: &str, asked: &JoinAsked) {
        let ui = &self.ui;
        ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
        crate::matrix_links::tests::finish_joined_link(
            ui,
            &self.live,
            &self.runtime,
            selection.clone(),
            &self.room,
            event,
            asked,
        );
    }

    /// Follow the link with the row absent until its continuation opens the
    /// conversation around the message, and hold that focused view after its
    /// load, before its commit.
    async fn focus_held(
        &self,
        selection: &AccountSelection,
        event: &str,
        asked: &JoinAsked,
    ) -> Arc<PausePoint> {
        let held = self.live.pauses.install("focus.before_commit");
        self.follow_link(selection, event, asked);
        advance_retries(&self.ui).await;
        let loaded = tokio::time::timeout(Duration::from_secs(60), held.entered()).await;
        assert!(
            loaded.is_ok(),
            "The continuation must load the focused view"
        );
        held
    }

    fn shows_scope(&self, ui: &AppWindow) -> bool {
        let account_visible = ui.get_active_account_local_id().as_str() == self.account;
        account_visible && ui.get_active_room_id().as_str() == self.room
    }

    /// The steps of `account_activated` without its background Space scan,
    /// whose closing refresh could legitimately reopen the room meanwhile.
    async fn activate(&self) {
        let (live, weak) = (self.live.clone(), self.ui.as_weak());
        let account = self.account.clone();
        on_runtime(&self.runtime, async move {
            let _transition = live.account_transition.lock().await;
            live.invalidate_navigation();
            live.clear_feed().await;
            live.is_live.store(true, Ordering::Release);
            let selection = live.manager.account_selection(&account).unwrap();
            live.adopt_recovered_sends(selection, weak.clone()).await;
            let refreshed = live.refresh(weak).await;
            refreshed.expect("The account snapshot must apply");
        })
        .await;
    }

    /// The production room-open path: the click handler invalidates
    /// navigation on the UI thread, then `select_room_for` projects the room
    /// and awaits the actual `open_room` for it.
    async fn select_room(&self) {
        let epoch = self.live.invalidate_navigation();
        let (live, weak) = (self.live.clone(), self.ui.as_weak());
        let (account, room) = (self.account.clone(), self.room.clone());
        on_runtime(&self.runtime, async move {
            let selected = live.select_room_for(&account, room, epoch, weak).await;
            assert!(matches!(selected, Ok(true)), "The room must be selected");
        })
        .await;
    }

    /// The production thread-open path after the Slint handler has named the
    /// root and cleared the reply list (see `open-thread` in the window).
    async fn open_thread(&self, root: &str) {
        let ui = &self.ui;
        ui.set_thread_root_id(root.into());
        ui.set_thread_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
        let request = self.live.begin_thread_request();
        let (live, weak) = (self.live.clone(), ui.as_weak());
        let (account, room, root) = (self.account.clone(), self.room.clone(), root.to_owned());
        on_runtime(&self.runtime, async move {
            let opened = live.open_thread(account, room, root, request, weak).await;
            opened.expect("The thread feed must open");
        })
        .await;
    }

    /// Add a message through the core, as any client of this account could;
    /// the controller only observes it through its feed.
    async fn send_room(&self, body: &str) {
        let manager = self.manager.clone();
        let (account, room, body) = (self.account.clone(), self.room.clone(), body.to_owned());
        on_runtime(&self.runtime, async move {
            let sent = manager.send_text_for(&account, &room, body).await;
            sent.expect("Cannot send the room marker");
        })
        .await;
    }

    async fn send_reply(&self, root: &str, body: &str) {
        let manager = self.manager.clone();
        let (account, room) = (self.account.clone(), self.room.clone());
        let (root, body) = (root.to_owned(), body.to_owned());
        on_runtime(&self.runtime, async move {
            let sent = manager
                .send_thread_text_for(&account, &room, &root, body)
                .await;
            sent.expect("Cannot send the thread marker");
        })
        .await;
    }

    /// The account's own cached SDK room timeline holds the server copy.
    async fn wait_for_core_room_event(&self, body: &str) {
        let manager = self.manager.clone();
        let (room, body) = (self.room.clone(), body.to_owned());
        on_runtime(&self.runtime, async move {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let snapshot = manager.messages_snapshot(&room).await.unwrap_or_default();
                if holds(&snapshot, &body) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "The core room timeline lacks the marker"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
    }

    /// The account's own cached SDK thread timeline holds the server copy.
    async fn wait_for_core_thread_event(&self, root: &str, body: &str) {
        let manager = self.manager.clone();
        let (account, room) = (self.account.clone(), self.room.clone());
        let (root, body) = (root.to_owned(), body.to_owned());
        on_runtime(&self.runtime, async move {
            let feed = manager
                .thread_message_feed_for(&account, &room, &root)
                .await;
            let feed = feed.expect("Cannot open the core thread timeline");
            let deadline = Instant::now() + Duration::from_secs(60);
            while !holds(&feed.snapshot(), &body) {
                assert!(
                    Instant::now() < deadline,
                    "The core thread timeline lacks the marker"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
    }

    /// Revocation that bypasses the controller: a second account signs in on
    /// this profile and becomes the active account.
    async fn switch_away(&self, other: FixtureAccount) -> String {
        let manager = self.manager.clone();
        on_runtime(&self.runtime, async move {
            let added = manager
                .add_password_account(&other.homeserver_url, &other.username, &other.password)
                .await;
            added.expect("Second login failed").id
        })
        .await
    }

    /// Reselect the first account in the core only, then wait until its room
    /// is covered for sending again under the new lease.
    async fn switch_back(&self) {
        let manager = self.manager.clone();
        let (account, room) = (self.account.clone(), self.room.clone());
        on_runtime(&self.runtime, async move {
            let switched = manager.switch_to(&account).await;
            switched.expect("Cannot switch back");
            wait_for_room(&manager, &room).await;
        })
        .await;
    }

    /// Bounded: revocation wakes every lease waiter immediately.
    async fn wait_until_stopped(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while feeds_running(&self.live).await != (false, false) {
            assert!(Instant::now() < deadline, "Revocation must end both feeds");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Remove both fixture devices and release the profile the way the app
    /// does: controller tasks drop their manager clones once they end.
    async fn close(self, directory: tempfile::TempDir, other: String) {
        stage!(cleanup);
        let manager = self.manager.clone();
        let account = self.account.clone();
        on_runtime(&self.runtime, async move {
            for id in [account, other] {
                let removed = manager.logout_account(&id).await;
                removed.expect("Cannot remove a fixture account");
            }
            manager.stop().await;
        })
        .await;
        let manager = self.manager;
        drop(self.live);
        drop(self.ui);
        let deadline = Instant::now() + Duration::from_secs(60);
        while Arc::strong_count(&manager) != 1 {
            assert!(
                Instant::now() < deadline,
                "Controller tasks must release the profile"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(manager);
        directory.close().unwrap();
        check!(cleanup_complete);
        stage!(complete);
    }
}

/// Signals once when dropped, so a test can observe that a future holding it
/// was dropped rather than inferring it from elapsed time.
struct DropWitness(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropWitness {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

/// Pure regression for `OwnedTask`: no Slint backend, network or fixture.
/// Cancelling the caller of `on_runtime` mid-await must abort the spawned
/// work so the runtime drops its future (and what it holds) while the runtime
/// is still running, instead of leaving it detached. The wait below is for
/// that drop signal, bounded; abort itself does not wait for completion.
#[test]
fn cancelling_runtime_work_aborts_and_drops_the_spawned_future() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.handle().clone();
    runtime.block_on(async move {
        let (started, running) = tokio::sync::oneshot::channel();
        let (dropped, released) = tokio::sync::oneshot::channel();
        let witness = DropWitness(Some(dropped));
        let mut caller = Box::pin(on_runtime(&handle, async move {
            let _witness = witness;
            let _ = started.send(());
            // Never completes on its own: only cancellation can drop it.
            std::future::pending::<()>().await
        }));
        // The first poll spawns the work and then waits for its result.
        assert!(futures_util::poll!(caller.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the spawned work must start within its bound")
            .expect("the spawned work must signal startup");
        // Cancel the caller, as the outer timeout or the UI scenario would.
        drop(caller);
        let signal = tokio::time::timeout(Duration::from_secs(5), released).await;
        assert!(
            matches!(signal, Ok(Ok(()))),
            "cancelling the caller must drop the spawned future"
        );
    });
}
