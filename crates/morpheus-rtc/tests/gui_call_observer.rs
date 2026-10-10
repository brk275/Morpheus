//! Opt-in headless Bob observer for a separate Morpheus GUI call as Alice.
//!
//! Start this test with MORPHEUS_RTC_OBSERVER_ACCOUNTS pointing to the protected
//! disposable fixture from scripts/local_matrix_rtc.py. It waits for the GUI to
//! open the encrypted room's normal MatrixRTC slot, joins on Bob's own verified
//! device, and counts decoded camera/screen I420 frames. It never opens a slot
//! or publishes local media, so the Alice GUI path is under test.

#![cfg(feature = "native-rtc")]

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    ruma::{
        RoomId,
        api::{client::state::get_state_event_for_key, client::uiaa, error::ErrorKind},
        events::StateEventType,
    },
};
use morpheus_matrix::{AccountManager, SyncBackend, SyncStatus};
use morpheus_rtc::{CallOptions, I420Buffer, MediaStreamKind, RtcService, RtcStreamKind};
use serde::Deserialize;
use tokio::{
    task::JoinHandle,
    time::{sleep, timeout},
};

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
            status
                .changed()
                .await
                .expect("Bob's Matrix sync task exited");
        }
    })
    .await
    .expect("Bob's sticky sliding sync did not start");
}

