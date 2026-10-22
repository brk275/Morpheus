//! Opt-in public room directory test against the two loopback Synapse servers.
//! The fixture path contains credentials; this test never prints its contents.

use std::{fs, path::PathBuf, time::Duration};

use matrix_sdk::{
    Client,
    ruma::{
        RoomId,
        api::client::{
            directory::{get_room_visibility, set_room_visibility},
            room::{Visibility, create_room},
        },
    },
};
use morpheus_matrix::{
    AccountManager, JoinCancellation, JoinError, MatrixCoreError, MatrixLinkTarget,
    PublicRoomsPage, PublicRoomsQuery, parse_matrix_link_target,
};
use serde::Deserialize;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    username: String,
    homeserver_url: String,
    password: String,
    user_id: String,
}

impl Fixture {
    fn account(&self, label: &str) -> &TestAccount {
        self.accounts
            .iter()
            .find(|account| account.label == label)
            .unwrap_or_else(|| panic!("Missing local test account {label}"))
    }
}

async fn publish_room(client: &Client, name: &str, alias_localpart: &str) -> String {
    let mut request = create_room::v3::Request::new();
    request.name = Some(name.into());
    request.topic = Some("Morpheus disposable directory test".into());
    request.room_alias_name = Some(alias_localpart.into());
    request.preset = Some(create_room::v3::RoomPreset::PublicChat);
    request.visibility = Visibility::Public;
    let room = client
        .create_room(request)
        .await
        .expect("Cannot create public test room");
    // Some homeservers leave createRoom visibility private even when requested;
    // publish through the dedicated directory endpoint before searching.
    client
        .send(set_room_visibility::v3::Request::new(
            room.room_id().to_owned(),
            Visibility::Public,
        ))
        .await
        .expect("Cannot publish test room in directory");
    let visibility = client
        .send(get_room_visibility::v3::Request::new(
            room.room_id().to_owned(),
        ))
        .await
        .expect("Cannot inspect published room visibility");
    assert!(matches!(visibility.visibility, Visibility::Public));
    room.room_id().to_string()
}

