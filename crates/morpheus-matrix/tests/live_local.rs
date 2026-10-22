//! Opt-in acceptance test against scripts/local_matrix_env.py's two local servers.
//! Run with MORPHEUS_MATRIX_TEST_ACCOUNTS pointing to its mode-0600 JSON file.
#![recursion_limit = "512"]

#[path = "support/opened_room.rs"]
mod opened_room;

use std::{collections::HashMap, fs, path::PathBuf, time::Duration};

use morpheus_matrix::{
    AccountInfo, AccountManager, MatrixCoreError, RoomInfo, SyncBackend, SyncPolicy, SyncStatus,
};
use serde::Deserialize;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

// Only source-defined identifiers enter the hosted runner's public diagnostics.
// Start a fresh line because libtest can leave its test-name prefix unfinished.
macro_rules! phase {
    ($name:ident) => {
        eprintln!("\nMORPHEUS_LIVE_STAGE {}", stringify!($name));
    };
}

#[path = "live_local/account_access.rs"]
mod account_access;

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
    rooms: HashMap<String, String>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    user_id: String,
    username: String,
    homeserver_url: String,
    password: String,
}

impl Fixture {
    fn account(&self, label: &str) -> &TestAccount {
        self.accounts
            .iter()
            .find(|account| account.label == label)
            .unwrap_or_else(|| panic!("Missing local test account {label}"))
    }

    fn room(&self, key: &str) -> &str {
        self.rooms
            .get(key)
            .unwrap_or_else(|| panic!("Missing local test room {key}"))
    }
}

async fn wait_for_room(manager: &AccountManager, expected: &str) -> Vec<RoomInfo> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let rooms = manager
            .rooms_snapshot()
            .await
            .expect("Room snapshot failed");
        if rooms.iter().any(|room| room.room_id == expected) {
            opened_room::scan(manager, expected)
                .await
                .expect("Opened fixture room must be scanned");
            return rooms;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for local room {expected}"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_for_sync_backend(manager: &AccountManager, expected: SyncBackend) {
    let mut status = manager.subscribe_sync_status();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let current = status.borrow().clone();
        if current == (SyncStatus::Running { backend: expected }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {expected:?} sync; last status: {current:?}"
        );
        tokio::select! {
            _ = status.changed() => {},
            _ = sleep(Duration::from_millis(300)) => {},
        }
    }
}

