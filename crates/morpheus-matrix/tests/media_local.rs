//! Opt-in encrypted attachment roundtrip against two disposable local Matrix
//! accounts. Credentials are read from a protected fixture and never logged.
#![recursion_limit = "512"]

#[path = "support/opened_room.rs"]
mod opened_room;

use std::{collections::HashMap, fs, mem::discriminant, path::Path, time::Duration};

use matrix_sdk::ruma::{
    EventId, RoomId,
    events::{
        AnyGlobalAccountDataEventContent, GlobalAccountDataEventType,
        relation::Thread,
        room::{
            ImageInfo,
            message::{Relation, RoomMessageEventContent},
        },
        sticker::StickerEventContent,
    },
    serde::Raw,
};
use morpheus_matrix::{
    AccountManager, AccountSelection, AttachmentKind, AttachmentSendUpdate, MatrixCoreError,
    MessageFeed, MessageInfo, PublicMediaConsent, QueuedSendOutcome, StickerCatalog,
    StickerPackSource,
};
use serde::Deserialize;
use serde_json::{Value, json};
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
                .response
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
                .response
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
                .response
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

const PACK_EVENT: &str = "m.room.image_pack";
const ENABLED_PACKS: &str = "m.image_pack.rooms";
// Fixed state keys: each run replaces the previous run's pack, so packs never
// accumulate in the shared fixture rooms.
const PACK_KEY: &str = "fixture-owls";
const ENABLED_KEY: &str = "fixture-enabled-owls";
// A well-formed room this account never joins. It is only counted as
// unavailable; nothing may join it or fetch anything from it.
const UNJOINED_ROOM: &str = "!fixture-unjoined:invalid";

/// A failed step names itself with fixed text; nothing secret is reported.
type Step<T = ()> = Result<T, &'static str>;

fn ensure(condition: bool, failure: &'static str) -> Step {
    condition.then_some(()).ok_or(failure)
}

/// The call failed with exactly this error variant.
fn expect_error<T>(
    result: Result<T, MatrixCoreError>,
    expected: MatrixCoreError,
    failure: &'static str,
) -> Step {
    match result {
        Err(error) if discriminant(&error) == discriminant(&expected) => Ok(()),
        _ => Err(failure),
    }
}

fn sent(outcome: Result<QueuedSendOutcome, MatrixCoreError>, failure: &'static str) -> Step {
    match outcome {
        Ok(QueuedSendOutcome::Sent) => Ok(()),
        _ => Err(failure),
    }
}

fn original(bytes: Result<Vec<u8>, MatrixCoreError>, failure: &'static str) -> Step {
    match bytes {
        Ok(bytes) if bytes == PREVIEW_PNG => Ok(()),
        _ => Err(failure),
    }
}

/// The production send path, observed to its outcome. The caller's deadline
/// bounds the wait; nothing is ever re-enqueued.
async fn send(
    manager: &AccountManager,
    catalog: &StickerCatalog,
    index: usize,
    consent: Option<&PublicMediaConsent>,
) -> Result<QueuedSendOutcome, MatrixCoreError> {
    let queued = manager.send_sticker_for(catalog, index, consent).await?;
    Ok(queued.wait_outcome().await)
}

async fn preview(
    manager: &AccountManager,
    catalog: &StickerCatalog,
    index: usize,
    consent: Option<&PublicMediaConsent>,
) -> Result<Vec<u8>, MatrixCoreError> {
    let preview = manager
        .download_sticker_preview_for(catalog, index, consent)
        .await?;
    Ok(preview.into_bytes())
}

fn sdk_room(client: &matrix_sdk::Client, room_id: &str) -> Step<matrix_sdk::Room> {
    let room_id = RoomId::parse(room_id).map_err(|_| "Invalid room")?;
    client.get_room(&room_id).ok_or("Room missing")
}

async fn publish_pack(room: &matrix_sdk::Room, key: &str, content: Value) -> Step {
    room.send_state_event_raw(PACK_EVENT, key, content)
        .await
        .map_err(|_| "Cannot publish fixture pack")?;
    Ok(())
}

