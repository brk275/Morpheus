//! Opt-in encrypted draft acceptance test against two disposable local Synapse
//! homeservers. Fixture credentials are read from a mode-0600 file and never
//! printed by this test.
#![recursion_limit = "512"]

#[path = "support/account_removal.rs"]
mod account_removal;
#[path = "support/opened_room.rs"]
mod opened_room;

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use matrix_sdk::{
    ComposerDraft, ComposerDraftType, DraftAttachment, DraftAttachmentContent, RoomState,
    ruma::{
        EventId, RoomId,
        api::client::state::get_state_event_for_key,
        events::StateEventType,
        events::room::{
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            message::RoomMessageEventContent,
        },
        serde::Raw,
    },
    send_queue::{LocalEchoContent, RoomSendQueueUpdate},
};
use morpheus_matrix::{
    AccountManager, AccountSelection, ComposeSendTarget, ComposeText, DraftEnvelope, Generation,
    LogoutResult, MatrixCoreError, MentionTarget, QuarantineReason, QueuedSendOutcome, RowRevision,
    TextFormat,
};
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
        opened_room::scan(&manager, a_room)
            .await
            .expect("Opened fixture room must be scanned");
        phase!(drafts_alice);
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &typed_room,
                &DraftEnvelope::fresh(),
            )
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
        // Records are written as enqueuing first and receive their ids in a
        // second write, as the controller does; the core refuses a record that
        // appears already queued, since nothing of this process created it.
        let mut pending_room_envelope = DraftEnvelope::fresh();
        pending_room_envelope
            .begin_pending(
                first_nonce.clone(),
                Generation::fresh(),
                &pending_room_text,
                false,
            )
            .expect("first room record");
        pending_room_envelope
            .begin_pending(
                second_nonce.clone(),
                Generation::fresh(),
                &pending_room_text,
                false,
            )
            .expect("second room record");
        assert!(
            pending_room_envelope
                .index_thread(pending_thread)
                .expect("thread index")
        );
        let mut forged = pending_room_envelope.clone();
        assert!(forged.mark_queued(&first_nonce, &first_txn));
        assert!(
            matches!(
                manager
                    .save_composed_draft_for(
                        &a_account.id,
                        a_room,
                        None,
                        &pending_room_text,
                        &forged
                    )
                    .await,
                Err(MatrixCoreError::StaleDraftSnapshot)
            ),
            "a record this process never saved as enqueuing cannot appear queued"
        );
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &pending_room_text,
                &pending_room_envelope,
            )
            .await
            .expect("Cannot persist enqueuing room records");
        assert!(pending_room_envelope.mark_queued(&first_nonce, &first_txn));
        assert!(pending_room_envelope.mark_queued(&second_nonce, &second_txn));
        let mut pending_thread_envelope = DraftEnvelope::fresh();
        pending_thread_envelope
            .begin_pending(
                thread_nonce.clone(),
                Generation::fresh(),
                &pending_thread_text,
                true,
            )
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
        // A room snapshot from before the thread was indexed (same records,
        // no index) would orphan the thread row from discovery: refused.
        let mut without_index = pending_room_envelope.clone();
        assert!(without_index.unindex_thread(pending_thread));
        assert!(
            matches!(
                manager
                    .save_composed_draft_for(
                        &a_account.id,
                        a_room,
                        None,
                        &pending_room_text,
                        &without_index
                    )
                    .await,
                Err(MatrixCoreError::StaleDraftSnapshot)
            ),
            "dropping an indexed thread that holds a record is refused"
        );
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
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &typed_room,
                &pending_room_envelope,
            )
            .await
            .expect("Cannot persist typed room draft with pending records");
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
        opened_room::scan(&manager, b_room)
            .await
            .expect("Opened fixture room must be scanned");
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
            !contains_plaintext(&root, pending_room_envelope.encode().unwrap().as_bytes()),
            "Encoded recovery envelope leaked into plaintext storage"
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
        assert!(
            restored_room.blocked().is_none(),
            "Recovery metadata failed closed on a valid envelope"
        );
        assert_eq!(
            restored_room.envelope.pending()[0]
                .text()
                .to_compose()
                .unwrap(),
            pending_room_text,
            "Each record keeps the text it was created from"
        );
        let a_selection = manager
            .account_selection(&a_account.id)
            .expect("Selection missing");
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
        // Without the exact release the core refuses to drop the record.
        assert!(matches!(
            manager
                .save_composed_draft_for(
                    &a_account.id,
                    a_room,
                    None,
                    &ComposeText::new(String::new(), TextFormat::Plain),
                    &room_envelope,
                )
                .await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ));
        manager
            .release_pending_record_for(
                &a_selection,
                a_room,
                None,
                &second_nonce,
                Some(&second_txn),
            )
            .await
            .expect("Exact release");
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
        assert_eq!(
            metadata_only.envelope.threads(),
            &[pending_thread.to_owned()]
        );
        // Put the typed text back for the remaining legacy checks, with the
        // generation refreshed as any edit would do.
        let mut edited_envelope = metadata_only.envelope.clone();
        edited_envelope.set_generation(Generation::fresh());
        assert_ne!(
            edited_envelope.generation(),
            metadata_only.envelope.generation()
        );
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &typed_room, &edited_envelope)
            .await
            .expect("Cannot restore typed room draft");
        manager
            .release_pending_record_for(
                &a_selection,
                a_room,
                Some(pending_thread),
                &thread_nonce,
                None,
            )
            .await
            .expect("Exact thread release");
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
        assert!(
            final_envelope
                .remove_pending(&first_nonce, Some(first_txn.as_str()))
                .is_some()
        );
        assert!(final_envelope.unindex_thread(pending_thread));
        assert!(final_envelope.is_empty());
        manager
            .release_pending_record_for(&a_selection, a_room, None, &first_nonce, Some(&first_txn))
            .await
            .expect("Exact release of the last record");
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
        phase!(logout_alice);
        account_removal::finish_logout(
            &manager,
            &a_account.id,
            &root.join("accounts").join(&a_account.id),
            manager.logout_account(&a_account.id).await,
        )
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
        account_removal::finish_logout(
            &manager,
            &b_account.id,
            &root.join("accounts").join(&b_account.id),
            manager.logout_account(&b_account.id).await,
        )
        .await
        .expect("Cannot remove second account");
        phase!(stop_final);
        manager.stop().await;
    });
    phase!(cleanup);
    fs::remove_dir_all(root).expect("Cannot clean local test data");
    phase!(complete);
}

fn read_fixture() -> Option<(Fixture, PathBuf)> {
    let credentials_path = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS")?;
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(credentials_path)).expect("Cannot read local fixture"),
    )
    .expect("Invalid local fixture");
    let root = std::env::temp_dir().join(format!("morpheus-recovery-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create account data directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect account data directory");
    }
    Some((fixture, root))
}

/// Body size of the hold event: well over the event size any homeserver
/// accepts, so the request is refused permanently rather than delivered.
const HOLD_BODY_BYTES: usize = 128 * 1024;

/// A fixture-owned hold on one room's SDK queue. The SDK sends a room's
/// requests strictly in order and never past a head request it has recorded
/// as wedged (refused with an unrecoverable error), so a wedged request at the
/// head keeps every later request persisted whatever the product's gate does
/// to the room's enabled flag. The hold is established only once the SDK
/// itself reports the permanent refusal and shows the request as wedged; a
/// delivered or still-pending hold event fails the fixture instead of
/// pretending to hold anything.
struct QueueHold {
    transaction_id: String,
    handle: matrix_sdk::send_queue::SendHandle,
}

