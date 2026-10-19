//! Account-fenced device trust and verification controls for the native
//! encryption center. Every call names the account the UI showed; a switched
//! account is rejected before the SDK is touched. Nothing here verifies or
//! trusts a device by itself: trust changes only through a SAS flow the user
//! compared and confirmed on both devices.

use std::time::Duration;

use futures_util::{StreamExt, stream::BoxStream};
use matrix_sdk::{
    Client,
    encryption::verification::{SasVerification, VerificationRequest},
    ruma::{
        OwnedDeviceId,
        events::key::verification::{VerificationMethod, cancel::CancelCode},
    },
};
use serde::{Deserialize, Serialize};

use crate::{
    AccountManager, MatrixCoreError, Result, VerificationInfo, find_sas, find_verification_request,
    parse_user_id, selected_loaded_for, verification_info,
};

/// Bound for one device-list snapshot shown in Settings.
const MAX_OWN_DEVICES: usize = 200;

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

impl AccountManager {
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
        ensure_own_user(client, user_id)?;
        find_sas(client, user_id, flow_id)
            .await?
            .mismatch()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
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
        ensure_own_user(client, user_id)?;
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
        ensure_own_user(&client, user_id)?;
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

/// Encryption-center flows verify devices of the signed-in account only.
fn ensure_own_user(client: &Client, user_id: &str) -> Result<()> {
    let user_id = parse_user_id(user_id)?;
    if client.user_id() != Some(&*user_id) {
        return Err(MatrixCoreError::Sdk(
            "verification belongs to another user".into(),
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
