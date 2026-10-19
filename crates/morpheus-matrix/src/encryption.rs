//! Account-fenced device trust and verification controls for the native
//! encryption center. Every call names the account the UI showed; a switched
//! account is rejected before the SDK is touched. Nothing here verifies or
//! trusts a device or person by itself: trust changes only through a SAS flow
//! the user compared and confirmed on both sides.

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream::BoxStream};
use matrix_sdk::{
    Client, Room, RoomMemberships,
    encryption::{
        VerificationState,
        recovery::RecoveryState,
        verification::{SasVerification, VerificationRequest},
    },
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedDeviceId,
        events::{
            key::verification::{VerificationMethod, cancel::CancelCode},
            room::message::{MessageType, OriginalSyncRoomMessageEvent},
        },
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::{
    AccountManager, MatrixCoreError, RecoveryKey, Result, VerificationInfo, find_sas,
    find_verification_request, identity_warnings::bounded_name, parse_user_id, selected_loaded_for,
    verification_info,
};

/// Bound for one device-list snapshot shown in Settings.
const MAX_OWN_DEVICES: usize = 200;
/// Direct chats with one user checked before asking them to verify.
const MAX_DIRECT_CHATS: usize = 16;
/// Incoming verification requests from other people kept per account.
const MAX_PERSON_REQUESTS: usize = 8;

/// A verification request another person sent this account in a room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonVerificationRequest {
    pub user_id: String,
    /// Bounded, sanitized room display name, if the person has one.
    pub display_name: Option<String>,
    pub flow_id: String,
}

/// Follows verification requests other people send this account. Nothing is
/// accepted here. Dropping it removes the SDK event handler.
pub struct PersonRequestWatch {
    client: Client,
    handler: Option<EventHandlerHandle>,
    requests: Arc<watch::Sender<Vec<PersonVerificationRequest>>>,
}

impl PersonRequestWatch {
    pub fn subscribe(&self) -> watch::Receiver<Vec<PersonVerificationRequest>> {
        self.requests.subscribe()
    }

    /// Forget a request that ended, was reviewed or is no longer pending.
    pub fn retire(&self, flow_id: &str) {
        self.requests
            .send_modify(|known| known.retain(|request| request.flow_id != flow_id));
    }
}

impl Drop for PersonRequestWatch {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.client.remove_event_handler(handler);
        }
    }
}

/// What this device still needs before it can read encrypted history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceSetupNeed {
    /// Nothing, or the state is not known yet.
    None,
    /// This device is not verified: verify it from another device or enter
    /// the recovery key.
    Verify,
    /// This device is verified, but recovery is incomplete here.
    EnterRecoveryKey,
}

/// This device's setup need, plus whether the account has no recovery at
/// all: logging out the last device holding its keys then loses encrypted
/// history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceSetup {
    pub need: DeviceSetupNeed,
    pub recovery_off: bool,
}

enum DeviceStateChange {
    Verification(VerificationState),
    Recovery(RecoveryState),
}

