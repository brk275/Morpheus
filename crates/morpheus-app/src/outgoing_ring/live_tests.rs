//! Hosted, opt-in invitation acceptance. Bob must discover an authenticated
//! notification from his sync stream before receiving the sender's event ID.
//! Separate GET and redaction assertions still check the server receipt. This
//! also exercises actual incoming actor/Answer with unavailable audio devices.
//! This does not establish incoming call UI, audible ringing, physical capture,
//! or Element receipt.
//! Each invocation requires a fresh private loopback RTC fixture and two fresh
//! AccountManager stores. Selected fixture audio IDs are verified absent;
//! native audio failure leaves the accepted call in its receive-only mode.

use std::{collections::HashMap, fs, future::Future, panic::AssertUnwindSafe, path::PathBuf};

use futures_util::FutureExt;
use matrix_sdk::{
    Client,
    deserialized_responses::TimelineEvent,
    ruma::{
        EventId, MilliSecondsSinceUnixEpoch,
        api::client::{room::get_room_event, uiaa},
    },
};
use morpheus_matrix::{AccountManager, SyncBackend, SyncStatus};
use morpheus_rtc::{
    CallOptions, ObserverError, PassiveCallObserver, RtcStreamKind,
    ringing::{
        CallParticipation, ResolvedJoinedCall, RingContext, RingNotification, RoomMentionPermission,
    },
};
use serde::Deserialize;

use crate::incoming_ring::{AdmissionFailure, IncomingOffer, ledger::OfferLedger};
use crate::voice_spaces::VoiceSpaces;

use super::*;

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

mod actor_tests;
#[cfg(target_os = "linux")]
mod alsa_playback;
#[cfg(target_os = "linux")]
mod outage_tests;
#[cfg(all(target_os = "linux", morpheus_timeline_acceptance))]
pub(crate) use outage_tests::run_received_video_ui_if_configured;
use actor_tests::ActorAcceptance;
use std::sync::Arc;

// Leave at least 107 seconds for retained-join/account cleanup inside the
// unchanged 300-second contained command. Original invitation windows stay 30s.
const WORK_LIMIT: Duration = Duration::from_secs(180);
const PHASE_LIMIT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy)]
enum Stage {
    Fixture,
    Login,
    StickySync,
    OwnCrypto,
    PeerTrust,
    Targets,
    Join,
    Membership,
    ObserverOpen,
    ObserverRoster,
    ActorSetup,
    ObserverLeave,
    Send,
    Receive,
    Fetch,
    Validate,
    Admission,
    Revalidate,
    Redact,
    Redaction,
    ReceiptRedaction,
    Withdraw,
    Cleanup,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Fixture => "fixture",
            Self::Login => "login",
            Self::StickySync => "sticky_sync",
            Self::OwnCrypto => "own_crypto",
            Self::PeerTrust => "peer_trust",
            Self::Targets => "targets",
            Self::Join => "join",
            Self::Membership => "membership",
            Self::ObserverOpen => "observer_open",
            Self::ObserverRoster => "observer_roster",
            Self::ActorSetup => "actor_setup",
            Self::ObserverLeave => "observer_leave",
            Self::Send => "send",
            Self::Receive => "receive",
            Self::Fetch => "fetch",
            Self::Validate => "validate",
            Self::Admission => "admission",
            Self::Revalidate => "revalidate",
            Self::Redact => "redact",
            Self::Redaction => "redaction",
            Self::ReceiptRedaction => "receipt_redaction",
            Self::Withdraw => "withdraw",
            Self::Cleanup => "cleanup",
        }
    }
}

fn phase(current: &mut Stage, next: Stage) {
    *current = next;
    eprintln!("MORPHEUS_RING_STAGE {}", next.name());
}

fn fact(name: &'static str) {
    eprintln!("MORPHEUS_RING_CHECK {name}=true");
}

fn require(condition: bool) -> Result<(), ()> {
    if condition { Ok(()) } else { Err(()) }
}

/// Only unexpected errors on paths that must succeed use this diagnostic.
/// Expected replay/withdrawal rejections remain silent and keep their checks.
fn admission_failure(error: AdmissionFailure) {
    let result = match error {
        AdmissionFailure::Account => "account",
        AdmissionFailure::Sync => "sync",
        AdmissionFailure::Receipt => "receipt",
        AdmissionFailure::Replay => "replay",
        AdmissionFailure::Expired => "expired",
        AdmissionFailure::Membership => "membership",
        AdmissionFailure::Trust => "trust",
        AdmissionFailure::Policy => "policy",
        AdmissionFailure::Quiet => "quiet",
        AdmissionFailure::Transport => "transport",
        AdmissionFailure::Unavailable => "unavailable",
        AdmissionFailure::TimedOut => "timed_out",
        AdmissionFailure::Revoked => "revoked",
    };
    eprintln!("\nMORPHEUS_RING_ADMISSION result={result}");
}

