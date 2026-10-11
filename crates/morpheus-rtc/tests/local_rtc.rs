//! Opt-in native MatrixRTC acceptance test against scripts/local_matrix_rtc.py.
//! It uses disposable Alice/Bob devices in a local encrypted room and synthetic
//! audio and video, so it needs no real capture device or public Matrix account.

#![cfg(feature = "native-rtc")]

use std::{collections::HashMap, fs, path::PathBuf, time::Duration};

use futures_util::StreamExt;
use livekit::SimulateScenario;
use matrix_rtc_call_sdk::CallEvent;
use matrix_sdk::{Client, ruma::api::client::uiaa};
use morpheus_matrix::{AccountInfo, AccountManager, SyncBackend, SyncStatus};
#[cfg(feature = "device-screen")]
use morpheus_rtc::ScreenCapture;
use morpheus_rtc::{
    AudioFrame, CallOptions, I420Buffer, MediaStreamKind, RtcError, RtcService, VideoFrame,
    VideoRotation, VoiceSpaceMarker,
};
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

const VIDEO_WIDTH: u32 = 640;
const VIDEO_HEIGHT: u32 = 360;

#[derive(Clone, Copy)]
enum RosterPhase {
    InitialAlice,
    InitialBob,
    DepartedBob,
    RejoinedBob,
}

impl RosterPhase {
    fn name(self) -> &'static str {
        match self {
            Self::InitialAlice => "initial_alice_roster",
            Self::InitialBob => "initial_bob_roster",
            Self::DepartedBob => "departed_bob_roster",
            Self::RejoinedBob => "rejoined_bob_roster",
        }
    }
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
    .expect("RTC fixture room was not loaded");
}

async fn prepare_bob(
    manager: &AccountManager,
    account: &TestAccount,
    room_id: &str,
    unencrypted_room_id: &str,
) -> (AccountInfo, Client) {
    let saved = manager
        .add_password_account(
            &account.homeserver_url,
            &account.username,
            &account.password,
        )
        .await
        .expect("Bob sign-in failed");
    wait_for_sticky_sync(manager).await;
    wait_for_room(manager, room_id).await;
    wait_for_room(manager, unencrypted_room_id).await;
    let client = manager.active_client().await.expect("No Bob client");
    bootstrap_test_identity(&client, account).await;
    (saved, client)
}

fn has_encrypted_membership(room: &matrix_sdk::Room, membership_id: &str) -> bool {
    room.sticky_events().live().iter().any(|entry| {
        entry.encryption_info().is_some()
            && entry
                .raw()
                .get_field::<serde_json::Value>("content")
                .ok()
                .flatten()
                .and_then(|content| {
                    content
                        .get("member")?
                        .get("id")?
                        .as_str()
                        .map(str::to_owned)
                })
                .is_some_and(|id| id == membership_id)
    })
}

