//! Opt-in encrypted draft acceptance test against two disposable local Synapse
//! homeservers. Fixture credentials are read from a mode-0600 file and never
//! printed by this test.
#![recursion_limit = "512"]

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use matrix_sdk::{
    ComposerDraft, ComposerDraftType,
    ruma::{EventId, RoomId},
};
use morpheus_matrix::{AccountManager, ComposeText, MentionTarget, QueuedSendOutcome, TextFormat};
use serde::Deserialize;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

// Fixed source tokens only: never identifiers, draft text or credential data.
macro_rules! phase {
    ($name:ident) => {
        eprintln!("\nMORPHEUS_DRAFTS_STAGE {}", stringify!($name));
    };
}

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

    fn room(&self, label: &str) -> &str {
        self.rooms.get(label).expect("Missing room")
    }
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) {
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
        assert!(Instant::now() < deadline, "Timed out waiting for room");
        sleep(Duration::from_millis(300)).await;
    }
}

fn contains_plaintext(path: &Path, needle: &[u8]) -> bool {
    let entries = fs::read_dir(path).expect("Cannot list account data");
    for entry in entries {
        let path = entry.expect("Cannot read account data entry").path();
        if path.is_dir() {
            if contains_plaintext(&path, needle) {
                return true;
            }
        } else if fs::read(&path)
            .expect("Cannot read account data file")
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return true;
        }
    }
    false
}