/// Retain only the last fixed observation. Cancellation or a failure emits it;
/// successful operations disarm it. No SDK error text is stored or printed.
struct ObserverDiagnostic(Option<&'static str>);

impl ObserverDiagnostic {
    fn new() -> Self {
        Self(Some("not_sampled"))
    }

    fn record(&mut self, result: &'static str) {
        self.0 = Some(result);
    }

    fn error(&mut self, error: ObserverError) {
        self.record(match error {
            ObserverError::UnsupportedDialect => "unsupported_dialect",
            ObserverError::InvalidRoom => "invalid_room",
            ObserverError::NotJoined => "not_joined",
            ObserverError::UnsupportedServer => "unsupported_server",
            ObserverError::NotEncrypted => "not_encrypted",
            ObserverError::SlotUnavailable => "slot_unavailable",
            ObserverError::Unavailable => "unavailable",
            ObserverError::TimedOut => "timed_out",
            ObserverError::Closed => "closed",
            ObserverError::InvalidMembership => "invalid_membership",
            ObserverError::RosterNotCurrent => "roster_not_current",
            ObserverError::TooManyParticipants => "too_many_participants",
        });
    }

    fn complete(&mut self) {
        self.0 = None;
    }
}

impl Drop for ObserverDiagnostic {
    fn drop(&mut self) {
        if let Some(result) = self.0 {
            eprintln!("\nMORPHEUS_RING_OBSERVER result={result}");
        }
    }
}

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<FixtureAccount>,
    rooms: HashMap<String, String>,
}

#[derive(Deserialize)]
struct FixtureAccount {
    label: String,
    username: String,
    user_id: String,
    homeserver_url: String,
    password: String,
}

struct Case {
    fixture: Fixture,
    profile: tempfile::TempDir,
    mode: InteropMode,
    dm: bool,
    room_id: OwnedRoomId,
}

impl Case {
    fn read() -> Result<Self, ()> {
        let path = PathBuf::from(std::env::var_os("MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS").ok_or(())?);
        let metadata = fs::symlink_metadata(&path).map_err(|_| ())?;
        require(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() < 1_048_576,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            require(metadata.permissions().mode() & 0o077 == 0)?;
        }
        let path = path.canonicalize().map_err(|_| ())?;
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .map_err(|_| ())?;
        require(!path.starts_with(repository))?;
        let fixture: Fixture =
            serde_json::from_slice(&fs::read(&path).map_err(|_| ())?).map_err(|_| ())?;
        require(fixture.accounts.len() == 2)?;
        for (label, prefix) in [("a_primary", "rtcalice"), ("a_second_device", "rtcbob")] {
            let account = fixture
                .accounts
                .iter()
                .find(|account| account.label == label)
                .ok_or(())?;
            require(account.username.starts_with(prefix) && account.password.len() >= 16)?;
            UserId::parse(&account.user_id).map_err(|_| ())?;
            let url = url::Url::parse(&account.homeserver_url).map_err(|_| ())?;
            require(
                matches!(url.scheme(), "http" | "https")
                    && url.username().is_empty()
                    && url.password().is_none(),
            )?;
            require(matches!(
                url.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]")
            ))?;
        }
        let room_id =
            RoomId::parse(fixture.rooms.get("encrypted_room").ok_or(())?).map_err(|_| ())?;
        let mode = match std::env::var("MORPHEUS_RING_TEST_MODE").as_deref() {
            Ok("current") => InteropMode::Current,
            Ok("sticky-2025") => InteropMode::Sticky2025,
            _ => return Err(()),
        };
        let dm = match std::env::var("MORPHEUS_RING_TEST_TARGET").as_deref() {
            Ok("dm") => true,
            Ok("room") => false,
            _ => return Err(()),
        };
        let profile = tempfile::Builder::new()
            .prefix("outgoing-invitation-")
            .tempdir_in(path.parent().ok_or(())?)
            .map_err(|_| ())?;
        Ok(Self {
            fixture,
            profile,
            mode,
            dm,
            room_id,
        })
    }

    fn account(&self, label: &str) -> Result<&FixtureAccount, ()> {
        self.fixture
            .accounts
            .iter()
            .find(|account| account.label == label)
            .ok_or(())
    }
}

struct Session {
    alice: Arc<AccountManager>,
    bob: Arc<AccountManager>,
    actor: Option<ActorAcceptance>,
    rtc: Option<RtcService>,
    observer: Option<PassiveCallObserver>,
    receipt: Option<InvitationReceipt>,
    stage: Stage,
}

impl Session {
    fn open(case: &Case) -> Result<Self, ()> {
        Ok(Self {
            alice: Arc::new(
                AccountManager::open(case.profile.path().join("alice")).map_err(|_| ())?,
            ),
            bob: Arc::new(AccountManager::open(case.profile.path().join("bob")).map_err(|_| ())?),
            actor: None,
            rtc: None,
            observer: None,
            receipt: None,
            stage: Stage::Fixture,
        })
    }

