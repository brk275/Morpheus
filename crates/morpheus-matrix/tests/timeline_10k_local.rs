//! Opt-in seeding of the 10,000-event encrypted room for the real-room
//! timeline acceptance (tests/TIMELINE_10K.md). Skipped unless both
//! MORPHEUS_MATRIX_TEST_ACCOUNTS and MORPHEUS_TIMELINE_10K_SEED are set.
//!
//! Only the loopback fixture from scripts/local_matrix_env.py is accepted.
//! Alice creates a new private encrypted room through the pinned SDK, invites
//! Bob, and Bob joins from a disposable seeder device. Before any message is
//! sent, Alice must see at least MORPHEUS_TIMELINE_10K_BOB_DEVICES of Bob's
//! devices, so every acceptance profile enrolled beforehand receives the room
//! keys for the full history. Alice then sends exactly 10,000 variable-height
//! messages, paced under the fixture's unchanged limit of 10 messages per
//! second. The room ID and event count go to a new owner-only file outside the
//! checkout. Credentials, IDs and message text are never printed; failures
//! report a fixed stage only.

#![recursion_limit = "512"]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use matrix_sdk::{
    Client, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedRoomId, OwnedUserId, UserId,
        api::client::room::create_room,
        events::{
            InitialStateEvent,
            room::{encryption::RoomEncryptionEventContent, message::RoomMessageEventContent},
        },
    },
};
use serde::Deserialize;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

/// Every body carries this prefix and a five-digit index; the app's
/// acceptance driver counts them in its message model.
const MARKER: &str = "tl10k-";
const EVENTS: usize = 10_000;
/// Stay below the fixture's 10 messages per second without weakening it.
const SEND_INTERVAL: Duration = Duration::from_millis(110);
const SEED_LIMIT: Duration = Duration::from_secs(45 * 60);
const WAIT_LIMIT: Duration = Duration::from_secs(120);
const FILLER: &str = "Timeline acceptance filler text that wraps across the row width. ";

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

/// Panic with a fixed stage; never an SDK error, ID or credential.
fn ok<T, E>(result: Result<T, E>, stage: &str) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("MORPHEUS_TIMELINE_SEED_FAILURE {stage}"),
    }
}

fn require(condition: bool, stage: &str) {
    if !condition {
        panic!("MORPHEUS_TIMELINE_SEED_FAILURE {stage}");
    }
}

fn loopback(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    let host = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    };
    host && url.scheme() == "http"
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
}

fn account<'a>(fixture: &'a Fixture, label: &str) -> &'a TestAccount {
    let account = fixture
        .accounts
        .iter()
        .find(|account| account.label == label);
    let account = ok(account.ok_or(()), "fixture_account");
    require(loopback(&account.homeserver_url), "homeserver_not_loopback");
    require(
        account
            .user_id
            .split_once(':')
            .is_some_and(|(_, server)| server.ends_with(".test")),
        "fixture_user_not_synthetic",
    );
    account
}

/// The disposable seeder stores, removed when the run ends either way.
struct Stores(PathBuf);

impl Drop for Stores {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn checkout() -> PathBuf {
    ok(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize(),
        "checkout",
    )
}

/// A new owner-only output file outside the checkout.
fn output_path(raw: PathBuf) -> PathBuf {
    require(raw.is_absolute() && !raw.exists(), "output_path");
    let parent = ok(
        raw.parent()
            .ok_or(())
            .and_then(|parent| parent.canonicalize().map_err(|_| ())),
        "output_path",
    );
    require(!parent.starts_with(checkout()), "output_in_checkout");
    parent.join(ok(raw.file_name().ok_or(()), "output_path"))
}

/// Deterministic fixture text of varying height: no personal data and no
/// links, so nothing is fetched.
fn message(index: usize) -> RoomMessageEventContent {
    let marker = format!("{MARKER}{index:05}");
    match index % 7 {
        0 => RoomMessageEventContent::text_plain(format!("{marker} short update.")),
        1 => RoomMessageEventContent::text_plain(format!("{marker} {}", FILLER.repeat(2))),
        2 => RoomMessageEventContent::text_plain(format!("{marker} {}", FILLER.repeat(9))),
        3 => RoomMessageEventContent::text_plain(format!(
            "{marker}\nfirst line\nsecond line\nthird line\nfourth line"
        )),
        4 => RoomMessageEventContent::text_markdown(format!(
            "{marker} checklist:\n\n- **first** item\n- second item with `code`\n- third item"
        )),
        5 => RoomMessageEventContent::text_markdown(format!(
            "{marker} snippet:\n\n```\nlet total = 10_000;\nlet rows = total / 50;\n```"
        )),
        _ => RoomMessageEventContent::text_markdown(format!(
            "> {marker} quoted line\n\nA follow-up with *emphasis* and **weight**."
        )),
    }
}

async fn signed_in(root: &Path, account: &TestAccount, name: &str) -> Client {
    let passphrase = Uuid::new_v4().simple().to_string();
    let client = ok(
        Client::builder()
            .homeserver_url(&account.homeserver_url)
            .sqlite_store(root.join(name), Some(passphrase.as_str()))
            .build()
            .await,
        "client",
    );
    ok(
        client
            .matrix_auth()
            .login_username(&account.username, &account.password)
            .initial_device_display_name("Morpheus timeline seeder")
            .send()
            .await,
        "login",
    );
    ok(client.user_id().ok_or(()), "login");
    client
}

async fn sync(client: &Client) {
    ok(
        client
            .sync_once(SyncSettings::default().timeout(Duration::from_secs(2)))
            .await,
        "sync",
    );
}

#[tokio::test]
async fn seed_ten_thousand_event_encrypted_room() {
    let (Some(fixture_path), Some(output)) = (
        std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS"),
        std::env::var_os("MORPHEUS_TIMELINE_10K_SEED"),
    ) else {
        eprintln!(
            "Skipped timeline seeding; set MORPHEUS_MATRIX_TEST_ACCOUNTS and MORPHEUS_TIMELINE_10K_SEED."
        );
        return;
    };
    let expected_devices: usize = ok(
        std::env::var("MORPHEUS_TIMELINE_10K_BOB_DEVICES")
            .map_err(|_| ())
            .and_then(|value| value.parse().map_err(|_| ())),
        "bob_devices",
    );
    require((2..=8).contains(&expected_devices), "bob_devices");
    let output = output_path(PathBuf::from(output));
    let fixture: Fixture = ok(
        serde_json::from_slice(&ok(fs::read(fixture_path), "fixture")),
        "fixture",
    );
    let alice = account(&fixture, "a_primary");
    let bob = account(&fixture, "a_second_device");
    let bob_id: OwnedUserId = ok(UserId::parse(bob.user_id.as_str()), "fixture_user");

    let stores = Stores(std::env::temp_dir().join(format!("morpheus-tl10k-{}", Uuid::new_v4())));
    ok(fs::create_dir_all(&stores.0), "stores");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ok(
            fs::set_permissions(&stores.0, fs::Permissions::from_mode(0o700)),
            "stores",
        );
    }

