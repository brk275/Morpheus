//! Opt-in actual-path coverage for Start a conversation while a creation is
//! busy.
//!
//! Like the live timeline controller scenario, this never creates a Slint
//! backend: the single real-window harness in the account access UI tests
//! calls `run_if_configured` on its event loop (mock time) after its own
//! scenarios. Without MORPHEUS_MATRIX_TEST_ACCOUNTS (the disposable local
//! fixture from `scripts/local_matrix_env.py`) it returns at once. With it,
//! the fixture's `a_primary` account signs in on a fresh profile, and the
//! production callbacks drive a real generated window registered through
//! `people_directory::register`:
//!
//! open -> search (exact ID row) -> choose (local known-direct lookup) ->
//! ask -> confirm. The confirmed creation is admitted (busy) and then held
//! by the test-build pause point `people.before_create`, before its request.
//! While it is held, the armed debounce Timer's interval elapses, Enter is
//! pressed in the search field, and the search, choose, ask, open-known,
//! confirm and reopen callbacks are invoked directly (as a bypassed Slint
//! guard would): none bumps the operation, changes a row, or cancels the
//! creation. Released, the creation's own completion clears busy.
//!
//! The target is a fresh, never-registered user ID on the fixture's first
//! homeserver, so the local lookup always finds no conversation; the
//! homeserver may refuse the invite. Every outcome (created, refused,
//! unknown) is a completion: the regression asserts that the completion
//! applies and clears busy, not which outcome the homeserver chose.

use std::time::{Duration, Instant};

use slint::platform::{Key, WindowEvent};

use super::*;
use crate::voice_spaces::VoiceSpaces;

const PAUSE: &str = "people.before_create";

struct Fixture {
    homeserver_url: String,
    username: String,
    password: String,
    server_name: String,
}

fn fixture() -> Option<Fixture> {
    let path = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS")?;
    let data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).expect("Cannot read local fixture"))
            .expect("Invalid local fixture");
    let entry = data["accounts"]
        .as_array()
        .expect("fixture accounts")
        .iter()
        .find(|entry| entry["label"].as_str() == Some("a_primary"))
        .expect("fixture account");
    let user_id = entry["user_id"].as_str().expect("fixture user ID");
    let (_, server_name) = user_id.split_once(':').expect("fixture server name");
    Some(Fixture {
        homeserver_url: entry["homeserver_url"].as_str().unwrap().to_owned(),
        username: entry["username"].as_str().unwrap().to_owned(),
        password: entry["password"].as_str().unwrap().to_owned(),
        server_name: server_name.to_owned(),
    })
}

/// Run the scenario on the harness's event loop when a local fixture is
/// configured. Called from inside `slint::spawn_local`.
pub(crate) async fn run_if_configured(runtime: Handle) {
    let Some(fixture) = fixture() else {
        eprintln!(
            "Skipped live people busy scenario; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
        );
        return;
    };
    tokio::time::timeout(Duration::from_secs(300), exercise(runtime, fixture))
        .await
        .expect("live people busy scenario must finish within its bound");
}

/// Core work on the Tokio runtime, awaited from the Slint event loop.
async fn on_runtime<T: Send + 'static>(
    runtime: &Handle,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    runtime.spawn(work).await.expect("runtime work must finish")
}

/// FIFO event-loop barrier, then instantiate conditionals and run their
/// handlers without advancing mock time.
async fn settle(ui: &AppWindow) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    ui.as_weak()
        .upgrade_in_event_loop(move |_| {
            let _ = sender.send(());
        })
        .expect("the harness event loop must accept callbacks");
    receiver.await.expect("the barrier callback must run");
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