    async fn run(&mut self, case: &Case) -> Result<(), ()> {
        phase(&mut self.stage, Stage::Login);
        let alice_fixture = case.account("a_primary")?;
        let bob_fixture = case.account("a_second_device")?;
        let alice = self
            .alice
            .add_password_account(
                &alice_fixture.homeserver_url,
                &alice_fixture.username,
                &alice_fixture.password,
            )
            .await
            .map_err(|_| ())?;
        let slot_selection = if case.mode == InteropMode::Sticky2025 {
            Some(self.alice.account_selection(&alice.id).ok_or(())?)
        } else {
            None
        };
        let bob = self
            .bob
            .add_password_account(
                &bob_fixture.homeserver_url,
                &bob_fixture.username,
                &bob_fixture.password,
            )
            .await
            .map_err(|_| ())?;
        require(
            alice.user_id == alice_fixture.user_id
                && bob.user_id == bob_fixture.user_id
                && alice.user_id != bob.user_id,
        )?;
        let alice_client = self.alice.active_client().await.map_err(|_| ())?;
        let bob_client = self.bob.active_client().await.map_err(|_| ())?;

        phase(&mut self.stage, Stage::StickySync);
        wait_ready(&self.alice, &alice_client, &case.room_id).await?;
        wait_ready(&self.bob, &bob_client, &case.room_id).await?;
        fact("sticky_sync");
        phase(&mut self.stage, Stage::OwnCrypto);
        bootstrap(&alice_client, alice_fixture).await?;
        bootstrap(&bob_client, bob_fixture).await?;
        fact("own_crypto");
        phase(&mut self.stage, Stage::PeerTrust);
        // Fixture-only manual trust: compare the expected locally bootstrapped
        // master public key before signing the server-fetched peer identity.
        trust_fixture_peer(&bob_client, &alice_client).await?;
        trust_fixture_peer(&alice_client, &bob_client).await?;
        fact("peer_trust");

        let alice_room = alice_client.get_room(&case.room_id).ok_or(())?;
        let bob_room = bob_client.get_room(&case.room_id).ok_or(())?;
        phase(&mut self.stage, Stage::Targets);
        alice_room.set_is_direct(case.dm).await.map_err(|_| ())?;
        time::timeout(PHASE_LIMIT, async {
            loop {
                let is_dm = alice_room.is_direct().await.map_err(|_| ())?;
                let targets = alice_room.direct_targets();
                if is_dm == case.dm
                    && if case.dm {
                        targets.len() == 1
                            && targets.iter().any(|target| target.as_str() == bob.user_id)
                    } else {
                        targets.is_empty()
                    }
                {
                    return Ok::<_, ()>(());
                }
                time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| ())??;

        phase(&mut self.stage, Stage::Join);
        self.rtc = Some(RtcService::new(alice_client.clone()));
        let rtc = self.rtc.as_mut().ok_or(())?;
        if case.mode == InteropMode::Sticky2025 {
            let original_selection = slot_selection.as_ref().ok_or(())?.clone();
            let admit: Arc<dyn Fn() -> bool + Send + Sync> =
                Arc::new(move || original_selection.is_current());
            rtc.open_outgoing_slot(
                case.room_id.as_str(),
                "m.call#room",
                true,
                case.mode,
                Arc::clone(&admit),
            )
            .await
            .map_err(|_| ())?;
            original_sticky_slot_without_lower(&alice_client, &case.room_id, admit.as_ref())
                .await?;
        } else {
            rtc.open_slot(case.room_id.as_str(), "m.call#room", true)
                .await
                .map_err(|_| ())?;
        }
        let options = match case.mode {
            InteropMode::Current => CallOptions::default(),
            InteropMode::Sticky2025 => CallOptions::element_call_matrix_2_0(),
            InteropMode::RoomState => return Err(()),
        };
        let info = time::timeout(PHASE_LIMIT, rtc.join(case.room_id.as_str(), options))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        require(
            rtc.participants(case.room_id.as_str())
                .map_err(|_| ())?
                .iter()
                .all(|member| {
                    member.streams.iter().all(|stream| {
                        !matches!(
                            stream.kind,
                            RtcStreamKind::Microphone
                                | RtcStreamKind::Camera
                                | RtcStreamKind::ScreenShare
                                | RtcStreamKind::ScreenShareAudio
                        )
                    })
                }),
        )?;
        fact("joined_without_media");
        let dialect = NotificationDialect::new(
            case.mode,
            &alice.user_id,
            &alice.device_id,
            &info.room_id,
            &info.slot_id,
        )
        .ok_or(())?;

        phase(&mut self.stage, Stage::Membership);
        time::timeout(PHASE_LIMIT, async {
            loop {
                if own_membership(&alice_room, &alice, &info, &dialect, case.mode)
                    .map_err(|_| ())?
                    .is_some()
                    && own_membership(&bob_room, &alice, &info, &dialect, case.mode)
                        .map_err(|_| ())?
                        .is_some()
                {
                    return Ok::<_, ()>(());
                }
                time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| ())??;
        // Bob observes the actual call without joining it. Keep its feed in
        // Session so cancellation still detaches it during bounded cleanup.
        phase(&mut self.stage, Stage::ObserverOpen);
        {
            let mut diagnostic = ObserverDiagnostic::new();
            self.observer = Some(
                PassiveCallObserver::open(bob_client.clone(), case.room_id.as_str(), case.mode)
                    .await
                    .map_err(|error| diagnostic.error(error))?,
            );
            diagnostic.complete();
        }
        phase(&mut self.stage, Stage::ObserverRoster);
        wait_for_passive_caller(
            self.observer.as_mut().ok_or(())?,
            &bob_room,
            &alice,
            &info,
            &dialect,
            case.mode,
        )
        .await?;
        fact("encrypted_membership");

        phase(&mut self.stage, Stage::ActorSetup);
        self.actor = Some(ActorAcceptance::new(case, self.bob.clone(), bob.clone())?);
        self.actor.as_mut().ok_or(())?.prepare_devices().await?;

        // Subscribe before the snapshot and before sending. This receiver
        // learns the event ID only from Bob's SDK sync, never Alice's receipt
        // or a GET request. An overflow/lag cannot count as successful delivery.
        let mut incoming = bob_room.sticky_events().subscribe_keyless();
        let initial = bob_room.sticky_events().keyless_live();
        require(!initial.invalidated)?;
        let incoming_generation = initial.generation;
        require(initial.events.iter().all(|event| {
            let kind: Option<String> = event.raw().get_field("type").ok().flatten();
            !matches!(
                kind.as_deref(),
                Some("m.rtc.notification" | "org.matrix.msc4075.rtc.notification")
            )
        }))?;

        phase(&mut self.stage, Stage::Send);
        let selection = self.alice.account_selection(&alice.id).ok_or(())?;
        let active_token = AtomicU64::new(1);
        let stop = CaptureStop::default();
        let local_voice_space = || false;
        let outcome = OutgoingRing::new(
            &alice,
            selection,
            &info,
            case.mode,
            1,
            &active_token,
            &stop,
            &local_voice_space,
        )
        .send(rtc)
        .await;
        let InvitationOutcome::Sent(receipt) = outcome else {
            return Err(());
        };
        self.receipt = Some(receipt);
        let receipt = self.receipt.as_ref().ok_or(())?;
        fact("invitation_receipt");

        phase(&mut self.stage, Stage::Receive);
        let discovered = time::timeout(Duration::from_secs(20), async {
            loop {
                // Reject an already-lagged stream even when the snapshot
                // would satisfy this assertion. Bound draining so update floods
                // cannot keep this task from reaching its outer deadline.
                let mut drained = false;
                for _ in 0..64 {
                    match incoming.try_recv() {
                        Ok(update) => require(
                            !update.invalidated && update.generation == incoming_generation,
                        )?,
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                            drained = true;
                            break;
                        }
                        Err(_) => return Err(()),
                    }
                }
                require(drained)?;
                let snapshot = bob_room.sticky_events().keyless_live();
                require(!snapshot.invalidated && snapshot.generation == incoming_generation)?;
                let mut found = None;
                for event in snapshot.events {
                    let kind: Option<String> = event.raw().get_field("type").map_err(|_| ())?;
                    if !matches!(
                        kind.as_deref(),
                        Some("m.rtc.notification" | "org.matrix.msc4075.rtc.notification")
                    ) {
                        continue;
                    }
                    require(event.raw().json().get().len() <= MAX_MEMBERSHIP_BYTES)?;
                    let encryption = event.encryption_info().ok_or(())?;
                    require(
                        encryption.verification_state == VerificationState::Verified
                            && encryption.sender.as_str() == alice.user_id
                            && encryption
                                .sender_device
                                .as_deref()
                                .is_some_and(|device| device.as_str() == alice.device_id),
                    )?;
                    let raw: Value =
                        serde_json::from_str(event.raw().json().get()).map_err(|_| ())?;
                    let parsed =
                        RingNotification::parse(&raw, case.room_id.as_str()).map_err(|_| ())?;
                    require(
                        parsed.sender() == alice.user_id
                            && parsed.sender() == event.sender.as_str()
                            && parsed.event_id() == event.event_id.as_str()
                            && found.is_none(),
                    )?;
                    found = Some(event);
                }
                if let Some(event) = found {
                    return Ok::<_, ()>(event);
                }
                let update = incoming.recv().await.map_err(|_| ())?;
                require(!update.invalidated && update.generation == incoming_generation)?;
            }
        })
        .await
        .map_err(|_| ())??;
        // Compare only after independently discovering and authenticating it.
        require(discovered.event_id == receipt.event_id)?;
        fact("unsolicited_encrypted_notification");
        self.actor
            .as_mut()
            .ok_or(())?
            .first_discovery(case, &alice)
            .await?;

        phase(&mut self.stage, Stage::Fetch);
        let notification = fetch_decrypted(&bob_room, &discovered.event_id, &alice).await?;
        fact("fetched_decrypted_notification");
        phase(&mut self.stage, Stage::Validate);
        let envelope: Value =
            serde_json::from_str(discovered.raw().json().get()).map_err(|_| ())?;
        // GET may include different unsigned metadata; compare the decrypted
        // content and identity, leaving all ring checks on the unsolicited copy.
        let fetched: Value =
            serde_json::from_str(notification.raw().json().get()).map_err(|_| ())?;
        require(envelope.get("content") == fetched.get("content"))?;
        let parsed = RingNotification::parse(&envelope, case.room_id.as_str()).map_err(|_| ())?;
        require(
            parsed.event_id() == receipt.event_id.as_str() && parsed.sender() == alice.user_id,
        )?;
        let content = envelope.get("content").ok_or(())?;
        require(content.pointer("/m.mentions/room").and_then(Value::as_bool) == Some(!case.dm))?;
        let targets = content
            .pointer("/m.mentions/user_ids")
            .and_then(Value::as_array)
            .ok_or(())?;
        require(if case.dm {
            targets.len() == 1 && targets[0].as_str() == Some(bob.user_id.as_str())
        } else {
            targets.is_empty()
        })?;
        require(content.get("application").is_some() == (case.mode == InteropMode::Current))?;
        require(
            content.get("sticky_key").is_none() && content.get("msc4354_sticky_key").is_none(),
        )?;
        let reference_id = EventId::parse(parsed.membership_event_id()).map_err(|_| ())?;
        let reference = fetch_decrypted(&bob_room, &reference_id, &alice).await?;
        let reference_content = membership_content(&reference)?;
        require(matches_own_membership(
            MembershipEvidence {
                sender: &alice.user_id,
                device: Some(&alice.device_id),
                verified: true,
                sticky_key: &info.membership_id,
                content: &reference_content,
            },
            &alice,
            &info,
            dialect.membership_slot(),
            case.mode,
        ))?;
        // The reference can be an earlier refresh of the same logical join.
        // Independently resolve Bob's currently live, authenticated membership.
        let current_id = own_membership(&bob_room, &alice, &info, &dialect, case.mode)
            .map_err(|_| ())?
            .ok_or(())?;
        let current = fetch_decrypted(&bob_room, &current_id, &alice).await?;
        let current_content = membership_content(&current)?;
        require(matches_own_membership(
            MembershipEvidence {
                sender: &alice.user_id,
                device: Some(&alice.device_id),
                verified: true,
                sticky_key: &info.membership_id,
                content: &current_content,
            },
            &alice,
            &info,
            dialect.membership_slot(),
            case.mode,
        ))?;
        let powers = bob_room.power_levels().await.map_err(|_| ())?;
        let permitted = powers
            .user_can_trigger_room_notification(&UserId::parse(&alice.user_id).map_err(|_| ())?);
        require(case.dm || permitted)?;
        let context = RingContext {
            recipient_user_id: &bob.user_id,
            room_mention_permission: if permitted {
                RoomMentionPermission::Allowed
            } else {
                RoomMentionPermission::Denied
            },
            call: ResolvedJoinedCall {
                referenced_event_id: reference_id.as_str(),
                referenced: participation(&reference_content, &alice, &case.room_id)?,
                current: participation(&current_content, &alice, &case.room_id)?,
            },
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ())?
            .as_millis();
        let window = parsed
            .validate_for(&context, u64::try_from(now).map_err(|_| ())?)
            .map_err(|_| ())?;
        require(window.remaining_ms > 0 && window.remaining_ms <= 30000)?;

        // Exercise the real receiver admission boundary against the receipt
        // Bob discovered through sync. It independently resolves current
        // trust, push policy, the reference, open slot and live membership.
        // This still does not join Bob or open any capture device.
        let preferences = self.actor.as_ref().ok_or(())?.preferences.clone();
        let mut ledger = OfferLedger::default();
        phase(&mut self.stage, Stage::Admission);
        let mut offer = IncomingOffer::admit(
            &self.bob,
            &preferences,
            &case.room_id,
            &discovered.event_id,
            &mut ledger,
        )
        .await
        .map_err(admission_failure)?;
        require(offer.mode() == case.mode)?;
        let original_deadline = offer.deadline();
        phase(&mut self.stage, Stage::Revalidate);
        offer
            .revalidate(&self.bob, &preferences)
            .await
            .map_err(admission_failure)?;
        require(offer.deadline() <= original_deadline)?;
        require(
            IncomingOffer::admit(
                &self.bob,
                &preferences,
                &case.room_id,
                &discovered.event_id,
                &mut ledger,
            )
            .await
            .is_err(),
        )?;
        fact("targeted_reference");

        phase(&mut self.stage, Stage::Redact);
        require(receipt.redact(rtc).await == RetractionOutcome::Accepted)?;
        fact("redaction_receipt");
        phase(&mut self.stage, Stage::Redaction);
        time::timeout(Duration::from_secs(20), async {
            loop {
                let response = bob_client
                    .send(get_room_event::v3::Request::new(
                        case.room_id.clone(),
                        receipt.event_id.clone(),
                    ))
                    .with_request_config(
                        RequestConfig::new()
                            .retry_limit(0)
                            .timeout(Duration::from_secs(5)),
                    )
                    .await
                    .map_err(|_| ())?;
                let event: Value =
                    serde_json::from_str(response.event.json().get()).map_err(|_| ())?;
                if let Some(redaction) = event.pointer("/unsigned/redacted_because") {
                    require(
                        event.get("event_id").and_then(Value::as_str)
                            == Some(receipt.event_id.as_str()),
                    )?;
                    require(
                        event
                            .get("content")
                            .and_then(Value::as_object)
                            .is_some_and(|content| content.is_empty()),
                    )?;
                    require(
                        redaction.get("sender").and_then(Value::as_str)
                            == Some(alice.user_id.as_str()),
                    )?;
                    let redacts = redaction
                        .get("redacts")
                        .or_else(|| redaction.pointer("/content/redacts"));
                    require(redacts.and_then(Value::as_str) == Some(receipt.event_id.as_str()))?;
                    return Ok::<_, ()>(());
                }
                time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| ())??;
        fact("redaction_observed");
        phase(&mut self.stage, Stage::ReceiptRedaction);
        time::timeout(Duration::from_secs(20), async {
            loop {
                // Reject an already-lagged stream even when the snapshot
                // would satisfy this assertion. Bound draining so update floods
                // cannot keep this task from reaching its outer deadline.
                let mut drained = false;
                for _ in 0..64 {
                    match incoming.try_recv() {
                        Ok(update) => require(
                            !update.invalidated && update.generation == incoming_generation,
                        )?,
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                            drained = true;
                            break;
                        }
                        Err(_) => return Err(()),
                    }
                }
                require(drained)?;
                let snapshot = bob_room.sticky_events().keyless_live();
                require(!snapshot.invalidated && snapshot.generation == incoming_generation)?;
                // Natural expiry must not satisfy the redaction observation.
                require(MilliSecondsSinceUnixEpoch::now() < discovered.expires_at)?;
                if snapshot
                    .events
                    .iter()
                    .all(|event| event.event_id != discovered.event_id)
                {
                    return Ok::<_, ()>(());
                }
                let update = incoming.recv().await.map_err(|_| ())?;
                require(!update.invalidated && update.generation == incoming_generation)?;
            }
        })
        .await
        .map_err(|_| ())??;
        // The admission must withdraw before natural expiry, then remain
        // revoked even if a caller asks it to revalidate again.
        phase(&mut self.stage, Stage::Withdraw);
        require(Instant::now() < offer.deadline())?;
        require(offer.revalidate(&self.bob, &preferences).await.is_err())?;
        require(offer.revalidate(&self.bob, &preferences).await.is_err())?;
        fact("unsolicited_redaction_observed");

        self.actor
            .as_mut()
            .ok_or(())?
            .first_withdrawal(original_deadline)
            .await?;
        // The known first receipt is already confirmed redacted. The actor
        // slice owns each later fresh receipt across cancellation for cleanup.
        self.receipt = None;
        self.actor
            .as_mut()
            .ok_or(())?
            .answer_and_leave(
                case,
                &alice,
                &self.alice,
                rtc,
                &info,
                &bob_room,
                self.observer.as_mut().ok_or(())?,
            )
            .await?;

        // This is a separate membership-removal assertion, after all existing
        // invitation checks. A naturally expired membership cannot satisfy it.
        phase(&mut self.stage, Stage::ObserverLeave);
        let mut diagnostic = ObserverDiagnostic::new();
        diagnostic.record("current_event_mismatch");
        let before_leave = own_membership(&bob_room, &alice, &info, &dialect, case.mode)
            .map_err(|_| ())?
            .ok_or(())?;
        let expires_at = bob_room
            .sticky_events()
            .live()
            .into_iter()
            .find(|event| event.event_id == before_leave)
            .ok_or(())?
            .expires_at;
        diagnostic.record("membership_expired");
        require(MilliSecondsSinceUnixEpoch::now() < expires_at)?;
        diagnostic.record("leave_unavailable");
        time::timeout(Duration::from_secs(12), rtc.leave(case.room_id.as_str()))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let observer = self.observer.as_mut().ok_or(())?;
        diagnostic.record("not_sampled");
        time::timeout(Duration::from_secs(20), async {
            loop {
                let unexpired = MilliSecondsSinceUnixEpoch::now() < expires_at;
                if !unexpired {
                    diagnostic.record("membership_expired");
                }
                require(unexpired)?;
                match observer.snapshot().await {
                    Err(error) => diagnostic.error(error),
                    Ok(snapshot) => {
                        diagnostic.record("unexpected_roster");
                        require(
                            snapshot.room_id == info.room_id
                                && snapshot.slot_id == info.slot_id
                                && snapshot.mode == case.mode,
                        )?;
                        diagnostic.record("membership_still_present");
                        if snapshot.participants.is_empty() {
                            diagnostic.record("current_event_mismatch");
                            if own_membership(&bob_room, &alice, &info, &dialect, case.mode)
                                .map_err(|_| ())?
                                .is_none()
                            {
                                diagnostic.record("membership_expired");
                                require(MilliSecondsSinceUnixEpoch::now() < expires_at)?;
                                return Ok::<_, ()>(());
                            }
                        }
                    }
                }
                time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| ())??;
        diagnostic.complete();
        Ok(())
    }

    /// All cleanup is outside the main deadline and every attempt runs even
    /// after a preceding failure. Client/RTC owners remain here on cancellation.
    async fn cleanup(&mut self, room_id: &OwnedRoomId) -> bool {
        let mut clean = true;
        // Revoke pending/established authority synchronously before any wait.
        if let Some(actor) = &self.actor {
            actor.shutdown();
        }
        self.observer = None;
        let mut receipts = Vec::new();
        if let Some(receipt) = self.receipt.take() {
            receipts.push(receipt);
        }
        if let Some(receipt) = self.actor.as_mut().and_then(ActorAcceptance::take_receipt) {
            receipts.push(receipt);
        }
        let had_actor = self.actor.is_some();
        let actor = self.actor.as_mut();
        let rtc = self.rtc.as_mut();
        // Bob's retained join/leave and Alice's receipt/leave cleanup are
        // independent. Preserve both owners and attempt both even on failure.
        let (actor_clean, sender_clean) = tokio::join!(
            async {
                match actor {
                    Some(actor) => {
                        cleanup_attempt(Duration::from_secs(77), actor.cleanup_call()).await
                    }
                    None => true,
                }
            },
            async {
                let mut clean = true;
                if let Some(rtc) = rtc {
                    for receipt in receipts {
                        clean &= cleanup_attempt(Duration::from_secs(3), async {
                            receipt.redact(rtc).await == RetractionOutcome::Accepted
                        })
                        .await;
                    }
                    if rtc.call(room_id.as_str()).is_some() {
                        clean &= cleanup_attempt(Duration::from_secs(12), async {
                            rtc.leave(room_id.as_str()).await.is_ok()
                        })
                        .await;
                    }
                } else {
                    clean &= receipts.is_empty();
                }
                clean
            },
        );
        clean &= actor_clean && sender_clean;
        if had_actor {
            eprintln!("\nMORPHEUS_INCOMING_CHECK actor_cleanup={clean}");
            if !clean {
                eprintln!("\nMORPHEUS_INCOMING_FAILURE actor_cleanup");
            }
        }
        for manager in [&self.alice, &self.bob] {
            clean &= cleanup_attempt(Duration::from_secs(10), async {
                let mut logged_out = true;
                for account in manager.accounts().await {
                    logged_out &= manager
                        .logout_account(&account.id)
                        .await
                        .is_ok_and(|result| result.remote_logout_succeeded);
                }
                logged_out
            })
            .await;
        }
        for manager in [&self.alice, &self.bob] {
            clean &= cleanup_attempt(Duration::from_secs(5), async {
                manager.stop().await;
                true
            })
            .await;
        }
        self.rtc = None;
        clean
    }
}

async fn wait_for_passive_caller(
    observer: &mut PassiveCallObserver,
    room: &Room,
    caller: &AccountInfo,
    info: &CallInfo,
    dialect: &NotificationDialect,
    mode: InteropMode,
) -> Result<(), ()> {
    let mut diagnostic = ObserverDiagnostic::new();
    time::timeout(Duration::from_secs(20), async {
        loop {
            // Feed seeding and SDK map updates are independent. Retry only
            // within this existing work budget, never accept an unproven row.
            match observer.snapshot().await {
                Err(error) => diagnostic.error(error),
                Ok(snapshot) => {
                    diagnostic.record("unexpected_roster");
                    require(
                        snapshot.room_id == info.room_id
                            && snapshot.slot_id == info.slot_id
                            && snapshot.mode == mode
                            && snapshot.participants.len() <= 1,
                    )?;
                    diagnostic.record("empty_roster");
                    if let Some(participant) = snapshot.participants.first() {
                        // Exactly the caller means the receiver has no logical
                        // membership of its own as a side effect of observing.
                        diagnostic.record("unexpected_roster");
                        require(
                            participant.user_id == caller.user_id
                                && participant.device_id == caller.device_id
                                && participant.membership_id == info.membership_id,
                        )?;
                        // No await between reading the observer and the existing
                        // independently Verified SDK membership proof. A refresh
                        // race waits for the two current event IDs to agree.
                        diagnostic.record("current_event_mismatch");
                        if own_membership(room, caller, info, dialect, mode)
                            .map_err(|_| ())?
                            .is_some_and(|event| event.as_str() == participant.membership_event_id)
                        {
                            return Ok::<_, ()>(());
                        }
                    }
                }
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| ())??;
    diagnostic.complete();
    Ok(())
}

async fn cleanup_attempt<F: Future<Output = bool>>(limit: Duration, work: F) -> bool {
    matches!(
        AssertUnwindSafe(time::timeout(limit, work))
            .catch_unwind()
            .await,
        Ok(Ok(true))
    )
}

async fn wait_ready(
    manager: &AccountManager,
    client: &Client,
    room_id: &OwnedRoomId,
) -> Result<(), ()> {
    let mut sync = manager.subscribe_sync_status();
    time::timeout(PHASE_LIMIT, async {
        loop {
            if matches!(
                &*sync.borrow(),
                SyncStatus::Running {
                    backend: SyncBackend::SlidingSticky
                }
            ) && client
                .get_room(room_id)
                .is_some_and(|room| room.state() == RoomState::Joined)
            {
                return Ok(());
            }
            tokio::select! {
                changed = sync.changed() => { changed.map_err(|_| ())?; }
                _ = time::sleep(Duration::from_millis(100)) => {}
            }
        }
    })
    .await
    .map_err(|_| ())?
}

async fn bootstrap(client: &Client, account: &FixtureAccount) -> Result<(), ()> {
    let encryption = client.encryption();
    encryption.wait_for_e2ee_initialization_tasks().await;
    let own = client.user_id().ok_or(())?;
    let identity = encryption
        .request_user_identity(own)
        .await
        .map_err(|_| ())?;
    let device = encryption.get_own_device().await.map_err(|_| ())?;
    if identity.is_some_and(|identity| identity.is_verified())
        && device.is_some_and(|device| device.is_verified_with_cross_signing())
    {
        return Ok(());
    }
    if let Err(error) = encryption.bootstrap_cross_signing(None).await {
        let challenge = error.as_uiaa_response().ok_or(())?;
        let mut password = uiaa::Password::new(
            uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(account.username.clone())),
            account.password.clone(),
        );
        password.session = challenge.session.clone();
        encryption
            .bootstrap_cross_signing(Some(uiaa::AuthData::Password(password)))
            .await
            .map_err(|_| ())?;
    }
    let identity = encryption
        .request_user_identity(own)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    require(identity.is_verified())?;
    let device = encryption
        .get_own_device()
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if !device.is_verified_with_cross_signing() {
        device.verify().await.map_err(|_| ())?;
        encryption
            .request_user_identity(own)
            .await
            .map_err(|_| ())?;
    }
    require(
        encryption
            .get_own_device()
            .await
            .map_err(|_| ())?
            .is_some_and(|device| device.is_verified_with_cross_signing()),
    )
}

async fn trust_fixture_peer(observer: &Client, subject: &Client) -> Result<(), ()> {
    let user = subject.user_id().ok_or(())?;
    let own = subject
        .encryption()
        .request_user_identity(user)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    require(own.is_verified())?;
    let expected = own.master_key().get_first_key().ok_or(())?.to_base64();
    let peer = observer
        .encryption()
        .request_user_identity(user)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    require(
        peer.user_id() == user
            && peer.master_key().get_first_key().ok_or(())?.to_base64() == expected,
    )?;
    peer.verify().await.map_err(|_| ())?;
    require(
        observer
            .encryption()
            .request_user_identity(user)
            .await
            .map_err(|_| ())?
            .is_some_and(|identity| identity.is_verified()),
    )
}

async fn fetch_decrypted(
    room: &Room,
    event_id: &OwnedEventId,
    sender: &AccountInfo,
) -> Result<TimelineEvent, ()> {
    time::timeout(Duration::from_secs(20), async {
        loop {
            let event = room
                .event(
                    event_id,
                    Some(
                        RequestConfig::new()
                            .retry_limit(0)
                            .timeout(Duration::from_secs(5)),
                    ),
                )
                .await
                .map_err(|_| ())?;
            if event.encryption_info().is_some_and(|info| {
                info.verification_state == VerificationState::Verified
                    && info.sender.as_str() == sender.user_id
                    && info
                        .sender_device
                        .as_deref()
                        .is_some_and(|device| device.as_str() == sender.device_id)
            }) {
                require(event.raw().json().get().len() <= MAX_MEMBERSHIP_BYTES)?;
                let raw_sender: Option<String> = event.raw().get_field("sender").map_err(|_| ())?;
                let raw_id: Option<String> = event.raw().get_field("event_id").map_err(|_| ())?;
                require(
                    raw_sender.as_deref() == Some(sender.user_id.as_str())
                        && raw_id.as_deref() == Some(event_id.as_str()),
                )?;
                return Ok(event);
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| ())?
}

fn participation<'a>(
    content: &'a Value,
    sender: &'a AccountInfo,
    room: &'a OwnedRoomId,
) -> Result<CallParticipation<'a>, ()> {
    Ok(CallParticipation {
        room_id: room.as_str(),
        sender: &sender.user_id,
        logical_membership_id: content
            .pointer("/member/id")
            .and_then(Value::as_str)
            .ok_or(())?,
        slot_id: content.get("slot_id").and_then(Value::as_str).ok_or(())?,
        application: content
            .pointer("/application/type")
            .and_then(Value::as_str)
            .ok_or(())?,
        device_id: Some(&sender.device_id),
    })
}

fn membership_content(event: &TimelineEvent) -> Result<Value, ()> {
    let event_type: Option<String> = event.raw().get_field("type").map_err(|_| ())?;
    require(matches!(
        event_type.as_deref(),
        Some("m.rtc.member" | "org.matrix.msc4143.rtc.member")
    ))?;
    event.raw().get_field("content").map_err(|_| ())?.ok_or(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Requires a fresh private loopback MatrixRTC fixture and credential vault"]
async fn actual_outgoing_invitation_encrypted_receipt_and_redaction() {
    eprintln!("MORPHEUS_RING_STAGE fixture");
    let opened = Case::read().and_then(|case| Session::open(&case).map(|session| (case, session)));
    let Ok((case, mut session)) = opened else {
        eprintln!("MORPHEUS_RING_FAILURE fixture");
        eprintln!("MORPHEUS_RING_RESULT passed=false");
        panic!("Outgoing invitation fixture rejected");
    };
    let result = AssertUnwindSafe(time::timeout(WORK_LIMIT, session.run(&case)))
        .catch_unwind()
        .await;
    let passed = matches!(result, Ok(Ok(Ok(()))));
    if !passed {
        eprintln!("MORPHEUS_RING_FAILURE {}", session.stage.name());
        if let Some(actor) = &session.actor {
            actor.report_failure();
        }
    }
    phase(&mut session.stage, Stage::Cleanup);
    let clean = session.cleanup(&case.room_id).await;
    drop(session);
    let removed = case.profile.close().is_ok();
    let clean = clean && removed;
    eprintln!("MORPHEUS_RING_CHECK cleanup={clean}");
    if !clean {
        eprintln!("MORPHEUS_RING_FAILURE cleanup");
    }
    let passed = passed && clean;
    eprintln!("MORPHEUS_RING_RESULT passed={passed}");
    assert!(
        passed,
        "Outgoing invitation acceptance failed; see fixed phase facts"
    );
}
