//! Current-only real SFU outage acceptance inside the existing owned fixture.
//! The app controller recovers automatically; fixture peers explicitly rejoin.

use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    process::{Child, Command, Stdio},
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};

use matrix_sdk::{
    event_handler::EventHandlerHandle,
    room::MessagesOptions,
    ruma::events::room::message::{OriginalSyncRoomMessageEvent, RoomMessageEventContent},
};
use morpheus_rtc::{AudioDeviceSelection, I420Buffer, Publication, VideoFrame, VideoRotation};
use tokio::sync::broadcast;

use crate::{
    calls::{
        CallController, CallInterruption, CallJoinMode, CallStartIntent, CallStatus,
        RecoveryKeyObservation,
    },
    updater::UpdateActivity,
    video_render::{TileMedium, TileState, VideoUpdate},
};

use super::{
    alsa_playback::{self, NativePlayback, Peer as TonePeer, Tone},
    *,
};

const BARRIER: &str = "Morpheus owned SFU recovery sync barrier";
const CASE_WORK: Duration = Duration::from_secs(420);

fn stage(name: &'static str) {
    eprintln!("\nMORPHEUS_RTC_OUTAGE_STAGE {name}");
}

fn fact(name: &'static str) {
    eprintln!("\nMORPHEUS_RTC_OUTAGE_CHECK {name}=true");
}

#[derive(Clone, Copy)]
enum WorkFailure {
    Error,
    Timeout,
    Panic,
}

fn failed_work(failure: WorkFailure, call: &CallStatus) {
    // Closed diagnostics only. Never inspect a call's reason, account/room
    // IDs, an SDK error or the caught panic payload.
    let outcome = match failure {
        WorkFailure::Error => "error",
        WorkFailure::Timeout => "timeout",
        WorkFailure::Panic => "panic",
    };
    let call = match call {
        CallStatus::Idle => "idle",
        CallStatus::OpeningSlot { .. } => "opening_slot",
        CallStatus::Starting { .. } => "starting",
        CallStatus::Joined { .. } => "joined",
        CallStatus::Reconnecting { .. } => "reconnecting",
        CallStatus::Ending { .. } => "ending",
        CallStatus::CleanupFailed { .. } => "cleanup_failed",
        CallStatus::Failed { .. } => "failed",
        CallStatus::Ended { .. } => "ended",
    };
    eprintln!("\nMORPHEUS_RTC_OUTAGE_FAILURE outcome={outcome} call={call}");
}

struct OutageCase {
    fixture: Fixture,
    profile: tempfile::TempDir,
    room: OwnedRoomId,
    creation: OwnedEventId,
    dm: bool,
}

#[derive(Deserialize)]
struct CreationReceipt {
    outage_creation_event_id: String,
}

impl OutageCase {
    fn read() -> Result<Self, ()> {
        require(std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true"))?;
        require(std::env::var("RUNNER_OS").as_deref() == Ok("Linux"))?;
        require(std::env::var("GITHUB_REPOSITORY").as_deref() == Ok("brk275/Morpheus"))?;
        let run = std::env::var("MORPHEUS_MATRIX_SFU_RUN_ID").map_err(|_| ())?;
        require(
            run.len() == 32
                && run
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        )?;
        let dm = match std::env::var("MORPHEUS_RTC_OUTAGE_TARGET").as_deref() {
            Ok("dm") => true,
            Ok("room") => false,
            _ => return Err(()),
        };
        let path = PathBuf::from(std::env::var_os("MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS").ok_or(())?);
        let root = PathBuf::from(std::env::var_os("MORPHEUS_MATRIX_TEST_HOME").ok_or(())?)
            .canonicalize()
            .map_err(|_| ())?;
        require(path.parent().ok_or(())?.canonicalize().map_err(|_| ())? == root.join("rtc"))?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|_| ())?;
        let before = file.metadata().map_err(|_| ())?;
        require(
            before.is_file()
                && before.uid() == unsafe { libc::geteuid() }
                && before.nlink() == 1
                && before.mode() & 0o077 == 0
                && before.len() < 1_048_576,
        )?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        let after = file.metadata().map_err(|_| ())?;
        require(
            bytes.len() as u64 == before.len()
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
        )?;
        let fixture: Fixture = serde_json::from_slice(&bytes).map_err(|_| ())?;
        let receipt: CreationReceipt = serde_json::from_slice(&bytes).map_err(|_| ())?;
        let creation = EventId::parse(receipt.outage_creation_event_id).map_err(|_| ())?;
        require(fixture.accounts.len() == if dm { 2 } else { 3 })?;
        for (label, prefix) in [("a_primary", "rtcalice"), ("a_second_device", "rtcbob")]
            .into_iter()
            .chain((!dm).then_some(("a_third_device", "rtccarol")))
        {
            let account = fixture
                .accounts
                .iter()
                .find(|account| account.label == label)
                .ok_or(())?;
            require(account.username.starts_with(prefix) && account.password.len() >= 16)?;
            let user = UserId::parse(&account.user_id).map_err(|_| ())?;
            require(user.server_name().as_str() == "morpheus-a.test")?;
            require(account.homeserver_url == "http://127.0.0.1:18008")?;
        }
        require(
            fixture
                .accounts
                .iter()
                .map(|account| &account.user_id)
                .collect::<HashSet<_>>()
                .len()
                == fixture.accounts.len(),
        )?;
        let room = RoomId::parse(fixture.rooms.get("encrypted_room").ok_or(())?).map_err(|_| ())?;
        let profile = tempfile::Builder::new()
            .prefix("rtc-sfu-outage-")
            .tempdir_in(path.parent().ok_or(())?)
            .map_err(|_| ())?;
        Ok(Self {
            fixture,
            profile,
            room,
            creation,
            dm,
        })
    }
}