#[test]
fn drafts_survive_restart_without_plaintext_or_account_crossover() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped local draft test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    phase!(fixture);
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(credentials_path)).expect("Cannot read local fixture"),
    )
    .expect("Invalid local fixture");
    let root = std::env::temp_dir().join(format!("morpheus-drafts-{}", Uuid::new_v4()));
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
        let a_room = fixture.room("a_room");
        let b_room = fixture.room("b_room");
        let target = &fixture.account("a_second_device").user_id;
        let a_body = format!("private-draft-A-{} **saved** {target}", Uuid::new_v4());
        let b_body = format!("private-draft-B-{}", Uuid::new_v4());
        // SDK draft keys accept a valid event ID without fetching the event.
        // Distinct synthetic roots keep this persistence check independent of
        // timeline delivery; live thread sending has its own acceptance test.
        let first_thread = "$draft-root-one:fixture.invalid";
        let second_thread = "$draft-root-two:fixture.invalid";
        let a_thread_body = format!("private-thread-A-one-{} {target} @room", Uuid::new_v4());
        let a_other_thread_body = format!("private-thread-A-two-{}", Uuid::new_v4());
        let b_thread_body = format!("private-thread-B-one-{} {target}", Uuid::new_v4());
        let mut typed_room = ComposeText::new(a_body.clone(), TextFormat::Markdown);
        let start = a_body.find(target).expect("Draft target missing");
        typed_room = typed_room
            .insert_mention(
                start..start + target.len(),
                MentionTarget::User(target.clone()),
            )
            .expect("Cannot encode room draft mention")
            .0;
        let mut typed_thread = ComposeText::new(a_thread_body.clone(), TextFormat::Plain);
        let start = a_thread_body.find(target).expect("Thread target missing");
        typed_thread = typed_thread
            .insert_mention(
                start..start + target.len(),
                MentionTarget::User(target.clone()),
            )
            .expect("Cannot encode thread draft mention")
            .0;
        let start = a_thread_body
            .find("@room")
            .expect("Thread room target missing");
        typed_thread = typed_thread
            .insert_mention(start..start + 5, MentionTarget::Room)
            .expect("Cannot encode room selection")
            .0;
        let legacy_root = "$legacy-plain-draft:fixture.invalid";
        let legacy_text = format!("legacy-plain-{} {target} @room", Uuid::new_v4());

        let manager = AccountManager::open(&root).expect("Cannot open account manager");
        phase!(login_alice);
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        phase!(room_alice);
        wait_for_room(&manager, a_room).await;
        phase!(drafts_alice);
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &typed_room)
            .await
            .expect("Cannot persist first draft");
        for (thread, body) in [
            (first_thread, &a_thread_body),
            (second_thread, &a_other_thread_body),
        ] {
            manager
                .save_scoped_composer_draft_for(&a_account.id, a_room, Some(thread), body)
                .await
                .expect("Cannot persist encrypted thread draft");
        }
        manager
            .save_composed_draft_for(&a_account.id, a_room, Some(first_thread), &typed_thread)
            .await
            .expect("Cannot persist typed thread draft");
        phase!(legacy_seed);
        let client = manager.active_client().await.expect("Draft client missing");
        let room = client
            .get_room(&RoomId::parse(a_room).expect("Invalid draft room"))
            .expect("Draft room missing");
        room.save_composer_draft(
            ComposerDraft {
                plain_text: legacy_text.clone(),
                html_text: None,
                draft_type: ComposerDraftType::NewMessage,
                attachments: Vec::new(),
            },
            Some(&EventId::parse(legacy_root).expect("Invalid legacy root")),
        )
        .await
        .expect("Cannot seed legacy plain draft");
        drop(room);
        drop(client);
        assert!(
            manager
                .save_scoped_composer_draft_for(
                    &a_account.id,
                    a_room,
                    Some("invalid-event-id"),
                    "rejected"
                )
                .await
                .is_err()
        );
        phase!(tracked_enqueue);
        let queued = manager
            .send_text_tracked_for(
                &a_account.id,
                a_room,
                format!("Morpheus tracked-send acceptance {}", Uuid::new_v4()),
            )
            .await
            .expect("Cannot queue tracked message");
        phase!(tracked_outcome);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), queued.wait_outcome())
                .await
                .expect("Timed out waiting for server send result"),
            QueuedSendOutcome::Sent
        );

        phase!(login_carol);
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        phase!(room_carol);
        wait_for_room(&manager, b_room).await;
        phase!(drafts_carol);
        assert_eq!(
            manager
                .load_composer_draft_for(&b_account.id, b_room)
                .await
                .unwrap(),
            ""
        );
        manager
            .save_composer_draft_for(&b_account.id, b_room, &b_body)
            .await
            .expect("Cannot persist second draft");
        assert_eq!(
            manager
                .load_scoped_composer_draft_for(&b_account.id, b_room, Some(first_thread))
                .await
                .unwrap(),
            ""
        );
        manager
            .save_scoped_composer_draft_for(
                &b_account.id,
                b_room,
                Some(first_thread),
                &b_thread_body,
            )
            .await
            .expect("Cannot persist second account thread draft");
        assert_eq!(
            manager
                .load_composer_draft_for(&a_account.id, a_room)
                .await
                .unwrap(),
            a_body
        );

        phase!(stop_primary);
        manager.stop().await;
        drop(manager);
        phase!(plaintext_scan);
        assert!(!contains_plaintext(&root, a_body.as_bytes()));
        assert!(!contains_plaintext(&root, b_body.as_bytes()));
        for body in [
            &a_thread_body,
            &a_other_thread_body,
            &b_thread_body,
            &legacy_text,
        ] {
            assert!(
                !contains_plaintext(&root, body.as_bytes()),
                "Thread draft leaked into plaintext storage"
            );
        }

        assert!(
            !contains_plaintext(&root, typed_room.draft_html().unwrap().as_bytes()),
            "Typed room intent encoding leaked into plaintext storage"
        );
        assert!(
            !contains_plaintext(&root, typed_thread.draft_html().unwrap().as_bytes()),
            "Typed thread intent encoding leaked into plaintext storage"
        );
        phase!(reopen);
        let manager = AccountManager::open(&root).expect("Cannot reopen account manager");
        phase!(restore_carol);
        let restored = manager
            .restore_last_active()
            .await
            .expect("Cannot restore session");
        assert_eq!(restored.expect("Missing restored account").id, b_account.id);
        assert_eq!(
            manager
                .load_composer_draft_for(&b_account.id, b_room)
                .await
                .unwrap(),
            b_body
        );
        assert_eq!(
            manager
                .load_scoped_composer_draft_for(&b_account.id, b_room, Some(first_thread))
                .await
                .unwrap(),
            b_thread_body
        );
        phase!(switch_alice);
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch accounts");
        phase!(verify_alice);
        assert_eq!(
            manager
                .load_composer_draft_for(&a_account.id, a_room)
                .await
                .unwrap(),
            a_body
        );
        for (thread, body) in [
            (first_thread, &a_thread_body),
            (second_thread, &a_other_thread_body),
        ] {
            assert_eq!(
                &manager
                    .load_scoped_composer_draft_for(&a_account.id, a_room, Some(thread))
                    .await
                    .unwrap(),
                body
            );
        }
        let restored_room = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .expect("Cannot restore typed room draft");
        assert!(
            restored_room.text == typed_room && !restored_room.discarded_rich_intent,
            "Room format or explicit mention intent changed across restart"
        );
        let restored_thread = manager
            .load_composed_draft_for(&a_account.id, a_room, Some(first_thread))
            .await
            .expect("Cannot restore typed thread draft");
        assert!(
            restored_thread.text == typed_thread && !restored_thread.discarded_rich_intent,
            "Thread format or explicit mention intent changed across restart"
        );
        let legacy = manager
            .load_composed_draft_for(&a_account.id, a_room, Some(legacy_root))
            .await
            .expect("Cannot restore legacy draft");
        assert!(
            legacy.text.body == legacy_text
                && legacy.text.format == TextFormat::Plain
                && legacy.text.mentions.is_empty()
                && !legacy.discarded_rich_intent,
            "Legacy text inferred mention intent"
        );
        let other = manager
            .load_composed_draft_for(&b_account.id, b_room, Some(first_thread))
            .await
            .expect("Cannot restore other-account typed draft");
        assert!(
            other.text.body == b_thread_body && other.text.mentions.is_empty(),
            "Selected mentions crossed account/room boundaries"
        );
        phase!(clear_alice);
        manager
            .save_scoped_composer_draft_for(&a_account.id, a_room, Some(first_thread), "")
            .await
            .expect("Cannot clear acknowledged thread draft");
        assert_eq!(
            manager
                .load_scoped_composer_draft_for(&a_account.id, a_room, Some(first_thread))
                .await
                .unwrap(),
            ""
        );
        assert_eq!(
            manager
                .load_composer_draft_for(&a_account.id, a_room)
                .await
                .unwrap(),
            a_body
        );
        assert_eq!(
            manager
                .load_scoped_composer_draft_for(&a_account.id, a_room, Some(second_thread))
                .await
                .unwrap(),
            a_other_thread_body
        );
        manager
            .save_composer_draft_for(&a_account.id, a_room, "")
            .await
            .expect("Cannot clear sent draft");
        assert_eq!(
            manager
                .load_composer_draft_for(&a_account.id, a_room)
                .await
                .unwrap(),
            ""
        );
        phase!(logout_alice);
        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        assert!(!root.join("accounts").join(&a_account.id).exists());
        phase!(switch_carol);
        manager
            .switch_to(&b_account.id)
            .await
            .expect("Cannot switch to second account");
        phase!(verify_carol);
        assert_eq!(
            manager
                .load_composer_draft_for(&b_account.id, b_room)
                .await
                .unwrap(),
            b_body
        );
        assert_eq!(
            manager
                .load_scoped_composer_draft_for(&b_account.id, b_room, Some(first_thread))
                .await
                .unwrap(),
            b_thread_body
        );
        phase!(logout_carol);
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        phase!(stop_final);
        manager.stop().await;
    });
    phase!(cleanup);
    fs::remove_dir_all(root).expect("Cannot clean local test data");
    phase!(complete);
}
