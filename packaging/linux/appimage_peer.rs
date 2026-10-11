//! Receiving peer for the ordinary installed AppImage ScreenCast gate.
//! No capture/publication source is created here. Only the normal GUI may share.
//! Pixels are compared in memory and never persisted or logged.
#![cfg(all(target_os = "linux", feature = "native-devices"))]

use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{Client, ruma::api::client::uiaa};
use morpheus_matrix::{AccountManager, SyncBackend, SyncStatus};
use morpheus_rtc::{CallOptions, I420Buffer, MediaStreamKind, RtcService, audio_devices};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{sleep, timeout};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    fixture: PathBuf,
    microphone: String,
    speaker: String,
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

fn private_bytes(path: &Path, maximum: u64) -> Vec<u8> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .expect("Cannot read private peer input");
    let metadata = file.metadata().expect("Cannot inspect peer input");
    assert!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::getuid() }
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.len() <= maximum,
        "Peer input is not private and bounded"
    );
    let mut data = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut data)
        .expect("Cannot read peer input");
    assert!(data.len() as u64 <= maximum, "Peer input exceeds bound");
    data
}

fn fact(root: &Path, name: &'static str) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join(format!("{name}.json")))
        .expect("Cannot create peer fact");
    file.write_all(b"{\"passed\":true}\n")
        .expect("Cannot write peer fact");
    file.sync_all().expect("Cannot sync peer fact");
    eprintln!("MORPHEUS_APPIMAGE_PEER {name}=true");
}