/// Each synthetic source owns a real RTC publication and a retained task. A
/// stop flag only asks for exit between settled native push_audio operations;
/// only the actual JoinHandle result permits unpublish/leave afterward.
struct AudioProducer {
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<Result<(), ()>>>,
}
impl Drop for AudioProducer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            // Terminal uncertainty only: never detach an admitted push and then
            // claim fixture cleanup. The outer owned child remains bounded.
            std::mem::forget(task);
        }
    }
}

struct Peer {
    manager: Arc<AccountManager>,
    account: Option<AccountInfo>,
    client: Option<Client>,
    rtc: Option<RtcService>,
    info: Option<CallInfo>,
    publication: Option<Publication>,
    audio: Option<Arc<Publication>>,
    producer: Option<AudioProducer>,
    operation_unproved: bool,
}

impl Peer {
    fn open(path: PathBuf) -> Result<Self, ()> {
        Ok(Self {
            manager: Arc::new(AccountManager::open(path).map_err(|_| ())?),
            account: None,
            client: None,
            rtc: None,
            info: None,
            publication: None,
            audio: None,
            producer: None,
            operation_unproved: false,
        })
    }
    async fn login(&mut self, account: &FixtureAccount, room: &OwnedRoomId) -> Result<(), ()> {
        self.account = Some(
            self.manager
                .add_password_account(
                    &account.homeserver_url,
                    &account.username,
                    &account.password,
                )
                .await
                .map_err(|_| ())?,
        );
        let client = self.manager.active_client().await.map_err(|_| ())?;
        wait_ready(&self.manager, &client, room).await?;
        bootstrap(&client, account).await?;
        self.client = Some(client);
        Ok(())
    }
    async fn join(&mut self, room: &OwnedRoomId) -> Result<(), ()> {
        require(!self.operation_unproved)?;
        self.operation_unproved = true;
        if self.rtc.is_none() {
            self.rtc = Some(RtcService::new(self.client.as_ref().ok_or(())?.clone()));
        }
        let rtc = self.rtc.as_mut().ok_or(())?;
        self.info = Some(
            time::timeout(PHASE_LIMIT, rtc.join(room.as_str(), CallOptions::default()))
                .await
                .map_err(|_| ())?
                .map_err(|_| ())?,
        );
        self.publication = Some(
            rtc.publish_camera(room.as_str(), 320, 180, false)
                .await
                .map_err(|_| ())?,
        );
        self.operation_unproved = false;
        Ok(())
    }
    async fn start_audio(
        &mut self,
        room: &OwnedRoomId,
        identity: TonePeer,
        tone: Tone,
    ) -> Result<(), ()> {
        require(!self.operation_unproved && self.audio.is_none() && self.producer.is_none())?;
        self.operation_unproved = true;
        // Peer owns an Arc before spawning. A discarded spawn closure cannot
        // drop the sole publication; no borrowed native work is transferred.
        self.audio = Some(Arc::new(
            self.rtc
                .as_ref()
                .ok_or(())?
                .publish_microphone(room.as_str(), 48_000, 1, false)
                .await
                .map_err(|_| ())?,
        ));
        let publication = self.audio.as_ref().ok_or(())?.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            let mut ticks = time::interval(Duration::from_millis(10));
            ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            let mut index = 0_u64;
            loop {
                if stopped.load(Ordering::Acquire) {
                    return Ok(());
                }
                ticks.tick().await;
                if stopped.load(Ordering::Acquire) {
                    return Ok(());
                }
                // Never select-drop a push which has entered the real native
                // source. Cancellation waits for this exact task's actual join.
                publication
                    .push_audio(alsa_playback::tone_frame(identity, tone, index))
                    .await
                    .map_err(|_| ())?;
                index = index.checked_add(1).ok_or(())?;
            }
        });
        self.producer = Some(AudioProducer {
            stop,
            task: Some(task),
        });
        self.operation_unproved = false;
        Ok(())
    }
    /// Ok(false) is a settled push error, not lost task ownership. It fails the
    /// evidence phase but still allows explicit unpublish/leave during cleanup.
    async fn stop_audio(&mut self) -> Result<bool, ()> {
        let Some(producer) = self.producer.as_mut() else {
            return Ok(true);
        };
        producer.stop.store(true, Ordering::Release);
        let task = producer.task.as_mut().ok_or(())?;
        let joined = match time::timeout(Duration::from_secs(10), task).await {
            Ok(joined) => joined,
            Err(_) => {
                self.operation_unproved = true;
                return Err(());
            }
        };
        // The JoinHandle was polled by mutable reference. Pending timeouts keep
        // it in Peer; only an actual returned receipt consumes that ownership.
        producer.task = None;
        self.producer = None;
        match joined {
            Ok(outcome) => Ok(outcome.is_ok()),
            Err(_) => {
                self.operation_unproved = true;
                Err(())
            }
        }
    }
    async fn leave(&mut self, room: &OwnedRoomId) -> Result<(), ()> {
        // A cancelled/failed earlier operation is never repaired into a clean
        // receipt by merely observing an empty RtcService map later.
        let prior_unproved = self.operation_unproved;
        // A producer still owns its admitted push. Do not unpublish or leave
        // concurrently if its actual completion could not be observed.
        if self.stop_audio().await.is_err() {
            self.operation_unproved = true;
            return Err(());
        }
        self.operation_unproved = true;
        let mut clean = !prior_unproved;
        if let Some(audio) = self.audio.as_ref() {
            let removed = matches!(
                time::timeout(Duration::from_secs(8), audio.unpublish()).await,
                Ok(Ok(()))
            );
            clean &= removed;
            if removed {
                self.audio = None;
            }
        }
        if let Some(publication) = self.publication.as_ref() {
            let removed = matches!(
                time::timeout(Duration::from_secs(8), publication.unpublish()).await,
                Ok(Ok(()))
            );
            clean &= removed;
            if removed {
                self.publication = None;
            }
        }
        if let Some(rtc) = &mut self.rtc {
            if rtc.call(room.as_str()).is_some() {
                clean &= matches!(
                    time::timeout(Duration::from_secs(12), rtc.leave(room.as_str())).await,
                    Ok(Ok(()))
                );
            }
        }
        self.operation_unproved = !clean;
        require(clean)
    }
}

