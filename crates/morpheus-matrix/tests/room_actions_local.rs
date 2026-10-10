//! Opt-in room workflow test against the two disposable local Synapse servers.
//! The fixture path contains credentials; this test never prints its contents.

use std::{fs, path::PathBuf, time::Duration};

use matrix_sdk::{
    Client,
    ruma::{
        EventEncryptionAlgorithm, RoomAliasId, RoomId,
        api::client::state::get_state_event_for_key,
        events::{StateEventType, room::encryption::RoomEncryptionEventContent},
    },
};
use morpheus_matrix::{AccountManager, RoomMembership};
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
}

async fn wait_for_membership(
    manager: &AccountManager,
    room_id: &str,
    expected: Option<RoomMembership>,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let current = manager
            .rooms_snapshot()
            .await
            .expect("Cannot read local room list")
            .into_iter()
            .find(|room| room.room_id == room_id)
            .map(|room| room.membership);
        if current == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for room membership {expected:?}; last: {current:?}"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn assert_server_room_encrypted(client: &Client, room_id: &str) {
    let response = client
        .send(get_state_event_for_key::v3::Request::new(
            RoomId::parse(room_id).expect("Created room ID is invalid"),
            StateEventType::RoomEncryption,
            String::new(),
        ))
        .await
        .expect("Created private room lacks server encryption state");
    let content: RoomEncryptionEventContent = serde_json::from_str(response.event_or_content.get())
        .expect("Created private room has invalid encryption state");
    assert_eq!(content.algorithm, EventEncryptionAlgorithm::MegolmV1AesSha2);
}

#[test]
fn create_join_and_respond_to_invites() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped local room workflow test; set MORPHEUS_MATRIX_TEST_ACCOUNTS.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(credentials_path)).expect("Cannot read local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let alice = fixture.account("a_primary");
    let bob = fixture.account("a_second_device");
    let root = std::env::temp_dir().join(format!("morpheus-room-actions-{}", Uuid::new_v4()));
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
        eprintln!("Room test: starting account sign-in");
        let creator =
            AccountManager::open(root.join("creator")).expect("Cannot open creator store");
        let invitee =
            AccountManager::open(root.join("invitee")).expect("Cannot open invitee store");
        let creator_account = creator
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Cannot sign in creator");
        let invitee_account = invitee
            .add_password_account(&bob.homeserver_url, &bob.username, &bob.password)
            .await
            .expect("Cannot sign in invitee");
        let creator_client = creator
            .active_client()
            .await
            .expect("Creator client missing");
        eprintln!("Room test: both accounts signed in");

        let declined = creator
            .create_private_room(&format!("Morpheus decline {}", Uuid::new_v4()))
            .await
            .expect("Cannot create private room");
        assert_server_room_encrypted(&creator_client, &declined).await;
        wait_for_membership(&creator, &declined, Some(RoomMembership::Joined)).await;
        creator
            .invite_user(&declined, &bob.user_id)
            .await
            .expect("Cannot invite second account");
        wait_for_membership(&invitee, &declined, Some(RoomMembership::Invited)).await;
        invitee
            .reject_invite(&declined)
            .await
            .expect("Cannot decline room invitation");
        wait_for_membership(&invitee, &declined, None).await;
        eprintln!("Room test: invitation declined");

        let accepted = creator
            .create_private_room(&format!("Morpheus accept {}", Uuid::new_v4()))
            .await
            .expect("Cannot create second private room");
        assert_server_room_encrypted(&creator_client, &accepted).await;
        creator
            .invite_user(&accepted, &bob.user_id)
            .await
            .expect("Cannot invite second account to second room");
        wait_for_membership(&invitee, &accepted, Some(RoomMembership::Invited)).await;
        invitee
            .accept_invite(&accepted)
            .await
            .expect("Cannot accept room invitation");
        wait_for_membership(&invitee, &accepted, Some(RoomMembership::Joined)).await;
        eprintln!("Room test: invitation accepted");

        let joined = creator
            .create_private_room(&format!("Morpheus join {}", Uuid::new_v4()))
            .await
            .expect("Cannot create third private room");
        assert_server_room_encrypted(&creator_client, &joined).await;
        creator
            .invite_user(&joined, &bob.user_id)
            .await
            .expect("Cannot invite second account to third room");
        wait_for_membership(&invitee, &joined, Some(RoomMembership::Invited)).await;
        assert_eq!(
            invitee
                .join_room(&joined)
                .await
                .expect("Cannot join by room ID"),
            joined
        );
        wait_for_membership(&invitee, &joined, Some(RoomMembership::Joined)).await;
        eprintln!("Room test: room ID joined");

        let aliased = creator
            .create_private_room(&format!("Morpheus alias {}", Uuid::new_v4()))
            .await
            .expect("Cannot create alias test room");
        assert_server_room_encrypted(&creator_client, &aliased).await;
        creator
            .invite_user(&aliased, &bob.user_id)
            .await
            .expect("Cannot invite second account to alias test room");
        wait_for_membership(&invitee, &aliased, Some(RoomMembership::Invited)).await;
        eprintln!("Room test: alias room invitation visible");
        let alias_owner = Client::builder()
            .homeserver_url(&alice.homeserver_url)
            .build()
            .await
            .expect("Cannot build alias owner client");
        alias_owner
            .matrix_auth()
            .login_username(&alice.username, &alice.password)
            .send()
            .await
            .expect("Cannot sign in alias owner");
        let dm = creator
            .create_dm(&bob.user_id)
            .await
            .expect("Cannot create direct room");
        for room_id in [&declined, &accepted, &joined, &aliased, &dm] {
            assert_server_room_encrypted(&alias_owner, room_id).await;
        }
        eprintln!("Private rooms and DM have server-confirmed Megolm state.");
        let server_name = alice
            .user_id
            .rsplit_once(':')
            .expect("Fixture user ID lacks server name")
            .1;
        let alias = RoomAliasId::parse(format!(
            "#morpheus-{}:{server_name}",
            Uuid::new_v4().simple()
        ))
        .expect("Cannot construct local room alias");
        let aliased_id = RoomId::parse(&aliased).expect("Server returned invalid room ID");
        alias_owner
            .create_room_alias(&alias, &aliased_id)
            .await
            .expect("Cannot register local room alias");
        eprintln!("Room test: alias registered");
        assert_eq!(
            invitee
                .join_room(alias.as_str())
                .await
                .expect("Cannot join by room alias"),
            aliased
        );
        wait_for_membership(&invitee, &aliased, Some(RoomMembership::Joined)).await;
        eprintln!("Room alias resolved and joined by invited account.");
        alias_owner
            .remove_room_alias(&alias)
            .await
            .expect("Cannot remove test room alias");
        alias_owner
            .logout()
            .await
            .expect("Cannot log out alias owner test session");

        let space = creator
            .create_space(&format!("Morpheus space {}", Uuid::new_v4()))
            .await
            .expect("Cannot create private Space");
        assert!(
            space.starts_with('!'),
            "Space create response lacked a room ID"
        );
        eprintln!("Private Space create request succeeded.");

        invitee
            .logout_account(&invitee_account.id)
            .await
            .expect("Cannot remove invitee test session");
        creator
            .logout_account(&creator_account.id)
            .await
            .expect("Cannot remove creator test session");
        invitee.stop().await;
        creator.stop().await;
    });
    fs::remove_dir_all(&root).expect("Cannot remove local test stores");
    eprintln!(
        "Local room actions passed: encrypted private rooms/DM, private Space, invite accept/decline, room-ID and alias joins."
    );
}