async fn wait_for_room(manager: &AccountManager, room_id: &str) {
    timeout(Duration::from_secs(90), async {
        loop {
            let rooms = manager
                .rooms_snapshot()
                .await
                .expect("Cannot read Bob's room list");
            if rooms.iter().any(|room| room.room_id == room_id) {
                return;
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("Bob's encrypted room did not load");
}

async fn bootstrap_test_identity(client: &Client, account: &TestAccount) {
    // Native MatrixRTC sends media keys only from a verified own identity to
    // devices signed by their owner. This freshly logged-in disposable device
    // must become verified before it joins Alice's encrypted call.
    let encryption = client.encryption();
    encryption.wait_for_e2ee_initialization_tasks().await;
    let user_id = client.user_id().expect("Bob has no Matrix user ID");
    let identity = encryption
        .request_user_identity(user_id)
        .await
        .expect("Cannot refresh Bob's identity");
    let device = encryption
        .get_own_device()
        .await
        .expect("Cannot read Bob's device");
    if identity.is_some_and(|identity| identity.is_verified())
        && device.is_some_and(|device| device.is_verified_with_cross_signing())
    {
        return;
    }

    let initial = timeout(
        Duration::from_secs(30),
        encryption.bootstrap_cross_signing(None),
    )
    .await
    .expect("Bob's cross-signing bootstrap challenge timed out");
    if let Err(error) = initial {
        let response = error
            .as_uiaa_response()
            .expect("Bob's cross-signing bootstrap failed without a password challenge");
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
        .expect("Bob's cross-signing bootstrap timed out")
        .expect("Bob's cross-signing bootstrap failed");
    }

    let identity = timeout(
        Duration::from_secs(30),
        encryption.request_user_identity(user_id),
    )
    .await
    .expect("Bob's identity refresh timed out")
    .expect("Cannot refresh Bob's identity")
    .expect("Bob's own cross-signing identity is missing");
    assert!(identity.is_verified(), "Bob's own identity is not verified");
    let mut device = encryption
        .get_own_device()
        .await
        .expect("Cannot read Bob's device after bootstrap")
        .expect("Bob's device is missing after bootstrap");
    if !device.is_verified_with_cross_signing() {
        timeout(Duration::from_secs(30), device.verify())
            .await
            .expect("Signing Bob's device timed out")
            .expect("Cannot sign Bob's device");
        timeout(
            Duration::from_secs(30),
            encryption.request_user_identity(user_id),
        )
        .await
        .expect("Refreshing Bob's signed device timed out")
        .expect("Cannot refresh Bob's signed device");
        device = encryption
            .get_own_device()
            .await
            .expect("Cannot read Bob's signed device")
            .expect("Bob's signed device is missing");
    }
    assert!(
        device.is_verified_with_cross_signing(),
        "Bob's device is not signed by his verified cross-signing identity"
    );
}

async fn wait_for_gui_slot(client: &Client, room_id: &str) {
    let room_id = RoomId::parse(room_id).expect("Invalid encrypted fixture room ID");
    timeout(Duration::from_secs(120), async {
        loop {
            let request = get_state_event_for_key::v3::Request::new(
                room_id.clone(),
                StateEventType::from("m.rtc.slot"),
                "m.call#room".to_owned(),
            );
            match client.send(request).await {
                Ok(_) => return,
                Err(error) if error.client_api_error_kind() == Some(&ErrorKind::NotFound) => {}
                Err(_) => panic!("Cannot query the GUI's MatrixRTC call slot"),
            }
            sleep(Duration::from_millis(300)).await;
        }
    })
    .await
    .expect("Alice's GUI did not open the encrypted MatrixRTC slot");
}

fn valid_decoded_i420(buffer: &I420Buffer) -> bool {
    let width = buffer.width as usize;
    let height = buffer.height as usize;
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    width > 0
        && height > 0
        && buffer.stride_y as usize >= width
        && buffer.stride_u as usize >= chroma_width
        && buffer.stride_v as usize >= chroma_width
        && (buffer.stride_y as usize)
            .checked_mul(height)
            .is_some_and(|size| buffer.data_y.len() >= size)
        && (buffer.stride_u as usize)
            .checked_mul(chroma_height)
            .is_some_and(|size| buffer.data_u.len() >= size)
        && (buffer.stride_v as usize)
            .checked_mul(chroma_height)
            .is_some_and(|size| buffer.data_v.len() >= size)
}

fn nonblank_i420(buffer: &I420Buffer) -> bool {
    if !valid_decoded_i420(buffer) {
        return false;
    }
    let width = buffer.width as usize;
    let height = buffer.height as usize;
    let stride = buffer.stride_y as usize;
    let mut levels = [false; 256];
    let mut visible = false;
    for row in 0..height {
        for &luma in &buffer.data_y[row * stride..row * stride + width] {
            levels[luma as usize] = true;
            visible |= luma > 20;
        }
    }
    visible && levels.into_iter().filter(|level| *level).count() >= 4
}

fn start_frame_counter(
    rtc: &RtcService,
    room_id: &str,
    alice_membership_id: &str,
    kind: MediaStreamKind,
    count: Arc<AtomicUsize>,
) -> Option<JoinHandle<()>> {
    let remote = rtc.call(room_id)?.remote_track(alice_membership_id, kind)?;
    let mut frames = remote.video_frames()?;
    Some(tokio::spawn(async move {
        while let Ok(Some(frame)) = timeout(Duration::from_secs(6), frames.next()).await {
            if nonblank_i420(&frame.buffer) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }))
}

fn required_frame_count() -> (bool, bool) {
    match std::env::var("MORPHEUS_RTC_OBSERVER_REQUIRE").as_deref() {
        Ok("camera") => (true, false),
        Ok("screen") => (false, true),
        Ok("both") | Err(_) => (true, true),
        Ok(_) => panic!("MORPHEUS_RTC_OBSERVER_REQUIRE must be camera, screen, or both"),
    }
}

async fn run_observer() {
    let Some(path) = std::env::var_os("MORPHEUS_RTC_OBSERVER_ACCOUNTS") else {
        eprintln!("Skipped GUI call observer; set MORPHEUS_RTC_OBSERVER_ACCOUNTS to opt in.");
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
    assert_ne!(
        alice.username, bob.username,
        "Observer needs distinct users"
    );
    let room_id = fixture
        .rooms
        .get("encrypted_room")
        .expect("Missing encrypted room fixture");
    let observe_secs = std::env::var("MORPHEUS_RTC_OBSERVER_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(120)
        .clamp(10, 300);
    let min_frames = std::env::var("MORPHEUS_RTC_OBSERVER_MIN_FRAMES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 300);
    let (require_camera, require_screen) = required_frame_count();
    let expect_screen_stop =
        std::env::var("MORPHEUS_RTC_OBSERVER_EXPECT_STOP").is_ok_and(|value| value == "screen");
    assert!(
        !expect_screen_stop || require_screen,
        "screen stop verification requires MORPHEUS_RTC_OBSERVER_REQUIRE=screen or both"
    );

    // Each run uses a fresh disposable device. A rebuilt macOS test binary may
    // not retain Keychain access to an earlier observer's credential item.
    let root = std::env::temp_dir().join(format!(
        "morpheus-rtc-observer-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Clock before Unix epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("Cannot create Bob's temporary RTC store");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect Bob's temporary RTC store");
    }
    let manager = AccountManager::open(root.join("bob")).expect("Cannot open Bob's store");
    let account = manager
        .add_password_account(&bob.homeserver_url, &bob.username, &bob.password)
        .await
        .expect("Bob sign-in failed");
    wait_for_sticky_sync(&manager).await;
    wait_for_room(&manager, room_id).await;
    let client = manager
        .active_client()
        .await
        .expect("Bob client is missing");
    bootstrap_test_identity(&client, bob).await;
    assert!(
        RtcService::new(client.clone())
            .discover_transport()
            .await
            .expect("Bob's RTC discovery failed")
            .is_supported(),
        "Bob's homeserver advertises no compatible MatrixRTC service"
    );

    eprintln!("Bob is verified and waiting for Alice's GUI call slot.");
    wait_for_gui_slot(&client, room_id).await;
    let mut rtc = RtcService::new(client);
    let join = timeout(
        Duration::from_secs(60),
        rtc.join(room_id, CallOptions::default()),
    )
    .await
    .expect("Bob's native RTC join timed out")
    .expect("Bob could not join Alice's GUI call");
    eprintln!("Bob joined the encrypted GUI call; waiting for Alice's roster entry.");

    let alice_member = timeout(Duration::from_secs(45), async {
        let mut last_roster = String::new();
        loop {
            let participants = rtc.participants(room_id).expect("Bob's call vanished");
            let roster = participants
                .iter()
                .map(|participant| {
                    format!(
                        "{} (local={}, reachable={})",
                        participant.user_id, participant.is_local, participant.reachable
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            if roster != last_roster {
                eprintln!("Bob's disposable-user roster: [{roster}]");
                last_roster = roster;
            }
            if let Some(alice_member) = participants
                .iter()
                .find(|participant| participant.user_id == alice.user_id && !participant.is_local)
            {
                eprintln!(
                    "Bob's roster has {} participants; Alice presence is visible, reachable={}.",
                    participants.len(),
                    alice_member.reachable
                );
                return alice_member.member_id.clone();
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Bob did not see Alice in the encrypted call roster");

    let camera_frames = Arc::new(AtomicUsize::new(0));
    let screen_frames = Arc::new(AtomicUsize::new(0));
    let mut camera_pump = None;
    let mut screen_pump = None;
    let deadline = Instant::now() + Duration::from_secs(observe_secs);
    while Instant::now() < deadline {
        if camera_pump.as_ref().is_some_and(JoinHandle::is_finished) {
            let finished = camera_pump.take().expect("Camera pump disappeared");
            finished.await.expect("Camera decoder task failed");
        }
        if screen_pump.as_ref().is_some_and(JoinHandle::is_finished) {
            let finished = screen_pump.take().expect("Screen pump disappeared");
            finished.await.expect("Screen decoder task failed");
        }
        if camera_pump.is_none() {
            camera_pump = start_frame_counter(
                &rtc,
                room_id,
                &alice_member,
                MediaStreamKind::Camera,
                camera_frames.clone(),
            );
        }
        if screen_pump.is_none() {
            screen_pump = start_frame_counter(
                &rtc,
                room_id,
                &alice_member,
                MediaStreamKind::ScreenShare,
                screen_frames.clone(),
            );
        }
        let camera_ready = !require_camera || camera_frames.load(Ordering::Relaxed) >= min_frames;
        let screen_ready = !require_screen || screen_frames.load(Ordering::Relaxed) >= min_frames;
        if camera_ready && screen_ready {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    for pump in [camera_pump, screen_pump].into_iter().flatten() {
        pump.abort();
        let _ = pump.await;
    }
    let camera_count = camera_frames.load(Ordering::Relaxed);
    let screen_count = screen_frames.load(Ordering::Relaxed);
    eprintln!(
        "Bob decoded Alice's encrypted GUI tracks: camera={camera_count} nonblank I420 frames, screen={screen_count} nonblank I420 frames."
    );

    if expect_screen_stop && screen_count >= min_frames {
        eprintln!("Bob has enough screen frames; stop Alice's display share now.");
        timeout(Duration::from_secs(45), async {
            loop {
                let alice_participant = rtc
                    .participants(room_id)
                    .expect("Bob's call vanished while waiting for share stop")
                    .into_iter()
                    .find(|participant| participant.user_id == alice.user_id)
                    .expect("Alice left the call before stopping display share");
                let advertised = alice_participant
                    .streams
                    .iter()
                    .any(|stream| stream.kind == RtcStreamKind::ScreenShare);
                let remote_track = rtc
                    .call(room_id)
                    .expect("Bob's call vanished while waiting for share stop")
                    .remote_track(&alice_member, MediaStreamKind::ScreenShare);
                if !advertised && remote_track.is_none() {
                    eprintln!(
                        "Bob observed Alice's screen publication end while she stayed in the call."
                    );
                    break;
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("Alice's display publication did not stop within 45 seconds");
    }

    rtc.leave(room_id)
        .await
        .expect("Bob could not leave the call");
    drop(rtc);
    manager
        .logout_account(&account.id)
        .await
        .expect("Bob logout failed");
    manager.stop().await;
    drop(manager);
    fs::remove_dir_all(&root).expect("Cannot remove Bob's temporary RTC store");
    assert!(
        !require_camera || camera_count >= min_frames,
        "Bob decoded fewer than {min_frames} nonblank GUI camera frames"
    );
    assert!(
        !require_screen || screen_count >= min_frames,
        "Bob decoded fewer than {min_frames} nonblank GUI screen frames"
    );
    assert!(
        !join.membership_id.is_empty(),
        "Bob had no MatrixRTC membership"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_observes_alice_gui_encrypted_call() {
    timeout(Duration::from_secs(900), run_observer())
        .await
        .expect("GUI call observer exceeded its global timeout");
}
