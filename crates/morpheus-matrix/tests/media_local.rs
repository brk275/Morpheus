//! Opt-in encrypted attachment roundtrip against two disposable local Matrix
//! accounts. Credentials are read from a protected fixture and never logged.
#![recursion_limit = "512"]

#[path = "support/opened_room.rs"]
mod opened_room;

use std::{collections::HashMap, fs, path::Path, time::Duration};

use matrix_sdk::ruma::{
    EventId, RoomId,
    events::{
        relation::Thread,
        room::{ImageInfo, message::Relation},
        sticker::StickerEventContent,
    },
};
use morpheus_matrix::{AccountManager, AttachmentKind, AttachmentSendUpdate, MatrixCoreError};
use serde::Deserialize;
use tokio::time::{Instant, sleep, timeout, timeout_at};
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
    homeserver_url: String,
    password: String,
}

impl Fixture {
    fn account(&self, label: &str) -> &TestAccount {
        self.accounts
            .iter()
            .find(|account| account.label == label)
            .expect("Missing local account")
    }
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if manager
            .rooms_snapshot()
            .await
            .expect("Room snapshot failed")
            .iter()
            .any(|room| room.room_id == room_id)
        {
            opened_room::scan(manager, room_id)
                .await
                .expect("Opened fixture room must be scanned");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for encrypted room"
        );
        sleep(Duration::from_millis(300)).await;
    }
}

fn contains_plaintext(path: &Path, needle: &[u8]) -> bool {
    for entry in fs::read_dir(path).expect("Cannot inspect account store") {
        let path = entry.expect("Cannot inspect store entry").path();
        if path.is_dir() {
            if contains_plaintext(&path, needle) {
                return true;
            }
        } else if fs::read(&path)
            .expect("Cannot read account store file")
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return true;
        }
    }
    false
}

// Original public 1×1 PNG: RGBA [13, 29, 47, 255], with standard PNG CRCs.
// No external asset, media decoder or dependency is needed by this API fixture.
const PREVIEW_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xe0, 0x95, 0xd5, 0xff,
    0x0f, 0x00, 0x01, 0xed, 0x01, 0x59, 0x8a, 0xb4, 0xa8, 0xad, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

// The caller's existing absolute deadline bounds both enqueue and observation.
// Retained requests are never re-enqueued as a substitute for acknowledgement.
async fn queue_preview_image(
    sender: &AccountManager,
    account_id: &str,
    room_id: &str,
    path: &Path,
) -> Result<String, ()> {
    let mut queued = sender
        .send_attachment_for(account_id, room_id, path, "image/png")
        .await
        .map_err(|_| ())?;
    loop {
        match queued.next_update().await {
            AttachmentSendUpdate::Sent { event_id } => return Ok(event_id),
            AttachmentSendUpdate::Progress { .. }
            | AttachmentSendUpdate::Retained { recoverable: true } => {}
            AttachmentSendUpdate::Retained { recoverable: false }
            | AttachmentSendUpdate::Cancelled
            | AttachmentSendUpdate::Unknown => return Err(()),
        }
    }
}

