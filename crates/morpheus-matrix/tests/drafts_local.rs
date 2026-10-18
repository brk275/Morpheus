//! Opt-in encrypted draft acceptance test against two disposable local Synapse
//! homeservers. Fixture credentials are read from a mode-0600 file and never
//! printed by this test.
#![recursion_limit = "512"]

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use matrix_sdk::{
    ComposerDraft, ComposerDraftType,
    ruma::{EventId, RoomId},
    send_queue::RoomSendQueueUpdate,
};
use morpheus_matrix::{
    AccountManager, AccountSelection, ComposeSendTarget, ComposeText, DraftEnvelope, Generation,
    MatrixCoreError, MentionTarget, QuarantineReason, QueuedSendOutcome, TextFormat,
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
        // Records are written as enqueuing first and receive their ids in a
        // second write, as the controller does; the core refuses a record that
        // appears already queued, since nothing of this process created it.
        let mut pending_room_envelope = DraftEnvelope::fresh();
        pending_room_envelope
            .begin_pending(first_nonce.clone(), Generation::fresh(), &pending_room_text, false)
            .expect("first room record");
        pending_room_envelope
            .begin_pending(second_nonce.clone(), Generation::fresh(), &pending_room_text, false)
            .expect("second room record");
        assert!(
            pending_room_envelope
                .index_thread(pending_thread)
                .expect("thread index")
        );
        let mut forged = pending_room_envelope.clone();
        assert!(forged.mark_queued(&first_nonce, &first_txn));
        assert!(matches!(
            manager
                .save_composed_draft_for(&a_account.id, a_room, None, &pending_room_text, &forged)
                .await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ), "a record this process never saved as enqueuing cannot appear queued");
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &pending_room_text, &pending_room_envelope)
            .await
            .expect("Cannot persist enqueuing room records");
        assert!(pending_room_envelope.mark_queued(&first_nonce, &first_txn));
        assert!(pending_room_envelope.mark_queued(&second_nonce, &second_txn));
        let mut pending_thread_envelope = DraftEnvelope::fresh();
        pending_thread_envelope
            .begin_pending(thread_nonce.clone(), Generation::fresh(), &pending_thread_text, true)
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
            restored_room.blocked().is_none(),
            "Recovery metadata failed closed on a valid envelope"
        );
        assert_eq!(
            restored_room.envelope.pending()[0].text().to_compose().unwrap(),
            pending_room_text,
            "Each record keeps the text it was created from"
        );
        let a_selection = manager.account_selection(&a_account.id).expect("Selection missing");
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
            .release_pending_record_for(&a_selection, a_room, None, &second_nonce, Some(&second_txn))
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
            .release_pending_record_for(&a_selection, a_room, Some(pending_thread), &thread_nonce, None)
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
        .send_composed_owned_for(selection, room_id, ComposeSendTarget::Room, text, None, Some(nonce))
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
                Ok(RoomSendQueueUpdate::SentEvent { transaction_id: sent, .. })
                    if sent.as_str() == transaction_id =>
                {
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
        let first_text = ComposeText::new(format!("recovery-first-{marker} **kept**"), TextFormat::Markdown);
        let second_text = ComposeText::plain(format!("recovery-second-{marker}"));
        let third_text = ComposeText::plain(format!("recovery-third-{marker}"));
        let fourth_text = ComposeText::new(format!("recovery-fourth-{marker} _adopted_"), TextFormat::Markdown);
        let fifth_text = ComposeText::plain(format!("recovery-fifth-{marker}"));
        let sixth_text = ComposeText::plain(format!("recovery-sixth-{marker}"));
        let (first, second, third, fourth, fifth, sixth) = (
            Generation::fresh(), Generation::fresh(), Generation::fresh(),
            Generation::fresh(), Generation::fresh(), Generation::fresh(),
        );

        let manager = AccountManager::open(&root).expect("Cannot open account manager");
        let a_account = manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("First login failed");
        wait_for_room(&manager, a_room).await;
        let selection = manager.account_selection(&a_account.id).expect("Selection missing");
        // Discovery is the gate for recorded sends; waiting for it is the
        // same wait the controller performs.
        assert!(manager.take_recovered_drafts(&selection).await.unwrap().is_empty());
        let client = manager.active_client().await.expect("Client missing");
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).expect("Room missing");

        // Barrier: hold the SDK queue so persisted requests stay observable.
        client.send_queue().set_enabled(false).await;
        assert!(persisted_requests(&room).await.is_empty());

        // 1. Pending record durable before enqueue; enqueue needs the grant.
        let mut envelope = DraftEnvelope::fresh();
        envelope
            .begin_pending(first.clone(), envelope.generation().clone(), &first_text, false)
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &first_text, &envelope)
            .await
            .unwrap();
        let on_disk = manager.load_composed_draft_for(&a_account.id, a_room, None).await.unwrap();
        assert_eq!(on_disk.envelope.pending().len(), 1);
        assert!(persisted_requests(&room).await.is_empty(), "the row leads the SDK");
        assert!(matches!(
            manager
                .send_composed_owned_for(&selection, a_room, ComposeSendTarget::Room, &first_text, None, Some(&first))
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ), "no grant, no enqueue");
        manager.grant_enqueue_for(&selection, a_room, None, &first).await.unwrap();
        let queued_first = manager
            .send_composed_owned_for(&selection, a_room, ComposeSendTarget::Room, &first_text, None, Some(&first))
            .await
            .unwrap();
        assert!(matches!(
            manager
                .send_composed_owned_for(&selection, a_room, ComposeSendTarget::Room, &first_text, None, Some(&first))
                .await,
            Err(MatrixCoreError::PendingSendUnknown)
        ), "the grant is single use");
        let first_txn = queued_first.transaction_id().to_owned();
        assert!(envelope.mark_queued(&first, &first_txn));
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &first_text, &envelope)
            .await
            .unwrap();
        let (queued_second, second_txn) =
            record_and_enqueue(&manager, &selection, &a_account.id, a_room, &mut envelope, &second, &second_text).await;
        assert_ne!(first_txn, second_txn);
        assert_eq!(
            persisted_requests(&room).await,
            HashSet::from([first_txn.clone(), second_txn.clone()])
        );

        // 2. Stale snapshots and wrong authority touch nothing.
        let mut stale = envelope.clone();
        assert!(stale.remove_pending(&first, Some(&first_txn)).is_some());
        assert!(matches!(
            manager.save_composed_draft_for(&a_account.id, a_room, None, &second_text, &stale).await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ));
        assert!(matches!(
            manager.cancel_pending_send_for(&selection, a_room, None, &first, &second_txn).await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(matches!(
            manager.cancel_pending_send_for(&selection, a_room, None, &Generation::fresh(), &first_txn).await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(matches!(
            manager.release_pending_record_for(&selection, a_room, None, &first, None).await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert_eq!(persisted_requests(&room).await.len(), 2);
        let on_disk = manager.load_composed_draft_for(&a_account.id, a_room, None).await.unwrap();
        assert_eq!(on_disk.envelope.pending().len(), 2);

        // 3. Out of order: the second is cancelled exactly; the first keeps
        // its typed Markdown submission.
        assert!(manager
            .cancel_pending_send_for(&selection, a_room, None, &second, &second_txn)
            .await
            .unwrap());
        assert_eq!(queued_second.wait_outcome().await, QueuedSendOutcome::Failed);
        manager
            .release_pending_record_for(&selection, a_room, None, &second, Some(&second_txn))
            .await
            .unwrap();
        assert!(envelope.remove_pending(&second, Some(&second_txn)).is_some());
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &second_text, &envelope)
            .await
            .unwrap();
        let after = manager.load_composed_draft_for(&a_account.id, a_room, None).await.unwrap();
        assert_eq!(after.envelope.pending().len(), 1);
        assert_eq!(after.envelope.pending()[0].text().to_compose().unwrap(), first_text);
        assert_eq!(after.text, second_text, "the live text is untouched by the resolution");
        assert_eq!(persisted_requests(&room).await, HashSet::from([first_txn.clone()]));

        // 4. Crash windows. A third record is saved as enqueuing and never
        // enqueued; the first sends while the row still says queued; the
        // process stops before either is acknowledged in the row.
        envelope.set_generation(Generation::fresh());
        envelope
            .begin_pending(third.clone(), envelope.generation().clone(), &third_text, false)
            .unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &third_text, &envelope)
            .await
            .unwrap();
        client.send_queue().set_enabled(true).await;
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
            assert!(!contains_plaintext(&root, text.body.as_bytes()), "record text leaked");
        }

        let manager = AccountManager::open(&root).expect("Cannot reopen account manager");
        let restored_account = manager.restore_last_active().await.unwrap().expect("No restored account");
        assert_eq!(restored_account.id, a_account.id);
        let selection = manager.account_selection(&a_account.id).unwrap();
        let recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The room scope was discovered");
        assert_eq!(scope.sends.len(), 2);
        assert!(scope.sends.iter().all(|send| send.queued.is_none()), "no SDK request: unconfirmed, never sent");
        let first_send = scope.sends.iter().find(|send| send.nonce == first).unwrap();
        assert_eq!(first_send.transaction_id.as_deref(), Some(first_txn.as_str()));
        let third_send = scope.sends.iter().find(|send| send.nonce == third).unwrap();
        assert!(third_send.transaction_id.is_none());
        assert_eq!(scope.restored.envelope.find(&first).unwrap().text().to_compose().unwrap(), first_text);
        assert_eq!(scope.restored.envelope.find(&third).unwrap().text().to_compose().unwrap(), third_text);
        assert_eq!(scope.restored.text, third_text);
        assert!(matches!(
            manager.cancel_pending_send_for(&selection, a_room, None, &first, &first_txn).await,
            Err(MatrixCoreError::PendingSendUnavailable)
        ));
        assert!(matches!(
            manager.retry_pending_send_for(&selection, a_room, None, &first, &first_txn).await,
            Err(MatrixCoreError::PendingSendUnavailable)
        ));
        // Restored records cannot be replayed through a new enqueue.
        assert!(matches!(
            manager.grant_enqueue_for(&selection, a_room, None, &third).await,
            Err(MatrixCoreError::PendingSendUnknown)
        ));
        assert!(matches!(
            manager
                .send_composed_owned_for(&selection, a_room, ComposeSendTarget::Room, &third_text, None, Some(&third))
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
            manager.save_composed_draft_for(&a_account.id, a_room, None, &third_text, &reviewed).await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ));
        manager.release_pending_record_for(&selection, a_room, None, &first, Some(&first_txn)).await.unwrap();
        manager.save_composed_draft_for(&a_account.id, a_room, None, &third_text, &reviewed).await.unwrap();
        manager.release_pending_record_for(&selection, a_room, None, &third, None).await.unwrap();
        assert!(reviewed.remove_pending(&third, None).is_some());
        manager.save_composed_draft_for(&a_account.id, a_room, None, &empty, &reviewed).await.unwrap();
        assert!(manager.load_composed_draft_for(&a_account.id, a_room, None).await.unwrap().envelope.is_empty());

        // 5. A request the SDK still holds across a restart is adopted
        // exactly, through the subscription taken before the queue resumes.
        client.send_queue().set_enabled(false).await;
        let mut envelope = DraftEnvelope::fresh();
        let (_queued_fourth, fourth_txn) =
            record_and_enqueue(&manager, &selection, &a_account.id, a_room, &mut envelope, &fourth, &fourth_text).await;
        drop(room);
        drop(client);
        manager.stop().await;
        drop(manager);
        let manager = AccountManager::open(&root).unwrap();
        manager.restore_last_active().await.unwrap().expect("No restored account");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let mut recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter_mut()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The room scope was discovered");
        let send = scope.sends.iter_mut().find(|send| send.nonce == fourth).unwrap();
        assert_eq!(send.transaction_id.as_deref(), Some(fourth_txn.as_str()));
        let adopted = send.queued.take().expect("the exact persisted request was adopted");
        assert!(matches!(
            manager.cancel_pending_send_for(&selection, a_room, None, &Generation::fresh(), &fourth_txn).await,
            Err(MatrixCoreError::PendingSendUnknown)
        ), "another owner cannot touch it");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), adopted.wait_outcome())
                .await
                .expect("Timed out waiting for the adopted send"),
            QueuedSendOutcome::Sent
        );
        assert_eq!(scope.restored.envelope.find(&fourth).unwrap().text().to_compose().unwrap(), fourth_text);
        manager.release_pending_record_for(&selection, a_room, None, &fourth, Some(&fourth_txn)).await.unwrap();
        let mut done = scope.restored.envelope.clone();
        assert!(done.remove_pending(&fourth, Some(&fourth_txn)).is_some());
        manager.save_composed_draft_for(&a_account.id, a_room, None, &empty, &done).await.unwrap();

        // 6. Revocation ends waits and refuses controls without another SDK
        // event; re-selection rediscovers and adopts before the queue resumes.
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        client.send_queue().set_enabled(false).await;
        let mut envelope = DraftEnvelope::fresh();
        let (_queued_sixth, sixth_txn) =
            record_and_enqueue(&manager, &selection, &a_account.id, a_room, &mut envelope, &sixth, &sixth_text).await;
        drop(room);
        drop(client);
        let old_selection = selection;
        let b_account = manager
            .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
            .await
            .expect("Second login failed");
        wait_for_room(&manager, b_room).await;
        assert!(!old_selection.is_current());
        assert!(matches!(
            manager.take_recovered_drafts(&old_selection).await,
            Err(MatrixCoreError::SelectedAccountChanged)
        ), "a revoked selection ends the wait at once");
        assert!(matches!(
            manager.cancel_pending_send_for(&old_selection, a_room, None, &sixth, &sixth_txn).await,
            Err(MatrixCoreError::SelectedAccountChanged)
        ));
        let b_selection = manager.account_selection(&b_account.id).unwrap();
        assert!(manager.take_recovered_drafts(&b_selection).await.unwrap().is_empty());
        manager.switch_to(&a_account.id).await.expect("Cannot switch back");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let mut recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter_mut()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The room scope was rediscovered");
        let send = scope.sends.iter_mut().find(|send| send.nonce == sixth).unwrap();
        let adopted = send.queued.take().expect("the request was adopted again on re-selection");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), adopted.wait_outcome())
                .await
                .expect("Timed out waiting for the re-adopted send"),
            QueuedSendOutcome::Sent
        );
        manager.release_pending_record_for(&selection, a_room, None, &sixth, Some(&sixth_txn)).await.unwrap();
        let mut done = scope.restored.envelope.clone();
        assert!(done.remove_pending(&sixth, Some(&sixth_txn)).is_some());
        manager.save_composed_draft_for(&a_account.id, a_room, None, &empty, &done).await.unwrap();

        // 7. A row this build cannot read pauses its room's queue, keeps the
        // request and the complete original, restores the typed body, and
        // resumes only after the explicit discard.
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        client.send_queue().set_enabled(false).await;
        let mut envelope = DraftEnvelope::fresh();
        let (_queued_fifth, fifth_txn) =
            record_and_enqueue(&manager, &selection, &a_account.id, a_room, &mut envelope, &fifth, &fifth_text).await;
        // What a newer Morpheus would leave behind: the same row under a
        // version this build does not know, written through the same SDK API.
        let row = room.load_composer_draft(None).await.unwrap().expect("Row missing");
        let broken = row
            .html_text
            .clone()
            .expect("Row has no envelope")
            .replace("data-morpheus-compose=\"2\"", "data-morpheus-compose=\"9\"");
        room.save_composer_draft(
            ComposerDraft {
                plain_text: row.plain_text.clone(),
                html_text: Some(broken.clone()),
                draft_type: ComposerDraftType::NewMessage,
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();
        drop(room);
        drop(client);
        manager.stop().await;
        drop(manager);
        let manager = AccountManager::open(&root).unwrap();
        manager.restore_last_active().await.unwrap().expect("No restored account");
        let selection = manager.account_selection(&a_account.id).unwrap();
        let recovered = manager.take_recovered_drafts(&selection).await.unwrap();
        let scope = recovered
            .iter()
            .find(|draft| draft.room_id == a_room && draft.thread_root.is_none())
            .expect("The quarantined scope was discovered");
        assert_eq!(scope.restored.blocked(), Some(QuarantineReason::UnsupportedVersion));
        assert_eq!(scope.restored.text.body, fifth_text.body, "the typed body still restores");
        assert!(scope.sends.is_empty(), "nothing is inferred from unreadable metadata");
        assert_eq!(
            scope.restored.envelope.quarantine().unwrap().original_html().as_deref(),
            Some(broken.as_str())
        );
        let client = manager.active_client().await.unwrap();
        let room = client.get_room(&RoomId::parse(a_room).unwrap()).unwrap();
        assert!(client.send_queue().is_enabled(), "other rooms are not held hostage");
        assert!(!room.send_queue().is_enabled(), "the affected room stays paused");
        assert_eq!(persisted_requests(&room).await, HashSet::from([fifth_txn.clone()]));
        assert!(matches!(
            manager.save_composer_draft_for(&a_account.id, a_room, "typed anyway").await,
            Err(MatrixCoreError::RecoveryBlocked)
        ));
        assert!(!manager.room_recovery_incomplete(&selection, a_room).await.unwrap());
        assert_eq!(
            manager
                .load_composed_draft_for(&a_account.id, a_room, None)
                .await
                .unwrap()
                .envelope
                .quarantine()
                .and_then(|row| row.original_html())
                .as_deref(),
            Some(broken.as_str()),
            "navigation reads the row without normalizing it"
        );
        assert!(matches!(
            manager.save_composed_draft_for(&a_account.id, a_room, None, &empty, &DraftEnvelope::fresh()).await,
            Err(MatrixCoreError::StaleDraftSnapshot)
        ), "an ordinary save cannot drop the carried original");
        let (_, resumed) = room.send_queue().subscribe().await.unwrap();
        manager.discard_quarantine_for(&selection, a_room, None).await.unwrap();
        manager
            .save_composed_draft_for(&a_account.id, a_room, None, &empty, &DraftEnvelope::fresh())
            .await
            .unwrap();
        assert!(room.send_queue().is_enabled(), "the explicit discard resumes the room");
        wait_sent(resumed, &fifth_txn).await;
        assert!(persisted_requests(&room).await.is_empty());
        assert!(manager.load_composed_draft_for(&a_account.id, a_room, None).await.unwrap().envelope.is_empty());

        drop(room);
        drop(client);
        manager.logout_account(&a_account.id).await.expect("Cannot remove first account");
        manager.switch_to(&b_account.id).await.expect("Cannot switch to second account");
        manager.logout_account(&b_account.id).await.expect("Cannot remove second account");
        manager.stop().await;
    });
    fs::remove_dir_all(root).expect("Cannot clean local test data");
}
