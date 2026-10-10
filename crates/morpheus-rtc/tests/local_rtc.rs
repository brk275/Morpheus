//! Opt-in native MatrixRTC acceptance test against scripts/local_matrix_rtc.py.
//! It uses disposable Alice/Bob devices in a local encrypted room and synthetic
//! audio, so it needs no real microphone or public Matrix account.

#![cfg(feature = "native-rtc")]

use std::{collections::HashMap, fs, path::PathBuf, time::Duration};

use futures_util::StreamExt;
use matrix_sdk::{Client, ruma::api::client::uiaa};
use morpheus_matrix::{AccountManager, SyncBackend, SyncStatus};
use morpheus_rtc::{AudioFrame, CallOptions, MediaStreamKind, RtcError, RtcService};
use serde::Deserialize;
use tokio::time::{sleep, timeout};

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

async fn wait_for_sticky_sync(manager: &AccountManager) {
    let mut status = manager.subscribe_sync_status();
    timeout(Duration::from_secs(90), async {
        loop {
            if matches!(
                &*status.borrow(),
                SyncStatus::Running {
                    backend: SyncBackend::SlidingSticky
                }
            ) {
                return;
            }
            status.changed().await.expect("Matrix sync task exited");
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "MSC4354 sticky sliding sync unavailable: {:?}",
            status.borrow().clone()
        )
    });
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) {
    timeout(Duration::from_secs(90), async {
        loop {
            let rooms = manager
                .rooms_snapshot()
                .await
                .expect("Cannot read room list");
            if rooms.iter().any(|room| room.room_id == room_id) {
                return;
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("Encrypted room was not loaded");
}

async fn bootstrap_test_identity(client: &Client, account: &TestAccount) {
    // The native RTC backend follows MSC4153 and sends media keys only from a
    // verified own cross-signing identity to devices signed by their owner.
    // These disposable accounts are fresh, so set that identity up before the
    // call instead of weakening key delivery for the test.
    let encryption = client.encryption();
    let initial = timeout(
        Duration::from_secs(30),
        encryption.bootstrap_cross_signing(None),
    )
    .await
    .expect("Cross-signing bootstrap challenge timed out");
    if let Err(error) = initial {
        let response = error
            .as_uiaa_response()
            .expect("Cross-signing bootstrap failed without a password challenge");
        let mut password = uiaa::Password::new(
            uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(account.username.clone())),
            account.password.clone(),
        );
        password.session = response.session.clone();
        timeout(
            Duration::from_secs(30),
            encryption.bootstrap_cross_signing(Some(uiaa::AuthData::Password(password))),
        )
        .await
        .expect("Cross-signing bootstrap timed out")
        .expect("Cross-signing bootstrap failed");
    }
    let user_id = client.user_id().expect("Signed-in client has no user ID");
    // The bootstrap uploads the signing keys and device signature, but the
    // local device view can still contain its pre-bootstrap keys. Refresh it
    // from /keys/query before asserting that the signature chain is usable.
    let identity = timeout(
        Duration::from_secs(30),
        encryption.request_user_identity(user_id),
    )
    .await
    .expect("Refreshing own cross-signing identity timed out")
    .expect("Cannot refresh own cross-signing identity")
    .expect("Own cross-signing identity missing after bootstrap");
    assert!(
        identity.is_verified(),
        "Own cross-signing identity is not verified"
    );
    let mut device = encryption
        .get_own_device()
        .await
        .expect("Cannot read own device after cross-signing bootstrap")
        .expect("Own device missing after cross-signing bootstrap");
    if !device.is_verified_with_cross_signing() {
        // Some homeservers do not reflect the bootstrap's initial device
        // signature in the first query. Sign this disposable device explicitly
        // with the newly created self-signing key, then refresh its keys.
        timeout(Duration::from_secs(30), device.verify())
            .await
            .expect("Signing own device timed out")
            .expect("Cannot sign own device");
        timeout(
            Duration::from_secs(30),
            encryption.request_user_identity(user_id),
        )
        .await
        .expect("Refreshing signed own device timed out")
        .expect("Cannot refresh signed own device");
        device = encryption
            .get_own_device()
            .await
            .expect("Cannot read signed own device")
            .expect("Signed own device missing");
    }
    assert!(
        device.is_verified_with_cross_signing(),
        "Own device is not signed by the cross-signing identity"
    );
}

async fn wait_for_peer(rtc: &RtcService, room_id: &str, peer_user_id: &str, present: bool) {
    timeout(Duration::from_secs(45), async {
        loop {
            let call = rtc.call(room_id).expect("Call vanished");
            if call
                .participants()
                .iter()
                .any(|member| member.user_id == peer_user_id)
                == present
            {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Peer {peer_user_id} did not change roster presence to {present}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_two_device_audio_and_reconnect() {
    let Some(path) = std::env::var_os("MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS") else {
        eprintln!("Skipped local RTC acceptance; set MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS to opt in.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(path)).expect("Cannot read protected local Matrix fixture"),
    )
    .expect("Invalid local Matrix fixture");
    let alice = fixture
        .accounts
        .iter()
        .find(|account| account.label == "a_primary")
        .expect("Missing Alice fixture");
    let bob = fixture
        .accounts
        .iter()
        .find(|account| account.label == "a_second_device")
        .expect("Missing Bob fixture");
    let room_id = fixture
        .rooms
        .get("encrypted_room")
        .expect("Missing encrypted room");
    let root = std::env::temp_dir().join(format!(
        "morpheus-rtc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Clock before Unix epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("Cannot create temporary RTC stores");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect temporary RTC stores");
    }
    let alice_manager = AccountManager::open(root.join("alice")).expect("Cannot open Alice store");
    let bob_manager = AccountManager::open(root.join("bob")).expect("Cannot open Bob store");
    let alice_account = alice_manager
        .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
        .await
        .expect("Alice sign-in failed");
    let bob_account = bob_manager
        .add_password_account(&bob.homeserver_url, &bob.username, &bob.password)
        .await
        .expect("Bob sign-in failed");
    wait_for_sticky_sync(&alice_manager).await;
    wait_for_sticky_sync(&bob_manager).await;
    wait_for_room(&alice_manager, room_id).await;
    wait_for_room(&bob_manager, room_id).await;

    let alice_client = alice_manager
        .active_client()
        .await
        .expect("No Alice client");
    let bob_client = bob_manager.active_client().await.expect("No Bob client");
    let unverified_call = RtcService::new(alice_client.clone());
    assert!(matches!(
        unverified_call
            .open_slot(room_id, "m.call#room", true)
            .await,
        Err(RtcError::EncryptionIdentityNotReady)
    ));
    bootstrap_test_identity(&alice_client, alice).await;
    bootstrap_test_identity(&bob_client, bob).await;

    let mut alice_rtc = RtcService::new(alice_client);
    let mut bob_rtc = RtcService::new(bob_client);
    assert!(
        alice_rtc
            .discover_transport()
            .await
            .expect("RTC discovery failed")
            .is_supported()
    );
    alice_rtc
        .open_slot(room_id, "m.call#room", true)
        .await
        .expect("Cannot open encrypted call slot");
    let join_timeout = std::env::var("MORPHEUS_RTC_JOIN_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);
    let alice_call = timeout(
        Duration::from_secs(join_timeout),
        alice_rtc.join(room_id, CallOptions::default()),
    )
    .await
    .expect("Alice RTC join timed out")
    .expect("Alice RTC join failed");
    let bob_call = timeout(
        Duration::from_secs(join_timeout),
        bob_rtc.join(room_id, CallOptions::default()),
    )
    .await
    .expect("Bob RTC join timed out")
    .expect("Bob RTC join failed");
    assert_ne!(alice_call.local_identity, bob_call.local_identity);
    wait_for_peer(&alice_rtc, room_id, &bob_account.user_id, true).await;
    wait_for_peer(&bob_rtc, room_id, &alice_account.user_id, true).await;
    timeout(Duration::from_secs(30), async {
        loop {
            if bob_rtc
                .call(room_id)
                .expect("Bob call vanished")
                .imported_key_for(&alice_call.local_identity)
            {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Bob did not import Alice's encrypted media key");

    let publication = alice_rtc
        .publish_microphone(room_id, 48_000, 1, false)
        .await
        .expect("Cannot publish synthetic encrypted audio");
    let remote = timeout(Duration::from_secs(30), async {
        loop {
            if let Some(track) = bob_rtc
                .call(room_id)
                .expect("Bob call vanished")
                .remote_track(&alice_call.membership_id, MediaStreamKind::Microphone)
            {
                break track;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Bob did not subscribe to Alice's microphone");
    let mut frames = remote
        .audio_frames()
        .expect("Remote microphone has no PCM stream");
    let sender = async {
        for _ in 0..800 {
            publication
                .push_audio(AudioFrame {
                    data: vec![2_500; 480],
                    sample_rate: 48_000,
                    num_channels: 1,
                    samples_per_channel: 480,
                })
                .await
                .expect("Synthetic audio send failed");
            sleep(Duration::from_millis(10)).await;
        }
    };
    let receiver = async {
        timeout(Duration::from_secs(20), async {
            while let Some(frame) = frames.next().await {
                if frame.data.iter().any(|sample| *sample != 0) {
                    return;
                }
            }
            panic!("Remote audio stream closed before a decoded frame arrived");
        })
        .await
        .expect("Encrypted audio did not decode at Bob");
    };
    tokio::join!(sender, receiver);
    publication
        .unpublish()
        .await
        .expect("Cannot unpublish synthetic audio");

    bob_rtc.leave(room_id).await.expect("Bob leave failed");
    wait_for_peer(&alice_rtc, room_id, &bob_account.user_id, false).await;
    let rejoined = timeout(
        Duration::from_secs(join_timeout),
        bob_rtc.join(room_id, CallOptions::default()),
    )
    .await
    .expect("Bob rejoin timed out")
    .expect("Bob rejoin failed");
    assert_ne!(bob_call.membership_id, rejoined.membership_id);
    wait_for_peer(&alice_rtc, room_id, &bob_account.user_id, true).await;
    bob_rtc
        .leave(room_id)
        .await
        .expect("Bob final leave failed");
    alice_rtc.leave(room_id).await.expect("Alice leave failed");
    drop(bob_rtc);
    drop(alice_rtc);
    bob_manager
        .logout_account(&bob_account.id)
        .await
        .expect("Bob logout failed");
    alice_manager
        .logout_account(&alice_account.id)
        .await
        .expect("Alice logout failed");
    bob_manager.stop().await;
    alice_manager.stop().await;
    drop(bob_manager);
    drop(alice_manager);
    fs::remove_dir_all(&root).expect("Cannot remove temporary RTC stores");
    eprintln!("Local encrypted MatrixRTC call, synthetic audio, leave, and rejoin passed.");
}
