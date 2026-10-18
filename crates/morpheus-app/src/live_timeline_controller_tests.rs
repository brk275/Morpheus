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

use std::future::Future;

use slint::ComponentHandle;

use super::*;

// Fixed tokens only: no account IDs, room IDs or message text.
macro_rules! stage {
    ($name:ident) => {
        eprintln!("\nMORPHEUS_LIVE_TIMELINE_STAGE {}", stringify!($name));
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
    tokio::time::timeout(Duration::from_secs(300), scenario)
        .await
        .expect("live timeline controller scenario must finish within its bound");
}

/// Run core work on the Tokio runtime and await it from the Slint event loop.
/// A panic in that work is re-raised here so the harness test fails with it.
async fn on_runtime<T, F>(runtime: &Handle, work: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    match runtime.spawn(work).await {
        Ok(value) => value,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => panic!("Live timeline runtime work was cancelled"),
    }
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
        assert!(*self.live.feed_key.lock().await == expected_key, "open_room must own the feed");

        stage!(room_delivery);
        let root_text = marker("live-timeline-root");
        self.send_room(&root_text).await;
        let published = |ui: &AppWindow| delivered(&ui.get_messages(), &root_text);
        wait_for_ui(&self.ui, "The room feed did not publish", published).await;
        let root = event_id_of(&self.ui.get_messages(), &root_text).unwrap();

        // Thread: the Slint handler names the root and clears the list, then
        // the actual open_thread subscribes to the SDK thread timeline.
        stage!(thread_delivery);
        self.open_thread(&root).await;
        let reply_text = marker("live-timeline-reply");
        self.send_reply(&root, &reply_text).await;
        let replied = |ui: &AppWindow| delivered(&ui.get_thread_messages(), &reply_text);
        wait_for_ui(&self.ui, "The thread feed did not publish", replied).await;
        assert!(
            !has_event_id(&self.ui.get_thread_messages(), &root),
            "The thread list must exclude its root"
        );

        // Both subscriptions published into a live window and keep waiting
        // for the next update: from here only their lease can end them.
        assert_eq!(feeds_running(&self.live).await, (true, true), "Feeds end too early");
        let lease = self.manager.account_selection(&self.account).unwrap();
        assert!(lease.is_current(), "The lease must be current before revocation");

        // A -> B, without the controller: another account signs in on this
        // profile and becomes active. Both feeds must end on their own.
        stage!(revocation);
        let other_id = self.switch_away(other).await;
        assert!(!lease.is_current(), "Signing in another account must end the lease");
        self.wait_until_stopped().await;
        settle(&self.ui).await;
        assert!(self.shows_scope(&self.ui), "The controller must not have been told");
        assert!(self.ui.get_thread_root_id().as_str() == root, "The thread must stay named");

        // A -> B -> A, without the controller: the same account and room are
        // visible again under a new lease. New server events reach the same
        // cached SDK timelines the ended feeds subscribed to, so a feed that
        // outlived its lease would publish them into this matching scope.
        stage!(renewal);
        self.switch_back().await;
        let renewed = self.manager.account_selection(&self.account).unwrap();
        assert!(renewed.is_current() && !lease.is_current(), "Renewal is a new lease");
        let late_room = marker("live-timeline-late-room");
        let late_reply = marker("live-timeline-late-reply");
        self.send_room(&late_room).await;
        self.send_reply(&root, &late_reply).await;
        self.wait_for_core_room_event(&late_room).await;
        self.wait_for_core_thread_event(&root, &late_reply).await;
        // Bounded window for any surviving subscriber to queue a callback,
        // then drain the event loop in order before checking.
        tokio::time::sleep(Duration::from_secs(2)).await;
        settle(&self.ui).await;
        settle(&self.ui).await;
        assert!(self.shows_scope(&self.ui), "The scope must still match");
        assert!(self.ui.get_thread_root_id().as_str() == root, "The thread must still match");
        assert!(
            !shows_body(&self.ui.get_messages(), &late_room),
            "A stale room feed published after A -> B -> A"
        );
        assert!(
            !shows_body(&self.ui.get_thread_messages(), &late_reply),
            "A stale thread feed published after A -> B -> A"
        );
        assert_eq!(feeds_running(&self.live).await, (false, false), "Ended feeds restarted");

        // Positive controls under the renewed lease: the same window and the
        // actual open paths deliver the late events, so the negative checks
        // above were not caused by a dead window or a broken feed.
        stage!(reopen);
        self.open_thread(&root).await;
        let reply_back = |ui: &AppWindow| delivered(&ui.get_thread_messages(), &late_reply);
        wait_for_ui(&self.ui, "The reopened thread did not publish", reply_back).await;
        self.select_room().await;
        let room_back = |ui: &AppWindow| delivered(&ui.get_messages(), &late_room);
        wait_for_ui(&self.ui, "The reselected room did not publish", room_back).await;
        assert!(feeds_running(&self.live).await.0, "The renewed room feed must keep running");
        other_id
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
            let sent = manager.send_thread_text_for(&account, &room, &root, body).await;
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
                assert!(Instant::now() < deadline, "The core room timeline lacks the marker");
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
            let feed = manager.thread_message_feed_for(&account, &room, &root).await;
            let feed = feed.expect("Cannot open the core thread timeline");
            let deadline = Instant::now() + Duration::from_secs(60);
            while !holds(&feed.snapshot(), &body) {
                assert!(Instant::now() < deadline, "The core thread timeline lacks the marker");
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
            assert!(Instant::now() < deadline, "Controller tasks must release the profile");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(manager);
        directory.close().unwrap();
        stage!(complete);
    }
}