async fn wait_for_encrypted_membership(client: &Client, room_id: &str, membership_id: &str) {
    let room_id = matrix_sdk::ruma::RoomId::parse(room_id).expect("Invalid fixture room ID");
    let room = client.get_room(&room_id).expect("Alice room is missing");
    timeout(Duration::from_secs(30), async {
        loop {
            if has_encrypted_membership(&room, membership_id) {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("Alice encrypted membership did not arrive before Bob's device creation");
}

async fn bootstrap_test_identity(client: &Client, account: &TestAccount) {
    // The native RTC backend follows MSC4153 and sends media keys only from a
    // verified own cross-signing identity to devices signed by their owner.
    // These disposable accounts are fresh, so set that identity up before the
    // call instead of weakening key delivery for the test.
    let encryption = client.encryption();
    encryption.wait_for_e2ee_initialization_tasks().await;
    let user_id = client.user_id().expect("Signed-in client has no user ID");
    let existing_identity = encryption
        .request_user_identity(user_id)
        .await
        .expect("Cannot refresh own cross-signing identity before bootstrap");
    let existing_device = encryption
        .get_own_device()
        .await
        .expect("Cannot read own device before bootstrap");
    if existing_identity.is_some_and(|identity| identity.is_verified())
        && existing_device.is_some_and(|device| device.is_verified_with_cross_signing())
    {
        // AccountManager may already have initialized and signed a fresh
        // password-login device. Do not rotate its identity a second time.
        return;
    }
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

async fn wait_for_peer(
    rtc: &RtcService,
    room_id: &str,
    peer_user_id: &str,
    present: bool,
    phase: RosterPhase,
) {
    // Start a new line even when libtest has left its test-name prefix open.
    // Never put room, user, device, membership or transport values in markers.
    eprintln!("\nMORPHEUS_RTC_STAGE {}", phase.name());
    let mut observed = None;
    let result = timeout(Duration::from_secs(45), async {
        loop {
            let found = rtc
                .participants(room_id)
                .expect("Call roster vanished")
                .iter()
                .any(|member| member.user_id == peer_user_id);
            observed = Some(found);
            if found == present {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    if result.is_err() {
        if let Some(observed) = observed {
            eprintln!(
                "\nMORPHEUS_RTC_ROSTER expected_present={present} observed_present={observed}"
            );
        }
    }
    result.expect("Peer did not reach the expected roster presence");
}

/// A red left half and green right half. The split survives VP8 scaling and
/// compression; the changed timestamp keeps the native encoder paced at 15fps.
fn synthetic_video_frame(index: u32) -> VideoFrame {
    let width = VIDEO_WIDTH as usize;
    let height = VIDEO_HEIGHT as usize;
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let mut data_y = vec![82; width * height];
    let mut data_u = vec![90; chroma_width * chroma_height];
    let mut data_v = vec![240; chroma_width * chroma_height];
    for row in data_y.chunks_exact_mut(width) {
        row[width / 2..].fill(145);
    }
    for row in data_u.chunks_exact_mut(chroma_width) {
        row[chroma_width / 2..].fill(54);
    }
    for row in data_v.chunks_exact_mut(chroma_width) {
        row[chroma_width / 2..].fill(34);
    }
    VideoFrame {
        buffer: I420Buffer {
            width: VIDEO_WIDTH,
            height: VIDEO_HEIGHT,
            data_y,
            stride_y: VIDEO_WIDTH,
            data_u,
            stride_u: chroma_width as u32,
            data_v,
            stride_v: chroma_width as u32,
        },
        rotation: VideoRotation::Deg0,
        timestamp_us: i64::from(index) * 66_667,
    }
}

/// Check decoded dimensions and YUV samples well inside each color half. A
/// receiver may select a lower simulcast layer, so require 16:9 within the
/// source dimensions rather than a specific layer's pixel count.
fn decoded_color_pattern(buffer: &I420Buffer) -> Option<(u32, u32)> {
    let (width, height) = (buffer.width as usize, buffer.height as usize);
    if width < 160
        || height < 90
        || width > VIDEO_WIDTH as usize
        || height > VIDEO_HEIGHT as usize
        || width * 9 != height * 16
    {
        return None;
    }
    let strides = [
        buffer.stride_y as usize,
        buffer.stride_u as usize,
        buffer.stride_v as usize,
    ];
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    if strides[0] < width
        || strides[1] < chroma_width
        || strides[2] < chroma_width
        || buffer.data_y.len() < strides[0].checked_mul(height)?
        || buffer.data_u.len() < strides[1].checked_mul(chroma_height)?
        || buffer.data_v.len() < strides[2].checked_mul(chroma_height)?
    {
        return None;
    }
    let sample = |x: usize| {
        let y = height / 2;
        (
            buffer.data_y[y * strides[0] + x],
            buffer.data_u[(y / 2) * strides[1] + x / 2],
            buffer.data_v[(y / 2) * strides[2] + x / 2],
        )
    };
    let red = sample(width / 4);
    let green = sample(width * 3 / 4);
    let red_detected = (50..=120).contains(&red.0) && red.1 < 135 && red.2 > 175;
    let green_detected = (105..=190).contains(&green.0) && green.1 < 120 && green.2 < 125;
    (red_detected && green_detected).then_some((buffer.width, buffer.height))
}

async fn verify_video_kind(
    alice_rtc: &RtcService,
    bob_rtc: &RtcService,
    room_id: &str,
    alice_membership_id: &str,
    kind: MediaStreamKind,
) {
    let publication = match kind {
        MediaStreamKind::Camera => {
            alice_rtc
                .publish_camera(room_id, VIDEO_WIDTH, VIDEO_HEIGHT, false)
                .await
        }
        MediaStreamKind::ScreenShare => {
            alice_rtc
                .publish_screen(room_id, VIDEO_WIDTH, VIDEO_HEIGHT, false)
                .await
        }
        _ => panic!("Only synthetic video kinds belong in this test"),
    }
    .expect("Cannot publish synthetic encrypted video");
    let (done, mut stop_sending) = oneshot::channel::<()>();
    let sending = tokio::spawn(async move {
        for index in 0..375 {
            publication
                .push_video(synthetic_video_frame(index))
                .expect("Synthetic video frame send failed");
            tokio::select! {
                _ = &mut stop_sending => break,
                _ = sleep(Duration::from_millis(66)) => {}
            }
        }
        publication
    });
    let remote = timeout(Duration::from_secs(30), async {
        loop {
            if let Some(track) = bob_rtc
                .call(room_id)
                .expect("Bob call vanished")
                .remote_track(alice_membership_id, kind)
            {
                break track;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Bob did not subscribe to Alice's {kind:?}"));
    let mut frames = remote
        .video_frames()
        .expect("Remote video has no I420 stream");
    let dimensions = timeout(Duration::from_secs(30), async {
        while let Some(frame) = frames.next().await {
            if let Some(dimensions) = decoded_color_pattern(&frame.buffer) {
                return dimensions;
            }
        }
        panic!("Remote video stream closed before a colored frame arrived");
    })
    .await
    .unwrap_or_else(|_| panic!("Bob did not decode Alice's red/green {kind:?} pattern"));
    done.send(()).expect("Synthetic video sender stopped early");
    let publication = sending.await.expect("Synthetic video sender panicked");
    publication
        .unpublish()
        .await
        .expect("Cannot unpublish synthetic video");
    eprintln!("Decoded encrypted {kind:?} I420 pattern at {dimensions:?}.");
}

#[cfg(feature = "device-screen")]
fn nonblank_frame(buffer: &I420Buffer) -> bool {
    let (width, height, stride) = (
        buffer.width as usize,
        buffer.height as usize,
        buffer.stride_y as usize,
    );
    if width == 0
        || height == 0
        || stride < width
        || buffer.data_y.len() < stride.saturating_mul(height)
    {
        return false;
    }
    let mut levels = [false; 256];
    let mut has_visible_luma = false;
    for row in 0..height {
        for &luma in &buffer.data_y[row * stride..row * stride + width] {
            levels[luma as usize] = true;
            has_visible_luma |= luma > 20;
        }
    }
    has_visible_luma && levels.into_iter().filter(|present| *present).count() >= 4
}

/// The separate real_capture test proves that native pixels enter I420. This
/// optional extension proves that an actual capture also crosses the encrypted
/// MatrixRTC screen track and decodes on Bob's device. Pixels stay local to the
/// disposable loopback call and are never written to a file or test log.
#[cfg(feature = "device-screen")]
async fn verify_real_screen_capture(
    alice_rtc: &RtcService,
    bob_rtc: &RtcService,
    room_id: &str,
    alice_membership_id: &str,
) {
    let source = std::env::var("MORPHEUS_CAPTURE_SOURCE").unwrap_or_else(|_| "display".into());
    let index = std::env::var("MORPHEUS_CAPTURE_INDEX")
        .ok()
        .map(|value| value.parse::<usize>().expect("Invalid capture index"))
        .unwrap_or(0);
    let capture = match source.as_str() {
        "display" => ScreenCapture::open(index),
        "window" => ScreenCapture::open_window(index),
        _ => panic!("MORPHEUS_CAPTURE_SOURCE must be display or window"),
    }
    .expect("Cannot open the selected native capture source");
    let first = capture
        .next_frame_sized(VIDEO_WIDTH, VIDEO_HEIGHT)
        .expect("Cannot capture native screen frame");
    assert!(nonblank_frame(&first.buffer), "Native capture was blank");

    timeout(Duration::from_secs(15), async {
        while bob_rtc
            .call(room_id)
            .expect("Bob call vanished")
            .remote_track(alice_membership_id, MediaStreamKind::ScreenShare)
            .is_some()
        {
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("Previous synthetic screen track did not close");
    let publication = alice_rtc
        .publish_screen(room_id, VIDEO_WIDTH, VIDEO_HEIGHT, false)
        .await
        .expect("Cannot publish real native screen capture");
    let sending = async {
        publication
            .push_video(first)
            .expect("Cannot send first native capture frame");
        loop {
            sleep(Duration::from_millis(125)).await;
            let frame = capture
                .next_frame_sized(VIDEO_WIDTH, VIDEO_HEIGHT)
                .expect("Native capture stopped during publication");
            publication
                .push_video(frame)
                .expect("Cannot send native capture frame");
        }
    };
    let receiving = async {
        let remote = timeout(Duration::from_secs(20), async {
            loop {
                if let Some(track) = bob_rtc
                    .call(room_id)
                    .expect("Bob call vanished")
                    .remote_track(alice_membership_id, MediaStreamKind::ScreenShare)
                {
                    break track;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Bob did not subscribe to the native screen track");
        let mut frames = remote
            .video_frames()
            .expect("Native screen track has no I420 stream");
        timeout(Duration::from_secs(20), async {
            while let Some(frame) = frames.next().await {
                if frame.buffer.width > 0
                    && frame.buffer.height > 0
                    && nonblank_frame(&frame.buffer)
                {
                    return;
                }
            }
            panic!("Native screen stream closed before Bob decoded a nonblank frame");
        })
        .await
        .expect("Bob did not decode nonblank native screen pixels");
    };
    tokio::select! {
        () = sending => unreachable!("Screen sender cannot finish on its own"),
        () = receiving => {},
    }
    publication
        .unpublish()
        .await
        .expect("Cannot unpublish native screen capture");
    eprintln!("Bob decoded nonblank encrypted native {source} pixels at 640×360.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_two_device_audio_video_and_reconnect() {
    let Some(path) = std::env::var_os("MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS") else {
        eprintln!("Skipped local RTC acceptance; set MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS to opt in.");
        return;
    };
    let late_join = match std::env::var("MORPHEUS_RTC_LATE_JOIN").as_deref() {
        Ok("1") => true,
        Err(std::env::VarError::NotPresent) => false,
        _ => panic!("MORPHEUS_RTC_LATE_JOIN must be absent or 1"),
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
    let unencrypted_room_id = fixture
        .rooms
        .get("unencrypted_room")
        .expect("Missing unencrypted guard room");
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
    wait_for_sticky_sync(&alice_manager).await;
    wait_for_room(&alice_manager, room_id).await;
    wait_for_room(&alice_manager, unencrypted_room_id).await;

    let alice_client = alice_manager
        .active_client()
        .await
        .expect("No Alice client");
    let mut unencrypted_call = RtcService::new(alice_client.clone());
    timeout(Duration::from_secs(30), async {
        loop {
            match unencrypted_call
                .open_slot(unencrypted_room_id, "m.call#room", false)
                .await
            {
                Err(RtcError::UnknownEncryptionState) => sleep(Duration::from_millis(200)).await,
                Err(RtcError::UnencryptedRoomUnsupported) => break,
                other => panic!("Unencrypted current-format slot was not rejected: {other:?}"),
            }
        }
    })
    .await
    .expect("Unencrypted room state did not load");
    assert!(matches!(
        unencrypted_call
            .join(unencrypted_room_id, CallOptions::default())
            .await,
        Err(RtcError::UnencryptedRoomUnsupported)
    ));
    bootstrap_test_identity(&alice_client, alice).await;
    let early_bob = if late_join {
        None
    } else {
        Some(prepare_bob(&bob_manager, bob, room_id, unencrypted_room_id).await)
    };

    let mut alice_rtc = RtcService::new(alice_client.clone());
    assert!(
        alice_rtc
            .discover_transport()
            .await
            .expect("RTC discovery failed")
            .is_supported()
    );
    alice_rtc
        .set_voice_space_marker(room_id, true)
        .await
        .expect("Cannot designate shared voice space");
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
    let (bob_account, bob_client) = match early_bob {
        Some(bob) => bob,
        None => {
            // A prior call leave/rejoin reuses Bob's old Megolm keys and does
            // not cover this case. His receiving device must not exist until
            // Alice's encrypted sticky membership has actually been sent.
            wait_for_encrypted_membership(&alice_client, room_id, &alice_call.membership_id).await;
            assert!(bob_manager.accounts().await.is_empty());
            let prepared = prepare_bob(&bob_manager, bob, room_id, unencrypted_room_id).await;
            let id = matrix_sdk::ruma::RoomId::parse(room_id).expect("Invalid fixture room ID");
            let room = prepared.1.get_room(&id).expect("Bob room is missing");
            eprintln!(
                "\nMORPHEUS_RTC_LATE_JOIN historical_member={}",
                has_encrypted_membership(&room, &alice_call.membership_id),
            );
            prepared
        }
    };
    let mut bob_rtc = RtcService::new(bob_client);
    timeout(Duration::from_secs(30), async {
        loop {
            if bob_rtc
                .voice_space_marker(room_id)
                .await
                .expect("Cannot read shared voice-space marker")
                == Some(VoiceSpaceMarker::ENABLED)
            {
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Bob did not receive the shared voice-space marker");
    let bob_call = timeout(
        Duration::from_secs(join_timeout),
        bob_rtc.join(room_id, CallOptions::default()),
    )
    .await
    .expect("Bob RTC join timed out")
    .expect("Bob RTC join failed");
    assert_ne!(alice_call.local_identity, bob_call.local_identity);
    wait_for_peer(
        &alice_rtc,
        room_id,
        &bob_account.user_id,
        true,
        RosterPhase::InitialAlice,
    )
    .await;
    wait_for_peer(
        &bob_rtc,
        room_id,
        &alice_account.user_id,
        true,
        RosterPhase::InitialBob,
    )
    .await;
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

    verify_video_kind(
        &alice_rtc,
        &bob_rtc,
        room_id,
        &alice_call.membership_id,
        MediaStreamKind::Camera,
    )
    .await;
    verify_video_kind(
        &alice_rtc,
        &bob_rtc,
        room_id,
        &alice_call.membership_id,
        MediaStreamKind::ScreenShare,
    )
    .await;

    // Force the pinned LiveKit client to lose its signalling socket. The
    // existing call must resume without leaving its MatrixRTC membership, and
    // newly published encrypted media must still decode afterward.
    let mut connection_events = alice_rtc
        .call(room_id)
        .expect("Alice call vanished before signal reconnect")
        .subscribe_call_events();
    alice_rtc
        .call(room_id)
        .expect("Alice call vanished before signal reconnect")
        .session()
        .room()
        .simulate_scenario(SimulateScenario::SignalReconnect)
        .await
        .expect("Cannot simulate LiveKit signalling reconnect");
    timeout(Duration::from_secs(45), async {
        let mut saw_degraded = false;
        loop {
            match connection_events.recv().await {
                Ok(CallEvent::MediaConnectionState { degraded: true }) => {
                    saw_degraded = true;
                }
                Ok(CallEvent::MediaConnectionState { degraded: false }) if saw_degraded => break,
                Ok(CallEvent::Ended { reason }) => {
                    panic!("Alice call ended during signalling reconnect: {reason:?}")
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    panic!("Alice reconnect event stream lagged by {skipped} events")
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("Alice reconnect event stream closed")
                }
            }
        }
    })
    .await
    .expect("Alice did not return to a healthy media connection after signal loss");
    eprintln!(
        "Alice's LiveKit signalling connection degraded and resumed without leaving the call."
    );
    verify_video_kind(
        &alice_rtc,
        &bob_rtc,
        room_id,
        &alice_call.membership_id,
        MediaStreamKind::Camera,
    )
    .await;
    #[cfg(feature = "device-screen")]
    if std::env::var("MORPHEUS_RTC_REAL_SCREEN_CAPTURE").as_deref() == Ok("1") {
        verify_real_screen_capture(&alice_rtc, &bob_rtc, room_id, &alice_call.membership_id).await;
    }

    // The first key for a join is index 0. Keep this receiver open before Bob
    // leaves so the next local key import must come from the live call's
    // departure-triggered rotation, not from a later test subscription.
    let mut alice_events = alice_rtc
        .call(room_id)
        .expect("Alice call vanished before key rotation")
        .subscribe_call_events();
    bob_rtc.leave(room_id).await.expect("Bob leave failed");
    wait_for_peer(
        &alice_rtc,
        room_id,
        &bob_account.user_id,
        false,
        RosterPhase::DepartedBob,
    )
    .await;
    let rotated_key_index = timeout(Duration::from_secs(40), async {
        loop {
            match alice_events.recv().await {
                Ok(CallEvent::KeyImported {
                    member_id,
                    identity,
                    key_index,
                }) if member_id == alice_call.membership_id
                    && identity == alice_call.local_identity
                    && key_index != 0 =>
                {
                    return key_index;
                }
                Ok(CallEvent::Ended { reason }) => {
                    panic!("Alice call ended before key rotation: {reason:?}")
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    panic!("Alice key-rotation event stream lagged by {skipped} events")
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("Alice key-rotation event stream closed")
                }
            }
        }
    })
    .await
    .expect("Alice did not activate a new media-key index after Bob left");
    eprintln!("Alice activated media-key index {rotated_key_index} after Bob left.");
    let rejoined = timeout(
        Duration::from_secs(join_timeout),
        bob_rtc.join(room_id, CallOptions::default()),
    )
    .await
    .expect("Bob rejoin timed out")
    .expect("Bob rejoin failed");
    assert_ne!(bob_call.membership_id, rejoined.membership_id);
    wait_for_peer(
        &alice_rtc,
        room_id,
        &bob_account.user_id,
        true,
        RosterPhase::RejoinedBob,
    )
    .await;
    timeout(Duration::from_secs(30), async {
        loop {
            if bob_rtc
                .call(room_id)
                .expect("Bob rejoined call vanished")
                .imported_key_for(&alice_call.local_identity)
            {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Bob did not import Alice's media key after rejoining");
    verify_video_kind(
        &alice_rtc,
        &bob_rtc,
        room_id,
        &alice_call.membership_id,
        MediaStreamKind::Camera,
    )
    .await;
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
    eprintln!(
        "Local encrypted MatrixRTC call, synthetic audio, camera, screen stream, leave-triggered media-key rotation, rejoin, and post-rejoin video passed."
    );
    if late_join {
        eprintln!(
            "Fresh-device late join passed without historical key import or membership refresh injection."
        );
    }
}
