//! Opt-in public-profile regression against the disposable local Synapse
//! fixture used by `drafts_local.rs`. It changes only the fixture account's
//! own display name and avatar, then restores the name. It checks the exact
//! identity read back, refused invalid names and cancelled steps (nothing
//! sent, the server profile kept), and that an A -> B -> A selection cannot
//! change another account or revive an old editor's selection. Names are
//! synthetic markers; nothing here prints identifiers or credentials.

use std::{collections::HashMap, fs, path::PathBuf, time::Duration};

use morpheus_matrix::{AccountManager, ProfileError, RoomSettingsCancellation};
use serde::Deserialize;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
    rooms: HashMap<String, String>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    username: String,
    user_id: String,
    homeserver_url: String,
    password: String,
}

impl Fixture {
    fn account(&self, label: &str) -> &TestAccount {
        self.accounts
            .iter()
            .find(|account| account.label == label)
            .expect("Missing account")
    }
}

async fn wait_for_rooms(manager: &AccountManager, room_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if manager
            .rooms_snapshot()
            .await
            .expect("Cannot read rooms")
            .iter()
            .any(|room| room.room_id == room_id)
        {
            return;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for sync");
        sleep(Duration::from_millis(300)).await;
    }
}

#[test]
fn own_profile_changes_only_the_exact_selected_identity() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped own profile test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(credentials_path)).expect("Cannot read local fixture"),
    )
    .expect("Invalid local fixture");
    let root = std::env::temp_dir().join(format!("morpheus-own-profile-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create account data directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect account data directory");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot start local test runtime");
    runtime.block_on(async {
        let alice = fixture.account("a_primary");
        let carol = fixture.account("b_primary");
        let a_room = fixture.rooms.get("a_room").expect("Missing room");
        let b_room = fixture.rooms.get("b_room").expect("Missing room");
        let marker = format!("Synthetic {}", &Uuid::new_v4().simple().to_string()[..8]);

        let manager = AccountManager::open(&root).expect("Cannot open account manager");
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_rooms(&manager, a_room).await;
        let selection = manager.account_selection(&a_account.id).unwrap();
        let editor = RoomSettingsCancellation::new();

        // The exact identity being edited.
        let before = manager.own_profile_for(&selection, &editor).await.unwrap();
        assert_eq!(before.user_id, alice.user_id);
        assert_eq!(before.account_id, a_account.id);
        assert!(!before.homeserver.is_empty());

        // Invalid names are refused before any request; the profile stays.
        for invalid in ["", "two\nlines", &"x".repeat(300)] {
            assert_eq!(
                manager
                    .set_own_display_name_for(&selection, &editor, invalid)
                    .await
                    .unwrap_err(),
                ProfileError::InvalidName
            );
        }
        assert_eq!(
            manager.own_profile_for(&selection, &editor).await.unwrap(),
            before
        );

        // A single send, confirmed only by the read-back that follows it.
        let report = manager
            .set_own_display_name_for(&selection, &editor, &marker)
            .await
            .unwrap();
        assert!(report.confirmed() && report.send_error.is_none());
        let saved = report.profile.clone().expect("read back after the send");
        assert_eq!(saved.display_name.as_deref(), Some(marker.as_str()));
        assert_eq!(
            manager.own_profile_for(&selection, &editor).await.unwrap(),
            saved
        );
        // Removing an avatar is its own request and leaves the name alone.
        let removed = manager
            .remove_own_avatar_for(&selection, &editor)
            .await
            .unwrap();
        assert!(removed.confirmed());
        let removed = removed.profile.unwrap();
        assert!(!removed.has_avatar());
        assert_eq!(removed.display_name.as_deref(), Some(marker.as_str()));

        // Clearing the name is explicit and reads back as no display name.
        let cleared = manager
            .clear_own_display_name_for(&selection, &editor)
            .await
            .unwrap();
        assert!(cleared.confirmed());
        assert_eq!(cleared.profile.as_ref().unwrap().display_name, None);
        let saved = manager
            .set_own_display_name_for(&selection, &editor, &marker)
            .await
            .unwrap()
            .profile
            .unwrap();

        // An editor closed while its send may be in flight: the report is
        // either confirmed by the read-back or unknown, never resent, and
        // whatever it says matches what the server holds afterwards.
        let racing = RoomSettingsCancellation::new();
        let racing_name = format!("{marker} race");
        let (report, ()) = tokio::join!(
            manager.set_own_display_name_for(&selection, &racing, &racing_name),
            async {
                tokio::task::yield_now().await;
                racing.cancel();
            }
        );
        let held = manager.own_profile_for(&selection, &editor).await.unwrap();
        match report {
            Ok(report) if report.confirmed() => {
                assert_eq!(held.display_name.as_deref(), Some(racing_name.as_str()));
            }
            // Unknown or refused before sending: the server keeps one value
            // and nothing sends it again.
            Ok(_) | Err(_) => {
                let again = manager.own_profile_for(&selection, &editor).await.unwrap();
                assert_eq!(again, held, "no hidden resend changed the profile");
            }
        }
        let saved = manager
            .set_own_display_name_for(&selection, &editor, &marker)
            .await
            .unwrap()
            .profile
            .unwrap_or(saved);
        assert_eq!(saved.display_name.as_deref(), Some(marker.as_str()));

        // A closed editor sends nothing more.
        let closed = RoomSettingsCancellation::new();
        closed.cancel();
        assert_eq!(
            manager
                .set_own_display_name_for(&selection, &closed, "Synthetic closed")
                .await
                .unwrap_err(),
            ProfileError::Cancelled
        );
        assert_eq!(
            manager
                .own_profile_for(&selection, &editor)
                .await
                .unwrap()
                .display_name
                .as_deref(),
            Some(marker.as_str())
        );

        // A -> B -> A: the old selection can change neither account.
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        wait_for_rooms(&manager, b_room).await;
        assert_eq!(
            manager
                .set_own_display_name_for(&selection, &editor, "Synthetic stale")
                .await
                .unwrap_err(),
            ProfileError::AccountChanged
        );
        let b_selection = manager.account_selection(&b_account.id).unwrap();
        let b_profile = manager
            .own_profile_for(&b_selection, &RoomSettingsCancellation::new())
            .await
            .unwrap();
        assert_eq!(b_profile.user_id, carol.user_id);
        assert_ne!(b_profile.display_name.as_deref(), Some("Synthetic stale"));
        manager.switch_to(&a_account.id).await.unwrap();
        assert!(!selection.is_current());
        // The switcher label read is for the active account only: once A is
        // active again, B's selection reads nothing.
        assert_eq!(
            manager
                .own_profile_for(&b_selection, &RoomSettingsCancellation::new())
                .await
                .unwrap_err(),
            ProfileError::AccountChanged
        );
        let active = manager.account_selection(&a_account.id).unwrap();
        assert_eq!(
            manager
                .own_profile_for(&active, &RoomSettingsCancellation::new())
                .await
                .unwrap()
                .display_name
                .as_deref(),
            Some(marker.as_str())
        );
        assert_eq!(
            manager
                .remove_own_avatar_for(&selection, &editor)
                .await
                .unwrap_err(),
            ProfileError::AccountChanged
        );

        // Restore the fixture account's name under a fresh selection.
        let fresh = manager.account_selection(&a_account.id).unwrap();
        if let Some(original) = before.display_name.as_deref() {
            manager
                .set_own_display_name_for(&fresh, &RoomSettingsCancellation::new(), original)
                .await
                .unwrap();
        }
    });
    let _ = fs::remove_dir_all(&root);
}