async fn wait_for_results(
    manager: &AccountManager,
    account_id: &str,
    query: PublicRoomsQuery,
    expected_count: usize,
) -> PublicRoomsPage {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let result = manager
            .public_rooms_for(account_id, query.clone())
            .await
            .expect("Cannot query the local public room directory");
        if result.rooms.len() >= expected_count {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for public directory entries"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn unpublish_room(client: &Client, room_id: &str) {
    let id = RoomId::parse(room_id).expect("Server returned invalid room ID");
    client
        .send(set_room_visibility::v3::Request::new(
            id,
            Visibility::Private,
        ))
        .await
        .expect("Cannot remove test room from public directory");
}

#[test]
fn public_directory_is_paginated_and_account_scoped() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped local directory test; set MORPHEUS_MATRIX_TEST_ACCOUNTS.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(credentials_path)).expect("Cannot read local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let alice = fixture.account("a_primary");
    let bob = fixture.account("a_second_device");
    let carol = fixture.account("b_primary");
    let root = std::env::temp_dir().join(format!("morpheus-discovery-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create local test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect local test directory");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot start local test runtime");
    runtime.block_on(async {
        let manager = AccountManager::open(&root).expect("Cannot open local account store");
        let alice_account = manager.add_password_account(
            &alice.homeserver_url, &alice.username, &alice.password,
        ).await.expect("Cannot sign in Alice");
        let alice_client = manager.active_client().await.expect("No Alice client");
        let marker = Uuid::new_v4().simple().to_string();
        let prefix = format!("Morpheus discovery {marker}");
        let a_one = publish_room(&alice_client, &format!("{prefix} one"), &format!("morpheus_{marker}_one")).await;
        let a_two = publish_room(&alice_client, &format!("{prefix} two"), &format!("morpheus_{marker}_two")).await;
        let a_server = alice.user_id.rsplit_once(':').expect("Alice ID has no server name").1;
        let first = wait_for_results(&manager, &alice_account.id, PublicRoomsQuery {
            search_term: Some(prefix.clone()),
            server: Some(a_server.into()),
            limit: 1,
            ..Default::default()
        }, 1).await;
        assert_eq!(first.rooms.len(), 1);
        let next = first.next_batch.expect("Expected a second public directory page");
        let second = manager.public_rooms_for(&alice_account.id, PublicRoomsQuery {
            search_term: Some(prefix.clone()),
            server: Some(a_server.into()),
            since: Some(next),
            limit: 1,
        }).await.expect("Cannot fetch second directory page");
        assert_eq!(second.rooms.len(), 1);
        let found = [first.rooms[0].room_id.as_str(), second.rooms[0].room_id.as_str()];
        assert!(found.contains(&a_one.as_str()) && found.contains(&a_two.as_str()));
        assert!(first.rooms.iter().chain(second.rooms.iter()).all(|room| {
            room.join_rule == "public" && room.name.as_deref().is_some_and(|name| name.starts_with(&prefix))
        }));

        let alias = format!("#morpheus_{marker}_one:{a_server}");
        let resolved = manager.resolve_room_alias_for(&alice_account.id, &alias).await
            .expect("Cannot resolve local room alias");
        assert_eq!(resolved.room_id, a_one);
        let link = format!("https://matrix.to/#/%23morpheus_{marker}_one:{a_server}");
        assert!(matches!(parse_matrix_link_target(&link).unwrap(), MatrixLinkTarget::Room { room_or_alias_id, .. } if room_or_alias_id == alias));

        // Captured while Alice is shown; signing in Carol revokes it.
        let alice_selection = manager.account_selection(&alice_account.id)
            .expect("Alice selection missing");
        let carol_account = manager.add_password_account(
            &carol.homeserver_url, &carol.username, &carol.password,
        ).await.expect("Cannot sign in Carol");
        assert!(matches!(
            manager.public_rooms_for(&alice_account.id, PublicRoomsQuery::default()).await,
            Err(MatrixCoreError::SelectedAccountChanged)
        ));
        assert!(matches!(
            manager.resolve_room_alias_for(&alice_account.id, &alias).await,
            Err(MatrixCoreError::SelectedAccountChanged)
        ));
        let carol_client = manager.active_client().await.expect("No Carol client");
        let b_name = format!("Morpheus discovery B {marker}");
        let b_room = publish_room(&carol_client, &b_name, &format!("morpheus_{marker}_b")).await;
        let b_server = carol.user_id.rsplit_once(':').expect("Carol ID has no server name").1;
        let b_page = wait_for_results(&manager, &carol_account.id, PublicRoomsQuery {
            search_term: Some(b_name),
            server: Some(b_server.into()),
            ..Default::default()
        }, 1).await;
        assert!(b_page.rooms.iter().any(|room| room.room_id == b_room));
        assert!(b_page.rooms.iter().all(|room| room.room_id != a_one && room.room_id != a_two));
        assert!(matches!(
            manager.public_rooms_for(&carol_account.id, PublicRoomsQuery {
                server: Some("https://not-a-server/".into()),
                ..Default::default()
            }).await,
            Err(MatrixCoreError::InvalidDirectoryQuery(_))
        ));

        unpublish_room(&carol_client, &b_room).await;
        let bob_account = manager.add_password_account(
            &bob.homeserver_url, &bob.username, &bob.password,
        ).await.expect("Cannot sign in Bob");
        let routed_link = format!("{link}?via={a_server}");
        let target = parse_matrix_link_target(&routed_link).expect("Cannot parse routed room link");
        // A departed selection, or a caller check refusing at the hand-off,
        // sends nothing.
        assert_eq!(
            manager.join_room_for(&alice_selection, &target, &JoinCancellation::new(), || true)
                .await,
            Err(JoinError::AccountChanged)
        );
        let bob_selection = manager.account_selection(&bob_account.id)
            .expect("Bob selection missing");
        assert_eq!(
            manager.join_room_for(&bob_selection, &target, &JoinCancellation::new(), || false)
                .await,
            Err(JoinError::Blocked)
        );
        assert_eq!(
            manager.join_room_for(&bob_selection, &target, &JoinCancellation::new(), || true).await
                .expect("Cannot join discovered room from matrix.to link"),
            a_one
        );
        // The room is shown as joined once sync says so, not before.
        assert_eq!(
            manager.await_joined_for(&bob_selection, &a_one, &JoinCancellation::new()).await,
            Some(false)
        );
        manager.switch_to(&alice_account.id).await.expect("Cannot return to Alice");
        unpublish_room(&alice_client, &a_one).await;
        unpublish_room(&alice_client, &a_two).await;
        manager.logout_account(&bob_account.id).await.expect("Cannot remove Bob test session");
        manager.logout_account(&carol_account.id).await.expect("Cannot remove Carol test session");
        manager.logout_account(&alice_account.id).await.expect("Cannot remove Alice test session");
        manager.stop().await;
    });
    fs::remove_dir_all(&root).expect("Cannot remove local test stores");
    eprintln!(
        "Local public directory passed: two-page search, alias resolution, routed link join, account-scoped servers."
    );
}