struct OutageSession {
    peers: Vec<Peer>,
    calls: Arc<CallController>,
    command: Option<Child>,
    control_unproved: bool,
    barrier_handler: Option<(Client, EventHandlerHandle)>,
    playback: NativePlayback,
}

impl OutageSession {
    fn open(case: &OutageCase) -> Result<Self, ()> {
        let mut peers = Vec::new();
        for account in &case.fixture.accounts {
            peers.push(Peer::open(case.profile.path().join(&account.label))?);
        }
        let owner = case
            .fixture
            .accounts
            .iter()
            .position(|account| account.label == "a_second_device")
            .ok_or(())?;
        // The exact ALSA config and paced borrowed descriptors exist before
        // any CallController or native device enumeration/open can begin.
        let playback = NativePlayback::start().map_err(|_| ())?;
        let calls = Arc::new(CallController::new(
            peers[owner].manager.clone(),
            UpdateActivity::default(),
        ));
        Ok(Self {
            peers,
            calls,
            command: None,
            control_unproved: false,
            barrier_handler: None,
            playback,
        })
    }
    fn owner(case: &OutageCase) -> Result<usize, ()> {
        case.fixture
            .accounts
            .iter()
            .position(|account| account.label == "a_second_device")
            .ok_or(())
    }
    async fn control(&mut self, action: &'static str) -> Result<(), ()> {
        require(self.command.is_none() && matches!(action, "sfu-outage" | "sfu-resume"))?;
        self.control_unproved = true;
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/local_matrix_rtc.py");
        self.command = Some(
            Command::new("/usr/bin/python3")
                .arg("-B")
                .arg(script)
                .arg(action)
                .env_remove("PYTHONPATH")
                .env_remove("PYTHONHOME")
                .env("PYTHONNOUSERSITE", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| ())?,
        );
        let status = time::timeout(Duration::from_secs(65), async {
            loop {
                if let Some(status) = self
                    .command
                    .as_mut()
                    .ok_or(())?
                    .try_wait()
                    .map_err(|_| ())?
                {
                    return Ok::<_, ()>(status);
                }
                time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| ())??;
        self.command = None;
        require(status.success())?;
        self.control_unproved = false;
        Ok(())
    }
    async fn run(&mut self, case: &OutageCase) -> Result<(), ()> {
        stage("login");
        for (peer, account) in self.peers.iter_mut().zip(&case.fixture.accounts) {
            peer.login(account, &case.room).await?;
        }
        for first in 0..self.peers.len() {
            for second in 0..self.peers.len() {
                if first != second {
                    trust_fixture_peer(
                        self.peers[first].client.as_ref().ok_or(())?,
                        self.peers[second].client.as_ref().ok_or(())?,
                    )
                    .await?;
                }
            }
        }
        let owner = Self::owner(case)?;
        let client = self.peers[owner].client.as_ref().ok_or(())?.clone();
        let account = self.peers[owner].account.as_ref().ok_or(())?.clone();
        let room = client.get_room(&case.room).ok_or(())?;
        let creator = &case
            .fixture
            .accounts
            .iter()
            .find(|account| account.label == "a_primary")
            .ok_or(())?
            .user_id;
        verify_creation(&room, &case.creation, creator).await?;
        room.set_is_direct(case.dm).await.map_err(|_| ())?;
        time::timeout(Duration::from_secs(10), async {
            loop {
                if room.is_direct().await.map_err(|_| ())? == case.dm {
                    return Ok::<_, ()>(());
                }
                time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| ())??;
        let store = morpheus_config::ConfigStore::at(case.profile.path().join("calls.toml"));
        store.load_or_create().map_err(|_| ())?;
        let preferences = Arc::new(VoiceSpaces::new(store));
        let selection = self.peers[owner]
            .manager
            .account_selection(&account.id)
            .ok_or(())?;
        let mut keys = self.calls.subscribe_recovery_keys();
        let mut status = self.calls.subscribe_status();
        let mut video = self.calls.subscribe_video();
        stage("join");
        self.calls
            .open_slot(&account.id, case.room.as_str())
            .await
            .map_err(|_| ())?;
        let original = self
            .calls
            .start_audio(
                selection.clone(),
                case.room.as_str(),
                CallJoinMode::Standard,
                AudioDeviceSelection {
                    microphone: Some(alsa_playback::INPUT_ID.to_owned()),
                    speaker: Some(alsa_playback::OUTPUT_ID.to_owned()),
                },
                CallStartIntent::Notify,
                preferences,
            )
            .await
            .map_err(|_| ())?;
        let stable_call = self.calls.call_token();
        let before_generation = self.calls.video_token();
        require(stable_call != 0 && before_generation != 0 && selection.is_current())?;
        let observer_index = (0..self.peers.len())
            .find(|index| *index != owner)
            .ok_or(())?;
        let observer = self.peers[observer_index]
            .client
            .as_ref()
            .ok_or(())?
            .clone();
        let observer_room = observer.get_room(&case.room).ok_or(())?;
        let notification = initial_notification(&observer_room, &account).await?;
        fact("initial_notification");
        stage("peer_join");
        for index in 0..self.peers.len() {
            if index != owner {
                self.peers[index].join(&case.room).await?;
            }
        }
        stage("initial_checkpoint");
        let before_audio = self.playback.checkpoint(Tone::Before).map_err(|_| ())?;
        stage("initial_audio_source");
        self.start_tones(case, owner, Tone::Before).await?;
        let before_presentation = self.media(case, owner, &mut video, false, None).await?;
        stage("initial_audio");
        self.audio_evidence(case, &before_audio).await?;
        fact("native_audio_before");
        // Settle every old real push BEFORE the transport is intentionally
        // destroyed. Keep the old publications until normal peer leave.
        self.stop_tones(owner).await?;
        stage("outage");
        self.control("sfu-outage").await?;
        fact("sfu_stopped");
        stage("app_rejoining");
        time::timeout(Duration::from_secs(150), async {
            loop {
                let observed = status.borrow_and_update().clone();
                match observed {
                    CallStatus::Reconnecting {
                        account_id,
                        call,
                        interruption: CallInterruption::Rejoining { .. },
                    } if account_id == account.id && call == original => return Ok::<_, ()>(()),
                    CallStatus::Failed { .. }
                    | CallStatus::Ended { .. }
                    | CallStatus::CleanupFailed { .. }
                    | CallStatus::Ending { .. }
                    | CallStatus::Idle => return Err(()),
                    _ => {}
                }
                status.changed().await.map_err(|_| ())?;
            }
        })
        .await
        .map_err(|_| ())??;
        require(
            self.calls.call_token() == stable_call
                && self.calls.video_token() != before_generation
                && selection.is_current(),
        )?;
        fact("app_ended_into_rejoin");
        stage("restart");
        self.control("sfu-resume").await?;
        fact("sfu_restarted");
        let rejoined = time::timeout(Duration::from_secs(60), async {
            loop {
                match status.borrow_and_update().clone() {
                    CallStatus::Joined { account_id, call }
                        if account_id == account.id
                            && call.membership_id != original.membership_id =>
                    {
                        return Ok::<_, ()>(call);
                    }
                    CallStatus::Failed { .. }
                    | CallStatus::Ended { .. }
                    | CallStatus::CleanupFailed { .. }
                    | CallStatus::Ending { .. }
                    | CallStatus::Idle => return Err(()),
                    _ => {}
                }
                status.changed().await.map_err(|_| ())?;
            }
        })
        .await
        .map_err(|_| ())??;
        require(
            rejoined.room_id == original.room_id
                && rejoined.slot_id == original.slot_id
                && rejoined.local_identity != original.local_identity
                && self.calls.call_token() == stable_call
                && selection.is_current(),
        )?;
        fact("automatic_new_join");
        stage("peer_rejoin");
        for index in 0..self.peers.len() {
            if index != owner {
                let old = self.peers[index].info.clone().ok_or(())?;
                self.peers[index].leave(&case.room).await?;
                self.peers[index].join(&case.room).await?;
                let new = self.peers[index].info.as_ref().ok_or(())?;
                require(
                    new.membership_id != old.membership_id
                        && new.local_identity != old.local_identity,
                )?;
            }
        }
        self.key_receipts(case, owner, &rejoined, &mut keys).await?;
        fact("fresh_key_identities");
        // Memberships, native identities and real key-event metadata are now
        // bound. Install a new sink epoch before either new tone publication.
        stage("recovered_checkpoint");
        let after_audio = self.playback.checkpoint(Tone::After).map_err(|_| ())?;
        stage("recovered_audio_source");
        self.start_tones(case, owner, Tone::After).await?;
        self.media(case, owner, &mut video, true, Some(before_presentation))
            .await?;
        fact("new_epoch_media_all_peers");
        stage("recovered_audio");
        self.audio_evidence(case, &after_audio).await?;
        fact("native_audio_after");
        let barrier = self.sync_barrier(case, owner, &observer).await?;
        let observer_room = observer.get_room(&case.room).ok_or(())?;
        let creator = &case
            .fixture
            .accounts
            .iter()
            .find(|account| account.label == "a_primary")
            .ok_or(())?
            .user_id;
        history_notifications(
            &observer_room,
            &account,
            &notification,
            &barrier,
            &case.creation,
            creator,
        )
        .await?;
        require(selection.is_current() && self.calls.call_token() == stable_call)?;
        fact("no_extra_notification");
        fact(if case.dm {
            "current_dm"
        } else {
            "current_three_participant_room"
        });
        Ok(())
    }

    async fn start_tones(&mut self, case: &OutageCase, owner: usize, tone: Tone) -> Result<(), ()> {
        for (index, account) in case.fixture.accounts.iter().enumerate() {
            if index == owner {
                continue;
            }
            let identity = match account.label.as_str() {
                "a_primary" => TonePeer::Alice,
                "a_third_device" => TonePeer::Carol,
                _ => return Err(()),
            };
            self.peers[index]
                .start_audio(&case.room, identity, tone)
                .await?;
        }
        Ok(())
    }
    async fn stop_tones(&mut self, owner: usize) -> Result<(), ()> {
        // Poll every actual owner even if one source reported an error.
        let mut clean = true;
        for (index, peer) in self.peers.iter_mut().enumerate() {
            if index != owner {
                clean &= matches!(peer.stop_audio().await, Ok(true));
            }
        }
        require(clean)
    }
    async fn audio_evidence(
        &self,
        case: &OutageCase,
        checkpoint: &alsa_playback::Checkpoint,
    ) -> Result<(), ()> {
        let observed = self.playback.wait_for(checkpoint).await.map_err(|_| ())?;
        require(
            observed.frames >= 14_400
                && observed.matching_windows >= 3
                && observed.remote_tones == if case.dm { 1 } else { 2 },
        )
    }

    async fn media(
        &self,
        case: &OutageCase,
        owner: usize,
        video: &mut tokio::sync::watch::Receiver<VideoUpdate>,
        recovered: bool,
        old_presentation: Option<u64>,
    ) -> Result<u64, ()> {
        stage(if recovered {
            "recovered_media"
        } else {
            "initial_media"
        });
        let generation = self.calls.video_token();
        self.calls.set_stage_presented(true);
        let roster = self.calls.subscribe_roster();
        let mut sequence = 0u32;
        time::timeout(Duration::from_secs(45), async {
            loop {
                // Only these retained, current fixture publications receive
                // frames. No capture devices, files or owner media are opened.
                for (index, peer) in self.peers.iter().enumerate() {
                    if index != owner {
                        let level = (if recovered { 180u8 } else { 45 }) + index as u8 * 12;
                        peer.publication
                            .as_ref()
                            .ok_or(())?
                            .push_video(frame(sequence, level))
                            .map_err(|_| ())?;
                    }
                }
                sequence = sequence.checked_add(1).ok_or(())?;
                let snapshot = video.borrow_and_update().clone();
                if let VideoUpdate::Stage { token, stage } = snapshot {
                    if token == generation && old_presentation != Some(stage.presentation) {
                        let mut matched = HashSet::new();
                        for tile in stage
                            .layout
                            .featured
                            .iter()
                            .chain(stage.layout.tiles.iter())
                        {
                            if tile.is_local
                                || tile.key.medium != TileMedium::Camera
                                || tile.state != TileState::Live
                            {
                                continue;
                            }
                            for (index, peer) in self.peers.iter().enumerate() {
                                if index == owner {
                                    continue;
                                }
                                let info = peer.info.as_ref().ok_or(())?;
                                let account = peer.account.as_ref().ok_or(())?;
                                if tile.user_id == account.user_id
                                    && tile.key.member_id == info.membership_id
                                {
                                    let slot =
                                        stage.frames.get(tile.frame_slot.ok_or(())?).ok_or(())?;
                                    if slot.epoch == 0 || slot.sequence == 0 {
                                        continue;
                                    }
                                    if let Some(pixels) = &slot.pixels {
                                        let width = pixels.width();
                                        let height = pixels.height();
                                        if !(160..=320).contains(&width)
                                            || !(90..=180).contains(&height)
                                            || width * 9 != height * 16
                                        {
                                            continue;
                                        }
                                        let pixel = &pixels.as_slice()
                                            [(height / 2 * width + width / 2) as usize];
                                        // Widely separated epochs survive VP8 conversion/scaling.
                                        // A queued dark pre-outage picture cannot pass recovery.
                                        let expected =
                                            (if recovered { 180u8 } else { 45 }) + index as u8 * 12;
                                        let expected_rgb = (i32::from(expected) - 16) * 255 / 219;
                                        if [pixel.r, pixel.g, pixel.b].iter().all(|value| {
                                            (i32::from(*value) - expected_rgb).abs() <= 18
                                        }) {
                                            matched.insert(info.membership_id.clone());
                                        }
                                    }
                                }
                            }
                        }
                        if matched.len() == self.peers.len() - 1 {
                            let update = roster.borrow().clone();
                            let own = match self.calls.subscribe_status().borrow().clone() {
                                CallStatus::Joined { call, .. } => Some(call),
                                _ => None,
                            };
                            let complete = own.is_some_and(|own| {
                                update.token == generation
                                    && update.members.len() == self.peers.len()
                                    && self.peers.iter().enumerate().all(|(index, peer)| {
                                        let info = if index == owner {
                                            Some(&own)
                                        } else {
                                            peer.info.as_ref()
                                        };
                                        info.zip(peer.account.as_ref()).is_some_and(
                                            |(info, account)| {
                                                update
                                                    .members
                                                    .iter()
                                                    .filter(|member| {
                                                        member.member_id == info.membership_id
                                                            && member.user_id == account.user_id
                                                            && member.is_local == (index == owner)
                                                    })
                                                    .count()
                                                    == 1
                                            },
                                        )
                                    })
                            });
                            if complete {
                                require(case.dm == (matched.len() == 1))?;
                                return Ok::<_, ()>(stage.presentation);
                            }
                        }
                    }
                }
                require(self.calls.video_token() == generation)?;
                time::sleep(Duration::from_millis(67)).await;
            }
        })
        .await
        .map_err(|_| ())?
    }

    async fn key_receipts(
        &self,
        _case: &OutageCase,
        owner: usize,
        rejoined: &CallInfo,
        keys: &mut broadcast::Receiver<RecoveryKeyObservation>,
    ) -> Result<(), ()> {
        let expected: HashSet<_> = self
            .peers
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != owner)
            .map(|(_, peer)| {
                peer.info
                    .as_ref()
                    .map(|info| (info.membership_id.clone(), info.local_identity.clone()))
            })
            .collect::<Option<HashSet<_>>>()
            .ok_or(())?;
        require(expected.len() == self.peers.len() - 1 && !rejoined.local_identity.is_empty())?;
        let generation = self.calls.video_token();
        let mut imported = HashSet::new();
        time::timeout(Duration::from_secs(30), async {
            loop {
                // This is the real worker's native KeyImported event, not a
                // snapshot flag or a guessed counter increment. A fresh join
                // may legitimately restart its key index at zero.
                let receipt = keys.recv().await.map_err(|_| ())?;
                require(receipt.bounded)?;
                let identity = (receipt.member_id, receipt.identity);
                if receipt.presentation == generation && expected.contains(&identity) {
                    let _native_key_index = receipt.key_index;
                    imported.insert(identity);
                }
                if imported == expected {
                    return Ok::<_, ()>(());
                }
            }
        })
        .await
        .map_err(|_| ())?
    }

    async fn sync_barrier(
        &mut self,
        case: &OutageCase,
        owner: usize,
        observer: &Client,
    ) -> Result<OwnedEventId, ()> {
        stage("sync_barrier");
        require(self.barrier_handler.is_none())?;
        let sender = self.peers[owner].client.as_ref().ok_or(())?;
        let user = sender.user_id().ok_or(())?.to_owned();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let reply = Arc::new(StdMutex::new(Some(tx)));
        let handle = observer.add_room_event_handler(
            &case.room,
            move |event: OriginalSyncRoomMessageEvent| {
                let reply = reply.clone();
                let user = user.clone();
                async move {
                    if event.sender == user && event.content.body() == BARRIER {
                        if let Ok(mut reply) = reply.lock() {
                            if let Some(reply) = reply.take() {
                                let _ = reply.send(event.event_id);
                            }
                        }
                    }
                }
            },
        );
        self.barrier_handler = Some((observer.clone(), handle));
        let sent = sender
            .get_room(&case.room)
            .ok_or(())?
            .send(RoomMessageEventContent::text_plain(BARRIER))
            .await
            .map_err(|_| ())?;
        let received = time::timeout(Duration::from_secs(20), rx)
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        if let Some((client, handle)) = self.barrier_handler.take() {
            client.remove_event_handler(handle);
        }
        require(received == sent.response.event_id)?;
        Ok(received)
    }

    async fn cleanup(&mut self, case: &OutageCase) -> bool {
        stage("cleanup");
        if let Some((client, handle)) = self.barrier_handler.take() {
            client.remove_event_handler(handle);
        }
        let mut clean = !self.control_unproved;
        if let Some(command) = self.command.as_mut() {
            // Poll only the retained actual Child. try_wait reaps the owned
            // direct child; no numeric PID/name lookup or detached waiter.
            let child_clean = match command.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => {
                    let killed = command.kill().is_ok();
                    let reaped = matches!(
                        time::timeout(Duration::from_secs(10), async {
                            loop {
                                match command.try_wait() {
                                    Ok(Some(_)) => return true,
                                    Err(_) => return false,
                                    Ok(None) => time::sleep(Duration::from_millis(25)).await,
                                }
                            }
                        })
                        .await,
                        Ok(true)
                    );
                    killed && reaped
                }
                Err(_) => false,
            };
            clean &= child_clean;
            if child_clean {
                self.command = None;
            }
        }
        let app_clean = cleanup_attempt(Duration::from_secs(77), async {
            self.calls.end_call().await.is_ok()
        });
        let peers_clean = async {
            let results = futures_util::future::join_all(self.peers.iter_mut().map(|peer| async {
                cleanup_attempt(Duration::from_secs(40), async {
                    peer.leave(&case.room).await.is_ok()
                })
                .await
            }))
            .await;
            results.into_iter().all(|result| result)
        };
        let (app_clean, peers_clean) = tokio::join!(app_clean, peers_clean);
        clean &= app_clean && peers_clean;
        // Always observe the native adapter after attempting every app/peer
        // cleanup, even when another result failed. Its borrowed pipe ends
        // cannot close before actual production ALSA/device retirement.
        clean &= self.playback.finish().await.is_ok();
        // Do not revoke credentials under an unproved native leave. Preserve
        // all owners/private data for the mandatory outer namespace cleanup.
        if !clean {
            return false;
        }
        let results = futures_util::future::join_all(self.peers.iter().map(|peer| async {
            let logout = cleanup_attempt(Duration::from_secs(10), async {
                let mut logged_out = true;
                for account in peer.manager.accounts().await {
                    logged_out &= peer
                        .manager
                        .logout_account(&account.id)
                        .await
                        .is_ok_and(|result| result.remote_logout_succeeded);
                }
                logged_out
            })
            .await;
            let stopped = cleanup_attempt(Duration::from_secs(5), async {
                peer.manager.stop().await;
                true
            })
            .await;
            logout && stopped
        }))
        .await;
        clean && results.into_iter().all(|result| result)
    }
}

fn frame(index: u32, level: u8) -> VideoFrame {
    VideoFrame {
        buffer: I420Buffer {
            width: 320,
            height: 180,
            data_y: vec![level; 320 * 180],
            stride_y: 320,
            data_u: vec![128; 160 * 90],
            stride_u: 160,
            data_v: vec![128; 160 * 90],
            stride_v: 160,
        },
        rotation: VideoRotation::Deg0,
        timestamp_us: i64::from(index) * 66_667,
    }
}

async fn initial_notification(room: &Room, sender: &AccountInfo) -> Result<OwnedEventId, ()> {
    time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = room.sticky_events().keyless_live();
            require(!snapshot.invalidated && snapshot.events.len() <= 512)?;
            let mut notification = None;
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
                        && encryption.sender.as_str() == sender.user_id
                        && encryption
                            .sender_device
                            .as_deref()
                            .is_some_and(|device| device.as_str() == sender.device_id),
                )?;
                let raw: Value = serde_json::from_str(event.raw().json().get()).map_err(|_| ())?;
                let parsed =
                    RingNotification::parse(&raw, room.room_id().as_str()).map_err(|_| ())?;
                require(
                    parsed.sender() == sender.user_id
                        && parsed.event_id() == event.event_id.as_str()
                        && notification.is_none(),
                )?;
                notification = Some(event.event_id);
            }
            if let Some(notification) = notification {
                return Ok::<_, ()>(notification);
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| ())?
}