fn device_setup_need(verification: VerificationState, recovery: RecoveryState) -> DeviceSetupNeed {
    match (verification, recovery) {
        (VerificationState::Unverified, _) => DeviceSetupNeed::Verify,
        (VerificationState::Verified, RecoveryState::Incomplete) => {
            DeviceSetupNeed::EnterRecoveryKey
        }
        _ => DeviceSetupNeed::None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceTrust {
    /// Signed by this account's cross-signing identity.
    Verified,
    /// Trusted only in this device's local store, not cross-signed.
    LocallyTrusted,
    Unverified,
    /// Explicitly blocked locally; it receives no room keys.
    Blocked,
    /// Signed in, but its encryption keys are not known to this device yet.
    NoKeys,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnDeviceInfo {
    pub device_id: String,
    pub display_name: Option<String>,
    pub is_current: bool,
    pub trust: DeviceTrust,
    /// Server-reported last activity, when the server device list loaded.
    pub last_seen_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCancelKind {
    User,
    Timeout,
    /// The compared values or keys did not match.
    Mismatch,
    /// Another device of this account answered the request.
    AcceptedElsewhere,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCancel {
    pub by_us: bool,
    pub kind: VerificationCancelKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationOutcome {
    pub info: VerificationInfo,
    /// Why the flow stopped, when it was cancelled.
    pub cancel: Option<VerificationCancel>,
}

/// A comparable summary of one self-verification request and its SAS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationSnapshot {
    /// The SDK still knows this request.
    pub found: bool,
    pub ready: bool,
    pub done: bool,
    pub cancelled: bool,
    pub sas_started: bool,
    pub sas_presentable: bool,
    pub sas_finished: bool,
}

impl VerificationSnapshot {
    /// No further change can follow: completed, cancelled or forgotten.
    pub fn finished(&self) -> bool {
        !self.found || self.done || self.cancelled
    }

    fn of(request: Option<&VerificationRequest>, sas: Option<&SasVerification>) -> Self {
        let Some(request) = request else {
            return Self::default();
        };
        Self {
            found: true,
            ready: request.is_ready(),
            done: request.is_done(),
            cancelled: request.is_cancelled(),
            sas_started: sas.is_some(),
            sas_presentable: sas.is_some_and(|sas| sas.can_be_presented()),
            sas_finished: sas.is_some_and(|sas| sas.is_done() || sas.is_cancelled()),
        }
    }
}

/// Shortest passphrase accepted for a room-key file.
pub const MIN_KEY_FILE_PASSPHRASE_CHARS: usize = 12;
/// Largest room-key file read for import.
const MAX_KEY_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// An export target must be a new or regular file, never a link, directory
/// or device: the SDK follows a link when it creates the file.
fn export_path_allowed(path: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata.file_type().is_file(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

impl AccountManager {
    /// Write this device's room keys for the account the UI showed to a file
    /// encrypted with the person's passphrase (the SDK's standard export).
    pub async fn export_room_keys_for(
        &self,
        expected_account_id: &str,
        path: PathBuf,
        passphrase: &str,
    ) -> Result<()> {
        if passphrase.chars().count() < MIN_KEY_FILE_PASSPHRASE_CHARS {
            return Err(MatrixCoreError::Sdk(
                "Use a passphrase of at least 12 characters.".into(),
            ));
        }
        if !export_path_allowed(&path) {
            return Err(MatrixCoreError::Sdk(
                "Choose a regular file to save the keys.".into(),
            ));
        }
        let client = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?
                .client
                .clone()
        };
        client
            .encryption()
            .export_room_keys(path, passphrase, |_| true)
            .await
            .map_err(|_| MatrixCoreError::Sdk("The room keys could not be exported.".into()))
    }

    /// Import room keys from a passphrase-protected key file. Returns how
    /// many keys were new and how many the file held.
    pub async fn import_room_keys_for(
        &self,
        expected_account_id: &str,
        path: PathBuf,
        passphrase: &str,
    ) -> Result<(usize, usize)> {
        let readable = std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= MAX_KEY_FILE_BYTES);
        // symlink_metadata does not follow links, so a link is refused here.
        if !readable {
            return Err(MatrixCoreError::Sdk(
                "Choose a room-key file to import.".into(),
            ));
        }
        let client = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?
                .client
                .clone()
        };
        let result = client
            .encryption()
            .import_room_keys(path, passphrase)
            .await
            .map_err(|_| MatrixCoreError::Sdk("Wrong passphrase, or not a key file.".into()))?;
        Ok((result.imported_count, result.total_count))
    }

    /// Replace the recovery key of the account the UI showed. Offered only
    /// while recovery is fully set up on this device (all cross-signing keys
    /// and the backup key are here), so every secret moves to the new key.
    /// The new key is returned only here and the previous key stops working;
    /// callers keep the account fixed until the person saved it.
    pub async fn change_recovery_key_for(&self, expected_account_id: &str) -> Result<RecoveryKey> {
        let client = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, expected_account_id)?
                .client
                .clone()
        };
        let recovery = client.encryption().recovery();
        if recovery.state() != RecoveryState::Enabled {
            return Err(MatrixCoreError::Sdk(
                "Recovery is not fully set up on this device.".into(),
            ));
        }
        let key = recovery
            .reset_key()
            .await
            .map_err(|_| MatrixCoreError::Sdk("The recovery key could not be changed.".into()))?;
        Ok(RecoveryKey {
            secret: Zeroizing::new(key),
        })
    }

    /// This account's signed-in devices and their trust. The server's device
    /// list decides which devices exist; the crypto store decides trust. When
    /// the server list is unavailable, known non-deleted crypto devices are
    /// listed without activity.
    pub async fn own_devices_for(&self, expected_account_id: &str) -> Result<Vec<OwnDeviceInfo>> {
        let client = self.encryption_client_for(expected_account_id).await?;
        let user_id = client
            .user_id()
            .ok_or(MatrixCoreError::NoActiveAccount)?
            .to_owned();
        let current = client.device_id().map(ToOwned::to_owned);
        let crypto = client
            .encryption()
            .get_user_devices(&user_id)
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        let trust_of = |device_id: &OwnedDeviceId| match crypto.get(device_id) {
            None => DeviceTrust::NoKeys,
            Some(device) if device.is_blacklisted() => DeviceTrust::Blocked,
            Some(device) if device.is_verified_with_cross_signing() => DeviceTrust::Verified,
            Some(device) if device.is_locally_trusted() => DeviceTrust::LocallyTrusted,
            Some(_) => DeviceTrust::Unverified,
        };
        let mut devices: Vec<OwnDeviceInfo> = match client.devices().await {
            Ok(response) => response
                .devices
                .into_iter()
                .map(|device| OwnDeviceInfo {
                    trust: trust_of(&device.device_id),
                    is_current: current.as_ref() == Some(&device.device_id),
                    device_id: device.device_id.to_string(),
                    display_name: device.display_name,
                    last_seen_ms: device.last_seen_ts.map(|seen| u64::from(seen.0)),
                })
                .collect(),
            Err(_) => crypto
                .devices()
                .filter(|device| !device.is_deleted() && !device.is_dehydrated())
                .map(|device| {
                    let device_id = device.device_id().to_owned();
                    OwnDeviceInfo {
                        trust: trust_of(&device_id),
                        is_current: current.as_ref() == Some(&device_id),
                        device_id: device_id.to_string(),
                        display_name: device.display_name().map(ToOwned::to_owned),
                        last_seen_ms: None,
                    }
                })
                .collect(),
        };
        sort_devices(&mut devices);
        devices.truncate(MAX_OWN_DEVICES);
        Ok(devices)
    }

    /// Ask one other device of this account for SAS verification. Either side
    /// may be the verified one; the comparison still needs both people's
    /// explicit confirmation before any signature is made.
    pub async fn request_device_verification_for(
        &self,
        expected_account_id: &str,
        device_id: &str,
    ) -> Result<VerificationInfo> {
        let state = self.state.lock().await;
        let client = &selected_loaded_for(&state, expected_account_id)?.client;
        let user_id = client.user_id().ok_or(MatrixCoreError::NoActiveAccount)?;
        let device_id: OwnedDeviceId = device_id.into();
        if client.device_id() == Some(&*device_id) {
            return Err(MatrixCoreError::Sdk(
                "this device cannot verify itself".into(),
            ));
        }
        let device = client
            .encryption()
            .get_device(user_id, &device_id)
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
            .ok_or_else(|| MatrixCoreError::Sdk("device keys are not available yet".into()))?;
        if device.is_blacklisted() {
            return Err(MatrixCoreError::Sdk("this device is blocked".into()));
        }
        let request = device
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        Ok(verification_info(&request))
    }

    /// Ask another user to verify identities with this account by comparing
    /// emoji or numbers. Morpheus never creates a direct chat or invites
    /// anyone for this: every direct chat with them must already include
    /// them, so the SDK neither invites them back nor creates a room.
    pub async fn request_user_verification_for(
        &self,
        expected_account_id: &str,
        user_id: &str,
    ) -> Result<VerificationInfo> {
        let client = self.encryption_client_for(expected_account_id).await?;
        let user = parse_user_id(user_id)?;
        if client.user_id() == Some(&*user) {
            return Err(MatrixCoreError::Sdk(
                "Verify your own devices from Verify this device.".into(),
            ));
        }
        let rooms: Vec<Room> = client.get_dm_rooms(&user).take(MAX_DIRECT_CHATS).collect();
        if rooms.is_empty() {
            return Err(MatrixCoreError::Sdk(
                "Start a direct chat with them first, then verify.".into(),
            ));
        }
        for room in &rooms {
            let members = room
                .members(RoomMemberships::ACTIVE)
                .await
                .map_err(|_| MatrixCoreError::Sdk("Room members are unavailable.".into()))?;
            if !members.iter().any(|member| member.user_id() == &*user) {
                return Err(MatrixCoreError::Sdk(
                    "A direct chat with them no longer includes them. Invite them back first."
                        .into(),
                ));
            }
        }
        let identity = client
            .encryption()
            .get_user_identity(&user)
            .await
            .map_err(|_| MatrixCoreError::Sdk("Their identity is unavailable.".into()))?
            .ok_or_else(|| MatrixCoreError::Sdk("Their identity is not known yet.".into()))?;
        let request = identity
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await
            .map_err(|_| MatrixCoreError::Sdk("The verification request was not sent.".into()))?;
        Ok(verification_info(&request))
    }

    /// Follow verification requests other people send this account in its
    /// rooms, newest last, at most [`MAX_PERSON_REQUESTS`]. Only pending
    /// requests addressed to this account are kept; the person reviews each.
    pub async fn watch_person_verification_requests_for(
        &self,
        expected_account_id: &str,
    ) -> Result<PersonRequestWatch> {
        let client = self.encryption_client_for(expected_account_id).await?;
        let own = client
            .user_id()
            .ok_or(MatrixCoreError::NoActiveAccount)?
            .to_owned();
        let (sender, _) = watch::channel(Vec::<PersonVerificationRequest>::new());
        let requests = Arc::new(sender);
        let sender = requests.clone();
        let handler = client.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
                let own = own.clone();
                let sender = sender.clone();
                async move {
                    let MessageType::VerificationRequest(content) = &event.content.msgtype else {
                        return;
                    };
                    if event.sender == own || content.to != own {
                        return;
                    }
                    let flow_id = event.event_id.to_string();
                    let Some(request) = client
                        .encryption()
                        .get_verification_request(&event.sender, &flow_id)
                        .await
                    else {
                        return;
                    };
                    let info = verification_info(&request);
                    if info.we_started || info.is_done || info.is_cancelled {
                        return;
                    }
                    let display_name = match room.get_member_no_sync(&event.sender).await {
                        Ok(Some(member)) => member.display_name().and_then(bounded_name),
                        _ => None,
                    };
                    let request = PersonVerificationRequest {
                        user_id: event.sender.to_string(),
                        display_name,
                        flow_id,
                    };
                    sender.send_modify(|known| remember_person_request(known, request));
                }
            },
        );
        Ok(PersonRequestWatch {
            client,
            handler: Some(handler),
            requests,
        })
    }

    /// Report that the compared SAS values differ. The SDK cancels the flow
    /// with a mismatch reason on both devices; nothing is signed.
    pub async fn sas_mismatch_for(
        &self,
        expected_account_id: &str,
        user_id: &str,
        flow_id: &str,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded_for(&state, expected_account_id)?.client;
        find_sas(client, user_id, flow_id)
            .await?
            .mismatch()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
    }

    /// Decline a verification request on this exact account. The SDK tells
    /// the other side the user cancelled; nothing is signed or trusted.
    pub async fn decline_verification_for(
        &self,
        expected_account_id: &str,
        user_id: &str,
        flow_id: &str,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let client = &selected_loaded_for(&state, expected_account_id)?.client;
        find_verification_request(client, user_id, flow_id)
            .await?
            .cancel()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
    }

    /// Follow what this device still needs (verification, or the recovery
    /// key) and whether recovery is off, through the SDK's state
    /// subscribers: the current state first, then every change. Nothing is
    /// changed here.
    pub async fn watch_device_setup_for(
        &self,
        expected_account_id: &str,
    ) -> Result<BoxStream<'static, DeviceSetup>> {
        let client = self.encryption_client_for(expected_account_id).await?;
        let encryption = client.encryption();
        let verification = encryption
            .verification_state()
            .map(DeviceStateChange::Verification);
        let recovery = encryption
            .recovery()
            .state_stream()
            .map(DeviceStateChange::Recovery);
        let changes = futures_util::stream::select(verification, recovery);
        let start = (VerificationState::Unknown, RecoveryState::Unknown);
        let needs = changes.scan(start, |latest, change| {
            match change {
                DeviceStateChange::Verification(state) => latest.0 = state,
                DeviceStateChange::Recovery(state) => latest.1 = state,
            }
            std::future::ready(Some(DeviceSetup {
                need: device_setup_need(latest.0, latest.1),
                recovery_off: latest.1 == RecoveryState::Disabled,
            }))
        });
        Ok(needs.boxed())
    }

    /// Request progress plus the reason a cancelled flow stopped, so the UI
    /// can say whether it timed out, was declined or the values differed.
    pub async fn verification_outcome_for(
        &self,
        expected_account_id: &str,
        user_id: &str,
        flow_id: &str,
    ) -> Result<VerificationOutcome> {
        let state = self.state.lock().await;
        let client = &selected_loaded_for(&state, expected_account_id)?.client;
        let request = find_verification_request(client, user_id, flow_id).await?;
        let info = verification_info(&request);
        let cancel_info = match request.cancel_info() {
            Some(cancel) => Some(cancel),
            None => find_sas(client, user_id, flow_id)
                .await
                .ok()
                .and_then(|sas| sas.cancel_info()),
        };
        let cancel = cancel_info.map(|cancel| VerificationCancel {
            by_us: cancel.cancelled_by_us(),
            kind: cancel_kind(cancel.cancel_code()),
        });
        Ok(VerificationOutcome { info, cancel })
    }

    /// Wait until this request or its SAS differs from `seen`, at most
    /// `limit`, using the SDK's change streams rather than polling. Returns
    /// the latest snapshot; it equals `seen` only when the limit passed. A
    /// default `seen` returns the current snapshot at once.
    pub async fn wait_verification_change_for(
        &self,
        expected_account_id: &str,
        user_id: &str,
        flow_id: &str,
        seen: VerificationSnapshot,
        limit: Duration,
    ) -> Result<VerificationSnapshot> {
        let client = self.encryption_client_for(expected_account_id).await?;
        let user_id = parse_user_id(user_id)?;
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let encryption = client.encryption();
            let request = encryption
                .get_verification_request(&user_id, flow_id)
                .await;
            let sas = encryption
                .get_verification(&user_id, flow_id)
                .await
                .and_then(|verification| verification.sas());
            // Subscribe before reading, so a change between the read and the
            // wait is still delivered.
            let mut request_changes = request
                .as_ref()
                .map(|request| request.changes().map(|_| ()).boxed());
            let mut sas_changes = sas.as_ref().map(|sas| sas.changes().map(|_| ()).boxed());
            let current = VerificationSnapshot::of(request.as_ref(), sas.as_ref());
            if current != seen || current.finished() {
                return Ok(current);
            }
            let changed = tokio::select! {
                Some(()) = next_change(&mut request_changes) => true,
                Some(()) = next_change(&mut sas_changes) => true,
                () = tokio::time::sleep_until(deadline) => false,
            };
            if !changed {
                return Ok(current);
            }
        }
    }

    async fn encryption_client_for(&self, expected_account_id: &str) -> Result<Client> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, expected_account_id)?;
        Ok(loaded.client.clone())
    }
}