#[test]
fn encrypted_file_roundtrips_between_disposable_accounts() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped live media test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(credentials_path).expect("Cannot read local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let root = std::env::temp_dir().join(format!("morpheus-media-test-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create test root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let filename = format!("encrypted-{}.bin", Uuid::new_v4());
    let sticker_body = format!("Original sticker {}", Uuid::new_v4());
    let thread_sticker_body = format!("Original thread sticker {}", Uuid::new_v4());
    let file_path = root.join(&filename);
    let preview_path = root.join("original-preview.png");
    fs::write(&preview_path, PREVIEW_PNG).expect("Cannot create original preview fixture");
    let payload = format!("morpheus-private-media-{}", Uuid::new_v4())
        .repeat(512)
        .into_bytes();
    fs::write(&file_path, &payload).expect("Cannot create test attachment");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot create test runtime");
    runtime.block_on(async {
        let alice = fixture.account("a_primary");
        let bob = fixture.account("a_second_device");
        let room_id = fixture
            .rooms
            .get("encrypted_room")
            .expect("Missing encrypted room");
        let sender = AccountManager::open(root.join("sender")).expect("Cannot open sender store");
        let sender_account = sender
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Sender login failed");
        let receiver =
            AccountManager::open(root.join("receiver")).expect("Cannot open receiver store");
        let receiver_account = receiver
            .add_password_account(&bob.homeserver_url, &bob.username, &bob.password)
            .await
            .expect("Receiver login failed");
        wait_for_room(&sender, room_id).await;
        wait_for_room(&receiver, room_id).await;
        let receiver_feed = receiver
            .message_feed(room_id)
            .await
            .expect("Cannot subscribe to receiver timeline");

        let mut queued = sender
            .send_attachment_for(
                &sender_account.id,
                room_id,
                &file_path,
                "application/octet-stream",
            )
            .await
            .expect("Cannot queue encrypted attachment");
        let event_id = timeout(Duration::from_secs(90), async {
            loop {
                match queued.next_update().await {
                    AttachmentSendUpdate::Progress { .. } => {}
                    AttachmentSendUpdate::Sent { event_id } => return event_id,
                    AttachmentSendUpdate::Retained { recoverable: true } => {
                        // The selected-account queue supervisor will resume a
                        // transient failure without enqueueing a duplicate.
                    }
                    AttachmentSendUpdate::Retained { recoverable: false }
                    | AttachmentSendUpdate::Cancelled
                    | AttachmentSendUpdate::Unknown => panic!("Encrypted upload did not finish"),
                }
            }
        })
        .await
        .expect("Timed out waiting for encrypted upload");
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(item) = receiver_feed
                .snapshot()
                .into_iter()
                .find(|item| item.event_id.as_deref() == Some(event_id.as_str()))
            {
                assert_eq!(item.attachment_name.as_deref(), Some(filename.as_str()));
                assert!(!item.body.contains("mxc://"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Timed out awaiting attachment card"
            );
            sleep(Duration::from_millis(300)).await;
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let downloaded = loop {
            if let Ok(downloaded) = receiver
                .download_attachment_for(&receiver_account.id, room_id, &event_id)
                .await
            {
                break downloaded;
            }
            assert!(
                Instant::now() < deadline,
                "Timed out decrypting received attachment"
            );
            sleep(Duration::from_millis(350)).await;
        };
        assert_eq!(downloaded.bytes, payload);
        assert_eq!(downloaded.metadata.filename, filename);
        assert_eq!(downloaded.metadata.mime_type, "application/octet-stream");
        assert_eq!(
            downloaded.metadata.declared_size,
            Some(payload.len() as u64)
        );
        assert_eq!(downloaded.metadata.kind, AttachmentKind::File);
        assert!(downloaded.metadata.encrypted);

        let mismatch = receiver
            .download_attachment_for(&sender_account.id, room_id, &event_id)
            .await;
        assert!(matches!(
            mismatch,
            Err(MatrixCoreError::SelectedAccountChanged)
        ));
        // Share the existing final receive/download deadline. The hosted
        // media_local command and its cleanup budget are not extended.
        timeout_at(deadline, async {
            let plain_room = fixture.rooms.get("a_room").ok_or(())?;
            wait_for_room(&sender, plain_room).await;
            let sender_client = sender.active_client().await.map_err(|_| ())?;
            let receiver_client = receiver.active_client().await.map_err(|_| ())?;
            let plain = sender_client
                .get_room(&RoomId::parse(plain_room).map_err(|_| ())?)
                .ok_or(())?;
            let encrypted = receiver_client
                .get_room(&RoomId::parse(room_id).map_err(|_| ())?)
                .ok_or(())?;
            let plain_state = plain.latest_encryption_state().await.map_err(|_| ())?;
            let encrypted_state = encrypted.latest_encryption_state().await.map_err(|_| ())?;
            // Check live room state, not just names or fixture conventions.
            assert!(!plain_state.is_unknown() && !plain_state.is_encrypted());
            assert!(!encrypted_state.is_unknown() && encrypted_state.is_encrypted());
            let sender_selection = sender.account_selection(&sender_account.id).ok_or(())?;
            let receiver_selection = receiver.account_selection(&receiver_account.id).ok_or(())?;

            let image_event =
                queue_preview_image(&sender, &sender_account.id, room_id, &preview_path).await?;
            assert!(matches!(
                receiver
                    .download_image_preview_for(&receiver_selection, room_id, &image_event, false,)
                    .await,
                Err(MatrixCoreError::InvalidMedia(_))
            ));
            // This is an actual current selection from the other manager,
            // rather than a malformed event or hand-built fake token.
            assert!(matches!(
                receiver
                    .download_image_preview_for(&sender_selection, room_id, &image_event, true,)
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            // Wait only for real decryption/key availability; never send a
            // duplicate or inject crypto material to make the assertion pass.
            let image = loop {
                if let Ok(download) = receiver
                    .download_image_preview_for(&receiver_selection, room_id, &image_event, true)
                    .await
                {
                    break download.into_plaintext().map_err(|_| ())?;
                }
                sleep(Duration::from_millis(300)).await;
            };
            assert_eq!(image.bytes, PREVIEW_PNG);
            assert_eq!(image.metadata.kind, AttachmentKind::Image);
            assert_eq!(image.metadata.mime_type, "image/png");
            assert!(image.metadata.encrypted);
            assert!(matches!(
                receiver
                    .download_image_preview_for(&receiver_selection, room_id, &event_id, true,)
                    .await,
                Err(MatrixCoreError::InvalidMedia(_))
            ));
            let plain_event =
                queue_preview_image(&sender, &sender_account.id, plain_room, &preview_path).await?;
            let plain_image = sender
                .download_image_preview_for(&sender_selection, plain_room, &plain_event, false)
                .await
                .map_err(|_| ())?
                .into_plaintext()
                .map_err(|_| ())?;
            assert_eq!(plain_image.bytes, PREVIEW_PNG);
            assert_eq!(plain_image.metadata.kind, AttachmentKind::Image);
            assert!(!plain_image.metadata.encrypted);

            // Pack media is public even when its m.sticker event is encrypted.
            // This deliberately uploads only our original non-secret 1px PNG.
            // No production compose API or credential/store shortcut is used.
            let sticker_uri = sender_client
                .media()
                .upload(&mime::IMAGE_PNG, PREVIEW_PNG.to_vec(), None)
                .await
                .map_err(|_| ())?
                .content_uri;
            let mut sticker_info = ImageInfo::new();
            sticker_info.mimetype = Some("image/png".into());
            sticker_info.size = Some((PREVIEW_PNG.len() as u32).into());
            let encrypted_sender = sender_client
                .get_room(&RoomId::parse(room_id).map_err(|_| ())?)
                .ok_or(())?;
            let sticker_event = encrypted_sender
                .send(StickerEventContent::new(
                    sticker_body.clone(),
                    sticker_info,
                    sticker_uri.clone(),
                ))
                .await
                .map_err(|_| ())?
                .event_id
                .to_string();
            loop {
                if let Some(item) = receiver_feed
                    .snapshot()
                    .into_iter()
                    .find(|item| item.event_id.as_deref() == Some(sticker_event.as_str()))
                {
                    assert_eq!(item.body, sticker_body);
                    assert_eq!(item.attachment_name.as_deref(), Some("Sticker"));
                    assert!(item.attachment_is_image);
                    assert!(item.rich_markdown.is_none());
                    break;
                }
                sleep(Duration::from_millis(300)).await;
            }
            let received_sticker = encrypted
                .load_or_fetch_event(&EventId::parse(&sticker_event).map_err(|_| ())?, None)
                .await
                .map_err(|_| ())?;
            assert!(received_sticker.encryption_info().is_some());
            assert!(matches!(
                receiver
                    .download_image_preview_for(&receiver_selection, room_id, &sticker_event, false)
                    .await,
                Err(MatrixCoreError::InvalidMedia(_))
            ));
            assert!(matches!(
                receiver
                    .download_image_preview_for(&sender_selection, room_id, &sticker_event, true)
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            let sticker = receiver
                .download_image_preview_for(&receiver_selection, room_id, &sticker_event, true)
                .await
                .map_err(|_| ())?
                .into_plaintext()
                .map_err(|_| ())?;
            assert_eq!(sticker.bytes, PREVIEW_PNG);
            assert_eq!(sticker.metadata.filename, "sticker.png");
            // Media encryption and enclosing-event encryption are independent.
            assert!(!sticker.metadata.encrypted);
            let saved_sticker = receiver
                .download_attachment_for(&receiver_account.id, room_id, &sticker_event)
                .await
                .map_err(|_| ())?;
            assert_eq!(saved_sticker.bytes, PREVIEW_PNG);
            assert_eq!(saved_sticker.metadata.kind, AttachmentKind::Image);

            // A sticker must also survive the real thread-focused timeline,
            // without being duplicated into the ordinary room conversation.
            // Use the existing encrypted image message as the thread root.
            let thread_feed = receiver
                .thread_message_feed_for(&receiver_account.id, room_id, &image_event)
                .await
                .map_err(|_| ())?;
            let root = EventId::parse(&image_event).map_err(|_| ())?;
            let mut thread_info = ImageInfo::new();
            thread_info.mimetype = Some("image/png".into());
            thread_info.size = Some((PREVIEW_PNG.len() as u32).into());
            let mut thread_content = StickerEventContent::new(
                thread_sticker_body.clone(),
                thread_info,
                sticker_uri.clone(),
            );
            thread_content.relates_to = Some(Relation::Thread(Thread::plain(root.clone(), root)));
            let thread_sticker_event = encrypted_sender
                .send(thread_content)
                .await
                .map_err(|_| ())?
                .event_id
                .to_string();
            loop {
                if let Some(item) = thread_feed
                    .snapshot()
                    .into_iter()
                    .find(|item| item.event_id.as_deref() == Some(thread_sticker_event.as_str()))
                {
                    assert_eq!(item.body, thread_sticker_body);
                    assert_eq!(item.attachment_name.as_deref(), Some("Sticker"));
                    assert!(item.attachment_is_image);
                    break;
                }
                sleep(Duration::from_millis(300)).await;
            }
            let thread_event = encrypted
                .load_or_fetch_event(
                    &EventId::parse(&thread_sticker_event).map_err(|_| ())?,
                    None,
                )
                .await
                .map_err(|_| ())?;
            assert!(thread_event.encryption_info().is_some());
            assert!(matches!(
                receiver
                    .download_image_preview_for(
                        &receiver_selection,
                        room_id,
                        &thread_sticker_event,
                        false,
                    )
                    .await,
                Err(MatrixCoreError::InvalidMedia(_))
            ));
            let thread_sticker = receiver
                .download_image_preview_for(
                    &receiver_selection,
                    room_id,
                    &thread_sticker_event,
                    true,
                )
                .await
                .map_err(|_| ())?
                .into_plaintext()
                .map_err(|_| ())?;
            assert_eq!(thread_sticker.bytes, PREVIEW_PNG);
            assert_eq!(thread_sticker.metadata.filename, "sticker.png");

            // An ordinary later event is the ordered live-timeline barrier;
            // testing absence immediately after thread delivery could race
            // the independent room projection task and give a false pass.
            let barrier = format!("Thread sticker barrier {}", Uuid::new_v4());
            sender
                .send_text_for(&sender_account.id, room_id, barrier.clone())
                .await
                .map_err(|_| ())?;
            loop {
                if receiver_feed
                    .snapshot()
                    .iter()
                    .any(|item| item.body == barrier)
                {
                    break;
                }
                sleep(Duration::from_millis(300)).await;
            }
            assert!(
                receiver_feed
                    .snapshot()
                    .iter()
                    .all(|item| item.event_id.as_deref() != Some(thread_sticker_event.as_str()))
            );
            let ambiguous_event = encrypted_sender
                .send_raw(
                    "m.sticker",
                    serde_json::json!({
                        "body": "Unreadable sticker source fixture",
                        "info": { "mimetype": "image/png" },
                        "url": sticker_uri,
                        "file": null
                    }),
                )
                .await
                .map_err(|_| ())?
                .event_id
                .to_string();
            assert!(matches!(
                receiver
                    .download_image_preview_for(
                        &receiver_selection,
                        room_id,
                        &ambiguous_event,
                        true,
                    )
                    .await,
                Err(MatrixCoreError::InvalidMedia(_))
            ));
            Ok::<(), ()>(())
        })
        .await
        .expect("Image preview checks exceeded the existing media deadline")
        .expect("Image preview API acceptance failed");
        sender.stop().await;
        receiver.stop().await;

        // The input file is intentionally plaintext; remove it before
        // checking the encrypted SDK stores. Then log out the disposable
        // devices so their OS-vault entries do not accumulate across runs.
        fs::remove_file(&file_path).expect("Cannot remove test input");
        fs::remove_file(&preview_path).expect("Cannot remove preview fixture input");
        assert!(!contains_plaintext(&root.join("sender"), &payload[..80]));
        assert!(!contains_plaintext(&root.join("receiver"), &payload[..80]));
        assert!(!contains_plaintext(
            &root.join("sender"),
            sticker_body.as_bytes()
        ));
        assert!(!contains_plaintext(
            &root.join("receiver"),
            sticker_body.as_bytes()
        ));
        assert!(!contains_plaintext(
            &root.join("sender"),
            thread_sticker_body.as_bytes()
        ));
        assert!(!contains_plaintext(
            &root.join("receiver"),
            thread_sticker_body.as_bytes()
        ));
        sender
            .logout_account(&sender_account.id)
            .await
            .expect("Cannot clean sender device");
        receiver
            .logout_account(&receiver_account.id)
            .await
            .expect("Cannot clean receiver device");
    });
    fs::remove_dir_all(&root).expect("Cannot clean test root");
}
