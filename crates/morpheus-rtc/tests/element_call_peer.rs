//! Opt-in MatrixRTC interop check with a real Element Call browser peer.
//! `scripts/element_call_peer.mjs` controls the other side. No public service
//! or hardware media device is involved.

#![cfg(feature = "native-rtc")]

use std::{fs, io::Write, time::Duration};

use futures_util::StreamExt;
use matrix_sdk::{Client, ruma::api::client::uiaa};
use morpheus_matrix::{AccountManager, SyncBackend, SyncStatus};
use morpheus_rtc::{
    CallOptions, I420Buffer, InteropMode, MediaStreamKind, RtcService, VideoFrame, VideoRotation,
};
use serde::Deserialize;
use tokio::time::{sleep, timeout};

const ELEMENT_ROOM_SLOT_ID: &str = "m.call#ROOM";
const NATIVE_ROOM_SLOT_ID: &str = "m.call#room";

fn write_marker(path: impl AsRef<std::path::Path>, value: &[u8]) {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .expect("Cannot create fresh private interop marker")
        .write_all(value)
        .expect("Cannot write private interop marker");
}

async fn wait_for_marker(path: &std::path::Path) {
    timeout(Duration::from_secs(120), async {
        while !path.is_file() {
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Stock browser coordination timed out");
}

// Deliberately distinct from the browser's red/green camera fixture. Browser
// canvas readback cannot mistake its own preview for this remote video.
fn native_video_frame(index: u32) -> VideoFrame {
    let mut data_y = vec![16; 640 * 360];
    for row in data_y.chunks_exact_mut(640) {
        row[..320].fill(235);
    }
    VideoFrame {
        buffer: I420Buffer {
            width: 640,
            height: 360,
            data_y,
            stride_y: 640,
            data_u: vec![128; 320 * 180],
            stride_u: 320,
            data_v: vec![128; 320 * 180],
            stride_v: 320,
        },
        rotation: VideoRotation::Deg0,
        timestamp_us: i64::from(index) * 66_667,
    }
}

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
    rooms: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    username: String,
    user_id: String,
    homeserver_url: String,
    password: String,
}

async fn bootstrap_identity(client: &Client, account: &TestAccount) {
    let encryption = client.encryption();
    encryption.wait_for_e2ee_initialization_tasks().await;
    let user_id = client.user_id().expect("Alice has no user ID");
    let existing_identity = encryption
        .request_user_identity(user_id)
        .await
        .expect("Cannot refresh Alice identity before bootstrap");
    let existing_device = encryption
        .get_own_device()
        .await
        .expect("Cannot read Alice device before bootstrap");
    if existing_identity.is_some_and(|identity| identity.is_verified())
        && existing_device.is_some_and(|device| device.is_verified_with_cross_signing())
    {
        return;
    }
    let initial = timeout(
        Duration::from_secs(30),
        encryption.bootstrap_cross_signing(None),
    )
    .await
    .expect("Cross-signing challenge timed out");
    if let Err(error) = initial {
        let challenge = error
            .as_uiaa_response()
            .expect("Cross-signing failed without a password challenge");
        let mut password = uiaa::Password::new(
            uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(account.username.clone())),
            account.password.clone(),
        );
        password.session = challenge.session.clone();
        timeout(
            Duration::from_secs(30),
            encryption.bootstrap_cross_signing(Some(uiaa::AuthData::Password(password))),
        )
        .await
        .expect("Cross-signing bootstrap timed out")
        .expect("Cross-signing bootstrap failed");
    }
    let identity = encryption
        .request_user_identity(user_id)
        .await
        .expect("Cannot refresh Alice identity")
        .expect("Alice identity is absent");
    assert!(identity.is_verified(), "Alice identity is not verified");
    let mut device = encryption
        .get_own_device()
        .await
        .expect("Cannot read Alice device")
        .expect("Alice device is absent");
    if !device.is_verified_with_cross_signing() {
        device.verify().await.expect("Cannot sign Alice device");
        encryption
            .request_user_identity(user_id)
            .await
            .expect("Cannot refresh Alice signed device");
        device = encryption
            .get_own_device()
            .await
            .expect("Cannot read Alice signed device")
            .expect("Alice signed device is absent");
    }
    assert!(device.is_verified_with_cross_signing());
}

fn has_red_green_halves(buffer: &morpheus_rtc::I420Buffer) -> bool {
    let width = buffer.width as usize;
    let height = buffer.height as usize;
    if width < 160 || height < 90 || width * 9 != height * 16 {
        return false;
    }
    let y_stride = buffer.stride_y as usize;
    let u_stride = buffer.stride_u as usize;
    let v_stride = buffer.stride_v as usize;
    if y_stride < width
        || u_stride < width.div_ceil(2)
        || v_stride < width.div_ceil(2)
        || buffer.data_y.len() < y_stride * height
        || buffer.data_u.len() < u_stride * height.div_ceil(2)
        || buffer.data_v.len() < v_stride * height.div_ceil(2)
    {
        return false;
    }
    let sample = |x: usize| {
        let y = height / 2;
        (
            buffer.data_y[y * y_stride + x],
            buffer.data_u[(y / 2) * u_stride + x / 2],
            buffer.data_v[(y / 2) * v_stride + x / 2],
        )
    };
    let left = sample(width / 4);
    let right = sample(width * 3 / 4);
    (50..=130).contains(&left.0)
        && left.1 < 145
        && left.2 > 155
        && (95..=190).contains(&right.0)
        && right.1 < 145
        && right.2 < 140
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn element_call_browser_peer_encrypted_media_and_departure() {
    assert!(
        std::env::var_os("MORPHEUS_ELEMENT_DIAG_REFRESH").is_none(),
        "Diagnostic browser refresh cannot establish stock interoperability"
    );
    let Some(fixture_path) = std::env::var_os("MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS") else {
        eprintln!("Skipped Element Call peer test; set MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS.");
        return;
    };
    let Some(go_path) = std::env::var_os("MORPHEUS_ELEMENT_PEER_GO_FILE") else {
        eprintln!("Skipped Element Call peer test; set MORPHEUS_ELEMENT_PEER_GO_FILE.");
        return;
    };
    let done_path = std::env::var_os("MORPHEUS_ELEMENT_PEER_DONE_FILE")
        .expect("Set MORPHEUS_ELEMENT_PEER_DONE_FILE");
    let stock_root = std::env::var_os("MORPHEUS_ELEMENT_PEER_ROOT").map(std::path::PathBuf::from);
    if let Some(root) = &stock_root {
        assert_eq!(
            std::env::var("MORPHEUS_ELEMENT_INTEROP_MODE").as_deref(),
            Ok("sticky-2025")
        );
        wait_for_marker(&root.join("browser-ready")).await;
        eprintln!("MORPHEUS_ELEMENT_CHECK native_presynced_baseline=true");
    }
    let fixture: Fixture =
        serde_json::from_slice(&fs::read(fixture_path).expect("Cannot read protected RTC fixture"))
            .expect("Invalid RTC fixture");
    let alice = fixture
        .accounts
        .iter()
        .find(|account| account.label == "a_primary")
        .expect("Alice fixture missing");
    let bob = fixture
        .accounts
        .iter()
        .find(|account| account.label == "a_second_device")
        .expect("Browser Bob fixture missing");
    let room_id = fixture
        .rooms
        .get("encrypted_room")
        .expect("Encrypted RTC room missing");
    let root = stock_root
        .as_ref()
        .map(|root| root.join("native-profile"))
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("morpheus-element-peer-{}", std::process::id()))
        });
    fs::create_dir_all(&root).expect("Cannot create private Alice store");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect Alice store");
    }
    let manager = AccountManager::open(root.join("alice")).expect("Cannot open Alice store");
    let alice_account = manager
        .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
        .await
        .expect("Alice login failed");
    timeout(Duration::from_secs(90), async {
        let mut status = manager.subscribe_sync_status();
        loop {
            if matches!(
                &*status.borrow(),
                SyncStatus::Running {
                    backend: SyncBackend::SlidingSticky
                }
            ) && manager
                .rooms_snapshot()
                .await
                .expect("Cannot read room list")
                .iter()
                .any(|room| room.room_id == *room_id)
            {
                return;
            }
            tokio::select! {
                _ = status.changed() => {},
                _ = sleep(Duration::from_millis(250)) => {},
            }
        }
    })
    .await
    .expect("Alice sticky sync or room never became ready");
    let client = manager.active_client().await.expect("Alice client missing");
    bootstrap_identity(&client, alice).await;
    let mut rtc = RtcService::new(client);
    let interop_mode = match std::env::var("MORPHEUS_ELEMENT_INTEROP_MODE").as_deref() {
        Ok("room-state") => InteropMode::RoomState,
        Ok("sticky-2025") => InteropMode::Sticky2025,
        Ok("current") | Err(_) => InteropMode::Current,
        Ok(other) => panic!("Unknown MORPHEUS_ELEMENT_INTEROP_MODE: {other}"),
    };
    let options = match interop_mode {
        InteropMode::RoomState => CallOptions::room_state_compatibility(),
        InteropMode::Sticky2025 => {
            // This matches the pinned RTC library's Element Call interop
            // fixture: its internal session and slot state stay lowercase,
            // while the compatibility dialect spells member/token uppercase.
            // Element Call treats an absent uppercase slot as implicitly open.
            rtc.open_slot(room_id, NATIVE_ROOM_SLOT_ID, true)
                .await
                .expect("Cannot open native encrypted slot");
            CallOptions::element_call_matrix_2_0()
        }
        InteropMode::Current => {
            rtc.open_slot(room_id, ELEMENT_ROOM_SLOT_ID, true)
                .await
                .expect("Cannot open Element Call encrypted slot");
            CallOptions {
                slot_id: ELEMENT_ROOM_SLOT_ID.to_owned(),
                interop_mode,
            }
        }
    };
    let call = rtc
        .join(room_id, options)
        .await
        .expect("Morpheus cannot join Element Call-compatible slot");
    let outgoing_video = if stock_root.is_some() {
        let publication = rtc
            .publish_camera(room_id, 640, 360, false)
            .await
            .expect("Cannot publish native interop camera");
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let sender = tokio::spawn(async move {
            for index in 0..3_600 {
                publication
                    .push_video(native_video_frame(index))
                    .expect("Native interop camera send failed");
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = sleep(Duration::from_millis(66)) => {},
                }
            }
            publication
        });
        Some((stop, sender))
    } else {
        None
    };
    write_marker(&go_path, b"ready\n");
    eprintln!("Morpheus joined Element Call's room call; waiting for browser peer.");
    let browser_member = timeout(Duration::from_secs(90), async {
        loop {
            if let Some(member) = rtc
                .participants(room_id)
                .expect("Call roster missing")
                .into_iter()
                .find(|member| member.user_id == bob.user_id)
            {
                return member;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Element Call browser never appeared in Morpheus roster");
    eprintln!("Element Call browser appeared in Morpheus roster.");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_roster=true");

    let audio = timeout(Duration::from_secs(45), async {
        loop {
            if let Some(track) = rtc
                .call(room_id)
                .expect("Morpheus call missing")
                .remote_track(&browser_member.member_id, MediaStreamKind::Microphone)
            {
                return track;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Browser microphone track was not subscribed");
    let video = timeout(Duration::from_secs(45), async {
        loop {
            if let Some(track) = rtc
                .call(room_id)
                .expect("Morpheus call missing")
                .remote_track(&browser_member.member_id, MediaStreamKind::Camera)
            {
                return track;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Browser camera track was not subscribed");
    let mut audio_frames = audio.audio_frames().expect("No browser PCM stream");
    let mut video_frames = video.video_frames().expect("No browser I420 stream");
    let audio_check = timeout(Duration::from_secs(45), async {
        let mut frames = 0;
        while let Some(frame) = audio_frames.next().await {
            frames += 1;
            if frame.data.iter().any(|sample| sample.abs() > 100) {
                return (frames, true);
            }
        }
        (frames, false)
    });
    let video_check = timeout(Duration::from_secs(45), async {
        let mut frames = 0;
        while let Some(frame) = video_frames.next().await {
            frames += 1;
            if has_red_green_halves(&frame.buffer) {
                return (frames, true);
            }
        }
        (frames, false)
    });
    let (audio_result, video_result) = tokio::join!(audio_check, video_check);
    eprintln!("Browser decoded media status: audio={audio_result:?}, video={video_result:?}");
    assert!(
        matches!(audio_result, Ok((_, true))),
        "Morpheus did not decode Element Call microphone audio"
    );
    assert!(
        matches!(video_result, Ok((_, true))),
        "Morpheus did not decode Element Call red/green video"
    );
    eprintln!("Morpheus decoded Element Call encrypted audio and synthetic red/green video.");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_audio=true");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_video=true");

    if let Some(root) = &stock_root {
        wait_for_marker(&root.join("browser-observed")).await;
    }

    write_marker(&done_path, b"leave\n");
    timeout(Duration::from_secs(45), async {
        loop {
            if rtc
                .participants(room_id)
                .expect("Call roster missing")
                .iter()
                .all(|member| member.user_id != bob.user_id)
            {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Element Call departure never reached Morpheus roster");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_departure=true");
    if let Some((stop, sender)) = outgoing_video {
        stop.send(()).expect("Native interop camera stopped early");
        sender
            .await
            .expect("Native interop camera sender panicked")
            .unpublish()
            .await
            .expect("Cannot unpublish native interop camera");
    }
    rtc.leave(room_id).await.expect("Morpheus leave failed");
    drop(rtc);
    manager
        .logout_account(&alice_account.id)
        .await
        .expect("Alice logout failed");
    manager.stop().await;
    drop(manager);
    fs::remove_dir_all(&root).expect("Cannot remove Alice test store");
    eprintln!("Element Call encrypted audio/video and participant departure passed.");
    let _ = call;
}