impl QueueHold {
    /// Place the hold. The subscription is taken before anything is enqueued
    /// so the refusal cannot be missed.
    async fn place(room: &matrix_sdk::Room) -> Self {
        let (echoes_before, mut updates) = room
            .send_queue()
            .subscribe()
            .await
            .expect("Cannot observe the room send queue");
        assert!(
            echoes_before.is_empty(),
            "a hold is placed only on an empty room queue"
        );
        let content = Raw::new(&RoomMessageEventContent::text_plain(
            "x".repeat(HOLD_BODY_BYTES),
        ))
        .expect("Cannot encode the hold event")
        .cast();
        let handle = room
            .send_queue()
            .send_raw(content, "m.room.message".into())
            .await
            .expect("Cannot enqueue the hold event");
        let transaction_id = handle.transaction_id().to_string();
        // Let the SDK attempt exactly this request so it records the refusal.
        room.send_queue().set_enabled(true);
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match updates.recv().await {
                    Ok(RoomSendQueueUpdate::SendError { transaction_id: failed, is_recoverable, .. })
                        if failed.as_str() == transaction_id =>
                    {
                        assert!(!is_recoverable, "the hold must be a permanent refusal, not a paused queue");
                        return;
                    }
                    Ok(RoomSendQueueUpdate::SentEvent { transaction_id: sent, .. })
                        if sent.as_str() == transaction_id =>
                    {
                        panic!("The hold event was delivered; the homeserver accepted it and nothing holds the queue");
                    }
                    Ok(_) => {}
                    Err(error) => panic!("Send queue observation ended: {error}"),
                }
            }
        })
        .await
        .expect("Timed out waiting for the SDK to refuse the hold event");
        // Leave the flag as the product expects; the parked task sends nothing
        // past the wedged head anyway.
        room.send_queue().set_enabled(false);
        let hold = Self {
            transaction_id,
            handle,
        };
        hold.assert_wedged_head(room).await;
        hold
    }

    /// After a restart: the exact hold, recovered from the SDK's own store.
    async fn recover(room: &matrix_sdk::Room, transaction_id: &str) -> Self {
        let (echoes, _) = room
            .send_queue()
            .subscribe()
            .await
            .expect("Cannot observe the room send queue");
        let handle = echoes
            .iter()
            .find_map(|echo| match &echo.content {
                LocalEchoContent::Event {
                    send_handle,
                    send_error,
                    ..
                } if echo.transaction_id.as_str() == transaction_id => {
                    assert!(
                        send_error.is_some(),
                        "the recovered hold is not recorded as wedged"
                    );
                    Some(send_handle.clone())
                }
                _ => None,
            })
            .expect("the hold did not survive the restart");
        let hold = Self {
            transaction_id: transaction_id.to_owned(),
            handle,
        };
        hold.assert_wedged_head(room).await;
        hold
    }

    /// The SDK's own view: the hold is present, wedged, and the head.
    async fn assert_wedged_head(&self, room: &matrix_sdk::Room) {
        let (echoes, _) = room
            .send_queue()
            .subscribe()
            .await
            .expect("Cannot observe the room send queue");
        let head = echoes.first().expect("the hold left the SDK queue");
        assert_eq!(
            head.transaction_id.as_str(),
            self.transaction_id,
            "the hold must be the head of the queue"
        );
        match &head.content {
            LocalEchoContent::Event { send_error, .. } => {
                assert!(send_error.is_some(), "the hold is not recorded as wedged");
            }
            _ => panic!("the hold is not an event request"),
        }
    }

    /// Lift the hold by aborting exactly it; the SDK then sends the next head
    /// if the room is enabled.
    async fn release(self, room: &matrix_sdk::Room) {
        assert!(
            self.handle
                .abort()
                .await
                .expect("Cannot abort the hold event"),
            "the hold was already gone"
        );
        assert!(
            !persisted_requests(room)
                .await
                .contains(&self.transaction_id),
            "the hold must leave the SDK queue on release"
        );
    }
}

/// Exact ids of the requests the SDK store still holds for a room.
async fn persisted_requests(room: &matrix_sdk::Room) -> HashSet<String> {
    room.send_queue()
        .subscribe()
        .await
        .expect("Cannot read the room send queue")
        .0
        .iter()
        .map(|echo| echo.transaction_id.to_string())
        .collect()
}

/// Record one typed room send the way the controller does: the enqueuing
/// record is written before the SDK knows anything, the single-use grant is
/// taken, the SDK assigns the id, and the id is written back.
async fn record_and_enqueue(
    manager: &AccountManager,
    selection: &AccountSelection,
    account_id: &str,
    room_id: &str,
    envelope: &mut DraftEnvelope,
    nonce: &Generation,
    text: &ComposeText,
) -> (morpheus_matrix::QueuedMessage, String) {
    envelope.set_generation(Generation::fresh());
    envelope
        .begin_pending(nonce.clone(), envelope.generation().clone(), text, false)
        .expect("Cannot record the send");
    manager
        .save_composed_draft_for(account_id, room_id, None, text, envelope)
        .await
        .expect("Cannot persist the enqueuing record");
    manager
        .grant_enqueue_for(selection, room_id, None, nonce)
        .await
        .expect("A fresh saved record earns its grant");
    let queued = manager
        .send_composed_owned_for(
            selection,
            room_id,
            ComposeSendTarget::Room,
            text,
            None,
            Some(nonce),
        )
        .await
        .expect("Cannot enqueue the recorded send");
    let transaction_id = queued.transaction_id().to_owned();
    assert!(envelope.mark_queued(nonce, &transaction_id));
    manager
        .save_composed_draft_for(account_id, room_id, None, text, envelope)
        .await
        .expect("Cannot persist the queued record");
    (queued, transaction_id)
}

/// Wait on a subscription taken before the queue was allowed to resume, so
/// the acknowledgement cannot be missed.
async fn wait_sent(
    mut updates: tokio::sync::broadcast::Receiver<RoomSendQueueUpdate>,
    transaction_id: &str,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match updates.recv().await {
                Ok(RoomSendQueueUpdate::SentEvent {
                    transaction_id: sent,
                    ..
                }) if sent.as_str() == transaction_id => {
                    return;
                }
                Ok(_) => {}
                Err(error) => panic!("Send queue observation ended: {error}"),
            }
        }
    })
    .await
    .expect("Timed out waiting for the persisted request to send");
}