    let seeded = timeout(SEED_LIMIT, async {
        let alice_client = signed_in(&stores.0, alice, "alice").await;
        sync(&alice_client).await;
        let mut request = create_room::v3::Request::new();
        request.name = Some("Morpheus timeline acceptance".into());
        request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
        request.invite = vec![bob_id.clone()];
        request.initial_state.push(
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        );
        let room = ok(alice_client.create_room(request).await, "create_room");
        let room_id: OwnedRoomId = room.room_id().to_owned();

        // Bob joins from a disposable device; his enrolled acceptance devices
        // need no action to receive the room keys.
        let bob_client = signed_in(&stores.0, bob, "bob").await;
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            sync(&bob_client).await;
            if bob_client
                .get_room(&room_id)
                .is_some_and(|room| room.state() == RoomState::Invited)
            {
                break;
            }
            require(Instant::now() < deadline, "invite");
            sleep(Duration::from_millis(300)).await;
        }
        ok(bob_client.join_room_by_id(&room_id).await, "join");

        // Every enrolled device must be known before the first message, or
        // its profile could not decrypt the full history.
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            sync(&alice_client).await;
            let devices = ok(
                alice_client.encryption().get_user_devices(&bob_id).await,
                "devices",
            );
            if room.joined_members_count() >= 2 && devices.devices().count() >= expected_devices {
                break;
            }
            require(Instant::now() < deadline, "devices");
            sleep(Duration::from_millis(300)).await;
        }

        for index in 1..=EVENTS {
            let started = Instant::now();
            ok(room.send(message(index)).await, "send");
            if index % 1_000 == 0 {
                eprintln!("MORPHEUS_TIMELINE_SEED_PROGRESS sent={index}");
            }
            let spent = started.elapsed();
            if spent < SEND_INTERVAL {
                sleep(SEND_INTERVAL - spent).await;
            }
        }

        let mut file = ok(
            {
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options.open(&output)
            },
            "output",
        );
        let text = serde_json::json!({"room_id": room_id.as_str(), "events": EVENTS}).to_string();
        ok(file.write_all(text.as_bytes()), "output");
        ok(file.sync_all(), "output");

        // The seeder devices are disposable; the enrolled devices keep their keys.
        let _ = bob_client.matrix_auth().logout().await;
        let _ = alice_client.matrix_auth().logout().await;
    })
    .await;
    require(seeded.is_ok(), "seed_deadline");
    drop(stores);
    eprintln!("MORPHEUS_TIMELINE_SEED_COMPLETE events={EVENTS}");
}

#[test]
fn every_message_carries_one_marker_and_heights_vary() {
    use matrix_sdk::ruma::events::room::message::MessageType;
    let mut kinds = std::collections::HashSet::new();
    for index in [1, 2, 3, 4, 5, 6, 7, 9_999, 10_000] {
        let content = message(index);
        let MessageType::Text(text) = &content.msgtype else {
            panic!("fixture messages are text");
        };
        let marker = format!("{MARKER}{index:05}");
        assert_eq!(text.body.matches(&marker).count(), 1);
        assert_eq!(text.body.matches(MARKER).count(), 1);
        assert!(!text.body.contains("http"));
        kinds.insert((text.body.lines().count(), text.formatted.is_some()));
    }
    assert!(kinds.len() >= 4, "fixture heights must vary");
}