async fn wait_for_message(manager: &AccountManager, room_id: &str, body: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let messages = manager
            .messages_snapshot(room_id)
            .await
            .expect("Encrypted room timeline failed");
        if messages.iter().any(|message| message.body == body) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for encrypted message"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_for_remote_echo(manager: &AccountManager, room_id: &str, body: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let messages = manager
            .messages_snapshot(room_id)
            .await
            .expect("Room timeline failed while waiting for remote echo");
        if messages
            .iter()
            .any(|message| message.body == body && !message.is_local_echo)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for remote echo"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_for_event_id(manager: &AccountManager, room_id: &str, body: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let messages = manager
            .messages_snapshot(room_id)
            .await
            .expect("Room timeline failed");
        if let Some(event_id) = messages
            .iter()
            .find(|message| message.body == body && !message.is_local_echo)
            .and_then(|message| message.event_id.clone())
        {
            return event_id;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for message event ID"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_for_reaction(
    manager: &AccountManager,
    room_id: &str,
    event_id: &str,
    reaction: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let messages = manager
            .messages_snapshot(room_id)
            .await
            .expect("Room timeline failed");
        if messages.iter().any(|message| {
            message.event_id.as_deref() == Some(event_id)
                && message.reaction_summary.contains(reaction)
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for encrypted reaction"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_for_reply_context(manager: &AccountManager, room_id: &str, body: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let messages = manager
            .messages_snapshot(room_id)
            .await
            .expect("Room timeline failed");
        if messages.iter().any(|message| {
            message.body == body
                && message
                    .reply_preview
                    .as_deref()
                    .is_some_and(|preview| !preview.is_empty())
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for reply context"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

async fn assert_cached_feed_contains(manager: &AccountManager, room_id: &str, body: &str) {
    let feed = manager
        .message_feed(room_id)
        .await
        .expect("Cannot subscribe to cached room timeline");
    assert!(
        feed.snapshot().iter().any(|message| message.body == body),
        "Account-scoped room feed lost a previously synced message"
    );
}

/// Shared fixture and store locations for the staged acceptance run.
struct Live<'a> {
    fixture: &'a Fixture,
    primary_store: PathBuf,
    peer_store: PathBuf,
    classic_store: PathBuf,
}

#[test]
fn two_homeservers_switch_restore_and_encrypted_restart() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!(
            "Skipped local Matrix acceptance test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in."
        );
        return;
    };
    phase!(fixture);
    let credentials_path = PathBuf::from(credentials_path);
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(&credentials_path).expect("Cannot read local Matrix credentials"),
    )
    .expect("Invalid local Matrix test fixture");
    let root = std::env::temp_dir().join(format!("morpheus-live-acceptance-{}", Uuid::new_v4()));
    let live = Live {
        fixture: &fixture,
        primary_store: root.join("primary"),
        peer_store: root.join("peer"),
        classic_store: root.join("classic"),
    };
    fs::create_dir_all(&root).expect("Cannot create local test data directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect local test data directory");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot start local test runtime");
    // Separate stages limit the temporaries retained in a single poll frame.
    // The previous monolithic test overflowed the hosted Linux test stack at
    // encrypted restart; see docs/VERIFICATION.md for the diagnosis and limits
    // of the local measurements. Add new stages as separate functions instead
    // of growing one async block. The runner's stack limits stay unchanged.
    runtime.block_on(async {
        let (manager, alice, a_body) = login_alice_and_send(&live).await;
        let carol = login_carol_and_switch(&live, manager, &alice, &a_body).await;
        let restored = restore_primary(&live, &alice, &carol, &a_body).await;
        let (peer, bob) = login_peer(&live).await;
        let (root_event_id, edited_body) =
            encrypted_send_and_edit(&live, &restored, &peer, &alice).await;
        encrypted_reply_and_reaction(&live, &restored, &peer, &alice, &root_event_id).await;
        encrypted_thread(&live, &restored, &peer, &alice, &bob, &root_event_id).await;
        action_isolation(&live, &restored, &alice, &carol, &root_event_id).await;
        let peer_restored = peer_restart(&live, peer, &bob, &edited_body).await;
        let restored = account_access::verify_existing_accounts(
            restored,
            &live.primary_store,
            &alice,
            &carol,
            live.fixture.room("a_room"),
            live.fixture.room("b_room"),
            &a_body,
        )
        .await;
        phase!(stop_restored);
        peer_restored.stop().await;
        restored.stop().await;
        classic_fallback(&live).await;
    });
    phase!(profile_cleanup);
    fs::remove_dir_all(&root).expect("Cannot remove local test account stores");
    phase!(complete);
    eprintln!(
        "Live local Matrix acceptance passed: account isolation, restore, both sync adapters, encrypted edit/reply/reaction/thread, message after restart, protected account activation, and local removal."
    );
}

/// Alice signs in on homeserver A, reaches sticky sliding sync and sends.
async fn login_alice_and_send(live: &Live<'_>) -> (AccountManager, AccountInfo, String) {
    let alice_fixture = live.fixture.account("a_primary");
    let a_room = live.fixture.room("a_room");
    let b_room = live.fixture.room("b_room");
    let encrypted_room = live.fixture.room("encrypted_room");

    let manager =
        AccountManager::open(live.primary_store.clone()).expect("Cannot open local account store");
    phase!(login_alice);
    let alice = manager
        .add_password_account(
            &alice_fixture.homeserver_url,
            &alice_fixture.username,
            &alice_fixture.password,
        )
        .await
        .expect("Alice Matrix login failed");
    assert_eq!(alice.user_id, alice_fixture.user_id);
    phase!(sync_alice);
    wait_for_room(&manager, a_room).await;
    wait_for_sync_backend(&manager, SyncBackend::SlidingSticky).await;
    let alice_rooms = wait_for_room(&manager, encrypted_room).await;
    assert!(
        alice_rooms
            .iter()
            .any(|room| room.room_id == encrypted_room)
    );
    assert!(!alice_rooms.iter().any(|room| room.room_id == b_room));
    phase!(send_alice);
    let a_body = format!("Morpheus account-switch acceptance {}", Uuid::new_v4());
    manager
        .send_text_for(&alice.id, a_room, a_body.clone())
        .await
        .expect("Alice room send failed");
    wait_for_remote_echo(&manager, a_room, &a_body).await;
    (manager, alice, a_body)
}

/// Carol signs in on homeserver B; switching keeps room lists, cached
/// messages and sends scoped to the selected account.
async fn login_carol_and_switch(
    live: &Live<'_>,
    manager: AccountManager,
    alice: &AccountInfo,
    a_body: &str,
) -> AccountInfo {
    let carol_fixture = live.fixture.account("b_primary");
    let a_room = live.fixture.room("a_room");
    let b_room = live.fixture.room("b_room");

    phase!(login_carol);
    let carol = manager
        .add_password_account(
            &carol_fixture.homeserver_url,
            &carol_fixture.username,
            &carol_fixture.password,
        )
        .await
        .expect("Carol Matrix login failed");
    assert_eq!(carol.user_id, carol_fixture.user_id);
    assert_ne!(alice.homeserver, carol.homeserver);
    phase!(sync_carol);
    let carol_rooms = wait_for_room(&manager, b_room).await;
    assert!(!carol_rooms.iter().any(|room| room.room_id == a_room));
    assert!(matches!(
        manager
            .send_text_for(&alice.id, b_room, "must never send")
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));

    phase!(switch_alice);
    manager
        .switch_to(&alice.id)
        .await
        .expect("Cannot switch to Alice");
    let alice_rooms = wait_for_room(&manager, a_room).await;
    assert!(!alice_rooms.iter().any(|room| room.room_id == b_room));
    phase!(cached_alice);
    assert_cached_feed_contains(&manager, a_room, a_body).await;
    wait_for_message(&manager, a_room, a_body).await;
    phase!(switch_carol);
    manager
        .switch_to(&carol.id)
        .await
        .expect("Cannot switch to Carol");
    phase!(stop_primary);
    manager.stop().await;
    drop(manager);
    carol
}

/// Reopening the primary store restores Carol, and switching back to Alice
/// serves her cached room feed before the encrypted room is awaited.
async fn restore_primary(
    live: &Live<'_>,
    alice: &AccountInfo,
    carol: &AccountInfo,
    a_body: &str,
) -> AccountManager {
    let carol_fixture = live.fixture.account("b_primary");
    let a_room = live.fixture.room("a_room");
    let b_room = live.fixture.room("b_room");
    let encrypted_room = live.fixture.room("encrypted_room");

    phase!(restore_carol);
    let restored = AccountManager::open(live.primary_store.clone())
        .expect("Cannot reopen local account store");
    let active = restored
        .restore_last_active()
        .await
        .expect("Cannot restore saved Matrix session")
        .expect("Saved active account missing");
    assert_eq!(active.id, carol.id);
    assert_eq!(active.user_id, carol_fixture.user_id);
    phase!(sync_restored);
    let carol_rooms = wait_for_room(&restored, b_room).await;
    assert!(!carol_rooms.iter().any(|room| room.room_id == a_room));

    phase!(restore_alice);
    restored
        .switch_to(&alice.id)
        .await
        .expect("Cannot restore Alice");
    phase!(cached_restored);
    assert_cached_feed_contains(&restored, a_room, a_body).await;
    wait_for_message(&restored, a_room, a_body).await;
    wait_for_room(&restored, encrypted_room).await;
    restored
}

/// Bob is Alice's second device on a separate store.
async fn login_peer(live: &Live<'_>) -> (AccountManager, AccountInfo) {
    let bob_fixture = live.fixture.account("a_second_device");
    let encrypted_room = live.fixture.room("encrypted_room");

    phase!(login_bob);
    let peer =
        AccountManager::open(live.peer_store.clone()).expect("Cannot open peer account store");
    let bob = peer
        .add_password_account(
            &bob_fixture.homeserver_url,
            &bob_fixture.username,
            &bob_fixture.password,
        )
        .await
        .expect("Bob Matrix login failed");
    assert_eq!(bob.user_id, bob_fixture.user_id);
    phase!(sync_bob);
    wait_for_room(&peer, encrypted_room).await;
    (peer, bob)
}

/// Encrypted send and edit from Alice's restored device reach Bob. Returns
/// the root event ID and the edited body.
async fn encrypted_send_and_edit(
    live: &Live<'_>,
    restored: &AccountManager,
    peer: &AccountManager,
    alice: &AccountInfo,
) -> (String, String) {
    let encrypted_room = live.fixture.room("encrypted_room");

    phase!(encrypted_send);
    let body = format!("Morpheus encrypted acceptance {}", Uuid::new_v4());
    restored
        .send_text_for(&alice.id, encrypted_room, body.clone())
        .await
        .expect("Encrypted send failed");
    wait_for_message(peer, encrypted_room, &body).await;

    phase!(encrypted_edit);
    let root_event_id = wait_for_event_id(restored, encrypted_room, &body).await;
    let edited_body = format!("Morpheus encrypted edit {}", Uuid::new_v4());
    restored
        .edit_text_for(
            &alice.id,
            encrypted_room,
            &root_event_id,
            edited_body.clone(),
        )
        .await
        .expect("Encrypted edit failed");
    wait_for_message(peer, encrypted_room, &edited_body).await;
    (root_event_id, edited_body)
}

/// An encrypted reply with context and a reaction on the root reach Bob.
async fn encrypted_reply_and_reaction(
    live: &Live<'_>,
    restored: &AccountManager,
    peer: &AccountManager,
    alice: &AccountInfo,
    root_event_id: &str,
) {
    let encrypted_room = live.fixture.room("encrypted_room");

    phase!(encrypted_reply);
    let reply_body = format!("Morpheus encrypted reply {}", Uuid::new_v4());
    restored
        .send_reply_for(&alice.id, encrypted_room, root_event_id, reply_body.clone())
        .await
        .expect("Encrypted reply failed");
    wait_for_message(peer, encrypted_room, &reply_body).await;
    wait_for_reply_context(peer, encrypted_room, &reply_body).await;

    phase!(encrypted_reaction);
    assert!(
        restored
            .toggle_reaction_for(&alice.id, encrypted_room, root_event_id, "❤️")
            .await
            .expect("Encrypted reaction failed")
    );
    wait_for_reaction(peer, encrypted_room, root_event_id, "❤️").await;
}

/// A tracked encrypted thread reply reaches Bob's thread feed and stays out
/// of the main room timeline.
async fn encrypted_thread(
    live: &Live<'_>,
    restored: &AccountManager,
    peer: &AccountManager,
    alice: &AccountInfo,
    bob: &AccountInfo,
    root_event_id: &str,
) {
    let encrypted_room = live.fixture.room("encrypted_room");

    phase!(encrypted_thread_send);
    let thread_body = format!("Morpheus encrypted thread {}", Uuid::new_v4());
    let queued_thread = restored
        .send_thread_formatted_tracked_for(
            &alice.id,
            encrypted_room,
            root_event_id,
            thread_body.clone(),
            morpheus_matrix::TextFormat::Plain,
        )
        .await
        .expect("Encrypted thread reply failed");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), queued_thread.wait_outcome())
            .await
            .expect("Thread server acknowledgement timed out"),
        morpheus_matrix::QueuedSendOutcome::Sent,
    );
    phase!(encrypted_thread_receive);
    let thread_feed = tokio::time::timeout(
        Duration::from_secs(30),
        peer.thread_message_feed_for(&bob.id, encrypted_room, root_event_id),
    )
    .await
    .expect("Thread timeline creation timed out")
    .expect("Thread timeline creation failed");
    let mut thread_updates = thread_feed.subscribe();
    let thread_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if thread_updates
            .borrow()
            .iter()
            .any(|message| message.body == thread_body)
        {
            break;
        }
        assert!(
            Instant::now() < thread_deadline,
            "Timed out waiting for encrypted thread reply"
        );
        tokio::select! {
            _ = thread_updates.changed() => {},
            _ = sleep(Duration::from_millis(300)) => {},
        }
    }
    drop(thread_feed);
    sleep(Duration::from_millis(700)).await;
    assert!(
        !peer
            .messages_snapshot(encrypted_room)
            .await
            .expect("Room timeline failed")
            .iter()
            .any(|message| message.body == thread_body),
        "Thread reply leaked into the main room timeline"
    );
}