async fn next_change(changes: &mut Option<BoxStream<'static, ()>>) -> Option<()> {
    match changes {
        Some(changes) => changes.next().await,
        None => std::future::pending().await,
    }
}

fn cancel_kind(code: &CancelCode) -> VerificationCancelKind {
    match code {
        CancelCode::User => VerificationCancelKind::User,
        CancelCode::Timeout => VerificationCancelKind::Timeout,
        CancelCode::KeyMismatch | CancelCode::MismatchedCommitment | CancelCode::MismatchedSas => {
            VerificationCancelKind::Mismatch
        }
        CancelCode::Accepted => VerificationCancelKind::AcceptedElsewhere,
        _ => VerificationCancelKind::Other,
    }
}

/// This device first, then devices that still need verification, then the
/// most recently active; the device ID keeps the order stable.
fn sort_devices(devices: &mut [OwnDeviceInfo]) {
    devices.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then_with(|| needs_attention(b.trust).cmp(&needs_attention(a.trust)))
            .then_with(|| b.last_seen_ms.cmp(&a.last_seen_ms))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });
}

fn needs_attention(trust: DeviceTrust) -> bool {
    matches!(trust, DeviceTrust::Unverified | DeviceTrust::NoKeys)
}

/// One entry per flow, newest last, at most [`MAX_PERSON_REQUESTS`]; a
/// repeated flow moves to the end instead of appearing twice.
fn remember_person_request(
    known: &mut Vec<PersonVerificationRequest>,
    request: PersonVerificationRequest,
) {
    known.retain(|existing| existing.flow_id != request.flow_id);
    known.push(request);
    if known.len() > MAX_PERSON_REQUESTS {
        known.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_key_files_are_never_written_into_directories() {
        let directory = std::env::temp_dir();
        assert!(!export_path_allowed(&directory));
        let missing = directory.join("no-such-morpheus-dir").join("keys");
        assert!(export_path_allowed(&missing));
    }

    fn device(id: &str, current: bool, trust: DeviceTrust, seen: Option<u64>) -> OwnDeviceInfo {
        OwnDeviceInfo {
            device_id: id.into(),
            display_name: None,
            is_current: current,
            trust,
            last_seen_ms: seen,
        }
    }

    #[test]
    fn device_list_puts_this_device_then_unverified_then_recent() {
        let mut devices = vec![
            device("OLD", false, DeviceTrust::Verified, Some(1)),
            device("NEW", false, DeviceTrust::Verified, Some(9)),
            device("RISK", false, DeviceTrust::Unverified, Some(2)),
            device("SELF", true, DeviceTrust::Verified, None),
            device("NOKEY", false, DeviceTrust::NoKeys, None),
        ];
        sort_devices(&mut devices);
        let order: Vec<_> = devices.iter().map(|d| d.device_id.as_str()).collect();
        assert_eq!(order, ["SELF", "RISK", "NOKEY", "NEW", "OLD"]);
    }

    #[test]
    fn mismatches_and_timeouts_are_reported_distinctly() {
        for (code, kind) in [
            (CancelCode::Timeout, VerificationCancelKind::Timeout),
            (CancelCode::User, VerificationCancelKind::User),
            (CancelCode::MismatchedSas, VerificationCancelKind::Mismatch),
            (CancelCode::KeyMismatch, VerificationCancelKind::Mismatch),
            (CancelCode::MismatchedCommitment, VerificationCancelKind::Mismatch),
            (CancelCode::Accepted, VerificationCancelKind::AcceptedElsewhere),
            (CancelCode::UnknownMethod, VerificationCancelKind::Other),
        ] {
            assert_eq!(cancel_kind(&code), kind, "{code:?}");
        }
    }

    #[test]
    fn device_setup_asks_only_for_what_this_device_still_needs() {
        use DeviceSetupNeed::{EnterRecoveryKey, Verify};
        use RecoveryState::{Disabled, Enabled, Incomplete, Unknown};

        let ready = DeviceSetupNeed::None;
        let unverified = VerificationState::Unverified;
        for recovery in [Unknown, Enabled, Disabled, Incomplete] {
            assert_eq!(device_setup_need(unverified, recovery), Verify);
            // Nothing is asked while the device state is still loading.
            let checking = device_setup_need(VerificationState::Unknown, recovery);
            assert_eq!(checking, ready, "{recovery:?}");
        }
        let verified = VerificationState::Verified;
        assert_eq!(device_setup_need(verified, Incomplete), EnterRecoveryKey);
        for recovery in [Unknown, Enabled, Disabled] {
            assert_eq!(device_setup_need(verified, recovery), ready);
        }
    }

    fn person_request(flow: usize) -> PersonVerificationRequest {
        PersonVerificationRequest {
            user_id: "@member:example.org".into(),
            display_name: None,
            flow_id: format!("$flow{flow}"),
        }
    }

    #[test]
    fn person_requests_keep_one_entry_per_flow_and_stay_bounded() {
        let mut known = Vec::new();
        for flow in 0..MAX_PERSON_REQUESTS + 3 {
            remember_person_request(&mut known, person_request(flow));
        }
        assert_eq!(known.len(), MAX_PERSON_REQUESTS);
        assert_eq!(known[0], person_request(3), "the oldest drop first");
        // A repeated delivery of a kept flow moves it to the end, never twice.
        remember_person_request(&mut known, person_request(5));
        assert_eq!(known.len(), MAX_PERSON_REQUESTS);
        assert_eq!(known.last(), Some(&person_request(5)));
        let repeats = known.iter().filter(|r| r.flow_id == "$flow5").count();
        assert_eq!(repeats, 1);
    }
}