/// Wait, bounded, until the window shows a condition.
async fn wait_for_ui(ui: &AppWindow, what: &'static str, ready: impl Fn(&AppWindow) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        settle(ui).await;
        if ready(ui) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn key(ui: &AppWindow, text: impl Into<SharedString>) {
    let text = text.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

/// What a refused step must leave exactly as it was.
#[derive(Debug, PartialEq)]
struct Held {
    operation: u64,
    epoch: u64,
    busy: bool,
    cancelled: bool,
    chosen: Option<String>,
    rows: Vec<String>,
    index: i32,
    query: String,
    status: String,
    chosen_id: String,
    open: bool,
    ui_busy: bool,
}

fn held(ui: &AppWindow) -> Held {
    let (operation, epoch, busy, cancelled, chosen) = with(|directory| {
        (
            directory.operation,
            directory.epoch,
            directory.busy,
            directory
                .cancellation
                .as_ref()
                .is_none_or(PeopleCancellation::is_cancelled),
            directory.chosen.clone(),
        )
    });
    Held {
        operation,
        epoch,
        busy,
        cancelled,
        chosen,
        rows: ui
            .get_people_items()
            .iter()
            .map(|item| item.user_id.to_string())
            .collect(),
        index: ui.get_people_index(),
        query: ui.get_people_query().to_string(),
        status: ui.get_people_status().to_string(),
        chosen_id: ui.get_people_chosen_id().to_string(),
        open: ui.get_people_open(),
        ui_busy: ui.get_people_busy(),
    }
}

async fn exercise(runtime: Handle, fixture: Fixture) {
    let directory = tempfile::tempdir().unwrap();
    let manager =
        Arc::new(morpheus_matrix::AccountManager::open(directory.path().join("profile")).unwrap());
    let voices = Arc::new(VoiceSpaces::new(morpheus_config::ConfigStore::at(
        directory.path().join("config.toml"),
    )));
    // A second generated window on the harness's one event loop, with the
    // production People registration.
    let ui = AppWindow::new().unwrap();
    let live = LiveUi::new(manager.clone(), runtime.clone(), ui.as_weak(), voices);
    register(&ui, live.clone(), runtime.clone());
    let account = {
        let manager = manager.clone();
        let (url, user, password) = (
            fixture.homeserver_url.clone(),
            fixture.username.clone(),
            fixture.password.clone(),
        );
        on_runtime(&runtime, async move {
            let added = manager.add_password_account(&url, &user, &password).await;
            added.expect("fixture login failed").id
        })
        .await
    };
    assert!(manager.account_selection(&account).is_some());
    // The live, unobstructed account the window opens for.
    live.is_live.store(true, Ordering::Release);
    ui.set_show_sign_in(false);
    ui.set_live_session(true);
    ui.set_active_account_local_id(account.as_str().into());
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.show().unwrap();
    ui.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    settle(&ui).await;

    // Open, then an exact ID: its row is offered at once.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let process = std::process::id();
    let target = format!("@morpheus-busy-{process}-{unique}:{}", fixture.server_name);
    ui.invoke_open_people(SharedString::new());
    settle(&ui).await;
    assert!(
        ui.get_people_open(),
        "the window opens for the live account"
    );
    ui.set_people_query(target.as_str().into());
    ui.invoke_people_search();
    assert_eq!(ui.get_people_items().row_count(), 1);
    assert_eq!(ui.get_people_items().row_data(0).unwrap().user_id, target);
    wait_for_ui(&ui, "the directory search must finish", |ui| {
        ui.get_people_status() != SEARCHING
    })
    .await;

    // Choose it: the local lookup knows no conversation, so a start is
    // offered and confirmed.
    ui.invoke_people_choose(0);
    wait_for_ui(&ui, "the known-direct lookup must finish", |ui| {
        ui.get_people_known() == KNOWN_NONE
    })
    .await;
    ui.invoke_people_ask_start();
    assert!(ui.get_people_confirm());

    // Arm the real debounce Timer from the search field, then confirm in
    // the same event-loop turn, before any time passes.
    let point = live.pauses.install(PAUSE);
    // Programmatic text keeps its caret; append through the real input.
    key(&ui, Key::End);
    key(&ui, "x");
    let typed = format!("{target}x");
    assert_eq!(ui.get_people_query().as_str(), typed);
    let before = with(|directory| directory.operation);
    ui.invoke_people_confirm_start();
    // Admitted: busy at once on both sides, one operation for the creation.
    assert!(with(|directory| directory.busy));
    assert!(ui.get_people_busy());
    assert_eq!(
        with(|directory| directory.operation),
        before.wrapping_add(1)
    );
    assert_eq!(ui.get_people_status().as_str(), CREATING);
    point.entered().await;
    settle(&ui).await;
    let admitted = held(&ui);
    assert!(admitted.busy && admitted.ui_busy && !admitted.cancelled && admitted.open);
    assert_eq!(admitted.chosen.as_deref(), Some(target.as_str()));

    // The armed Timer's interval elapses: no search.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1_000));
    settle(&ui).await;
    assert_eq!(held(&ui), admitted, "the debounce Timer while busy");
    // Enter in the search field: no search and no choice.
    key(&ui, Key::Return);
    settle(&ui).await;
    assert_eq!(held(&ui), admitted, "Enter while busy");
    // Typing while busy changes nothing (the field is disabled).
    key(&ui, "y");
    settle(&ui).await;
    assert_eq!(held(&ui), admitted, "typing while busy");
    // The production callbacks themselves, as a bypassed Slint guard would
    // call them: each is refused before it bumps or clears anything.
    ui.invoke_people_search();
    assert_eq!(held(&ui), admitted, "search callback while busy");
    ui.invoke_people_choose(0);
    assert_eq!(held(&ui), admitted, "choose callback while busy");
    ui.invoke_people_ask_start();
    ui.invoke_people_open_dm();
    assert_eq!(held(&ui), admitted, "ask and open-known while busy");
    ui.set_people_confirm(true);
    ui.invoke_people_confirm_start();
    ui.set_people_confirm(false);
    assert_eq!(held(&ui), admitted, "a second confirmation while busy");
    let other = format!("@morpheus-other:{}", fixture.server_name);
    ui.invoke_open_people(other.as_str().into());
    assert_eq!(held(&ui), admitted, "a reopen while busy");
    settle(&ui).await;
    assert_eq!(held(&ui), admitted);

    // Released: the creation's own completion applies and clears busy.
    point.proceed();
    wait_for_ui(&ui, "the creation's completion must clear busy", |ui| {
        !ui.get_people_busy()
    })
    .await;
    let done = held(&ui);
    assert!(!done.busy, "the completion clears the Rust busy state");
    assert_ne!(
        done.status, CREATING,
        "the completion replaced the creating status"
    );
    if done.open {
        // Refused, unknown, or created but not yet synced or listed: the
        // completion applied to the same window and operation, and the
        // window takes a new step again.
        assert_eq!(
            done.epoch, admitted.epoch,
            "no clear or reopen happened meanwhile"
        );
        assert_eq!(
            done.operation, admitted.operation,
            "the completion was current"
        );
        ui.invoke_people_search();
        assert_eq!(
            with(|directory| directory.operation),
            admitted.operation.wrapping_add(1),
            "a step after completion is admitted"
        );
    } else {
        // Created, synced and listed: the completion's own success path
        // closed the window and opened the conversation.
        assert_eq!(
            done.epoch,
            admitted.epoch.wrapping_add(1),
            "closed once, by the completion"
        );
        assert!(done.rows.is_empty() && done.query.is_empty());
    }
    clear_presentation(&ui);
    ui.hide().unwrap();

    on_runtime(&runtime, {
        let manager = manager.clone();
        async move {
            let removed = manager.logout_account(&account).await;
            removed.expect("Cannot remove the fixture account");
            manager.stop().await;
        }
    })
    .await;
    drop(live);
    drop(ui);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Arc::strong_count(&manager) != 1 {
        assert!(
            Instant::now() < deadline,
            "People tasks must release the profile"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(manager);
    directory.close().unwrap();
    eprintln!("\nMORPHEUS_LIVE_PEOPLE_CHECK busy_creation=true");
}
