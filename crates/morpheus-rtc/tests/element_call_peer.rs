//! Opt-in MatrixRTC interop check with a real Element Call browser peer.
//! `scripts/element_call_stock_peer.mjs` controls the stock other side. No public service
//! or hardware media device is involved.

#![cfg(feature = "native-rtc")]

use std::{
    fs,
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use matrix_sdk::{Client, ruma::api::client::uiaa};
use morpheus_matrix::{AccountManager, SyncBackend, SyncStatus};
use morpheus_rtc::{
    AudioFrame, CallOptions, I420Buffer, InteropMode, MediaStreamKind, Publication, RtcService,
    VideoFrame, VideoRotation,
};
use serde::Deserialize;
use tokio::time::{sleep, timeout};

const ELEMENT_ROOM_SLOT_ID: &str = "m.call#ROOM";
const NATIVE_ROOM_SLOT_ID: &str = "m.call#room";

// A fresh Sticky fixture must use only the ORIGINAL upper wire slot. A
// successful own-HS write alone cannot rule out a concealed lower-slot preseed.
async fn original_sticky_slot_without_lower(
    client: &Client,
    room_id: &matrix_sdk::ruma::RoomId,
    admit: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(), ()> {
    use matrix_sdk::{
        config::RequestConfig,
        ruma::{
            api::{client::state::get_state_event_for_key, error::ErrorKind},
            events::StateEventType,
        },
    };
    let room = client.get_room(room_id).ok_or(())?;
    for event_type in ["m.rtc.slot", "org.matrix.msc4143.rtc.slot"] {
        if !admit() || room.state() != matrix_sdk::RoomState::Joined {
            return Err(());
        }
        let response = client
            .send(get_state_event_for_key::v3::Request::new(
                room_id.to_owned(),
                StateEventType::from(event_type),
                "m.call#room".into(),
            ))
            .with_request_config(
                RequestConfig::new()
                    .retry_limit(0)
                    .timeout(Duration::from_secs(5))
                    .max_response_bytes(64 * 1024),
            )
            .await;
        if !admit() || room.state() != matrix_sdk::RoomState::Joined {
            return Err(());
        }
        match response {
            Err(error)
                if error.as_client_api_error().is_some_and(|answer| {
                    answer.status_code.as_u16() == 404
                        && answer.error_kind() == Some(&ErrorKind::NotFound)
                }) => {}
            _ => return Err(()),
        }
    }
    // The pinned SDK translates its stable slot write to this unstable type.
    let response = client
        .send(get_state_event_for_key::v3::Request::new(
            room_id.to_owned(),
            StateEventType::from("org.matrix.msc4143.rtc.slot"),
            "m.call#ROOM".into(),
        ))
        .with_request_config(
            RequestConfig::new()
                .retry_limit(0)
                .timeout(Duration::from_secs(5))
                .max_response_bytes(64 * 1024),
        )
        .await
        .map_err(|_| ())?;
    if !admit() || room.state() != matrix_sdk::RoomState::Joined {
        return Err(());
    }
    let content: serde_json::Value =
        serde_json::from_str(response.event_or_content.get()).map_err(|_| ())?;
    if content.get("status").and_then(serde_json::Value::as_str) != Some("open")
        || content
            .pointer("/application/type")
            .and_then(serde_json::Value::as_str)
            != Some("m.call")
        || !matches!(
            content
                .pointer("/encryption/type")
                .and_then(serde_json::Value::as_str),
            Some("m.per_member" | "org.matrix.msc4143.per_member")
        )
    {
        return Err(());
    }
    Ok(())
}

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

// The browser's real fake-capture microphone is 440 Hz. Only the native
// microphone publication below supplies this distinct, phase-continuous tone.
fn native_audio_frame(index: u32, frequency: f64) -> AudioFrame {
    AudioFrame {
        data: (0..480)
            .map(|sample| {
                let position = u64::from(index) * 480 + sample;
                let phase = std::f64::consts::TAU * frequency * position as f64 / 48_000.0;
                (8_000.0 * phase.sin()) as i16
            })
            .collect(),
        sample_rate: 48_000,
        num_channels: 1,
        samples_per_channel: 480,
    }
}

// Own the task even if an existing roster/media assertion panics. Aborting
// drops Publication (which mutes synchronously); process/namespace cleanup is
// the outer failure boundary. A successful test also awaits and unpublishes.
struct NativeTone {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    sender: tokio::task::JoinHandle<Publication>,
    changed: Arc<AtomicBool>,
}

impl NativeTone {
    fn start(publication: Publication) -> Self {
        Self::with_frequency(publication, 997.0)
    }

    fn with_frequency(publication: Publication, frequency: f64) -> Self {
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let changed = Arc::new(AtomicBool::new(false));
        let phase = changed.clone();
        let sender = tokio::spawn(async move {
            for index in 0..36_000 {
                let frequency = if phase.load(Ordering::Acquire) {
                    1301.0
                } else {
                    frequency
                };
                tokio::select! {
                    _ = &mut stopped => return publication,
                    result = publication.push_audio(native_audio_frame(index, frequency)) => {
                        result.expect("Native interop microphone send failed");
                    },
                }
                tokio::select! {
                    _ = &mut stopped => return publication,
                    _ = sleep(Duration::from_millis(10)) => {},
                }
            }
            publication
        });
        Self {
            stop: Some(stop),
            sender,
            changed,
        }
    }

    async fn finish(mut self) {
        self.stop
            .take()
            .expect("Native interop microphone stop missing")
            .send(())
            .expect("Native interop microphone stopped early");
        let publication = timeout(Duration::from_secs(5), &mut self.sender)
            .await
            .expect("Native interop microphone stop timed out")
            .expect("Native interop microphone sender panicked");
        timeout(Duration::from_secs(5), publication.unpublish())
            .await
            .expect("Native interop microphone unpublish timed out")
            .expect("Cannot unpublish native interop microphone");
    }
}

impl Drop for NativeTone {
    fn drop(&mut self) {
        self.sender.abort();
    }
}

// A real ScreenShare publication with a picture that changes only after the
// stock receiver acknowledges a new exact-sender key notification. Neither the
// camera fixture nor a queued old screen picture can satisfy the second phase.
struct NativeScreen {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    sender: tokio::task::JoinHandle<Publication>,
    changed: Arc<AtomicBool>,
}

impl NativeScreen {
    fn start(publication: Publication) -> Self {
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let changed = Arc::new(AtomicBool::new(false));
        let phase = changed.clone();
        let sender = tokio::spawn(async move {
            for index in 0..3_600 {
                let mut frame = native_video_frame(index);
                let changed = phase.load(Ordering::Acquire);
                for (row, pixels) in frame.buffer.data_y.chunks_exact_mut(640).enumerate() {
                    let white = (row < 180) != changed;
                    pixels.fill(if white { 235 } else { 16 });
                }
                publication
                    .push_video(frame)
                    .expect("Native screen send failed");
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = sleep(Duration::from_millis(66)) => {},
                }
            }
            publication
        });
        Self {
            stop: Some(stop),
            sender,
            changed,
        }
    }

    async fn finish(mut self) {
        self.stop
            .take()
            .expect("Screen stop missing")
            .send(())
            .expect("Screen stopped early");
        let publication = timeout(Duration::from_secs(5), &mut self.sender)
            .await
            .expect("Screen stop timed out")
            .expect("Screen sender panicked");
        timeout(Duration::from_secs(5), publication.unpublish())
            .await
            .expect("Screen unpublish timed out")
            .expect("Screen unpublish failed");
    }
}

impl Drop for NativeScreen {
    fn drop(&mut self) {
        self.sender.abort();
    }
}

fn write_owner(root: &std::path::Path, name: &str, publication: &Publication, sid: Option<&str>) {
    let call = publication.call_info();
    let mut owner = serde_json::json!({
        "room_id": call.room_id,
        "user_id": publication.account_user_id(),
        "device_id": publication.account_device_id(),
        "member_id": call.membership_id,
        "identity": call.local_identity,
    });
    if let Some(sid) = sid {
        owner["track_sid"] = sid.into();
    }
    write_marker(
        root.join(name),
        &serde_json::to_vec(&owner).expect("Cannot encode interop media owner"),
    );
}

// Read only the enabled cryptor for our unique actual local media publication.
// This is the native sender's index, not the provider's optimistic latest-key
// atomic or a remote participant's imported-key event. No key bytes are read.
fn native_sender(
    rtc: &RtcService,
    room: &str,
    source: livekit::track::TrackSource,
    sid: Option<&str>,
) -> Option<(String, u8)> {
    let call = rtc.call(room)?;
    let room = call.session().room();
    let local = room.local_participant();
    let identity = local.identity();
    if identity.as_str() != call.local_identity() || !room.e2ee_manager().enabled() {
        return None;
    }
    let mut publications = local
        .track_publications()
        .into_iter()
        .filter(|(_, publication)| publication.source() == source);
    let (track_sid, publication) = publications.next()?;
    if publications.next().is_some()
        || publication.sid() != track_sid
        || sid.is_some_and(|expected| expected != track_sid.as_str())
    {
        return None;
    }
    let cryptors = room.e2ee_manager().frame_cryptors();
    let cryptor = cryptors.get(&(identity.clone(), track_sid.clone()))?;
    if !cryptor.enabled() || cryptor.participant_id() != identity.as_str() {
        return None;
    }
    let index = u8::try_from(cryptor.key_index()).ok()?;
    Some((track_sid.to_string(), index))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceivedKey {
    key_index: u8,
}

fn received_key(path: &std::path::Path) -> u8 {
    let metadata = fs::symlink_metadata(path).expect("Key receipt metadata missing");
    assert!(
        metadata.file_type().is_file() && metadata.len() <= 256,
        "Invalid private key receipt"
    );
    let file = fs::File::open(path).expect("Cannot open private key receipt");
    let metadata = file.metadata().expect("Cannot inspect private key receipt");
    assert!(
        metadata.is_file() && metadata.len() <= 256,
        "Invalid opened key receipt"
    );
    let mut bytes = Vec::new();
    file.take(257)
        .read_to_end(&mut bytes)
        .expect("Cannot read private key receipt");
    assert!(bytes.len() <= 256, "Key receipt exceeded bound");
    serde_json::from_slice::<ReceivedKey>(&bytes)
        .expect("Invalid key receipt shape")
        .key_index
}

fn exact_roster(rtc: &RtcService, room: &str, users: &[&str]) -> bool {
    let mut actual = rtc
        .participants(room)
        .expect("Group roster missing")
        .into_iter()
        .map(|member| member.user_id)
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = users
        .iter()
        .map(|user| (*user).to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    actual == expected
}

// A fixed-label view of the same in-memory roster used by exact_roster.
// This runs only after the unchanged group deadline has already failed.
fn roster_discriminator(rtc: &RtcService, room: &str, users: [&str; 3]) -> &'static str {
    let Ok(members) = rtc.participants(room) else {
        return "unavailable";
    };
    if members.len() > 64 {
        return "unavailable";
    }
    let mut counts = [0_usize; 3];
    for member in members {
        let Some(index) = users.iter().position(|user| member.user_id.as_str() == *user) else {
            return "unexpected_or_duplicate";
        };
        counts[index] += 1;
        if counts[index] > 1 {
            return "unexpected_or_duplicate";
        }
    }
    let missing = counts.iter().filter(|count| **count == 0).count();
    if missing == 0 {
        "exact"
    } else if missing > 1 {
        "missing_multiple"
    } else if counts[0] == 0 {
        "missing_alice"
    } else if counts[1] == 0 {
        "missing_bob"
    } else {
        "missing_carol"
    }
}

// Only the authenticated, already-decrypted sticky cache is sampled. IDs stay
// in this test process; no GET, decrypt, key request or sender refresh is added.
fn cached_bob_memberships(
    rtc: &RtcService,
    room: &str,
    bob: &str,
) -> Option<Vec<matrix_sdk::ruma::OwnedEventId>> {
    use matrix_sdk::ruma::events::TimelineEventType;
    let room_id = matrix_sdk::ruma::RoomId::parse(room).ok()?;
    let joined = rtc.client().get_room(&room_id)?;
    if joined.state() != matrix_sdk::RoomState::Joined {
        return None;
    }
    let live = joined.sticky_events().live();
    if live.len() > 64 {
        return None;
    }
    let types = [
        TimelineEventType::from("m.rtc.member"),
        TimelineEventType::from("org.matrix.msc4143.rtc.member"),
    ];
    let mut events = Vec::new();
    for event in live
        .iter()
        .filter(|event| {
            event.key.sender.as_str() == bob
                && types.contains(&event.key.event_type)
                && event
                    .encryption_info()
                    .is_some_and(|info| info.sender.as_str() == bob)
        })
        .take(2)
    {
        if event.event_id.as_str().len() > 1024 {
            return None;
        }
        events.push(event.event_id.clone());
    }
    Some(events)
}

fn group_failure_discriminator(
    rtc: &RtcService,
    late: &RtcService,
    room: &str,
    users: [&str; 3],
    before: Option<&[matrix_sdk::ruma::OwnedEventId]>,
) {
    // Sample BOTH sides even when the unchanged predicate short-circuited.
    let original_roster = roster_discriminator(rtc, room, users);
    let late_roster = roster_discriminator(late, room, users);
    let current = cached_bob_memberships(rtc, room, users[1]);
    let received = cached_bob_memberships(late, room, users[1]);
    // Event-ID continuity only: no assertion about ciphertext or key continuity.
    let continuity_detail = match (before, current.as_deref()) {
        (Some([before]), Some([after])) if before == after => "same_live_event",
        (Some([_]), Some([_])) => "changed_live_event",
        (Some(_), Some(_)) => "not_unique",
        _ => "unknown",
    };
    let late_current_bob = match (current.as_deref(), received.as_deref()) {
        (Some([current]), Some([received])) if current == received => "present",
        (Some([_]), Some(received)) if received.len() > 1 => "ambiguous",
        (Some([_]), Some(_)) => "missing",
        _ => "unknown",
    };
    eprintln!(
        "MORPHEUS_ELEMENT_GROUP original_roster={original_roster} late_roster={late_roster} continuity_detail={continuity_detail} late_current_bob={late_current_bob}"
    );
}

async fn group_and_screen(
    rtc: &mut RtcService,
    root: &std::path::Path,
    alice: &TestAccount,
    bob: &TestAccount,
    carol: &TestAccount,
    room: &str,
    original_microphone: &NativeTone,
) {
    eprintln!("MORPHEUS_ELEMENT_NATIVE_STAGE microphone_cryptor");
    let (microphone_sid, _) =
        native_sender(rtc, room, livekit::track::TrackSource::Microphone, None)
            .expect("Actual original microphone sender cryptor missing");
    write_marker(
        root.join("native-microphone-sid"),
        microphone_sid.as_bytes(),
    );
    // The unmodified pinned core keeps keys fresh for ten seconds. Make the
    // late arrival occur outside that window rather than asking a test-only
    // API to rotate keys or assuming a fast bulk join has to rotate them.
    sleep(Duration::from_secs(11)).await;
    let bob_before = cached_bob_memberships(rtc, room, &bob.user_id);
    let profile = root.join("late-native-profile");
    fs::create_dir(&profile).expect("Cannot create fresh late-peer profile");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700))
            .expect("Cannot protect late-peer profile");
    }
    let manager = AccountManager::open(profile.join("carol")).expect("Cannot open late-peer store");
    eprintln!("MORPHEUS_ELEMENT_NATIVE_STAGE late_login");
    let account = manager
        .add_password_account(&carol.homeserver_url, &carol.username, &carol.password)
        .await
        .expect("Late-peer login failed");
    eprintln!("MORPHEUS_ELEMENT_NATIVE_STAGE late_sync");
    timeout(Duration::from_secs(90), async {
        let status = manager.subscribe_sync_status();
        loop {
            let running = matches!(
                &*status.borrow(),
                SyncStatus::Running {
                    backend: SyncBackend::SlidingSticky
                }
            );
            if running
                && manager
                    .rooms_snapshot()
                    .await
                    .expect("Late-peer room list failed")
                    .iter()
                    .any(|value| value.room_id == room)
            {
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Late-peer sync timed out");
    let client = manager
        .active_client()
        .await
        .expect("Late-peer client missing");
    eprintln!("MORPHEUS_ELEMENT_NATIVE_STAGE late_bootstrap");
    bootstrap_identity(&client, carol).await;
    let mut late = RtcService::new(client);
    eprintln!("MORPHEUS_ELEMENT_NATIVE_STAGE late_join");
    late.join(room, CallOptions::element_call_matrix_2_0())
        .await
        .expect("Late-peer join failed");
    eprintln!("MORPHEUS_ELEMENT_NATIVE_STAGE group_roster");
    let roster = timeout(Duration::from_secs(90), async {
        while !exact_roster(rtc, room, &[&alice.user_id, &bob.user_id, &carol.user_id])
            || !exact_roster(&late, room, &[&alice.user_id, &bob.user_id, &carol.user_id])
        {
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    if roster.is_err() {
        group_failure_discriminator(
            rtc,
            &late,
            room,
            [
                alice.user_id.as_str(),
                bob.user_id.as_str(),
                carol.user_id.as_str(),
            ],
            bob_before.as_deref(),
        );
    }
    roster.expect("Three-member native rosters did not converge");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_group_roster=true");

    // A newly created device must actually decrypt the already-running sender,
    // not just appear in the signalling roster or receive RTP counters.
    timeout(Duration::from_secs(45), async {
        let member = late
            .participants(room)
            .expect("Late roster missing")
            .into_iter()
            .find(|member| member.user_id == alice.user_id)
            .expect("Alice not in late roster");
        let call = late.call(room).expect("Late call missing");
        let audio = loop {
            if let Some(track) = call.remote_track(&member.member_id, MediaStreamKind::Microphone) {
                break track;
            }
            sleep(Duration::from_millis(200)).await;
        };
        let video = loop {
            if let Some(track) = call.remote_track(&member.member_id, MediaStreamKind::Camera) {
                break track;
            }
            sleep(Duration::from_millis(200)).await;
        };
        let mut audio = audio.audio_frames().expect("Late-peer PCM missing");
        let mut video = video.video_frames().expect("Late-peer picture missing");
        let sound = async {
            while let Some(frame) = audio.next().await {
                if frame.data.iter().any(|sample| sample.unsigned_abs() > 100) {
                    return true;
                }
            }
            false
        };
        let picture = async {
            while let Some(frame) = video.next().await {
                let b = frame.buffer;
                let width = b.width as usize;
                let height = b.height as usize;
                let stride = b.stride_y as usize;
                if width >= 160
                    && height >= 90
                    && stride >= width
                    && b.data_y.len() >= stride * height
                    && b.data_y[(height / 2) * stride + width / 4] > 190
                    && b.data_y[(height / 2) * stride + width * 3 / 4] < 55
                {
                    return true;
                }
            }
            false
        };
        let (sound, picture) = tokio::join!(sound, picture);
        assert!(sound && picture, "Late peer did not decrypt Alice media");
    })
    .await
    .expect("Late-peer decoded media timed out");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_late_join_media=true");
    let late_microphone = late
        .publish_microphone(room, 48_000, 1, false)
        .await
        .expect("Late microphone failed");
    assert_eq!(late_microphone.account_user_id(), carol.user_id);
    write_owner(root, "group-audio-owner.json", &late_microphone, None);
    let tone = NativeTone::with_frequency(late_microphone, 1700.0);
    let screen = rtc
        .publish_screen(room, 640, 360, false)
        .await
        .expect("Native screen publish failed");
    assert_eq!(screen.account_user_id(), alice.user_id);
    let (screen_sid, _) = timeout(Duration::from_secs(45), async {
        loop {
            if let Some(value) =
                native_sender(rtc, room, livekit::track::TrackSource::Screenshare, None)
            {
                break value;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Actual screen sender cryptor missing");
    write_owner(root, "screen-owner.json", &screen, Some(&screen_sid));
    let screen = NativeScreen::start(screen);
    write_marker(root.join("group-ready"), b"ready\n");
    wait_for_marker(&root.join("group-observed.json")).await;
    let before_leave = received_key(&root.join("group-observed.json"));
    timeout(Duration::from_secs(45), async {
        while native_sender(
            rtc,
            room,
            livekit::track::TrackSource::Screenshare,
            Some(&screen_sid),
        )
        .map(|(_, index)| index)
            != Some(before_leave)
        {
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Screen sender did not adopt the arrival key");
    tone.finish().await;
    late.leave(room).await.expect("Late-peer leave failed");
    drop(late);
    manager
        .logout_account(&account.id)
        .await
        .expect("Late-peer logout failed");
    manager.stop().await;
    drop(manager);
    fs::remove_dir_all(profile).expect("Cannot remove late-peer store");
    timeout(Duration::from_secs(45), async {
        while !exact_roster(rtc, room, &[&alice.user_id, &bob.user_id]) {
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Late-peer departure missing from native roster");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_group_departure=true");
    wait_for_marker(&root.join("new-key-observed.json")).await;
    let after_leave = received_key(&root.join("new-key-observed.json"));
    assert_ne!(
        after_leave, before_leave,
        "Departure did not produce a new sender index"
    );
    timeout(Duration::from_secs(45), async {
        while native_sender(
            rtc,
            room,
            livekit::track::TrackSource::Screenshare,
            Some(&screen_sid),
        )
        .map(|(_, index)| index)
            != Some(after_leave)
            || native_sender(
                rtc,
                room,
                livekit::track::TrackSource::Microphone,
                Some(&microphone_sid),
            )
            .map(|(_, index)| index)
                != Some(after_leave)
        {
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("Actual same media senders did not adopt the departure key");
    eprintln!("MORPHEUS_ELEMENT_CHECK native_key_rotation=true");
    original_microphone.changed.store(true, Ordering::Release);
    screen.changed.store(true, Ordering::Release);
    write_marker(root.join("screen-changed"), b"changed\n");
    wait_for_marker(&root.join("screen-change-observed")).await;
    screen.finish().await;
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
    let slot_selection = manager
        .account_selection(&alice_account.id)
        .expect("Original Alice slot selection missing");
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
            // Keep the internal id lower, but write ONLY the original stock
            // upper slot. No Current preseed may conceal the ingest boundary.
            let original_selection = slot_selection.clone();
            let admit: Arc<dyn Fn() -> bool + Send + Sync> =
                Arc::new(move || original_selection.is_current());
            rtc.open_outgoing_slot(
                room_id,
                NATIVE_ROOM_SLOT_ID,
                true,
                interop_mode,
                Arc::clone(&admit),
            )
            .await
            .expect("Cannot open original Sticky encrypted slot");
            let original_room =
                matrix_sdk::ruma::RoomId::parse(room_id).expect("Invalid owned fixture room");
            original_sticky_slot_without_lower(rtc.client(), &original_room, admit.as_ref())
                .await
                .expect("Original Sticky slot or no-lower proof failed");
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
    let outgoing_audio = if let Some(root) = &stock_root {
        let publication = rtc
            .publish_microphone(room_id, 48_000, 1, false)
            .await
            .expect("Cannot publish native interop microphone");
        assert_eq!(publication.call_info(), &call);
        assert_eq!(publication.account_user_id(), alice.user_id);
        // Private, exclusive marker from the actual publication's lease. The
        // browser must match this exact join/device/backend identity, not a
        // display name, DOM tag, or another device of the same Matrix user.
        write_marker(
            root.join("native-audio-owner.json"),
            &serde_json::to_vec(&serde_json::json!({
                "room_id": call.room_id,
                "user_id": publication.account_user_id(),
                "device_id": publication.account_device_id(),
                "member_id": call.membership_id,
                "identity": call.local_identity,
            }))
            .expect("Cannot encode native microphone owner"),
        );
        Some(NativeTone::start(publication))
    } else {
        None
    };
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
        let carol = fixture
            .accounts
            .iter()
            .find(|account| account.label == "a_third_device")
            .expect("Late group peer fixture missing");
        group_and_screen(
            &mut rtc,
            root,
            alice,
            bob,
            carol,
            room_id,
            outgoing_audio
                .as_ref()
                .expect("Original native microphone missing"),
        )
        .await;
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
    if let Some(tone) = outgoing_audio {
        tone.finish().await;
    }
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