async fn set_enabled_packs(client: &matrix_sdk::Client, list: &Value) -> Step {
    let content: Raw<AnyGlobalAccountDataEventContent> =
        Raw::from_json_string(list.to_string()).map_err(|_| "Invalid pack list")?;
    let event_type = GlobalAccountDataEventType::from(ENABLED_PACKS);
    client
        .account()
        .set_account_data_raw(event_type, content)
        .await
        .map_err(|_| "Cannot update enabled sticker packs")?;
    Ok(())
}

fn owl_pack(uri: &str, body: &str) -> Value {
    json!({
        "pack": {
            "display_name": "Fixture owls",
            "usage": ["sticker"],
            "attribution": "Original fixture art"
        },
        "images": {
            "sleepy_owl": {
                "url": uri,
                "body": body,
                "info": { "mimetype": "image/png", "size": PREVIEW_PNG.len(), "w": 1, "h": 1 }
            },
            // Only standard fields are forwarded: the thumbnail and extension
            // field must not reach the sent sticker.
            "owl_wave": {
                "url": uri,
                "info": {
                    "mimetype": "image/png",
                    "thumbnail_url": uri,
                    "fixture.extension": "dropped"
                }
            },
            // Not a Matrix media address: skipped and never fetched.
            "remote_owl": { "url": "https://example.invalid/owl.png" }
        }
    })
}

fn label_index(catalog: &StickerCatalog, label: &str) -> Option<usize> {
    catalog
        .entries()
        .iter()
        .position(|entry| entry.label == label)
}

fn has(catalog: &StickerCatalog, label: &str) -> bool {
    label_index(catalog, label).is_some()
}

/// Another sticker from the pack of the entry labelled `label`.
fn sibling(catalog: &StickerCatalog, label: &str, shortcode: &str) -> Option<usize> {
    let index = label_index(catalog, label)?;
    let pack = catalog.entries()[index].pack;
    catalog
        .entries()
        .iter()
        .position(|entry| entry.pack == pack && entry.shortcode == shortcode)
}

fn source_of(catalog: &StickerCatalog, index: usize) -> StickerPackSource {
    catalog.packs()[catalog.entries()[index].pack].source
}

/// Pack changes arrive through ordinary sync. Poll the real catalog API until
/// the label is present (or gone); the caller's deadline bounds the wait.
async fn catalog_with(
    manager: &AccountManager,
    selection: &AccountSelection,
    room_id: &str,
    thread_root: Option<&str>,
    label: &str,
    present: bool,
) -> StickerCatalog {
    loop {
        let catalog = manager
            .sticker_catalog_for(selection, room_id, thread_root)
            .await;
        match catalog {
            Ok(catalog) if has(&catalog, label) == present => return catalog,
            _ => sleep(Duration::from_millis(300)).await,
        }
    }
}

/// The first row with this body, once the feed shows one.
async fn feed_row(feed: &MessageFeed, body: &str) -> MessageInfo {
    loop {
        let rows = feed.snapshot();
        if let Some(row) = rows.into_iter().find(|row| row.body == body) {
            return row;
        }
        sleep(Duration::from_millis(300)).await;
    }
}

/// Whether the received event was encrypted, and its decrypted content.
async fn received_content(room: &matrix_sdk::Room, event_id: &str) -> Step<(bool, Value)> {
    let event_id = EventId::parse(event_id).map_err(|_| "Invalid event")?;
    let event = room
        .load_or_fetch_event(&event_id, None)
        .await
        .map_err(|_| "Cannot load received sticker")?;
    let content = event.raw().get_field::<Value>("content");
    let content = content.ok().flatten().ok_or("Unreadable content")?;
    Ok((event.encryption_info().is_some(), content))
}