/// With Carol selected, every Alice-scoped encrypted action is rejected.
async fn action_isolation(
    live: &Live<'_>,
    restored: &AccountManager,
    alice: &AccountInfo,
    carol: &AccountInfo,
    root_event_id: &str,
) {
    let encrypted_room = live.fixture.room("encrypted_room");

    phase!(action_isolation);
    restored
        .switch_to(&carol.id)
        .await
        .expect("Cannot switch to Carol for action isolation");
    assert!(matches!(
        restored
            .send_reply_for(&alice.id, encrypted_room, root_event_id, "blocked")
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    assert!(matches!(
        restored
            .edit_text_for(&alice.id, encrypted_room, root_event_id, "blocked")
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    assert!(matches!(
        restored
            .toggle_reaction_for(&alice.id, encrypted_room, root_event_id, "❤️")
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    assert!(matches!(
        restored
            .send_thread_text_for(&alice.id, encrypted_room, root_event_id, "blocked")
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    assert!(matches!(
        restored
            .thread_message_feed_for(&alice.id, encrypted_room, root_event_id)
            .await,
        Err(MatrixCoreError::SelectedAccountChanged)
    ));
    phase!(return_alice);
    restored
        .switch_to(&alice.id)
        .await
        .expect("Cannot switch back to Alice");
}

/// Bob's store is reopened; the edited encrypted message must survive the
/// restart before his session is removed.
async fn peer_restart(
    live: &Live<'_>,
    peer: AccountManager,
    bob: &AccountInfo,
    edited_body: &str,
) -> AccountManager {
    let encrypted_room = live.fixture.room("encrypted_room");

    phase!(stop_peer);
    peer.stop().await;
    drop(peer);
    phase!(restore_peer);
    let peer_restored =
        AccountManager::open(live.peer_store.clone()).expect("Cannot reopen peer account store");
    let peer_active = peer_restored
        .restore_last_active()
        .await
        .expect("Cannot restore peer Matrix session")
        .expect("Saved peer account missing");
    assert_eq!(peer_active.id, bob.id);
    phase!(encrypted_restart_room);
    wait_for_room(&peer_restored, encrypted_room).await;
    phase!(encrypted_restart_message);
    wait_for_message(&peer_restored, encrypted_room, edited_body).await;

    phase!(logout_bob);
    peer_restored
        .logout_account(&bob.id)
        .await
        .expect("Cannot remove peer test session");
    peer_restored
}

/// Both current Synapse fixtures advertise sliding sync. Force the classic
/// adapter on a fresh Carol session to verify the fallback's room list and
/// account isolation without relying on server age.
async fn classic_fallback(live: &Live<'_>) {
    let carol_fixture = live.fixture.account("b_primary");
    let a_room = live.fixture.room("a_room");
    let b_room = live.fixture.room("b_room");

    phase!(classic_login);
    let classic =
        AccountManager::open_with_sync_policy(live.classic_store.clone(), SyncPolicy::ClassicOnly)
            .expect("Cannot open classic-sync test store");
    let classic_password = account_access::prepare_protected_login(&classic).await;
    let classic_carol = classic
        .add_password_account(
            &carol_fixture.homeserver_url,
            &carol_fixture.username,
            &carol_fixture.password,
        )
        .await
        .expect("Carol classic-sync login failed");
    account_access::verify_new_login_paused(&classic, &classic_carol, &classic_password).await;
    phase!(classic_sync);
    wait_for_sync_backend(&classic, SyncBackend::Classic).await;
    let classic_rooms = wait_for_room(&classic, b_room).await;
    assert!(!classic_rooms.iter().any(|room| room.room_id == a_room));
    phase!(classic_logout);
    classic
        .logout_account(&classic_carol.id)
        .await
        .expect("Cannot remove classic-sync test session");
    classic
        .reset_app_password_after_removal()
        .await
        .expect("Cannot reset empty classic test profile");
    phase!(classic_stop);
    classic.stop().await;
}
