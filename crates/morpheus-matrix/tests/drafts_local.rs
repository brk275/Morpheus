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
use morpheus_matrix::{
    AccountManager, ComposeText, DraftEnvelope, Generation, MentionTarget, QueuedSendOutcome,
    TextFormat,
};
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
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, a_room).await;
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &typed_room, &DraftEnvelope::fresh())
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
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                Some(first_thread),
                &typed_thread,
                &DraftEnvelope::fresh(),
            )
            .await
            .expect("Cannot persist typed thread draft");
        // Durable send recovery: a room row carries two independent pending
        // records and a thread index; a thread row carries one pending record.
        // Nonces, generations and transaction ids are opaque and must never
        // appear in plaintext storage either.
        let pending_room_text = ComposeText::new(
            format!("private-pending-room-{}", Uuid::new_v4()),
            TextFormat::Plain,
        );
        let pending_thread = "$draft-pending-root:fixture.invalid";
        let pending_thread_text = ComposeText::new(
            format!("private-pending-thread-{}", Uuid::new_v4()),
            TextFormat::Plain,
        );
        let first_nonce = Generation::fresh();
        let second_nonce = Generation::fresh();
        let thread_nonce = Generation::fresh();
        let first_txn = format!("fixture{}", Uuid::new_v4().simple());
        let second_txn = format!("fixture{}", Uuid::new_v4().simple());
        let mut pending_room_envelope = DraftEnvelope::fresh();
        pending_room_envelope
            .begin_pending(first_nonce.clone(), Generation::fresh(), false)
            .expect("first room record");
        pending_room_envelope
            .begin_pending(second_nonce.clone(), Generation::fresh(), false)
            .expect("second room record");
        assert!(pending_room_envelope.mark_queued(&first_nonce, &first_txn));
        assert!(pending_room_envelope.mark_queued(&second_nonce, &second_txn));
        assert!(
            pending_room_envelope
                .index_thread(pending_thread)
                .expect("thread index")
        );
        let mut pending_thread_envelope = DraftEnvelope::fresh();
        pending_thread_envelope
            .begin_pending(thread_nonce.clone(), Generation::fresh(), true)
            .expect("thread record");
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                Some(pending_thread),
                &pending_thread_text,
                &pending_thread_envelope,
            )
            .await
            .expect("Cannot persist pending thread row");
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &pending_room_text,
                &pending_room_envelope,
            )
            .await
            .expect("Cannot persist pending room row");
        // The envelope rides the room text; restore the typed draft with the
        // same envelope so the later text checks and the recovery checks share
        // one row.
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &typed_room, &pending_room_envelope)
            .await
            .expect("Cannot persist typed room draft with pending records");
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
        let queued = manager
            .send_text_tracked_for(
                &a_account.id,
                a_room,
                format!("Morpheus tracked-send acceptance {}", Uuid::new_v4()),
            )
            .await
            .expect("Cannot queue tracked message");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), queued.wait_outcome())
                .await
                .expect("Timed out waiting for server send result"),
            QueuedSendOutcome::Sent
        );

        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        wait_for_room(&manager, b_room).await;
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

        manager.stop().await;
        drop(manager);
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
        for secret in [
            pending_room_text.body.as_str(),
            pending_thread_text.body.as_str(),
            first_txn.as_str(),
            second_txn.as_str(),
            "data-morpheus-meta",
        ] {
            assert!(
                !contains_plaintext(&root, secret.as_bytes()),
                "Send recovery metadata leaked into plaintext storage"
            );
        }
        assert!(
            !contains_plaintext(
                &root,
                pending_room_envelope.encode().unwrap().as_bytes()
            ),
            "Encoded recovery envelope leaked into plaintext storage"
        );
        let manager = AccountManager::open(&root).expect("Cannot reopen account manager");
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
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch accounts");
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
        assert!(
            !restored_room.metadata_unreadable,
            "Recovery metadata failed closed on a valid envelope"
        );
        let recovered: Vec<(&Generation, Option<&str>)> = restored_room
            .envelope
            .pending()
            .iter()
            .map(|record| (record.nonce(), record.transaction_id()))
            .collect();
        assert_eq!(
            recovered,
            vec![
                (&first_nonce, Some(first_txn.as_str())),
                (&second_nonce, Some(second_txn.as_str())),
            ],
            "Queued room records or their order changed across restart"
        );
        assert_eq!(
            restored_room.envelope.threads(),
            &[pending_thread.to_owned()],
            "Thread index changed across restart"
        );
        let restored_pending_thread = manager
            .load_composed_draft_for(&a_account.id, a_room, Some(pending_thread))
            .await
            .expect("Cannot restore pending thread row");
        assert!(restored_pending_thread.text == pending_thread_text);
        let thread_records = restored_pending_thread.envelope.pending();
        assert!(
            thread_records.len() == 1
                && thread_records[0].nonce() == &thread_nonce
                && !thread_records[0].is_queued(),
            "Enqueuing thread record did not survive restart as unconfirmed"
        );
        // Resolving the second room record out of order keeps the first and
        // the thread index; clearing the room text keeps a metadata-only row.
        let mut room_envelope = restored_room.envelope.clone();
        assert!(
            room_envelope
                .remove_pending(&second_nonce, Some(second_txn.as_str()))
                .is_some()
        );
        assert!(
            room_envelope
                .remove_pending(&second_nonce, Some(second_txn.as_str()))
                .is_none(),
            "A resolved record was removable twice"
        );
        assert!(
            room_envelope
                .remove_pending(&first_nonce, Some(second_txn.as_str()))
                .is_none(),
            "A record was removable with another record's transaction id"
        );
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &ComposeText::new(String::new(), TextFormat::Plain),
                &room_envelope,
            )
            .await
            .expect("Cannot save metadata-only room row");
        let metadata_only = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .expect("Cannot reload metadata-only room row");
        assert!(metadata_only.text.body.is_empty());
        assert_eq!(metadata_only.envelope.pending().len(), 1);
        assert_eq!(metadata_only.envelope.pending()[0].nonce(), &first_nonce);
        assert_eq!(metadata_only.envelope.threads(), &[pending_thread.to_owned()]);
        // Put the typed text back for the remaining legacy checks, with the
        // generation refreshed as any edit would do.
        let mut edited_envelope = metadata_only.envelope.clone();
        edited_envelope.set_generation(Generation::fresh());
        assert_ne!(edited_envelope.generation(), metadata_only.envelope.generation());
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &typed_room, &edited_envelope)
            .await
            .expect("Cannot restore typed room draft");
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                Some(pending_thread),
                &ComposeText::new(String::new(), TextFormat::Plain),
                &DraftEnvelope::fresh(),
            )
            .await
            .expect("Cannot clear resolved thread row");
        assert_eq!(
            manager
                .load_scoped_composer_draft_for(&a_account.id, a_room, Some(pending_thread))
                .await
                .unwrap(),
            ""
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
        // The plain clear keeps the pending record and thread index.
        let after_clear = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .expect("Cannot reload cleared room row");
        assert_eq!(after_clear.envelope.pending().len(), 1);
        assert_eq!(after_clear.envelope.threads(), &[pending_thread.to_owned()]);
        // Resolving the last record and dropping the index leaves nothing to
        // keep: the row is deleted.
        let mut final_envelope = after_clear.envelope.clone();
        assert!(final_envelope.remove_pending(&first_nonce, Some(first_txn.as_str())).is_some());
        assert!(final_envelope.unindex_thread(pending_thread));
        assert!(final_envelope.is_empty());
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &ComposeText::new(String::new(), TextFormat::Plain),
                &final_envelope,
            )
            .await
            .expect("Cannot delete empty room row");
        let gone = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .expect("Cannot reload deleted room row");
        assert!(gone.text.body.is_empty() && gone.envelope.is_empty());
        manager
            .logout_account(&a_account.id)
            .await
            .expect("Cannot remove first account");
        assert!(!root.join("accounts").join(&a_account.id).exists());
        manager
            .switch_to(&b_account.id)
            .await
            .expect("Cannot switch to second account");
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
        manager
            .logout_account(&b_account.id)
            .await
            .expect("Cannot remove second account");
        manager.stop().await;
    });
    fs::remove_dir_all(root).expect("Cannot clean local test data");
}