async fn command(root: &Path, name: &'static str) {
    timeout(Duration::from_secs(60), async {
        let path = root.join(format!("request-{name}.json"));
        loop {
            if path.exists() {
                assert_eq!(
                    private_bytes(&path, 64),
                    b"{\"proceed\":true}\n",
                    "Invalid peer command"
                );
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("Installed app did not reach the next peer phase");
}

async fn ready(manager: &AccountManager, room: &str) {
    let mut status = manager.subscribe_sync_status();
    timeout(Duration::from_secs(90), async {
        loop {
            let running = matches!(&*status.borrow(), SyncStatus::Running { backend: SyncBackend::SlidingSticky });
            if running && manager.rooms_snapshot().await.expect("Cannot read peer rooms").iter().any(|item| item.room_id == room) {
                return;
            }
            tokio::select! { _ = sleep(Duration::from_millis(200)) => {}, result = status.changed() => { result.expect("Peer sync closed"); } }
        }
    }).await.expect("Peer sticky room sync did not become ready");
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

#[derive(Clone, Copy)]
enum Color {
    Red,
    Green,
    Yellow,
    Magenta,
}

fn color_matches(buffer: &I420Buffer, color: Color) -> bool {
    let width = buffer.width as usize;
    let height = buffer.height as usize;
    if width < 64 || height < 64 || width > 3840 || height > 2160 {
        return false;
    }
    let (sy, su, sv) = (
        buffer.stride_y as usize,
        buffer.stride_u as usize,
        buffer.stride_v as usize,
    );
    if sy < width
        || su < width.div_ceil(2)
        || sv < width.div_ceil(2)
        || buffer.data_y.len() < sy * height
        || buffer.data_u.len() < su * height.div_ceil(2)
        || buffer.data_v.len() < sv * height.div_ceil(2)
    {
        return false;
    }
    // Sample a grid across the output; one stale thumbnail/toolbar is not proof.
    (1..8).all(|row| {
        (1..8).all(|column| {
            let (x, y) = (width * column / 8, height * row / 8);
            let yy = buffer.data_y[y * sy + x];
            let u = buffer.data_u[(y / 2) * su + x / 2];
            let v = buffer.data_v[(y / 2) * sv + x / 2];
            match color {
                Color::Red => (50..125).contains(&yy) && u < 135 && v > 175,
                Color::Green => (105..200).contains(&yy) && u < 120 && v < 125,
                Color::Yellow => yy > 150 && u < 90 && (110..175).contains(&v),
                Color::Magenta => (75..165).contains(&yy) && u > 160 && v > 165,
            }
        })
    })
}

fn exact_sender(
    rtc: &RtcService,
    client: &Client,
    room_id: &str,
    user_id: &str,
    expected: Option<(&str, &str)>,
) -> Option<(String, String)> {
    let members = rtc.participants(room_id).expect("Cannot read peer roster");
    let candidates: Vec<_> = members
        .iter()
        .filter(|member| member.user_id == user_id && !member.is_local)
        .collect();
    assert!(candidates.len() <= 1, "Ambiguous installed-app participant");
    let member = candidates.first()?;
    let device = member.device_id.as_deref()?;
    if let Some((membership, prior_device)) = expected {
        assert!(
            member.member_id == membership && device == prior_device,
            "Installed app participant changed"
        );
    }
    let room_id = matrix_sdk::ruma::RoomId::parse(room_id).expect("Invalid peer room ID");
    let room = client.get_room(&room_id).expect("Peer room vanished");
    let live = room.sticky_events().live();
    let matches = live
        .iter()
        .filter(|event| {
            event.key.sticky_key == member.member_id
                && event.key.sender.as_str() == user_id
                && matches!(
                    event.key.event_type.to_string().as_str(),
                    "m.rtc.member" | "org.matrix.msc4143.rtc.member"
                )
                && event.encryption_info().is_some_and(|encryption| {
                    encryption.sender.as_str() == user_id
                        && encryption.sender_device.as_deref().map(|id| id.as_str()) == Some(device)
                })
        })
        .count();
    assert!(matches <= 1, "Ambiguous encrypted installed-app membership");
    (matches == 1 && member.reachable).then(|| (member.member_id.clone(), device.to_owned()))
}

async fn frame(
    rtc: &RtcService,
    client: &Client,
    room: &str,
    alice: &str,
    member: &str,
    device: &str,
    color: Color,
) {
    timeout(Duration::from_secs(45), async {
        let track = loop {
            assert!(
                exact_sender(rtc, client, room, alice, Some((member, device))).is_some(),
                "Installed app sender disappeared"
            );
            if let Some(track) = rtc
                .call(room)
                .expect("Peer call vanished")
                .remote_track(member, MediaStreamKind::ScreenShare)
            {
                break track;
            }
            sleep(Duration::from_millis(100)).await;
        };
        let mut frames = track
            .video_frames()
            .expect("Screen track has no video frames");
        while let Some(frame) = frames.next().await {
            assert!(
                exact_sender(rtc, client, room, alice, Some((member, device))).is_some(),
                "Installed app sender changed"
            );
            if color_matches(&frame.buffer, color) {
                return;
            }
        }
        panic!("Installed screen track ended before matching pixels");
    })
    .await
    .expect("Installed screen pixels were not delivered");
}

async fn stopped(
    rtc: &RtcService,
    client: &Client,
    room: &str,
    alice: &str,
    member: &str,
    device: &str,
) {
    timeout(Duration::from_secs(20), async {
        loop {
            assert!(
                exact_sender(rtc, client, room, alice, Some((member, device))).is_some(),
                "Call ended instead of screen share"
            );
            if rtc
                .call(room)
                .expect("Peer call vanished")
                .remote_track(member, MediaStreamKind::ScreenShare)
                .is_none()
            {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("Installed screen track did not stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "ordinary installed AppImage, disposable accounts, Sway and portal required"]
async fn receives_actual_installed_appimage_screen() {
    let root = PathBuf::from(
        std::env::var_os("MORPHEUS_APPIMAGE_PEER_CONTROL").expect("Missing private peer control"),
    );
    let control: Control = serde_json::from_slice(&private_bytes(&root.join("control.json"), 4096))
        .expect("Invalid peer control");
    let fixture: Fixture = serde_json::from_slice(&private_bytes(&control.fixture, 65536))
        .expect("Invalid peer fixture");
    let bob = fixture
        .accounts
        .iter()
        .find(|item| item.label == "a_second_device")
        .expect("Missing receiving peer");
    let alice = fixture
        .accounts
        .iter()
        .find(|item| item.label == "a_primary")
        .expect("Missing installed-app account");
    let room = fixture
        .rooms
        .get("encrypted_room")
        .expect("Missing encrypted fixture room");
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(audio_devices());
    });
    let catalog = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("Audio catalog did not finish")
        .expect("Audio catalog unavailable");
    assert!(
        !catalog
            .microphones
            .iter()
            .any(|item| item.id == control.microphone)
            && !catalog
                .speakers
                .iter()
                .any(|item| item.id == control.speaker),
        "Fixture audio IDs unexpectedly exist"
    );
    fact(&root, "unavailable_audio_verified");
    let manager = AccountManager::open(root.join("peer-store")).expect("Cannot open peer store");
    let account = manager
        .add_password_account(&bob.homeserver_url, &bob.username, &bob.password)
        .await
        .expect("Peer login failed");
    ready(&manager, room).await;
    let client = manager.active_client().await.expect("Peer client missing");
    bootstrap_test_identity(&client, bob).await;
    let mut rtc = RtcService::new(client.clone());
    rtc.open_slot(room, "m.call#room", true)
        .await
        .expect("Cannot open fixture slot");
    rtc.join(room, CallOptions::default())
        .await
        .expect("Peer join failed");
    fact(&root, "peer_ready");
    let work = timeout(
        Duration::from_secs(420),
        std::panic::AssertUnwindSafe(async {
            let (member, device) = timeout(Duration::from_secs(90), async {
                loop {
                    if let Some(identity) = exact_sender(&rtc, &client, room, &alice.user_id, None)
                    {
                        break identity;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("Installed app did not join with encrypted membership");
            fact(&root, "exact_sender_joined");
            command(&root, "static").await;
            frame(
                &rtc,
                &client,
                room,
                &alice.user_id,
                &member,
                &device,
                Color::Red,
            )
            .await;
            fact(&root, "static_frame");
            command(&root, "changed").await;
            frame(
                &rtc,
                &client,
                room,
                &alice.user_id,
                &member,
                &device,
                Color::Green,
            )
            .await;
            fact(&root, "changed_frame");
            command(&root, "stopped").await;
            stopped(&rtc, &client, room, &alice.user_id, &member, &device).await;
            fact(&root, "screen_stopped");
            command(&root, "reopened").await;
            frame(
                &rtc,
                &client,
                room,
                &alice.user_id,
                &member,
                &device,
                Color::Yellow,
            )
            .await;
            fact(&root, "reopened_frame");
            command(&root, "changed_again").await;
            frame(
                &rtc,
                &client,
                room,
                &alice.user_id,
                &member,
                &device,
                Color::Magenta,
            )
            .await;
            fact(&root, "changed_after_reopen");
            command(&root, "source_lost").await;
            stopped(&rtc, &client, room, &alice.user_id, &member, &device).await;
            fact(&root, "source_loss_stopped_track");
            command(&root, "left").await;
            timeout(Duration::from_secs(25), async {
                while rtc
                    .participants(room)
                    .expect("Cannot read final roster")
                    .iter()
                    .any(|item| item.member_id == member)
                {
                    sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("Installed app did not leave the call");
            fact(&root, "app_left");
        })
        .catch_unwind(),
    )
    .await;
    let leave = timeout(Duration::from_secs(15), rtc.leave(room)).await;
    let logout = timeout(Duration::from_secs(15), manager.logout_account(&account.id)).await;
    let stop = timeout(Duration::from_secs(10), manager.stop()).await;
    assert!(
        leave.is_ok_and(|value| value.is_ok())
            && logout.is_ok_and(|value| value.is_ok_and(|result| result.remote_logout_succeeded))
            && stop.is_ok(),
        "Peer cleanup failed"
    );
    assert!(
        work.is_ok_and(|value| value.is_ok()),
        "Installed capture phase deadline exceeded"
    );
    fact(&root, "cleanup");
    fact(&root, "complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(y: u8, u: u8, v: u8) -> I420Buffer {
        I420Buffer {
            width: 128,
            height: 72,
            stride_y: 128,
            stride_u: 64,
            stride_v: 64,
            data_y: vec![y; 128 * 72],
            data_u: vec![u; 64 * 36],
            data_v: vec![v; 64 * 36],
        }
    }

    #[test]
    fn colors_require_the_new_source_not_a_stale_previous_frame() {
        let frames = [
            (solid(93, 97, 217), Color::Red),
            (solid(151, 66, 51), Color::Green),
            (solid(207, 32, 144), Color::Yellow),
            (solid(114, 187, 202), Color::Magenta),
        ];
        for (index, (frame, color)) in frames.iter().enumerate() {
            assert!(color_matches(frame, *color));
            for (other, (_, color)) in frames.iter().enumerate() {
                if index != other {
                    assert!(!color_matches(frame, *color));
                }
            }
        }
    }

    #[test]
    fn empty_or_truncated_planes_and_tiny_thumbnail_do_not_pass() {
        let mut frame = solid(93, 97, 217);
        frame.data_v.truncate(1);
        assert!(!color_matches(&frame, Color::Red));
        frame = solid(93, 97, 217);
        frame.width = 1;
        assert!(!color_matches(&frame, Color::Red));
        assert!(!color_matches(&solid(16, 128, 128), Color::Red));
    }
}