/// Room and thread sends with consent, stale and foreign choices, a changed
/// pack, an enabled and then disabled pack, the recovery gate and departure.
/// Returns a valid choice so the caller can check it after the selection ends.
async fn sticker_scenario(
    sender: &AccountManager,
    receiver: &AccountManager,
    selection: &AccountSelection,
    receiver_selection: &AccountSelection,
    room_id: &str,
    plain_room: &str,
    marker: &str,
) -> Step<(StickerCatalog, usize, PublicMediaConsent)> {
    let client = sender
        .active_client()
        .await
        .map_err(|_| "Sender client missing")?;
    let receiver_client = receiver
        .active_client()
        .await
        .map_err(|_| "Receiver client missing")?;
    let receiver_room = sdk_room(&receiver_client, room_id)?;
    let receiver_feed = receiver
        .message_feed(room_id)
        .await
        .map_err(|_| "Receiver feed failed")?;
    // Pack media is public even for an encrypted conversation. Upload only
    // the original non-secret 1px PNG; nothing is fetched from elsewhere.
    let uri = client
        .media()
        .upload(&mime::IMAGE_PNG, PREVIEW_PNG.to_vec(), None)
        .await
        .map_err(|_| "Cannot upload original sticker image")?
        .content_uri
        .to_string();
    let room = sdk_room(&client, room_id)?;
    let room_body = format!("Sleeping owl {marker}");
    publish_pack(&room, PACK_KEY, owl_pack(&uri, &room_body)).await?;
    let catalog = catalog_with(sender, selection, room_id, None, &room_body, true).await;
    let owl = label_index(&catalog, &room_body).ok_or("No room sticker")?;
    let wave = sibling(&catalog, &room_body, "owl_wave").ok_or("No second sticker")?;
    let pack = &catalog.packs()[catalog.entries()[owl].pack];
    let described = pack.name == "Fixture owls"
        && pack.attribution.as_deref() == Some("Original fixture art")
        && pack.source == StickerPackSource::Room
        && pack.stickers == 2
        && catalog.entries()[wave].label == "owl_wave";
    ensure(described, "Room pack was not described")?;
    let skipped = catalog.skipped() >= 1;
    ensure(skipped, "Unsafe pack image was kept")?;
    let (entries, packs) = (catalog.entries(), catalog.packs());
    let public = format!("{catalog:?} {entries:?} {packs:?}");
    let hidden = !public.contains("mxc://");
    ensure(hidden, "Media address was exposed")?;

    // An encrypted conversation needs explicit consent for this exact
    // catalog before public pack media is loaded or sent.
    let asks = catalog.requires_public_media_consent();
    ensure(asks, "Encrypted room did not ask for consent")?;
    expect_error(
        send(sender, &catalog, owl, None).await,
        MatrixCoreError::StickerConsentRequired,
        "Sticker sent without consent",
    )?;
    expect_error(
        preview(sender, &catalog, owl, None).await,
        MatrixCoreError::StickerConsentRequired,
        "Sticker loaded without consent",
    )?;
    let other = sender
        .sticker_catalog_for(selection, room_id, None)
        .await
        .map_err(|_| "Second catalog failed")?;
    let distinct = other.id() != catalog.id();
    ensure(distinct, "Catalog identity was reused")?;
    let other_consent = other.public_media_consent();
    expect_error(
        send(sender, &catalog, owl, Some(&other_consent)).await,
        MatrixCoreError::StickerConsentRequired,
        "Another catalog's consent was accepted",
    )?;

    // A selection from another manager is never accepted here, and neither
    // is a catalog built under it.
    expect_error(
        sender
            .sticker_catalog_for(receiver_selection, room_id, None)
            .await,
        MatrixCoreError::SelectedAccountChanged,
        "A foreign selection built a catalog",
    )?;
    let foreign = receiver
        .sticker_catalog_for(receiver_selection, room_id, None)
        .await
        .map_err(|_| "Receiver catalog failed")?;
    let bound = foreign.is_for(receiver_selection, room_id, None)
        && !foreign.is_for(selection, room_id, None);
    ensure(bound, "Catalog was not bound to its selection")?;
    let foreign_consent = foreign.public_media_consent();
    expect_error(
        send(sender, &foreign, 0, Some(&foreign_consent)).await,
        MatrixCoreError::SelectedAccountChanged,
        "A foreign catalog sent a sticker",
    )?;
    expect_error(
        preview(sender, &foreign, 0, Some(&foreign_consent)).await,
        MatrixCoreError::SelectedAccountChanged,
        "A foreign catalog loaded a sticker",
    )?;

    let room_consent = catalog.public_media_consent();
    original(
        preview(sender, &catalog, owl, Some(&room_consent)).await,
        "Sticker preview bytes differ",
    )?;
    // The exact lookup that reconciles a lost outcome finds the request by its
    // transaction while it is queued, never another one, and nothing once the
    // request is sent.
    let queued = sender
        .send_sticker_for(&catalog, owl, Some(&room_consent))
        .await
        .map_err(|_| "Room sticker was not queued")?;
    let transaction = queued.transaction_id().to_owned();
    let early = sender
        .queued_sticker_for(selection, room_id, &transaction)
        .await;
    let exact = match early {
        Ok(Some(found)) => found.queued.transaction_id() == transaction,
        Ok(None) => true,
        Err(_) => false,
    };
    ensure(exact, "The exact lookup found another request")?;
    sent(Ok(queued.wait_outcome().await), "Room sticker was not sent")?;
    let late = sender
        .queued_sticker_for(selection, room_id, &transaction)
        .await;
    let gone = matches!(late, Ok(None));
    ensure(gone, "A sent sticker was still found by its transaction")?;
    let row = feed_row(&receiver_feed, &room_body).await;
    let card = row.attachment_name.as_deref() == Some("Sticker") && row.attachment_is_image;
    ensure(card, "Room sticker was not shown as one")?;
    let event_id = row.event_id.ok_or("Room sticker has no event")?;
    let (encrypted, content) = received_content(&receiver_room, &event_id).await?;
    ensure(encrypted, "Room sticker was not encrypted")?;
    let forwarded = content["body"] == room_body.as_str()
        && content["url"] == uri.as_str()
        && content["info"]["mimetype"] == "image/png"
        && content["info"]["size"] == PREVIEW_PNG.len()
        && content.get("m.relates_to").is_none();
    ensure(forwarded, "Room sticker content differs")?;
    let received = receiver
        .download_image_preview_for(receiver_selection, room_id, &event_id, true)
        .await
        .map_err(|_| "Received sticker preview failed")?
        .into_plaintext()
        .map_err(|_| "Received sticker preview failed")?;
    let same = received.bytes == PREVIEW_PNG;
    ensure(same, "Received sticker bytes differ")?;
    // Once sent, the persistent queue holds nothing more for this room; the
    // read-only lookup used after restarts and uncertain sends agrees.
    loop {
        let lookup = sender.pending_sticker_for(selection, room_id, None);
        if matches!(lookup.await, Ok(None)) {
            break;
        }
        sleep(Duration::from_millis(300)).await;
    }

    // A thread target goes through the thread timeline, which relates it.
    let root_text = RoomMessageEventContent::text_plain(format!("Sticker thread {marker}"));
    let root = room
        .send(root_text)
        .await
        .map_err(|_| "Cannot send thread root")?
        .response
        .event_id
        .to_string();
    let thread_feed = receiver
        .thread_message_feed_for(receiver_selection.account_id(), room_id, &root)
        .await
        .map_err(|_| "Receiver thread feed failed")?;
    let thread = Some(root.as_str());
    let threaded = catalog_with(sender, selection, room_id, thread, &room_body, true).await;
    let in_thread = threaded.thread_root() == thread;
    ensure(in_thread, "Catalog lost its thread")?;
    let reply = sibling(&threaded, &room_body, "owl_wave").ok_or("No thread sticker")?;
    expect_error(
        send(sender, &threaded, reply, Some(&room_consent)).await,
        MatrixCoreError::StickerConsentRequired,
        "Room consent was accepted for a thread",
    )?;
    let thread_consent = threaded.public_media_consent();
    sent(
        send(sender, &threaded, reply, Some(&thread_consent)).await,
        "Thread sticker was not sent",
    )?;
    let row = feed_row(&thread_feed, "owl_wave").await;
    let thread_event = row.event_id.ok_or("Thread sticker has no event")?;
    let (encrypted, content) = received_content(&receiver_room, &thread_event).await?;
    ensure(encrypted, "Thread sticker was not encrypted")?;
    let related = content["m.relates_to"]["rel_type"] == "m.thread"
        && content["m.relates_to"]["event_id"] == root.as_str()
        && content["body"] == "owl_wave"
        && content["info"]["mimetype"] == "image/png"
        && content["info"].get("thumbnail_url").is_none()
        && content["info"].get("fixture.extension").is_none();
    ensure(related, "Thread sticker content differs")?;
    loop {
        let lookup = sender.pending_sticker_for(selection, room_id, thread);
        if matches!(lookup.await, Ok(None)) {
            break;
        }
        sleep(Duration::from_millis(300)).await;
    }
    // A later ordinary message is the ordered barrier for the room feed;
    // checking absence straight after thread delivery could pass falsely.
    let barrier = format!("Sticker barrier {marker}");
    sender
        .send_text_for(selection.account_id(), room_id, barrier.clone())
        .await
        .map_err(|_| "Barrier send failed")?;
    feed_row(&receiver_feed, &barrier).await;
    let rows = receiver_feed.snapshot();
    let kept_out = rows
        .iter()
        .all(|row| row.event_id.as_deref() != Some(thread_event.as_str()));
    ensure(kept_out, "Thread sticker reached the room feed")?;

    // Changing the pack revokes earlier choices and their consent.
    let changed_body = format!("Owl after change {marker}");
    publish_pack(&room, PACK_KEY, owl_pack(&uri, &changed_body)).await?;
    let changed = catalog_with(sender, selection, room_id, None, &changed_body, true).await;
    let gone = !has(&changed, &room_body);
    ensure(gone, "Changed pack kept the old sticker")?;
    let refreshed = sender.refresh_sticker_catalog(&catalog).await;
    let reported = matches!(refreshed, Ok(Some(_)));
    ensure(reported, "Pack change was not reported")?;
    expect_error(
        send(sender, &catalog, owl, Some(&room_consent)).await,
        MatrixCoreError::StickerUnavailable,
        "A changed sticker was still sent",
    )?;
    expect_error(
        preview(sender, &catalog, owl, Some(&room_consent)).await,
        MatrixCoreError::StickerUnavailable,
        "A changed sticker still loaded",
    )?;
    expect_error(
        send(sender, &threaded, reply, Some(&thread_consent)).await,
        MatrixCoreError::StickerUnavailable,
        "A changed thread sticker was still sent",
    )?;
    let changed_owl = label_index(&changed, &changed_body).ok_or("No changed sticker")?;
    expect_error(
        preview(sender, &changed, changed_owl, Some(&room_consent)).await,
        MatrixCoreError::StickerConsentRequired,
        "Consent survived a pack change",
    )?;
    let changed_consent = changed.public_media_consent();
    original(
        preview(sender, &changed, changed_owl, Some(&changed_consent)).await,
        "Changed sticker preview differs",
    )?;

    // A pack enabled from another joined room serves every conversation, and
    // disabling it revokes earlier choices. An unjoined room is only counted.
    let plain = sdk_room(&client, plain_room)?;
    let enabled_body = format!("Enabled owl {marker}");
    let enabled_pack = json!({
        "pack": { "display_name": "Fixture enabled owls", "usage": [] },
        "images": { "enabled_owl": { "url": uri, "body": enabled_body } }
    });
    publish_pack(&plain, ENABLED_KEY, enabled_pack).await?;
    let mut list = json!({ "rooms": {} });
    list["rooms"][plain_room][ENABLED_KEY] = json!({});
    list["rooms"][UNJOINED_ROOM][PACK_KEY] = json!({});
    set_enabled_packs(&client, &list).await?;
    let enabled = catalog_with(sender, selection, room_id, None, &enabled_body, true).await;
    let enabled_owl = label_index(&enabled, &enabled_body).ok_or("No enabled sticker")?;
    let listed = source_of(&enabled, enabled_owl) == StickerPackSource::Enabled;
    let unjoined = enabled.unavailable_packs() >= 1;
    ensure(listed && unjoined, "Enabled packs were misdescribed")?;
    let enabled_consent = enabled.public_media_consent();
    expect_error(
        preview(sender, &enabled, enabled_owl, None).await,
        MatrixCoreError::StickerConsentRequired,
        "Enabled sticker loaded without consent",
    )?;
    original(
        preview(sender, &enabled, enabled_owl, Some(&enabled_consent)).await,
        "Enabled sticker preview differs",
    )?;
    // An unencrypted destination needs no consent; its own pack is a room
    // pack there, even while the list also names it.
    let open = catalog_with(sender, selection, plain_room, None, &enabled_body, true).await;
    let open_owl = label_index(&open, &enabled_body).ok_or("No plain room sticker")?;
    let local = source_of(&open, open_owl) == StickerPackSource::Room;
    let free = !open.requires_public_media_consent();
    ensure(local && free, "Plain room pack was misdescribed")?;
    original(
        preview(sender, &open, open_owl, None).await,
        "Plain room sticker preview differs",
    )?;
    let disabled = json!({ "rooms": {} });
    set_enabled_packs(&client, &disabled).await?;
    catalog_with(sender, selection, room_id, None, &enabled_body, false).await;
    expect_error(
        preview(sender, &enabled, enabled_owl, Some(&enabled_consent)).await,
        MatrixCoreError::StickerUnavailable,
        "A disabled pack still loaded",
    )?;
    expect_error(
        send(sender, &enabled, enabled_owl, Some(&enabled_consent)).await,
        MatrixCoreError::StickerUnavailable,
        "A disabled pack still sent",
    )?;
    original(
        preview(sender, &open, open_owl, None).await,
        "Room pack was revoked with the list",
    )?;

    // A room created after discovery stays behind the recovery gate until it
    // is opened: the sticker is refused there, never sent around the gate.
    let gate_room_id = sender
        .create_private_room(&format!("Sticker gate {marker}"))
        .await
        .map_err(|_| "Cannot create gate room")?;
    let gate = loop {
        if let Ok(gate) = sdk_room(&client, &gate_room_id) {
            break gate;
        }
        sleep(Duration::from_millis(300)).await;
    };
    let gate_body = format!("Gate owl {marker}");
    let gate_pack = json!({ "images": { "gate_owl": { "url": uri, "body": gate_body } } });
    publish_pack(&gate, PACK_KEY, gate_pack).await?;
    let gated = catalog_with(sender, selection, &gate_room_id, None, &gate_body, true).await;
    let gate_owl = label_index(&gated, &gate_body).ok_or("No gate sticker")?;
    let gate_consent = gated.public_media_consent();
    let uncovered = sender
        .room_recovery_incomplete(selection, &gate_room_id)
        .await;
    let uncovered = matches!(uncovered, Ok(true));
    ensure(uncovered, "New room was already covered")?;
    expect_error(
        send(sender, &gated, gate_owl, Some(&gate_consent)).await,
        MatrixCoreError::RecoveryIncomplete,
        "A sticker bypassed the recovery gate",
    )?;
    let paused = !gate.send_queue().is_enabled();
    ensure(paused, "Gate room queue was enabled")?;
    opened_room::scan(sender, &gate_room_id).await?;
    sent(
        send(sender, &gated, gate_owl, Some(&gate_consent)).await,
        "Opened room sticker was not sent",
    )?;
    // Leaving the room revokes its choices.
    sender
        .leave_room(&gate_room_id)
        .await
        .map_err(|_| "Cannot leave gate room")?;
    while gate.state() != matrix_sdk::RoomState::Left {
        sleep(Duration::from_millis(300)).await;
    }
    expect_error(
        send(sender, &gated, gate_owl, Some(&gate_consent)).await,
        MatrixCoreError::StickerUnavailable,
        "A sticker was sent after leaving",
    )?;
    expect_error(
        preview(sender, &gated, gate_owl, Some(&gate_consent)).await,
        MatrixCoreError::StickerUnavailable,
        "A sticker loaded after leaving",
    )?;
    Ok((changed, changed_owl, changed_consent))
}