/// Real SDK requests, real restarts, controlled queue barriers: records are
/// durable before enqueue, discovery adopts exactly the persisted request
/// before the queue resumes, crash windows stay explicitly unconfirmed,
/// out-of-order results keep each typed submission, wrong or stale authority
/// touches nothing, and an unreadable row pauses its room until discarded.
#[test]
fn pending_sends_are_recovered_from_real_sdk_requests() {
    let Some((fixture, root)) = read_fixture() else {
        eprintln!("Skipped local send recovery test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    phase!(recovery_fixture);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot start local test runtime");
    runtime.block_on(async {
        let alice = fixture.account("a_primary");
        let carol = fixture.account("b_primary");
        let a_room = fixture.room("a_room");
        let b_room = fixture.room("b_room");
        let empty = ComposeText::new(String::new(), TextFormat::Plain);
        let marker = Uuid::new_v4();
        let first_text = ComposeText::new(
            format!("recovery-first-{marker} **kept**"),
            TextFormat::Markdown,
        );
        let second_text = ComposeText::plain(format!("recovery-second-{marker}"));
        let third_text = ComposeText::plain(format!("recovery-third-{marker}"));
        let fourth_text = ComposeText::new(
            format!("recovery-fourth-{marker} _adopted_"),
            TextFormat::Markdown,
        );
        let fifth_text = ComposeText::plain(format!("recovery-fifth-{marker}"));
        let sixth_text = ComposeText::plain(format!("recovery-sixth-{marker}"));
        let eighth_text = ComposeText::plain(format!("recovery-eighth-{marker}"));
        let ninth_text = ComposeText::plain(format!("recovery-ninth-{marker}"));
        let (first, second, third, fourth, fifth, sixth, eighth, ninth) = (
            Generation::fresh(),
            Generation::fresh(),
            Generation::fresh(),
            Generation::fresh(),
            Generation::fresh(),
            Generation::fresh(),
            Generation::fresh(),
            Generation::fresh(),
        );

        let manager = AccountManager::open(&root).expect("Cannot open account manager");
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, a_room).await;
        opened_room::scan(&manager, a_room)
            .await
            .expect("Opened fixture room must be scanned");
        let selection = manager
            .account_selection(&a_account.id)
            .expect("Selection missing");
        // Discovery is the gate for recorded sends; waiting for it is the
        // same wait the controller performs.
        assert!(
            manager
                .take_recovered_drafts(&selection)
                .await
                .unwrap()
                .is_empty()
        );
        let client = manager.active_client().await.expect("Client missing");
        let room = client
            .get_room(&RoomId::parse(a_room).unwrap())
            .expect("Room missing");

        // Hold: a fixture-owned wedged head keeps every later request
        // persisted; the product's own per-room resume cannot send past it.
        let hold = QueueHold::place(&room).await;
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold.transaction_id.clone()])
        );

        phase!(recovery_prepare);
        // 1. Pending record durable before enqueue; enqueue needs the grant.
        let mut envelope = DraftEnvelope::fresh();
        envelope
            .begin_pending(
                first.clone(),
                envelope.generation().clone(),
                &first_text,
                false,
            )
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &first_text, &envelope)
            .await
            .unwrap();
        let on_disk = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(on_disk.envelope.pending().len(), 1);
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold.transaction_id.clone()]),
            "the row leads the SDK"
        );
        assert!(
            matches!(
                manager
                    .send_composed_owned_for(
                        &selection,
                        a_room,
                        ComposeSendTarget::Room,
                        &first_text,
                        None,
                        Some(&first)
                    )
                    .await,
                Err(MatrixCoreError::PendingSendUnknown)
            ),
            "no grant, no enqueue"
        );
        manager
            .grant_enqueue_for(&selection, a_room, None, &first)
            .await
            .unwrap();
        let queued_first = manager
            .send_composed_owned_for(
                &selection,
                a_room,
                ComposeSendTarget::Room,
                &first_text,
                None,
                Some(&first),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                manager
                    .send_composed_owned_for(
                        &selection,
                        a_room,
                        ComposeSendTarget::Room,
                        &first_text,
                        None,
                        Some(&first)
                    )
                    .await,
                Err(MatrixCoreError::PendingSendUnknown)
            ),
            "the grant is single use"
        );
        let first_txn = queued_first.transaction_id().to_owned();
        assert!(envelope.mark_queued(&first, &first_txn));
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &first_text, &envelope)
            .await
            .unwrap();
        let (queued_second, second_txn) = record_and_enqueue(
            &manager,
            &selection,
            &a_account.id,
            a_room,
            &mut envelope,
            &second,
            &second_text,
        )
        .await;
        assert_ne!(first_txn, second_txn);
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([
                hold.transaction_id.clone(),
                first_txn.clone(),
                second_txn.clone()
            ])
        );

        phase!(recovery_wrong_owner);
        // 2. Stale snapshots and wrong authority touch nothing.
        let mut stale = envelope.clone();
        assert!(stale.remove_pending(&first, Some(&first_txn)).is_some());
        assert!(matches!(
            manager
                .save_composed_draft_for(&a_account.id, a_room, None, &second_text, &stale)
                .await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ));
        assert!(matches!(
            manager
                .cancel_pending_send_for(&selection, a_room, None, &first, &second_txn)
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(matches!(
            manager
                .cancel_pending_send_for(&selection, a_room, None, &Generation::fresh(), &first_txn)
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(matches!(
            manager
                .release_pending_record_for(&selection, a_room, None, &first, None)
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert_eq!(persisted_requests(&room).await.len(), 3);
        let on_disk = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(on_disk.envelope.pending().len(), 2);

        phase!(recovery_cancel);
        // 3. Out of order: the second is cancelled exactly; the first keeps
        // its typed Markdown submission.
        assert!(
            manager
                .cancel_pending_send_for(&selection, a_room, None, &second, &second_txn)
                .await
                .unwrap()
        );
        assert_eq!(
            queued_second.wait_outcome().await,
            QueuedSendOutcome::Failed
        );
        manager
            .release_pending_record_for(&selection, a_room, None, &second, Some(&second_txn))
            .await
            .unwrap();
        assert!(
            envelope
                .remove_pending(&second, Some(&second_txn))
                .is_some()
        );
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &second_text, &envelope)
            .await
            .unwrap();
        let after = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(after.envelope.pending().len(), 1);
        assert_eq!(
            after.envelope.pending()[0].text().to_compose().unwrap(),
            first_text
        );
        assert_eq!(
            after.text, second_text,
            "the live text is untouched by the resolution"
        );
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold.transaction_id.clone(), first_txn.clone()])
        );

        phase!(recovery_crash);
        // 4. Crash windows. A third record is saved as enqueuing and never
        // enqueued; the first sends while the row still says queued; the
        // process stops before either is acknowledged in the row.
        envelope.set_generation(Generation::fresh());
        envelope
            .begin_pending(
                third.clone(),
                envelope.generation().clone(),
                &third_text,
                false,
            )
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &third_text, &envelope)
            .await
            .unwrap();
        hold.release(&room).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), queued_first.wait_outcome())
                .await
                .expect("Timed out waiting for the first send"),
            QueuedSendOutcome::Sent
        );
        drop(room);
        drop(client);
        manager.stop().await;
        drop(manager);
        for text in [&first_text, &second_text, &third_text] {
            assert!(
                !contains_plaintext(&root, text.body.as_bytes()),
                "record text leaked"
            );
        }

        let manager = AccountManager::open(&root).expect("Cannot reopen account manager");
        let restored_account = manager
            .restore_last_active()
            .await
            .unwrap()
            .expect("No restored account");
        assert_eq!(restored_account.id, a_account.id);
        let selection = manager.account_selection(&a_account.id).unwrap();
        let recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The room scope was discovered");
        assert_eq!(scope.sends.len(), 2);
        assert!(
            scope.sends.iter().all(|send| send.queued.is_none()),
            "no SDK request: unconfirmed, never sent"
        );
        let first_send = scope.sends.iter().find(|send| send.nonce == first).unwrap();
        assert_eq!(
            first_send.transaction_id.as_deref(),
            Some(first_txn.as_str())
        );
        let third_send = scope.sends.iter().find(|send| send.nonce == third).unwrap();
        assert!(third_send.transaction_id.is_none());
        assert_eq!(
            scope
                .restored
                .envelope
                .find(&first)
                .unwrap()
                .text()
                .to_compose()
                .unwrap(),
            first_text
        );
        assert_eq!(
            scope
                .restored
                .envelope
                .find(&third)
                .unwrap()
                .text()
                .to_compose()
                .unwrap(),
            third_text
        );
        assert_eq!(scope.restored.text, third_text);
        assert!(matches!(
            manager
                .cancel_pending_send_for(&selection, a_room, None, &first, &first_txn)
                .await,
            Err(MatrixCoreError::PendingSendUnavailable)
        ));
        assert!(matches!(
            manager
                .retry_pending_send_for(&selection, a_room, None, &first, &first_txn)
                .await,
            Err(MatrixCoreError::PendingSendUnavailable)
        ));
        // Restored records cannot be replayed through a new enqueue.
        assert!(matches!(
            manager
                .grant_enqueue_for(&selection, a_room, None, &third)
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(matches!(
            manager
                .send_composed_owned_for(
                    &selection,
                    a_room,
                    ComposeSendTarget::Room,
                    &third_text,
                    None,
                    Some(&third)
                )
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        assert!(persisted_requests(&room).await.is_empty());
        // Only the explicit review releases them, one exact record at a time.
        let mut reviewed = scope.restored.envelope.clone();
        assert!(reviewed.remove_pending(&first, Some(&first_txn)).is_some());
        assert!(matches!(
            manager
                .save_composed_draft_for(&a_account.id, a_room, None, &third_text, &reviewed)
                .await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ));
        manager
            .release_pending_record_for(&selection, a_room, None, &first, Some(&first_txn))
            .await
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &third_text, &reviewed)
            .await
            .unwrap();
        manager
            .release_pending_record_for(&selection, a_room, None, &third, None)
            .await
            .unwrap();
        assert!(reviewed.remove_pending(&third, None).is_some());
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &reviewed)
            .await
            .unwrap();
        assert!(
            manager
                .load_composed_draft_for(&a_account.id, a_room, None)
                .await
                .unwrap()
                .envelope
                .is_empty()
        );

        phase!(recovery_adopt);
        // 5. A request the SDK still holds across a restart is adopted
        // exactly, through the subscription taken before the queue resumes.
        let hold = QueueHold::place(&room).await;
        let hold_id = hold.transaction_id.clone();
        let mut envelope = DraftEnvelope::fresh();
        let (_queued_fourth, fourth_txn) = record_and_enqueue(
            &manager,
            &selection,
            &a_account.id,
            a_room,
            &mut envelope,
            &fourth,
            &fourth_text,
        )
        .await;
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold_id.clone(), fourth_txn.clone()])
        );
        drop(hold);
        drop(room);
        drop(client);
        manager.stop().await;
        drop(manager);
        let manager = AccountManager::open(&root).unwrap();
        manager
            .restore_last_active()
            .await
            .unwrap()
            .expect("No restored account");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let mut recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter_mut()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The room scope was discovered");
        let send = scope
            .sends
            .iter_mut()
            .find(|send| send.nonce == fourth)
            .unwrap();
        assert_eq!(send.transaction_id.as_deref(), Some(fourth_txn.as_str()));
        let adopted = send
            .queued
            .take()
            .expect("the exact persisted request was adopted");
        {
            // The hold survived the restart ahead of the adopted request;
            // lifting it lets the room (resumed by discovery) send.
            let client = manager.active_client().await.unwrap();
            let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
            assert_eq!(
                persisted_requests(&room).await,
                HashSet::from([hold_id.clone(), fourth_txn.clone()])
            );
            QueueHold::recover(&room, &hold_id)
                .await
                .release(&room)
                .await;
        }
        assert!(
            matches!(
                manager
                    .cancel_pending_send_for(
                        &selection,
                        a_room,
                        None,
                        &Generation::fresh(),
                        &fourth_txn
                    )
                    .await,
                Err(MatrixCoreError::PendingSendUnknown)
            ),
            "another owner cannot touch it"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), adopted.wait_outcome())
                .await
                .expect("Timed out waiting for the adopted send"),
            QueuedSendOutcome::Sent
        );
        assert_eq!(
            scope
                .restored
                .envelope
                .find(&fourth)
                .unwrap()
                .text()
                .to_compose()
                .unwrap(),
            fourth_text
        );
        manager
            .release_pending_record_for(&selection, a_room, None, &fourth, Some(&fourth_txn))
            .await
            .unwrap();
        let mut done = scope.restored.envelope.clone();
        assert!(done.remove_pending(&fourth, Some(&fourth_txn)).is_some());
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &done)
            .await
            .unwrap();

        phase!(recovery_reselect);
        // 6. Revocation ends waits and refuses controls without another SDK
        // event; re-selection rediscovers and adopts before the queue resumes.
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        let hold = QueueHold::place(&room).await;
        let hold_id = hold.transaction_id.clone();
        let mut envelope = DraftEnvelope::fresh();
        let (_queued_sixth, sixth_txn) = record_and_enqueue(
            &manager,
            &selection,
            &a_account.id,
            a_room,
            &mut envelope,
            &sixth,
            &sixth_text,
        )
        .await;
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold_id.clone(), sixth_txn.clone()])
        );
        drop(hold);
        drop(room);
        drop(client);
        let old_selection = selection;
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        wait_for_room(&manager, b_room).await;
        opened_room::scan(&manager, b_room)
            .await
            .expect("Opened fixture room must be scanned");
        assert!(!old_selection.is_current());
        assert!(
            matches!(
                manager.take_recovered_drafts(&old_selection).await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ),
            "a revoked selection ends the wait at once"
        );
        assert!(matches!(
            manager
                .cancel_pending_send_for(&old_selection, a_room, None, &sixth, &sixth_txn)
                .await,
            Err(MatrixCoreError::SelectedAccountChanged)
        ));
        let b_selection = manager.account_selection(&b_account.id).unwrap();
        assert!(
            manager
                .take_recovered_drafts(&b_selection)
                .await
                .unwrap()
                .is_empty()
        );
        manager
            .switch_to(&a_account.id)
            .await
            .expect("Cannot switch back");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let mut recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter_mut()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The room scope was rediscovered");
        let send = scope
            .sends
            .iter_mut()
            .find(|send| send.nonce == sixth)
            .unwrap();
        let adopted = send
            .queued
            .take()
            .expect("the request was adopted again on re-selection");
        {
            let client = manager.active_client().await.unwrap();
            let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
            assert_eq!(
                persisted_requests(&room).await,
                HashSet::from([hold_id.clone(), sixth_txn.clone()])
            );
            QueueHold::recover(&room, &hold_id)
                .await
                .release(&room)
                .await;
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), adopted.wait_outcome())
                .await
                .expect("Timed out waiting for the re-adopted send"),
            QueuedSendOutcome::Sent
        );
        manager
            .release_pending_record_for(&selection, a_room, None, &sixth, Some(&sixth_txn))
            .await
            .unwrap();
        let mut done = scope.restored.envelope.clone();
        assert!(done.remove_pending(&sixth, Some(&sixth_txn)).is_some());
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &done)
            .await
            .unwrap();

        phase!(recovery_missing_id);
        // 7. Enqueue before the id is persisted: the SDK holds the request,
        // the row still says enqueuing. After restart the record is reviewed,
        // never matched by body or time and never enqueued again; the SDK's
        // own persisted request is what sends once the room resumes.
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        let hold = QueueHold::place(&room).await;
        let hold_id = hold.transaction_id.clone();
        let seventh = Generation::fresh();
        let seventh_text = ComposeText::plain(format!("recovery-seventh-{marker}"));
        let mut envelope = DraftEnvelope::fresh();
        envelope
            .begin_pending(
                seventh.clone(),
                envelope.generation().clone(),
                &seventh_text,
                false,
            )
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &seventh_text, &envelope)
            .await
            .unwrap();
        manager
            .grant_enqueue_for(&selection, a_room, None, &seventh)
            .await
            .unwrap();
        let queued_seventh = manager
            .send_composed_owned_for(
                &selection,
                a_room,
                ComposeSendTarget::Room,
                &seventh_text,
                None,
                Some(&seventh),
            )
            .await
            .unwrap();
        let seventh_txn = queued_seventh.transaction_id().to_owned();
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold_id.clone(), seventh_txn.clone()])
        );
        // The process stops before the id reaches the row.
        drop(queued_seventh);
        drop(hold);
        drop(room);
        drop(client);
        manager.stop().await;
        drop(manager);
        let manager = AccountManager::open(&root).unwrap();
        manager
            .restore_last_active()
            .await
            .unwrap()
            .expect("No restored account");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        // Subscribe before taking discovery's result: the room resumes as
        // part of discovery and the persisted request sends on its own.
        let (echoes_now, mut seventh_updates) = room.send_queue().subscribe().await.unwrap();
        let recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        // Discovery resumed the room; only lifting the hold lets the SDK's own
        // persisted request go.
        QueueHold::recover(&room, &hold_id)
            .await
            .release(&room)
            .await;
        let scope = recovered
            .iter()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The room scope was discovered");
        let send = scope
            .sends
            .iter()
            .find(|send| send.nonce == seventh)
            .unwrap();
        assert!(
            send.transaction_id.is_none() && send.queued.is_none(),
            "an enqueuing record is unconfirmed, never matched by body"
        );
        assert_eq!(
            scope
                .restored
                .envelope
                .find(&seventh)
                .unwrap()
                .text()
                .to_compose()
                .unwrap(),
            seventh_text
        );
        if echoes_now
            .iter()
            .any(|echo| echo.transaction_id.as_str() == seventh_txn)
        {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match seventh_updates.recv().await {
                        Ok(RoomSendQueueUpdate::SentEvent {
                            transaction_id: sent,
                            ..
                        }) if sent.as_str() == seventh_txn => return,
                        Ok(_) => {}
                        Err(error) => panic!("Send queue observation ended: {error}"),
                    }
                }
            })
            .await
            .expect("Timed out waiting for the SDK's own persisted request");
        }
        assert!(persisted_requests(&room).await.is_empty());
        // Nothing of ours can replay it: no grant, no enqueue, no cancel.
        assert!(matches!(
            manager
                .grant_enqueue_for(&selection, a_room, None, &seventh)
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(matches!(
            manager
                .send_composed_owned_for(
                    &selection,
                    a_room,
                    ComposeSendTarget::Room,
                    &seventh_text,
                    None,
                    Some(&seventh)
                )
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(
            matches!(
                manager
                    .cancel_pending_send_for(&selection, a_room, None, &seventh, &seventh_txn)
                    .await,
                Err(MatrixCoreError::PendingSendUnknown)
            ),
            "a record without an id owns no request, even the one that sent"
        );
        // Only the explicit review resolves it; the same row is written once.
        manager
            .release_pending_record_for(&selection, a_room, None, &seventh, None)
            .await
            .unwrap();
        let mut reviewed = scope.restored.envelope.clone();
        assert!(reviewed.remove_pending(&seventh, None).is_some());
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &reviewed)
            .await
            .unwrap();
        assert!(
            manager
                .load_composed_draft_for(&a_account.id, a_room, None)
                .await
                .unwrap()
                .envelope
                .is_empty()
        );
        drop(room);
        drop(client);

        phase!(recovery_quarantine);
        // 8. A row this build cannot read pauses its room's queue, keeps the
        // request and the complete original, restores the stored plain
        // column, and resumes only after the explicit discard of exactly it.
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        let hold = QueueHold::place(&room).await;
        let hold_id = hold.transaction_id.clone();
        let mut envelope = DraftEnvelope::fresh();
        let (_queued_fifth, fifth_txn) = record_and_enqueue(
            &manager,
            &selection,
            &a_account.id,
            a_room,
            &mut envelope,
            &fifth,
            &fifth_text,
        )
        .await;
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold_id.clone(), fifth_txn.clone()])
        );
        // What a newer Morpheus would leave behind: the same row under a
        // version this build does not know, written through the same SDK API.
        let row = room
            .load_composer_draft(None)
            .await
            .unwrap()
            .expect("Row missing");
        let broken = row
            .html_text
            .clone()
            .expect("Row has no envelope")
            .replace("data-morpheus-compose=\"2\"", "data-morpheus-compose=\"9\"");
        let plain_column = format!("{} (plain column differs)", row.plain_text);
        room.save_composer_draft(
            ComposerDraft {
                plain_text: plain_column.clone(),
                html_text: Some(broken.clone()),
                draft_type: ComposerDraftType::NewMessage,
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();
        drop(hold);
        drop(room);
        drop(client);
        manager.stop().await;
        drop(manager);
        let manager = AccountManager::open(&root).unwrap();
        manager
            .restore_last_active()
            .await
            .unwrap()
            .expect("No restored account");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The quarantined scope was discovered");
        assert_eq!(
            scope.restored.blocked(),
            Some(QuarantineReason::UnsupportedVersion)
        );
        assert_eq!(
            scope.restored.text.body, plain_column,
            "the stored plain column is what restores"
        );
        assert!(
            scope.sends.is_empty(),
            "nothing is inferred from unreadable metadata"
        );
        assert_eq!(
            scope
                .restored
                .envelope
                .quarantine()
                .unwrap()
                .original_html()
                .as_deref(),
            Some(broken.as_str())
        );
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        assert!(
            !client.send_queue().is_enabled(),
            "the client-wide queue is never enabled; rooms resume one by one"
        );
        assert!(
            !room.send_queue().is_enabled(),
            "the affected room stays paused"
        );
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([hold_id.clone(), fifth_txn.clone()])
        );
        // Both stored columns and the kind are carried, so the plain column
        // that differs from the HTML survives an edit of the live draft.
        let carried = scope.restored.envelope.quarantine().unwrap();
        assert_eq!(
            carried.original_plain().as_deref(),
            Some(plain_column.as_str())
        );
        assert_eq!(carried.original_kind(), "new");
        let revision = scope
            .restored
            .blocked_revision()
            .cloned()
            .expect("an unreadable original has an identity");
        assert!(matches!(
            manager
                .save_composer_draft_for(&a_account.id, a_room, "typed anyway")
                .await,
            Err(MatrixCoreError::RecoveryBlocked)
        ));
        assert!(
            !manager
                .room_recovery_incomplete(&selection, a_room)
                .await
                .unwrap()
        );
        let navigated = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(
            navigated
                .envelope
                .quarantine()
                .and_then(|row| row.original_html())
                .as_deref(),
            Some(broken.as_str()),
            "navigation reads the row without normalizing it"
        );
        assert_eq!(
            navigated.blocked_revision(),
            Some(&revision),
            "the identical carry keeps its identity across reloads"
        );
        assert!(
            matches!(
                manager
                    .save_composed_draft_for(
                        &a_account.id,
                        a_room,
                        None,
                        &empty,
                        &DraftEnvelope::fresh()
                    )
                    .await,
                Err(MatrixCoreError::StaleDraftSnapshot)
            ),
            "an ordinary save cannot drop the carried original"
        );
        // An edit of the live draft keeps the exact carry; both columns are
        // still there after the write and another restart.
        let mut edited_envelope = scope.restored.envelope.clone();
        edited_envelope.set_generation(Generation::fresh());
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &ComposeText::plain("edited while unreadable"),
                &edited_envelope,
            )
            .await
            .unwrap();
        let reloaded = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(reloaded.text.body, "edited while unreadable");
        let reloaded_carry = reloaded.envelope.quarantine().unwrap();
        assert_eq!(
            reloaded_carry.original_plain().as_deref(),
            Some(plain_column.as_str())
        );
        assert_eq!(
            reloaded_carry.original_html().as_deref(),
            Some(broken.as_str())
        );
        assert_eq!(
            reloaded.blocked_revision(),
            Some(&revision),
            "an edit around the carry does not change the original's identity"
        );
        // A different carry cannot replace the original, and a discard must
        // name exactly this original.
        let mut swapped = edited_envelope.clone();
        swapped.set_quarantine(morpheus_matrix::QuarantinedRow::new(
            QuarantineReason::Malformed,
            "someone else's original",
            None,
            "new",
            false,
        ));
        assert!(matches!(
            manager
                .save_composed_draft_for(&a_account.id, a_room, None, &empty, &swapped)
                .await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ));
        assert!(
            matches!(
                manager
                    .discard_quarantine_for(&selection, a_room, None, &RowRevision::fresh())
                    .await,
                Err(MatrixCoreError::PendingSendUnknown)
            ),
            "a stale discard names another original"
        );
        assert!(!room.send_queue().is_enabled());
        let (_, resumed) = room.send_queue().subscribe().await.unwrap();
        // The hold goes first; the room itself stays paused by the gate until
        // the explicit discard below resumes it.
        QueueHold::recover(&room, &hold_id)
            .await
            .release(&room)
            .await;
        assert!(
            !room.send_queue().is_enabled(),
            "lifting the hold resumes nothing by itself"
        );
        manager
            .discard_quarantine_for(&selection, a_room, None, &revision)
            .await
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &DraftEnvelope::fresh())
            .await
            .unwrap();
        assert!(
            room.send_queue().is_enabled(),
            "the explicit discard resumes the room"
        );
        wait_sent(resumed, &fifth_txn).await;
        assert!(persisted_requests(&room).await.is_empty());
        assert!(
            manager
                .load_composed_draft_for(&a_account.id, a_room, None)
                .await
                .unwrap()
                .envelope
                .is_empty()
        );

        phase!(recovery_unsupported);
        // 9. Readable rows another client stored with fields this build does
        // not write (attachments, a reply or edit kind) are shown but never
        // rewritten, and a discard is bound to the complete exact row: a
        // change to an attachment value or to the kind alone makes the earlier
        // review stale, with the same text in both columns.
        let readable = ComposeText::plain(format!("readable-with-fields-{marker}"));
        let readable_html = readable.encode_draft(&DraftEnvelope::fresh()).unwrap();
        let attachment = |data: &[u8]| DraftAttachment {
            filename: "note.txt".into(),
            content: DraftAttachmentContent::File {
                data: data.to_vec(),
                mimetype: Some("text/plain".into()),
                size: Some(data.len() as u64),
            },
        };
        let stored_row =
            |html: Option<String>,
             draft_type: ComposerDraftType,
             attachments: Vec<DraftAttachment>| ComposerDraft {
                plain_text: readable.body.clone(),
                html_text: html,
                draft_type,
                attachments,
            };
        // 9a. Attachment-only difference. Both text columns and the kind are
        // ordinary; only the attachment makes the row one this build must not
        // rewrite.
        let with_attachment = stored_row(
            Some(readable_html.clone()),
            ComposerDraftType::NewMessage,
            vec![attachment(b"v1")],
        );
        room.save_composer_draft(with_attachment.clone(), None)
            .await
            .unwrap();
        let shown = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(shown.text, readable, "the readable text restores");
        assert_eq!(shown.blocked(), Some(QuarantineReason::UnsupportedFields));
        assert!(
            shown.envelope.quarantine().is_none(),
            "nothing is carried; the row stays in place"
        );
        let first_review = shown
            .blocked_revision()
            .cloned()
            .expect("a preserved row has an identity");
        assert_eq!(
            manager
                .load_composed_draft_for(&a_account.id, a_room, None)
                .await
                .unwrap()
                .blocked_revision(),
            Some(&first_review),
            "the identical complete row keeps its identity across reloads"
        );
        assert!(matches!(
            manager
                .save_composer_draft_for(&a_account.id, a_room, "typed over it")
                .await,
            Err(MatrixCoreError::RecoveryBlocked)
        ));
        assert!(
            matches!(
                manager
                    .send_composed_owned_for(
                        &selection,
                        a_room,
                        ComposeSendTarget::Room,
                        &readable,
                        None,
                        None
                    )
                    .await,
                Err(MatrixCoreError::RecoveryBlocked)
            ),
            "a preserved scope refuses sends until reviewed"
        );
        assert_eq!(
            room.load_composer_draft(None).await.unwrap(),
            Some(with_attachment.clone()),
            "the stored row is untouched, attachment included"
        );
        // The attachment value changes underneath the review (same text, same
        // kind, one byte of attachment data): the reviewed identity is stale.
        room.save_composer_draft(
            stored_row(
                Some(readable_html.clone()),
                ComposerDraftType::NewMessage,
                vec![attachment(b"v2")],
            ),
            None,
        )
        .await
        .unwrap();
        assert!(
            matches!(
                manager
                    .discard_quarantine_for(&selection, a_room, None, &first_review)
                    .await,
                Err(MatrixCoreError::StaleDraftSnapshot)
            ),
            "a discard reviewed against the old attachment value releases nothing"
        );
        assert!(matches!(
            manager
                .save_composer_draft_for(&a_account.id, a_room, "still blocked")
                .await,
            Err(MatrixCoreError::RecoveryBlocked)
        ));
        let changed = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        let second_review = changed.blocked_revision().cloned().unwrap();
        assert_ne!(
            second_review, first_review,
            "a different complete row is a different identity"
        );
        assert!(matches!(
            manager
                .discard_quarantine_for(&selection, a_room, None, &first_review)
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        manager
            .discard_quarantine_for(&selection, a_room, None, &second_review)
            .await
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &DraftEnvelope::fresh())
            .await
            .unwrap();
        assert_eq!(
            room.load_composer_draft(None).await.unwrap(),
            None,
            "only the exact reviewed row was replaced"
        );
        // 9b. Kind-only difference: a reply draft becomes an edit draft with
        // identical text and no attachments.
        let target = EventId::parse("$kind-only:example.org").unwrap();
        room.save_composer_draft(
            stored_row(
                Some(readable_html.clone()),
                ComposerDraftType::Reply {
                    event_id: target.clone(),
                },
                Vec::new(),
            ),
            None,
        )
        .await
        .unwrap();
        let reply = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(reply.text, readable);
        assert_eq!(reply.blocked(), Some(QuarantineReason::UnsupportedFields));
        let reply_review = reply.blocked_revision().cloned().unwrap();
        room.save_composer_draft(
            stored_row(
                Some(readable_html.clone()),
                ComposerDraftType::Edit {
                    event_id: target.clone(),
                },
                Vec::new(),
            ),
            None,
        )
        .await
        .unwrap();
        assert!(
            matches!(
                manager
                    .discard_quarantine_for(&selection, a_room, None, &reply_review)
                    .await,
                Err(MatrixCoreError::StaleDraftSnapshot)
            ),
            "the kind changed underneath the review"
        );
        let edit_review = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap()
            .blocked_revision()
            .cloned()
            .unwrap();
        assert_ne!(edit_review, reply_review);
        manager
            .discard_quarantine_for(&selection, a_room, None, &edit_review)
            .await
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &DraftEnvelope::fresh())
            .await
            .unwrap();
        assert_eq!(room.load_composer_draft(None).await.unwrap(), None);
        // 9c. A plain draft without HTML but with an attachment: the body
        // shows, every save is refused and the row (attachment included) is
        // still there until the explicit discard; the save after it writes
        // Morpheus's own row.
        let plain_with_attachment = stored_row(
            None,
            ComposerDraftType::NewMessage,
            vec![attachment(b"keep me")],
        );
        room.save_composer_draft(plain_with_attachment.clone(), None)
            .await
            .unwrap();
        let preserved = manager
            .load_composed_draft_for(&a_account.id, a_room, None)
            .await
            .unwrap();
        assert_eq!(preserved.text, readable);
        assert!(!preserved.discarded_rich_intent);
        assert_eq!(
            preserved.blocked(),
            Some(QuarantineReason::UnsupportedFields)
        );
        assert!(matches!(
            manager
                .save_composer_draft_for(&a_account.id, a_room, "normalize me")
                .await,
            Err(MatrixCoreError::RecoveryBlocked)
        ));
        assert_eq!(
            room.load_composer_draft(None).await.unwrap(),
            Some(plain_with_attachment.clone()),
            "a readable plain row with an attachment is never normalized"
        );
        let plain_review = preserved.blocked_revision().cloned().unwrap();
        manager
            .discard_quarantine_for(&selection, a_room, None, &plain_review)
            .await
            .unwrap();
        // Discard releases this exact revision for a typed replacement. The
        // plain convenience API still refuses to normalize unsupported data.
        assert!(matches!(
            manager
                .save_composer_draft_for(&a_account.id, a_room, "after review")
                .await,
            Err(MatrixCoreError::RecoveryBlocked)
        ));
        manager
            .save_composed_draft_for(
                &a_account.id,
                a_room,
                None,
                &ComposeText::plain("after review"),
                &DraftEnvelope::fresh(),
            )
            .await
            .unwrap();
        let rewritten = room
            .load_composer_draft(None)
            .await
            .unwrap()
            .expect("Morpheus wrote its own row");
        assert_eq!(rewritten.plain_text, "after review");
        assert!(
            rewritten.attachments.is_empty()
                && rewritten.draft_type == ComposerDraftType::NewMessage
        );
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &DraftEnvelope::fresh())
            .await
            .unwrap();

        phase!(recovery_coverage);
        // 10. Positive coverage. A room that was not in the discovery snapshot
        // is not covered: it stays paused and recovery-incomplete until it is
        // scanned, after which ordinary sends work. A room that was left is
        // skipped by discovery and stays paused with whatever it holds; after
        // it is joined again it is scanned on demand, and unreadable saved
        // data there pauses it until the explicit discard.
        let new_room = manager
            .create_private_room(&format!("recovery-new-{marker}"))
            .await
            .expect("Cannot create a room after discovery");
        wait_for_room(&manager, &new_room).await;
        let sdk_new_room = client
            .get_room(&RoomId::parse(&new_room).unwrap())
            .expect("New room missing");
        assert!(
            manager
                .room_recovery_incomplete(&selection, &new_room)
                .await
                .unwrap(),
            "a room joined after the snapshot is not covered"
        );
        assert!(
            matches!(
                manager
                    .send_composed_owned_for(
                        &selection,
                        &new_room,
                        ComposeSendTarget::Room,
                        &ninth_text,
                        None,
                        None
                    )
                    .await,
                Err(MatrixCoreError::RecoveryIncomplete)
            ),
            "nothing resumes an uncovered room's queue"
        );
        assert!(!sdk_new_room.send_queue().is_enabled());
        assert!(
            manager
                .recover_room_for(&selection, &new_room)
                .await
                .unwrap()
                .is_empty(),
            "the safe scan finds nothing to recover"
        );
        assert!(
            !manager
                .room_recovery_incomplete(&selection, &new_room)
                .await
                .unwrap()
        );
        assert!(
            sdk_new_room.send_queue().is_enabled(),
            "a covered room without saved data resumes"
        );
        let mut new_envelope = DraftEnvelope::fresh();
        let (queued_ninth, ninth_txn) = record_and_enqueue(
            &manager,
            &selection,
            &a_account.id,
            &new_room,
            &mut new_envelope,
            &ninth,
            &ninth_text,
        )
        .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), queued_ninth.wait_outcome())
                .await
                .expect("Timed out waiting for the send in the new room"),
            QueuedSendOutcome::Sent,
            "a legitimate new room works after the safe scan"
        );
        manager
            .release_pending_record_for(
                &selection,
                &new_room,
                None,
                &ninth,
                Some(ninth_txn.as_str()),
            )
            .await
            .unwrap();
        assert!(
            new_envelope
                .remove_pending(&ninth, Some(&ninth_txn))
                .is_some()
        );
        manager
            .save_composed_draft_for(&a_account.id, &new_room, None, &empty, &new_envelope)
            .await
            .unwrap();
        // Leave with a persisted request and an unreadable row behind. Keep
        // a different fixture user joined so the room remains hosted while the
        // tested account is away. Its public join rule permits that account to
        // return without an invitation changing the Left-state assertions.
        sdk_new_room
            .send_state_event(RoomJoinRulesEventContent::new(JoinRule::Public))
            .await
            .expect("Cannot open the room's join rule");
        let keeper_identity = fixture.account("a_second_device");
        assert_ne!(keeper_identity.user_id, alice.user_id);
        assert_eq!(keeper_identity.homeserver_url, alice.homeserver_url);
        let keeper = AccountManager::open(root.join("room-keeper"))
            .expect("Cannot open the separate keeper profile");
        let keeper_account = keeper
            .add_password_account(
                &keeper_identity.homeserver_url,
                &keeper_identity.username,
                &keeper_identity.password,
            )
            .await
            .expect("Cannot sign in the fixture room keeper");
        keeper
            .join_room(&new_room)
            .await
            .expect("Cannot join the fixture room keeper");
        // The hold works in an encrypted room too: the oversize event is
        // encrypted and still refused, or fails to encrypt, either way
        // permanently; a delivered hold fails this fixture.
        let hold = QueueHold::place(&sdk_new_room).await;
        let hold_id = hold.transaction_id.clone();
        let mut left_envelope = DraftEnvelope::fresh();
        let (_queued_eighth, eighth_txn) = record_and_enqueue(
            &manager,
            &selection,
            &a_account.id,
            &new_room,
            &mut left_envelope,
            &eighth,
            &eighth_text,
        )
        .await;
        assert_eq!(
            persisted_requests(&sdk_new_room).await,
            HashSet::from([hold_id.clone(), eighth_txn.clone()])
        );
        let left_row = sdk_new_room
            .load_composer_draft(None)
            .await
            .unwrap()
            .expect("Row missing");
        let left_broken = left_row
            .html_text
            .clone()
            .expect("Row has no envelope")
            .replace("data-morpheus-compose=\"2\"", "data-morpheus-compose=\"9\"");
        sdk_new_room
            .save_composer_draft(
                ComposerDraft {
                    plain_text: left_row.plain_text.clone(),
                    html_text: Some(left_broken.clone()),
                    draft_type: ComposerDraftType::NewMessage,
                    attachments: Vec::new(),
                },
                None,
            )
            .await
            .unwrap();
        manager
            .leave_room(&new_room)
            .await
            .expect("Cannot leave the room");
        let deadline = Instant::now() + Duration::from_secs(60);
        while sdk_new_room.state() != RoomState::Left {
            assert!(Instant::now() < deadline, "Timed out waiting to leave");
            sleep(Duration::from_millis(300)).await;
        }
        drop(hold);
        drop(sdk_new_room);
        drop(room);
        drop(client);
        manager.stop().await;
        drop(manager);
        let manager = AccountManager::open(&root).unwrap();
        manager
            .restore_last_active()
            .await
            .unwrap()
            .expect("No restored account");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        assert!(
            recovered.iter().all(|draft| draft.room_id != new_room),
            "discovery skips a left room; nothing there is adopted or enabled"
        );
        let client = manager.active_client().await.unwrap();
        let sdk_new_room = client
            .get_room(&RoomId::parse(&new_room).unwrap())
            .expect("Left room missing from the store");
        assert_eq!(sdk_new_room.state(), RoomState::Left);
        assert!(
            manager
                .room_recovery_incomplete(&selection, &new_room)
                .await
                .unwrap(),
            "a skipped room is not covered"
        );
        assert!(
            matches!(
                manager.recover_room_for(&selection, &new_room).await,
                Err(MatrixCoreError::RecoveryIncomplete)
            ),
            "a room that is not joined is not scanned"
        );
        assert!(
            !sdk_new_room.send_queue().is_enabled(),
            "the left room stays paused"
        );
        assert_eq!(
            persisted_requests(&sdk_new_room).await,
            HashSet::from([hold_id.clone(), eighth_txn.clone()]),
            "its persisted request is kept, not dropped or sent"
        );
        // Composed sends reject a non-joined room before recovery coverage.
        // Require that exact local guard, then prove rejection changed no queue
        // state; do not loosen this to accepting an arbitrary failed send.
        assert!(matches!(
            manager
                .send_composed_owned_for(
                    &selection,
                    &new_room,
                    ComposeSendTarget::Room,
                    &eighth_text,
                    None,
                    None
                )
                .await,
            Err(MatrixCoreError::Sdk(message))
                if message == "A selected mention is unavailable or is no longer allowed in this room."
        ));
        assert!(!sdk_new_room.send_queue().is_enabled());
        assert_eq!(
            persisted_requests(&sdk_new_room).await,
            HashSet::from([hold_id.clone(), eighth_txn.clone()]),
            "the rejected send neither queues nor removes a request"
        );
        assert!(manager
            .room_recovery_incomplete(&selection, &new_room)
            .await
            .unwrap());
        // Confirm on the homeserver, under the independent keeper account,
        // that another user remains joined after the tested account left.
        // No invite or queue action changes the tested account's Left state.
        let keeper_client = keeper.active_client().await.expect("Keeper client missing");
        let membership = keeper_client
            .send(get_state_event_for_key::v3::Request::new(
                RoomId::parse(&new_room).unwrap(),
                StateEventType::RoomMember,
                keeper_identity.user_id.clone(),
            ))
            .await
            .expect("Cannot fetch the keeper membership");
        let membership: serde_json::Value =
            serde_json::from_str(membership.event_or_content.get()).unwrap();
        assert_eq!(membership["membership"], "join");
        assert_eq!(sdk_new_room.state(), RoomState::Left);
        drop(keeper_client);
        // Joined again: still not covered until the on-demand scan, which
        // finds the unreadable row and keeps the room paused.
        manager
            .join_room(&new_room)
            .await
            .expect("Cannot rejoin the room");
        wait_for_room(&manager, &new_room).await;
        let deadline = Instant::now() + Duration::from_secs(60);
        while sdk_new_room.state() != RoomState::Joined {
            assert!(Instant::now() < deadline, "Timed out waiting to rejoin");
            sleep(Duration::from_millis(300)).await;
        }
        assert!(
            manager
                .room_recovery_incomplete(&selection, &new_room)
                .await
                .unwrap(),
            "rejoining covers nothing by itself"
        );
        assert!(!sdk_new_room.send_queue().is_enabled());
        let rejoined = manager
            .recover_room_for(&selection, &new_room)
            .await
            .unwrap();
        let left_scope = rejoined
            .iter()
            .find(|draft| draft.room_id == new_room && draft.thread_root.is_none())
            .expect("the rejoined room's unreadable row is found by the on-demand scan");
        assert_eq!(
            left_scope.restored.blocked(),
            Some(QuarantineReason::UnsupportedVersion)
        );
        assert!(
            left_scope.sends.is_empty(),
            "nothing is inferred from unreadable metadata"
        );
        assert!(
            !manager
                .room_recovery_incomplete(&selection, &new_room)
                .await
                .unwrap(),
            "covered now, but blocked"
        );
        assert!(
            !sdk_new_room.send_queue().is_enabled(),
            "unreadable saved data keeps the room paused"
        );
        assert!(matches!(
            manager
                .send_composed_owned_for(
                    &selection,
                    &new_room,
                    ComposeSendTarget::Room,
                    &eighth_text,
                    None,
                    None
                )
                .await,
            Err(MatrixCoreError::RecoveryBlocked)
        ));
        let left_revision = left_scope.restored.blocked_revision().cloned().unwrap();
        let (_, resumed_again) = sdk_new_room.send_queue().subscribe().await.unwrap();
        QueueHold::recover(&sdk_new_room, &hold_id)
            .await
            .release(&sdk_new_room)
            .await;
        assert!(
            !sdk_new_room.send_queue().is_enabled(),
            "lifting the hold resumes nothing by itself"
        );
        manager
            .discard_quarantine_for(&selection, &new_room, None, &left_revision)
            .await
            .unwrap();
        manager
            .save_composed_draft_for(
                &a_account.id,
                &new_room,
                None,
                &empty,
                &DraftEnvelope::fresh(),
            )
            .await
            .unwrap();
        assert!(
            sdk_new_room.send_queue().is_enabled(),
            "the explicit discard resumes the rejoined room"
        );
        wait_sent(resumed_again, &eighth_txn).await;
        assert!(persisted_requests(&sdk_new_room).await.is_empty());
        manager
            .leave_room(&new_room)
            .await
            .expect("Cannot leave the room again");
        drop(sdk_new_room);

        drop(client);
        account_removal::finish_logout(
            &manager,
            &a_account.id,
            &root.join("accounts").join(&a_account.id),
            manager.logout_account(&a_account.id).await,
        )
        .await
        .expect("Cannot remove first account");
        manager
            .switch_to(&b_account.id)
            .await
            .expect("Cannot switch to second account");
        account_removal::finish_logout(
            &manager,
            &b_account.id,
            &root.join("accounts").join(&b_account.id),
            manager.logout_account(&b_account.id).await,
        )
        .await
        .expect("Cannot remove second account");
        keeper
            .leave_room(&new_room)
            .await
            .expect("Cannot leave the fixture keeper room");
        account_removal::finish_logout(
            &keeper,
            &keeper_account.id,
            &root.join("room-keeper").join("accounts").join(&keeper_account.id),
            keeper.logout_account(&keeper_account.id).await,
        )
        .await
        .expect("Cannot remove the fixture keeper account");
        keeper.stop().await;
        drop(keeper);
        phase!(recovery_stop);
        manager.stop().await;
    });
    phase!(recovery_cleanup);
    fs::remove_dir_all(root).expect("Cannot clean local test data");
    phase!(recovery_complete);
}