fn creation_matches(raw: &Value, creation: &OwnedEventId, creator: &str) -> Result<bool, ()> {
    if raw.get("type").and_then(Value::as_str) != Some("m.room.create") {
        return Ok(false);
    }
    require(
        raw.get("event_id").and_then(Value::as_str) == Some(creation.as_str())
            && raw.get("sender").and_then(Value::as_str) == Some(creator)
            && raw.get("state_key").and_then(Value::as_str) == Some("")
            && raw.pointer("/unsigned/redacted_because").is_none(),
    )?;
    Ok(true)
}

async fn verify_creation(room: &Room, creation: &OwnedEventId, creator: &str) -> Result<(), ()> {
    let event = time::timeout(
        Duration::from_secs(10),
        room.event(
            creation,
            Some(
                RequestConfig::new()
                    .retry_limit(0)
                    .timeout(Duration::from_secs(5)),
            ),
        ),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    require(event.raw().json().get().len() <= MAX_MEMBERSHIP_BYTES)?;
    let raw: Value = serde_json::from_str(event.raw().json().get()).map_err(|_| ())?;
    require(creation_matches(&raw, creation, creator)?)
}

async fn history_notifications(
    room: &Room,
    sender: &AccountInfo,
    initial: &OwnedEventId,
    barrier: &OwnedEventId,
    creation: &OwnedEventId,
    creator: &str,
) -> Result<(), ()> {
    stage("history");
    time::timeout(Duration::from_secs(30), async {
        let mut token = None;
        let mut tokens = HashSet::new();
        let mut events = HashSet::new();
        let mut notifications = 0;
        let mut saw_barrier = false;
        // No event-type filter: an undecryptable, missing or truncated interval
        // cannot be interpreted as absence of another notification.
        for _ in 0..16 {
            let mut options = MessagesOptions::backward();
            options.from = token;
            options.limit = matrix_sdk::ruma::uint!(64);
            let page = room.messages(options).await.map_err(|_| ())?;
            require(page.chunk.len() <= 64)?;
            let mut reached_creation = false;
            for event in page.chunk {
                require(!reached_creation)?;
                let raw = event.raw();
                require(raw.json().get().len() <= MAX_MEMBERSHIP_BYTES)?;
                let id: String = raw.get_field("event_id").map_err(|_| ())?.ok_or(())?;
                require(events.len() < 1024 && events.insert(id.clone()))?;
                saw_barrier |= id == barrier.as_str();
                let kind: String = raw.get_field("type").map_err(|_| ())?.ok_or(())?;
                if kind == "m.room.create" {
                    let envelope: Value = serde_json::from_str(raw.json().get()).map_err(|_| ())?;
                    reached_creation = creation_matches(&envelope, creation, creator)?;
                    require(reached_creation)?;
                }
                if id == initial.as_str() {
                    // The app may retract its original invitation on leaving
                    // the old join. Count that known authenticated event even
                    // if its ciphertext/content has since been redacted.
                    notifications += 1;
                } else {
                    require(kind != "m.room.encrypted")?;
                    if matches!(
                        kind.as_str(),
                        "m.rtc.notification" | "org.matrix.msc4075.rtc.notification"
                    ) {
                        notifications += 1;
                        let encryption = event.encryption_info().ok_or(())?;
                        require(encryption.sender.as_str() == sender.user_id)?;
                    }
                }
            }
            if reached_creation {
                return require(
                    saw_barrier && notifications == 1 && events.contains(initial.as_str()),
                );
            }
            match page.end {
                None => return Err(()), // Missing creation is not complete history.
                Some(next) => {
                    require(next.len() <= 4096 && tokens.insert(next.clone()))?;
                    token = Some(next);
                }
            }
        }
        Err(())
    })
    .await
    .map_err(|_| ())?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Requires the explicit owned Current-only SFU outage Matrix acceptance route"]
async fn current_call_recovers_from_owned_sfu_restart() {
    stage("fixture");
    let opened = OutageCase::read()
        .and_then(|case| OutageSession::open(&case).map(|session| (case, session)));
    let Ok((case, mut session)) = opened else {
        eprintln!("\nMORPHEUS_RTC_OUTAGE_RESULT passed=false");
        panic!("Owned SFU outage fixture rejected");
    };
    let observed = AssertUnwindSafe(time::timeout(CASE_WORK, session.run(&case)))
        .catch_unwind()
        .await;
    let failure = match observed {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(()))) => Some(WorkFailure::Error),
        Ok(Err(_)) => Some(WorkFailure::Timeout),
        Err(payload) => {
            // An arbitrary payload destructor must not run before mandatory
            // original cleanup or while reporting the fixed failure facts.
            std::mem::forget(payload);
            Some(WorkFailure::Panic)
        }
    };
    let passed = failure.is_none();
    if let Some(failure) = failure {
        let status = session.calls.subscribe_status();
        failed_work(failure, &status.borrow());
    }
    let clean = session.cleanup(&case).await;
    let clean = if clean {
        drop(session);
        case.profile.close().is_ok()
    } else {
        // Terminal uncertainty, not a retryable receipt or successful join:
        // retain the actual owners until outer PID-namespace destruction.
        std::mem::forget(session);
        let _private_retained = case.profile.keep();
        false
    };
    eprintln!("\nMORPHEUS_RTC_OUTAGE_CHECK cleanup={clean}");
    let passed = passed && clean;
    eprintln!("\nMORPHEUS_RTC_OUTAGE_RESULT passed={passed}");
    assert!(
        passed,
        "Owned SFU outage acceptance failed; see fixed stage facts"
    );
}

#[test]
fn history_requires_the_captured_creation_identity_not_just_the_creator() {
    let creation = EventId::parse("$creation:example.org").unwrap();
    let raw = serde_json::json!({"type":"m.room.create", "event_id":creation.as_str(),
        "sender":"@creator:example.org", "state_key":"", "content":{}});
    assert_eq!(
        creation_matches(&raw, &creation, "@creator:example.org"),
        Ok(true)
    );
    for (key, value) in [
        ("event_id", "$different:example.org"),
        ("sender", "@foreign:example.org"),
        ("state_key", "not-empty"),
    ] {
        let mut changed = raw.clone();
        changed[key] = Value::String(value.to_owned());
        assert!(creation_matches(&changed, &creation, "@creator:example.org").is_err());
    }
    let mut noncreation = raw.clone();
    noncreation["type"] = Value::String("m.room.message".to_owned());
    assert_eq!(
        creation_matches(&noncreation, &creation, "@creator:example.org"),
        Ok(false)
    );
}