/// Clear the fixed packs and the enabled list. Runs even after a failed step.
async fn sticker_cleanup(sender: &AccountManager, room_id: &str, plain_room: &str) -> Step {
    let client = sender
        .active_client()
        .await
        .map_err(|_| "Sender client missing")?;
    for (room_id, key) in [(room_id, PACK_KEY), (plain_room, ENABLED_KEY)] {
        publish_pack(&sdk_room(&client, room_id)?, key, json!({})).await?;
    }
    let cleared = json!({ "rooms": {} });
    set_enabled_packs(&client, &cleared).await
}

/// After the selection ends, a kept catalog and consent must not be used.
async fn stale_choice_refused(
    sender: &AccountManager,
    selection: &AccountSelection,
    kept: &(StickerCatalog, usize, PublicMediaConsent),
) -> Step {
    let (catalog, index, consent) = (&kept.0, kept.1, Some(&kept.2));
    let ended = !catalog.is_for(selection, catalog.room_id(), None);
    ensure(ended, "A catalog outlived its selection")?;
    expect_error(
        send(sender, catalog, index, consent).await,
        MatrixCoreError::SelectedAccountChanged,
        "A stale selection sent a sticker",
    )?;
    expect_error(
        preview(sender, catalog, index, consent).await,
        MatrixCoreError::SelectedAccountChanged,
        "A stale selection loaded a sticker",
    )
}

#[test]
fn sticker_packs_send_between_disposable_accounts() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped live sticker test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(credentials_path).expect("Cannot read local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let root = std::env::temp_dir().join(format!("morpheus-sticker-test-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create test root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }
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
        let plain_room = fixture
            .rooms
            .get("a_room")
            .expect("Missing unencrypted fixture room");
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
        wait_for_room(&sender, plain_room).await;
        wait_for_room(&receiver, room_id).await;
        let selection = sender
            .account_selection(&sender_account.id)
            .expect("Sender selection missing");
        let receiver_selection = receiver
            .account_selection(&receiver_account.id)
            .expect("Receiver selection missing");
        let marker = Uuid::new_v4().simple().to_string();
        let outcome = timeout(
            Duration::from_secs(180),
            sticker_scenario(
                &sender,
                &receiver,
                &selection,
                &receiver_selection,
                room_id,
                plain_room,
                &marker,
            ),
        )
        .await
        .unwrap_or(Err("Sticker scenario exceeded its deadline"));
        // Restore the shared fixture rooms and account data even after a
        // failed step, while the selection is still current.
        let cleaned = timeout(
            Duration::from_secs(30),
            sticker_cleanup(&sender, room_id, plain_room),
        )
        .await
        .unwrap_or(Err("Sticker cleanup exceeded its deadline"));
        sender.stop().await;
        receiver.stop().await;
        // Stopping ends the selection: a kept catalog and its consent must
        // not load or send anything afterwards.
        let stale = match &outcome {
            Ok(kept) => stale_choice_refused(&sender, &selection, kept).await,
            Err(_) => Ok(()),
        };
        sender
            .logout_account(&sender_account.id)
            .await
            .expect("Cannot clean sender device");
        receiver
            .logout_account(&receiver_account.id)
            .await
            .expect("Cannot clean receiver device");
        outcome.expect("Sticker scenario failed");
        cleaned.expect("Sticker cleanup failed");
        stale.expect("Stale sticker choice was accepted");
    });
    fs::remove_dir_all(&root).expect("Cannot clean test root");
}
