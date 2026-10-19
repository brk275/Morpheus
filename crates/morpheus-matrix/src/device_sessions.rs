//! The signed-in account's devices and sessions: a bounded roster from the
//! homeserver's device list and this device's crypto store, renaming, a
//! verification request for the existing SAS comparison, and signing out
//! another session with the server's user-interactive authentication.
//!
//! A [`DeviceSessions`] view belongs to one exact [`AccountSelection`] and one
//! [`DeviceSessionCancellation`]. Both are checked immediately before every
//! request leaves and again before any result is returned, so a switch,
//! removal, logout, an A -> B -> A reselection or shutdown ends the work and
//! nothing is sent for, or reported to, another account. A row grants
//! nothing: each change names a device ID that is checked against a fresh
//! server list of this account first. This device is never signed out here;
//! the normal guarded logout owns that. Nothing is verified or trusted
//! automatically. Passwords and server authentication sessions are never
//! logged, serialized, stored or put into error text, and the IP addresses
//! the server reports are dropped as soon as the list arrives.

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    AuthApi, Client, HttpError,
    config::RequestConfig,
    encryption::{VerificationState, identities::Device},
    ruma::{
        DeviceId, OwnedDeviceId, OwnedUserId,
        api::{
            client::{
                device::{self, delete_devices, get_devices, update_device},
                discovery::get_authorization_server_metadata::v1::{
                    AccountManagementActionData, DeviceDeleteData,
                },
                uiaa::{
                    AuthData, FallbackAcknowledgement, MatrixUserIdentifier, Password, UiaaInfo,
                    UserIdentifier,
                },
            },
            error::ErrorKind,
        },
        events::key::verification::VerificationMethod,
    },
};
use tokio::{
    sync::{Mutex, Notify},
    time,
};
use url::Url;
use zeroize::Zeroizing;

use crate::{
    AccountManager, AccountSelection, VerificationInfo, account_access, selected_loaded_for,
    verification_info,
};

/// Sessions one roster lists; the rest are counted.
pub const MAX_DEVICE_SESSIONS: usize = 200;
/// Longest device name Morpheus shows or sends, in characters.
pub const MAX_DEVICE_NAME_CHARS: usize = 64;
// Longest device ID Morpheus accepts from the person or the server.
const MAX_DEVICE_ID_BYTES: usize = 255;
// Reject an oversized or ambiguous server roster instead of truncating it:
// a partial read must never be used as proof that a session was signed out.
const MAX_SERVER_DEVICE_SESSIONS: usize = 4096;
const MAX_DEVICE_NAME_INPUT_BYTES: usize = 4096;
const MAX_AUTH_SESSION_BYTES: usize = 4096;
const MAX_AUTH_FLOWS: usize = 32;
const MAX_BROWSER_PAGE_BYTES: usize = 16 * 1024;
// Bound raw device-list and UIAA JSON before SDK deserialization, separately
// from the roster/field limits below. Sync and media keep their own policies.
const MAX_DEVICE_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

const PASSWORD_STAGE: &str = "m.login.password";
const SSO_STAGE: &str = "m.login.sso";

/// Why a device-session request did not complete. The text is fixed: no
/// server message, identifier or secret is ever part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeviceSessionError {
    #[error("This device list is closed")]
    Cancelled,
    #[error("The selected account changed")]
    AccountChanged,
    #[error("The session list is unavailable; refresh before retrying")]
    Unavailable,
    #[error("The homeserver is limiting requests; wait, then refresh")]
    RateLimited,
    #[error("The request timed out; refresh before retrying")]
    TimedOut,
    #[error("That session is no longer signed in to this account")]
    NotFound,
    #[error("This device is signed out with Log out, not from the session list")]
    CurrentDevice,
    #[error("Enter a device name of at most 64 characters")]
    InvalidName,
    #[error("The homeserver refused this change")]
    Forbidden,
    #[error("The homeserver does not support this change")]
    Unsupported,
    #[error("Verify this device before verifying another session")]
    ThisDeviceUnverified,
    #[error("That session has no encryption keys to verify")]
    NoKeys,
    #[error("That session is already verified")]
    AlreadyVerified,
    #[error("This sign-out step belongs to another session or account")]
    ChallengeMismatch,
}

pub type Result<T> = std::result::Result<T, DeviceSessionError>;

/// Ends one device-session view and everything it is waiting for. A request
/// already handed to the homeserver may still apply.
#[derive(Clone, Default)]
pub struct DeviceSessionCancellation(Arc<CancellationState>);

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl DeviceSessionCancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    /// Wait until this operation or view is cancelled. Registration precedes
    /// the state check, so cancellation cannot be missed while going to sleep.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register before checking: notify_waiters keeps no permit.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// How far this device can vouch for a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceTrust {
    /// Cross-signed by the account, whose identity this device trusts.
    Verified,
    /// Trusted only on this device (an earlier manual verification).
    LocallyTrusted,
    /// Cross-signed by the account, but this device cannot confirm it yet
    /// because this device is not verified itself.
    SignedUnconfirmed,
    /// Has encryption keys and is not verified.
    Unverified,
    /// Blocked on this device.
    Blocked,
    /// Signed in, but it published no encryption keys this device knows of.
    NoKeys,
    /// This device could not read its encryption store.
    Unknown,
}

/// The account's cross-signing identity as this device sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossSigningState {
    /// This device holds every cross-signing key and trusts the identity.
    Ready,
    /// The account has an identity this device does not hold or trust yet.
    NotReadyHere,
    /// No cross-signing identity is known for the account.
    NotSetUp,
    /// This device could not read it.
    Unknown,
}

/// How another session is signed out for this account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignOutMethod {
    /// The homeserver's user-interactive authentication.
    Interactive,
    /// The account's sign-in provider manages sessions on its own page.
    ProviderPage,
}

/// One signed-in session of the account. Display text is already bounded
/// and free of control and direction-override characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceSession {
    pub device_id: String,
    /// `None` when the session has no name.
    pub name: Option<String>,
    /// The device this app is running on.
    pub current: bool,
    pub trust: DeviceTrust,
    /// Last activity the homeserver reported, in milliseconds since the Unix
    /// epoch. The address it also reports is never kept.
    pub last_seen_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceSessionSnapshot {
    pub user_id: String,
    pub current_device_id: String,
    /// This device first, then the most recently active. At most
    /// `MAX_DEVICE_SESSIONS`.
    pub sessions: Vec<DeviceSession>,
    /// Sessions the server listed beyond the bound.
    pub omitted: usize,
    pub cross_signing: CrossSigningState,
    /// `None` when this device's own verification state is not known yet.
    pub this_device_verified: Option<bool>,
    pub sign_out: SignOutMethod,
}

impl DeviceSessionSnapshot {
    pub fn session(&self, device_id: &str) -> Option<&DeviceSession> {
        self.sessions.iter().find(|row| row.device_id == device_id)
    }
}

/// The outcome of a rename the homeserver accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameOutcome {
    /// Reading the list back showed the new name.
    pub confirmed: bool,
    /// The fresh list, when it could be read.
    pub snapshot: Option<DeviceSessionSnapshot>,
}

/// What signing out another session needs next. Deliberately not `Debug`: a
/// browser page carries the server's authentication session.
pub enum SignOutStep {
    /// The homeserver signed the session out. `gone` is whether a fresh list
    /// no longer shows it.
    Done { gone: bool },
    /// Enter this account's password to authorize it.
    Password(SignOutChallenge),
    /// Finish the homeserver's page in the system browser, then continue.
    Browser {
        challenge: SignOutChallenge,
        page: Url,
    },
    /// The sign-in provider signs sessions out on its own page; Morpheus
    /// cannot observe the result and refreshes the list instead.
    ProviderPage { page: Url },
    /// The homeserver asks for something this app cannot do honestly.
    Unsupported(SignOutBlocker),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignOutBlocker {
    /// Several stages, or a kind of authentication this app does not offer.
    UnsupportedAuthentication,
    /// The page the server named is not HTTPS (or loopback development).
    InsecurePage,
    /// The sign-in provider names no page for managing sessions.
    NoProviderPage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChallengeKind {
    Password,
    Browser,
}

/// One server authentication session for signing out one session, bound to
/// the account selection and device it started for. Opaque, single use (each
/// step consumes it) and never shown, logged or serialized.
pub struct SignOutChallenge {
    selection: AccountSelection,
    device_id: OwnedDeviceId,
    session: Zeroizing<String>,
    kind: ChallengeKind,
    rejected: bool,
}

impl fmt::Debug for SignOutChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SignOutChallenge { .. }")
    }
}

impl SignOutChallenge {
    pub fn device_id(&self) -> &str {
        self.device_id.as_str()
    }
    /// The server refused the previous password.
    pub fn rejected(&self) -> bool {
        self.rejected
    }
}

/// The account's sessions, for one account selection.
pub struct DeviceSessions {
    selection: AccountSelection,
    cancellation: DeviceSessionCancellation,
    client: Client,
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
    sign_out: SignOutMethod,
    // Rename, verification and sign-out run one at a time.
    operation: Mutex<()>,
}

impl AccountManager {
    /// Open the session manager for exactly `selection`. Refused when that
    /// selection is no longer current, the account is being removed, or it
    /// is not the active account.
    pub async fn open_device_sessions_for(
        &self,
        selection: &AccountSelection,
        cancellation: DeviceSessionCancellation,
    ) -> Result<DeviceSessions> {
        scope_error(selection, &cancellation)?;
        let client = {
            let state = self.state.lock().await;
            let current = self.account_selection(selection.account_id());
            let current = current.ok_or(DeviceSessionError::AccountChanged)?;
            if !selection.same_epoch(&current) {
                return Err(DeviceSessionError::AccountChanged);
            }
            account_access::ensure_eligible(&state, selection.account_id())
                .map_err(|_| DeviceSessionError::AccountChanged)?;
            let loaded = selected_loaded_for(&state, selection.account_id())
                .map_err(|_| DeviceSessionError::AccountChanged)?;
            loaded.client.clone()
        };
        let user_id = client.user_id().ok_or(DeviceSessionError::Unavailable)?;
        let user_id = user_id.to_owned();
        let device_id = client.device_id().ok_or(DeviceSessionError::Unavailable)?;
        let device_id = device_id.to_owned();
        let sign_out = match client.auth_api() {
            Some(AuthApi::OAuth(_)) => SignOutMethod::ProviderPage,
            _ => SignOutMethod::Interactive,
        };
        scope_error(selection, &cancellation)?;
        Ok(DeviceSessions {
            selection: selection.clone(),
            cancellation,
            client,
            user_id,
            device_id,
            sign_out,
            operation: Mutex::new(()),
        })
    }
}

impl DeviceSessions {
    pub fn account_id(&self) -> &str {
        self.selection.account_id()
    }

    /// Read the server's list and this device's view of each session.
    pub async fn refresh(&self) -> Result<DeviceSessionSnapshot> {
        let listed = self.guarded(READ_TIMEOUT, self.server_sessions()).await?;
        self.compose(listed).await
    }

    /// Rename one session of this account, this device included. The name
    /// is bounded and stripped of control and direction-override characters.
    pub async fn rename(&self, device_id: &str, name: &str) -> Result<RenameOutcome> {
        let name = bounded_device_name(name);
        let name = name.ok_or(DeviceSessionError::InvalidName)?;
        let device_id = parse_device_id(device_id)?;
        let _operation = self.guarded(READ_TIMEOUT, self.lock_operation()).await?;
        let listed = self.guarded(READ_TIMEOUT, self.server_sessions()).await?;
        let found = find(&listed, device_id.as_str());
        let found = found.ok_or(DeviceSessionError::NotFound)?;
        if found.name.as_deref() == Some(name.as_str()) {
            let snapshot = self.compose(listed).await.ok();
            self.ensure_scope()?;
            return Ok(RenameOutcome {
                confirmed: true,
                snapshot,
            });
        }
        let mut request = update_device::v3::Request::new(device_id.clone());
        request.display_name = Some(name.clone());
        let sent = async {
            let request = self.client.send(request).with_request_config(single_send());
            match request.await {
                Ok(_) => Ok(()),
                Err(error) => Err(request_error(&error)),
            }
        };
        self.guarded(WRITE_TIMEOUT, sent).await?;
        // Accepted. A failed read-back is reported as unconfirmed.
        let after = self.guarded(READ_TIMEOUT, self.server_sessions()).await;
        let after = match after {
            Err(DeviceSessionError::Cancelled) => return Err(DeviceSessionError::Cancelled),
            Err(DeviceSessionError::AccountChanged) => {
                return Err(DeviceSessionError::AccountChanged);
            }
            Err(_) => None,
            Ok(after) => Some(after),
        };
        let confirmed = after.as_ref().is_some_and(|after| {
            let shown = find(after, device_id.as_str());
            shown.is_some_and(|session| session.name.as_deref() == Some(name.as_str()))
        });
        let snapshot = match after {
            Some(after) => self.compose(after).await.ok(),
            None => None,
        };
        self.ensure_scope()?;
        Ok(RenameOutcome {
            confirmed,
            snapshot,
        })
    }

    /// Ask another session of this account to verify with this device. The
    /// returned flow is completed by the existing SAS comparison; nothing is
    /// trusted until the person compares and confirms on both devices.
    pub async fn request_verification(&self, device_id: &str) -> Result<VerificationInfo> {
        let device_id = parse_device_id(device_id)?;
        if device_id == self.device_id {
            return Err(DeviceSessionError::CurrentDevice);
        }
        let _operation = self.guarded(READ_TIMEOUT, self.lock_operation()).await?;
        let listed = self.guarded(READ_TIMEOUT, self.server_sessions()).await?;
        if find(&listed, device_id.as_str()).is_none() {
            return Err(DeviceSessionError::NotFound);
        }
        let encryption = self.client.encryption();
        if encryption.verification_state().get() != VerificationState::Verified {
            return Err(DeviceSessionError::ThisDeviceUnverified);
        }
        let device = encryption.get_device(&self.user_id, &device_id).await;
        let device = device.map_err(|_| DeviceSessionError::Unavailable)?;
        let device = device.ok_or(DeviceSessionError::NoKeys)?;
        if device.is_verified() {
            return Err(DeviceSessionError::AlreadyVerified);
        }
        let methods = vec![VerificationMethod::SasV1];
        let request = async {
            let request = device.request_verification_with_methods(methods).await;
            request.map_err(|_| DeviceSessionError::Unavailable)
        };
        let request = self.guarded(WRITE_TIMEOUT, request).await?;
        Ok(verification_info(&request))
    }

    /// Start signing out another session, after the person confirmed it.
    /// This device is refused; it signs out with the normal Log out.
    pub async fn begin_sign_out(&self, device_id: &str) -> Result<SignOutStep> {
        let device_id = parse_device_id(device_id)?;
        let _operation = self.guarded(READ_TIMEOUT, self.lock_operation()).await?;
        let listed = self.guarded(READ_TIMEOUT, self.server_sessions()).await?;
        let listed: Vec<&str> = listed.iter().map(|row| row.device_id.as_str()).collect();
        sign_out_target(&device_id, &self.device_id, &listed)?;
        if self.sign_out == SignOutMethod::ProviderPage {
            let page = self
                .guarded(READ_TIMEOUT, self.provider_page(&device_id))
                .await?;
            return Ok(page);
        }
        let request = delete_devices::v3::Request::new(vec![device_id.clone()]);
        self.send_sign_out(device_id, request).await
    }

    /// Continue a password challenge with the person's explicit input. An
    /// empty password returns the same step without contacting the server.
    pub async fn sign_out_with_password(
        &self,
        challenge: SignOutChallenge,
        password: Zeroizing<String>,
    ) -> Result<SignOutStep> {
        self.check_challenge(&challenge, ChallengeKind::Password)?;
        if password.is_empty() {
            return Ok(SignOutStep::Password(challenge));
        }
        let _operation = self.guarded(READ_TIMEOUT, self.lock_operation()).await?;
        let identifier = MatrixUserIdentifier::new(self.user_id.to_string());
        // The request body needs an owned String; it is dropped with the
        // request. The caller's copy stays in zeroizing memory.
        let mut auth = Password::new(UserIdentifier::Matrix(identifier), password.to_string());
        auth.session = Some(challenge.session.to_string());
        let mut request = delete_devices::v3::Request::new(vec![challenge.device_id.clone()]);
        request.auth = Some(AuthData::Password(auth));
        self.send_sign_out(challenge.device_id, request).await
    }

    /// Continue after the person finished the homeserver's browser page.
    pub async fn sign_out_after_browser(&self, challenge: SignOutChallenge) -> Result<SignOutStep> {
        self.check_challenge(&challenge, ChallengeKind::Browser)?;
        let _operation = self.guarded(READ_TIMEOUT, self.lock_operation()).await?;
        let acknowledgement = FallbackAcknowledgement::new(challenge.session.to_string());
        let mut request = delete_devices::v3::Request::new(vec![challenge.device_id.clone()]);
        request.auth = Some(AuthData::FallbackAcknowledgement(acknowledgement));
        self.send_sign_out(challenge.device_id, request).await
    }

    /// A challenge continues only under the selection and kind it started
    /// with, for a session that is still not this device.
    fn check_challenge(&self, challenge: &SignOutChallenge, kind: ChallengeKind) -> Result<()> {
        self.ensure_scope()?;
        challenge_usable(challenge, &self.selection, &self.device_id, kind)
    }

    async fn send_sign_out(
        &self,
        device_id: OwnedDeviceId,
        request: delete_devices::v3::Request,
    ) -> Result<SignOutStep> {
        // The last check before the request leaves: never this device.
        if device_id == self.device_id {
            return Err(DeviceSessionError::CurrentDevice);
        }
        let sent = async {
            let request = self.client.send(request).with_request_config(single_send());
            Ok(request.await)
        };
        let sent = self.guarded(WRITE_TIMEOUT, sent).await?;
        match sent {
            Ok(_) => {
                let after = self.guarded(READ_TIMEOUT, self.server_sessions()).await;
                let gone = match after {
                    Ok(after) => find(&after, device_id.as_str()).is_none(),
                    Err(DeviceSessionError::Cancelled) => {
                        return Err(DeviceSessionError::Cancelled);
                    }
                    Err(DeviceSessionError::AccountChanged) => {
                        return Err(DeviceSessionError::AccountChanged);
                    }
                    Err(_) => false,
                };
                Ok(SignOutStep::Done { gone })
            }
            Err(error) => match error.as_uiaa_response() {
                Some(info) => {
                    let homeserver = self.client.homeserver();
                    Ok(challenge_step(
                        &homeserver,
                        &self.selection,
                        device_id,
                        info,
                    ))
                }
                None => Err(request_error(&error)),
            },
        }
    }

    /// The provider's page for signing out `device_id`, HTTPS only.
    async fn provider_page(&self, device_id: &DeviceId) -> Result<SignOutStep> {
        let Ok(metadata) = self.client.oauth().server_metadata().await else {
            return Ok(SignOutStep::Unsupported(SignOutBlocker::NoProviderPage));
        };
        let action = AccountManagementActionData::DeviceDelete(DeviceDeleteData::new(device_id));
        Ok(match metadata.account_management_url_with_action(action) {
            Some(page) if browser_page_allowed(&page) => SignOutStep::ProviderPage { page },
            Some(_) => SignOutStep::Unsupported(SignOutBlocker::InsecurePage),
            None => SignOutStep::Unsupported(SignOutBlocker::NoProviderPage),
        })
    }

    /// The homeserver's list of this account's sessions, without addresses.
    async fn server_sessions(&self) -> Result<Vec<ServerSession>> {
        let request = self
            .client
            .send(get_devices::v3::Request::new())
            .with_request_config(
                self.client
                    .request_config()
                    .max_response_bytes(MAX_DEVICE_RESPONSE_BYTES),
            );
        let response = request.await.map_err(|error| request_error(&error))?;
        checked_server_sessions(response.devices)
    }

    /// Join the server's list with this device's crypto store.
    async fn compose(&self, listed: Vec<ServerSession>) -> Result<DeviceSessionSnapshot> {
        let crypto = self.guarded(READ_TIMEOUT, async { Ok(self.crypto_view().await) });
        let crypto = crypto.await?;
        let current = self.device_id.as_str();
        let (sessions, omitted) = roster(listed, current, |id| crypto.trust(id));
        Ok(DeviceSessionSnapshot {
            user_id: self.user_id.to_string(),
            current_device_id: current.to_owned(),
            sessions,
            omitted,
            cross_signing: crypto.cross_signing,
            this_device_verified: crypto.this_device_verified,
            sign_out: self.sign_out,
        })
    }

    async fn crypto_view(&self) -> CryptoView {
        let encryption = self.client.encryption();
        let this_device_verified = match encryption.verification_state().get() {
            VerificationState::Verified => Some(true),
            VerificationState::Unverified => Some(false),
            VerificationState::Unknown => None,
        };
        let devices = encryption.get_user_devices(&self.user_id).await.ok();
        let devices = devices.and_then(|devices| {
            let mut known = HashMap::new();
            for (index, device) in devices
                .devices()
                .take(MAX_SERVER_DEVICE_SESSIONS + 1)
                .enumerate()
            {
                if index == MAX_SERVER_DEVICE_SESSIONS {
                    return None;
                }
                if parse_device_id(device.device_id().as_str()).is_err() {
                    return None;
                }
                known.insert(device.device_id().to_string(), device_trust(&device));
            }
            Some(known)
        });
        let identity = encryption.get_user_identity(&self.user_id).await;
        let status = encryption.cross_signing_status().await;
        let cross_signing = match (identity, status) {
            (Err(_), _) | (_, None) => CrossSigningState::Unknown,
            (Ok(None), _) => CrossSigningState::NotSetUp,
            (Ok(Some(identity)), Some(status)) => {
                let keys = status.has_master && status.has_self_signing && status.has_user_signing;
                if keys && identity.is_verified() {
                    CrossSigningState::Ready
                } else {
                    CrossSigningState::NotReadyHere
                }
            }
        };
        CryptoView {
            devices,
            cross_signing,
            this_device_verified,
        }
    }

    async fn lock_operation(&self) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        Ok(self.operation.lock().await)
    }

    fn ensure_scope(&self) -> Result<()> {
        scope_error(&self.selection, &self.cancellation)
    }

    /// Run one wait under this view: ends at once on cancellation, account
    /// change or timeout, and is re-checked before its result is used.
    async fn guarded<T>(
        &self,
        limit: Duration,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        guarded(&self.selection, &self.cancellation, limit, work).await
    }
}

impl Drop for DeviceSessions {
    fn drop(&mut self) {
        // Anything still waiting for this view ends with it.
        self.cancellation.cancel();
    }
}

/// One session as the homeserver lists it. The reported IP address is
/// deliberately not part of it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ServerSession {
    device_id: String,
    name: Option<String>,
    last_seen_ms: Option<u64>,
}

fn server_session(listed: device::Device) -> Option<ServerSession> {
    let device_id = listed.device_id.to_string();
    if parse_device_id(&device_id).is_err() {
        return None;
    }
    let name = listed.display_name.as_deref().and_then(bounded_device_name);
    let last_seen_ms = listed.last_seen_ts.map(|seen| u64::from(seen.get()));
    Some(ServerSession {
        device_id,
        name,
        last_seen_ms,
    })
}

fn checked_server_sessions(listed: Vec<device::Device>) -> Result<Vec<ServerSession>> {
    if listed.len() > MAX_SERVER_DEVICE_SESSIONS {
        return Err(DeviceSessionError::Unavailable);
    }
    let mut seen = std::collections::HashSet::with_capacity(listed.len());
    let mut sessions = Vec::with_capacity(listed.len());
    for device in listed {
        let session = server_session(device).ok_or(DeviceSessionError::Unavailable)?;
        if !seen.insert(session.device_id.clone()) {
            return Err(DeviceSessionError::Unavailable);
        }
        sessions.push(session);
    }
    Ok(sessions)
}

struct CryptoView {
    /// `None` when the crypto store could not be read.
    devices: Option<HashMap<String, DeviceTrust>>,
    cross_signing: CrossSigningState,
    this_device_verified: Option<bool>,
}

impl CryptoView {
    fn trust(&self, device_id: &str) -> DeviceTrust {
        let Some(devices) = &self.devices else {
            return DeviceTrust::Unknown;
        };
        devices
            .get(device_id)
            .copied()
            .unwrap_or(DeviceTrust::NoKeys)
    }
}

fn device_trust(device: &Device) -> DeviceTrust {
    trust_from(
        device.is_blacklisted(),
        device.is_verified_with_cross_signing(),
        device.is_locally_trusted(),
        device.is_cross_signed_by_owner(),
    )
}

/// Blocking wins; cross-signing trust comes before local trust; a signature
/// this device cannot confirm yet is named as such.
fn trust_from(blocked: bool, cross_signed: bool, local: bool, signed: bool) -> DeviceTrust {
    if blocked {
        DeviceTrust::Blocked
    } else if cross_signed {
        DeviceTrust::Verified
    } else if local {
        DeviceTrust::LocallyTrusted
    } else if signed {
        DeviceTrust::SignedUnconfirmed
    } else {
        DeviceTrust::Unverified
    }
}

/// This device first, then the most recently active, then by ID; bounded,
/// with the number left out.
fn roster(
    listed: Vec<ServerSession>,
    current: &str,
    trust: impl Fn(&str) -> DeviceTrust,
) -> (Vec<DeviceSession>, usize) {
    let mut sessions: Vec<DeviceSession> = listed
        .into_iter()
        .map(|listed| DeviceSession {
            current: listed.device_id == current,
            trust: trust(&listed.device_id),
            device_id: listed.device_id,
            name: listed.name,
            last_seen_ms: listed.last_seen_ms,
        })
        .collect();
    sessions.sort_by(|a, b| {
        let order = |session: &DeviceSession| (!session.current, session.last_seen_ms.is_none());
        let seen = |session: &DeviceSession| std::cmp::Reverse(session.last_seen_ms);
        order(a)
            .cmp(&order(b))
            .then_with(|| seen(a).cmp(&seen(b)))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });
    let omitted = sessions.len().saturating_sub(MAX_DEVICE_SESSIONS);
    sessions.truncate(MAX_DEVICE_SESSIONS);
    (sessions, omitted)
}

fn find<'a>(listed: &'a [ServerSession], device_id: &str) -> Option<&'a ServerSession> {
    listed.iter().find(|session| session.device_id == device_id)
}

/// A sign-out may only name another session this account still lists.
fn sign_out_target(requested: &DeviceId, current: &DeviceId, listed: &[&str]) -> Result<()> {
    if requested == current {
        return Err(DeviceSessionError::CurrentDevice);
    }
    if !listed.contains(&requested.as_str()) {
        return Err(DeviceSessionError::NotFound);
    }
    Ok(())
}

fn challenge_usable(
    challenge: &SignOutChallenge,
    selection: &AccountSelection,
    current: &DeviceId,
    kind: ChallengeKind,
) -> Result<()> {
    if !challenge.selection.is_current() || !challenge.selection.same_epoch(selection) {
        return Err(DeviceSessionError::AccountChanged);
    }
    if challenge.device_id.as_str() == current.as_str() {
        return Err(DeviceSessionError::CurrentDevice);
    }
    if challenge.kind != kind {
        return Err(DeviceSessionError::ChallengeMismatch);
    }
    Ok(())
}

/// The next step for a server authentication request: a single-stage
/// password or browser stage; anything else is reported, never attempted.
fn challenge_step(
    homeserver: &Url,
    selection: &AccountSelection,
    device_id: OwnedDeviceId,
    info: &UiaaInfo,
) -> SignOutStep {
    let unsupported = SignOutStep::Unsupported(SignOutBlocker::UnsupportedAuthentication);
    let Some(session) = info.session.as_deref() else {
        return unsupported;
    };
    if session.is_empty()
        || session.len() > MAX_AUTH_SESSION_BYTES
        || info.flows.len() > MAX_AUTH_FLOWS
    {
        return unsupported;
    }
    let offers = |stage: &str| {
        info.flows.iter().any(|flow| match flow.stages.as_slice() {
            [only] => only.as_str() == stage,
            _ => false,
        })
    };
    let challenge = |kind| SignOutChallenge {
        selection: selection.clone(),
        device_id: device_id.clone(),
        session: Zeroizing::new(session.to_owned()),
        kind,
        rejected: info.auth_error.is_some(),
    };
    if offers(PASSWORD_STAGE) {
        return SignOutStep::Password(challenge(ChallengeKind::Password));
    }
    if offers(SSO_STAGE) {
        return match fallback_page(homeserver, SSO_STAGE, session) {
            Some(page) => SignOutStep::Browser {
                challenge: challenge(ChallengeKind::Browser),
                page,
            },
            None => SignOutStep::Unsupported(SignOutBlocker::InsecurePage),
        };
    }
    unsupported
}

/// The spec's authentication fallback page for one stage of one session on
/// this account's own homeserver, the origin the client already trusts.
fn fallback_page(homeserver: &Url, stage: &str, session: &str) -> Option<Url> {
    let mut page = homeserver.clone();
    page.set_query(None);
    page.set_fragment(None);
    let segments = ["_matrix", "client", "v3", "auth", stage, "fallback", "web"];
    {
        let mut path = page.path_segments_mut().ok()?;
        path.pop_if_empty().extend(segments);
    }
    page.query_pairs_mut().append_pair("session", session);
    browser_page_allowed(&page).then_some(page)
}

/// HTTPS without embedded credentials, or plain HTTP only for loopback
/// development servers, as homeserver validation allows.
fn browser_page_allowed(page: &Url) -> bool {
    if page.as_str().len() > MAX_BROWSER_PAGE_BYTES
        || !page.username().is_empty()
        || page.password().is_some()
    {
        return false;
    }
    let loopback = match page.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => return false,
    };
    page.scheme() == "https" || (page.scheme() == "http" && loopback)
}

/// Trimmed, bounded and free of control or direction-override characters;
/// `None` when nothing printable remains.
pub fn bounded_device_name(name: &str) -> Option<String> {
    let bounded: String = name
        .char_indices()
        .take_while(|(offset, _)| *offset < MAX_DEVICE_NAME_INPUT_BYTES)
        .map(|(_, character)| character)
        .filter(|&character| !hidden(character))
        .skip_while(|character| character.is_whitespace())
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    let bounded = bounded.trim_end();
    (!bounded.is_empty()).then(|| bounded.to_owned())
}

fn hidden(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

fn parse_device_id(device_id: &str) -> Result<OwnedDeviceId> {
    let valid = !device_id.is_empty()
        && device_id.len() <= MAX_DEVICE_ID_BYTES
        && !device_id.chars().any(|c| c.is_whitespace() || hidden(c));
    if !valid {
        return Err(DeviceSessionError::NotFound);
    }
    Ok(device_id.into())
}

/// Destructive and naming requests are sent once; a failure after sending
/// is followed by a fresh read, never by a resend.
fn single_send() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(WRITE_TIMEOUT)
        .max_response_bytes(MAX_DEVICE_RESPONSE_BYTES)
}

fn request_error(error: &HttpError) -> DeviceSessionError {
    match error.client_api_error_kind() {
        Some(ErrorKind::LimitExceeded(_)) => DeviceSessionError::RateLimited,
        Some(ErrorKind::Forbidden) => DeviceSessionError::Forbidden,
        Some(ErrorKind::NotFound) => DeviceSessionError::NotFound,
        Some(ErrorKind::Unrecognized) => DeviceSessionError::Unsupported,
        _ => DeviceSessionError::Unavailable,
    }
}

fn scope_error(
    selection: &AccountSelection,
    cancellation: &DeviceSessionCancellation,
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(DeviceSessionError::Cancelled);
    }
    if !selection.is_current() {
        return Err(DeviceSessionError::AccountChanged);
    }
    Ok(())
}

async fn guarded<T>(
    selection: &AccountSelection,
    cancellation: &DeviceSessionCancellation,
    limit: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    scope_error(selection, cancellation)?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(DeviceSessionError::Cancelled),
        _ = selection.revoked() => Err(DeviceSessionError::AccountChanged),
        result = time::timeout(limit, work) => result.map_err(|_| DeviceSessionError::TimedOut)?,
    }?;
    scope_error(selection, cancellation)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_selection::Selections;
    use matrix_sdk::ruma::{
        MilliSecondsSinceUnixEpoch, UInt,
        api::{client::uiaa::AuthFlow, error::StandardErrorBody},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    fn listed(id: &str, name: Option<&str>, seen: Option<u64>) -> ServerSession {
        ServerSession {
            device_id: id.to_owned(),
            name: name.map(str::to_owned),
            last_seen_ms: seen,
        }
    }

    fn uiaa(stages: &[&[&str]], session: Option<&str>) -> UiaaInfo {
        let flow =
            |stages: &&[&str]| AuthFlow::new(stages.iter().map(|&stage| stage.into()).collect());
        let mut info = UiaaInfo::new(stages.iter().map(flow).collect());
        info.session = session.map(str::to_owned);
        info
    }

    #[test]
    fn device_names_are_trimmed_bounded_and_stripped_of_spoofing_characters() {
        assert_eq!(bounded_device_name("  Laptop  "), Some("Laptop".into()));
        let spoofed = bounded_device_name("Work\u{202e}pc\n");
        assert_eq!(spoofed, Some("Workpc".into()));
        assert_eq!(bounded_device_name(" \u{200b}\t "), None);
        let long = bounded_device_name(&"x".repeat(200)).unwrap();
        assert_eq!(long.chars().count(), MAX_DEVICE_NAME_CHARS);
        // A hostile prefix cannot force an unbounded temporary allocation
        // or make the sanitizer scan arbitrary server-controlled text.
        let excessive = format!("{}hidden tail", " ".repeat(MAX_DEVICE_NAME_INPUT_BYTES));
        assert_eq!(bounded_device_name(&excessive), None);
        let unicode = bounded_device_name(&"界".repeat(200)).unwrap();
        assert_eq!(unicode.chars().count(), MAX_DEVICE_NAME_CHARS);
        assert!(parse_device_id("ABCDEF").is_ok());
        for bad in ["", "AB CD", "AB\u{202e}CD", &"x".repeat(300)] {
            assert_eq!(parse_device_id(bad), Err(DeviceSessionError::NotFound));
        }
    }

    #[test]
    fn invalid_or_partial_server_rosters_cannot_be_used_as_sign_out_confirmation() {
        let duplicate = vec![
            device::Device::new("SAME".into()),
            device::Device::new("SAME".into()),
        ];
        assert_eq!(
            checked_server_sessions(duplicate),
            Err(DeviceSessionError::Unavailable)
        );
        let deceptive = vec![device::Device::new("OTHER\u{202e}".into())];
        assert_eq!(
            checked_server_sessions(deceptive),
            Err(DeviceSessionError::Unavailable)
        );
        let oversized = (0..MAX_SERVER_DEVICE_SESSIONS + 1)
            .map(|index| device::Device::new(format!("D{index}").into()))
            .collect();
        assert_eq!(
            checked_server_sessions(oversized),
            Err(DeviceSessionError::Unavailable)
        );
        let complete = checked_server_sessions(vec![
            device::Device::new("THIS".into()),
            device::Device::new("OTHER".into()),
        ])
        .unwrap();
        assert_eq!(complete.len(), 2);
        assert!(find(&complete, "OTHER").is_some());
    }

    #[test]
    fn sessions_put_this_device_first_drop_addresses_and_stay_bounded() {
        let mut from_server = device::Device::new("PHONE".into());
        from_server.display_name = Some("Phone\u{202e}".into());
        from_server.last_seen_ip = Some("203.0.113.7".into());
        from_server.last_seen_ts = Some(MilliSecondsSinceUnixEpoch(UInt::from(5_u32)));
        let phone = server_session(from_server).unwrap();
        assert_eq!(phone, listed("PHONE", Some("Phone"), Some(5)));
        assert!(!format!("{phone:?}").contains("203.0.113.7"));

        let sessions = vec![
            listed("OLD", None, Some(1)),
            listed("NEVER", None, None),
            listed("THIS", Some("This"), Some(2)),
            phone,
        ];
        let trust = |id: &str| match id {
            "THIS" => DeviceTrust::Verified,
            "OLD" => DeviceTrust::NoKeys,
            _ => DeviceTrust::Unverified,
        };
        let (rows, omitted) = roster(sessions, "THIS", trust);
        let ids: Vec<&str> = rows.iter().map(|row| row.device_id.as_str()).collect();
        assert_eq!(ids, ["THIS", "PHONE", "OLD", "NEVER"]);
        assert!(rows[0].current && rows.iter().skip(1).all(|row| !row.current));
        assert_eq!(rows[2].trust, DeviceTrust::NoKeys);
        assert_eq!(omitted, 0);

        let ids = (0..MAX_DEVICE_SESSIONS + 5).map(|n| format!("D{n}"));
        let many = ids.map(|id| listed(&id, None, None)).collect();
        let (rows, omitted) = roster(many, "D7", |_| DeviceTrust::Unknown);
        assert_eq!((rows.len(), omitted), (MAX_DEVICE_SESSIONS, 5));
        assert_eq!(rows[0].device_id, "D7");
    }

    #[test]
    fn trust_follows_cross_signing_then_local_trust_and_blocking_wins() {
        assert_eq!(trust_from(true, true, true, true), DeviceTrust::Blocked);
        assert_eq!(trust_from(false, true, false, true), DeviceTrust::Verified);
        assert_eq!(
            trust_from(false, false, true, false),
            DeviceTrust::LocallyTrusted
        );
        let unconfirmed = trust_from(false, false, false, true);
        assert_eq!(unconfirmed, DeviceTrust::SignedUnconfirmed);
        assert_eq!(
            trust_from(false, false, false, false),
            DeviceTrust::Unverified
        );
        let unread = CryptoView {
            devices: None,
            cross_signing: CrossSigningState::Unknown,
            this_device_verified: None,
        };
        assert_eq!(unread.trust("ANY"), DeviceTrust::Unknown);
        let read = CryptoView {
            devices: Some(HashMap::from([("KNOWN".into(), DeviceTrust::Verified)])),
            ..unread
        };
        assert_eq!(read.trust("KNOWN"), DeviceTrust::Verified);
        assert_eq!(read.trust("OTHER"), DeviceTrust::NoKeys);
    }

    #[test]
    fn this_device_and_stale_rows_are_refused_before_any_sign_out_is_sent() {
        let this: OwnedDeviceId = "THIS".into();
        let other: OwnedDeviceId = "OTHER".into();
        let gone: OwnedDeviceId = "GONE".into();
        let listed = ["THIS", "OTHER"];
        let current = sign_out_target(&this, &this, &listed);
        assert_eq!(current, Err(DeviceSessionError::CurrentDevice));
        let stale = sign_out_target(&gone, &this, &listed);
        assert_eq!(stale, Err(DeviceSessionError::NotFound));
        assert_eq!(sign_out_target(&other, &this, &listed), Ok(()));
    }

    #[test]
    fn challenges_offer_one_stage_hide_their_session_and_end_with_their_account() {
        let selections = Selections::default();
        selections.replace(Some("alice"));
        let selection = selections.for_account("alice").unwrap();
        let homeserver = Url::parse("https://matrix.example.org/").unwrap();
        let device: OwnedDeviceId = "OTHER".into();
        let this: OwnedDeviceId = "THIS".into();

        for session in [String::new(), "s".repeat(MAX_AUTH_SESSION_BYTES + 1)] {
            let excessive = uiaa(&[&[PASSWORD_STAGE]], Some(&session));
            assert!(matches!(
                challenge_step(&homeserver, &selection, device.clone(), &excessive),
                SignOutStep::Unsupported(_)
            ));
        }
        let mut excessive_flows = uiaa(&[&[PASSWORD_STAGE]], Some("bounded"));
        excessive_flows.flows =
            vec![AuthFlow::new(vec![PASSWORD_STAGE.into()]); MAX_AUTH_FLOWS + 1];
        assert!(matches!(
            challenge_step(&homeserver, &selection, device.clone(), &excessive_flows),
            SignOutStep::Unsupported(_)
        ));

        let info = uiaa(&[&[PASSWORD_STAGE], &[SSO_STAGE]], Some("s3cr3t-session"));
        let SignOutStep::Password(challenge) =
            challenge_step(&homeserver, &selection, device.clone(), &info)
        else {
            panic!("a single password stage asks for the password");
        };
        assert!(!challenge.rejected());
        assert_eq!(format!("{challenge:?}"), "SignOutChallenge { .. }");
        let usable = challenge_usable(&challenge, &selection, &this, ChallengeKind::Password);
        assert_eq!(usable, Ok(()));
        let wrong = challenge_usable(&challenge, &selection, &this, ChallengeKind::Browser);
        assert_eq!(wrong, Err(DeviceSessionError::ChallengeMismatch));
        let at_this = challenge_usable(&challenge, &selection, &device, ChallengeKind::Password);
        assert_eq!(at_this, Err(DeviceSessionError::CurrentDevice));

        // A refused password is reported, and the session is never shown.
        let mut refused = info.clone();
        refused.auth_error = Some(Box::new(StandardErrorBody::new(
            ErrorKind::Forbidden,
            "Invalid password".into(),
        )));
        let SignOutStep::Password(again) =
            challenge_step(&homeserver, &selection, device.clone(), &refused)
        else {
            panic!("a refused password asks again");
        };
        assert!(again.rejected());

        // Browser only: the homeserver's own HTTPS fallback page.
        let browser = uiaa(&[&[SSO_STAGE]], Some("abc"));
        let SignOutStep::Browser { page, .. } =
            challenge_step(&homeserver, &selection, device.clone(), &browser)
        else {
            panic!("a single SSO stage opens the homeserver's page");
        };
        let expected = "https://matrix.example.org/_matrix/client/v3/auth/m.login.sso/fallback/web?session=abc";
        assert_eq!(page.as_str(), expected);

        // Several stages, unknown stages or no session are never attempted.
        for unsupported in [
            uiaa(&[&[PASSWORD_STAGE, "m.login.email.identity"]], Some("abc")),
            uiaa(&[&["m.login.recaptcha"]], Some("abc")),
            uiaa(&[&[PASSWORD_STAGE]], None),
        ] {
            let step = challenge_step(&homeserver, &selection, device.clone(), &unsupported);
            let blocker = SignOutBlocker::UnsupportedAuthentication;
            assert!(matches!(step, SignOutStep::Unsupported(found) if found == blocker));
        }

        // Switching away, or back again (A -> B -> A), ends the challenge.
        selections.replace(Some("bob"));
        let after = challenge_usable(&challenge, &selection, &this, ChallengeKind::Password);
        assert_eq!(after, Err(DeviceSessionError::AccountChanged));
        selections.replace(Some("alice"));
        let returned = selections.for_account("alice").unwrap();
        let reselected = challenge_usable(&challenge, &returned, &this, ChallengeKind::Password);
        assert_eq!(reselected, Err(DeviceSessionError::AccountChanged));
    }

    #[test]
    fn fallback_pages_stay_on_the_trusted_homeserver_origin() {
        let insecure = Url::parse("http://matrix.example.org/").unwrap();
        assert_eq!(fallback_page(&insecure, SSO_STAGE, "abc"), None);
        let loopback = Url::parse("http://127.0.0.1:8008/").unwrap();
        assert!(fallback_page(&loopback, SSO_STAGE, "abc").is_some());
        let credentials = Url::parse("https://user:secret@matrix.example.org/").unwrap();
        assert!(!browser_page_allowed(&credentials));
        let oversized = Url::parse(&format!(
            "https://matrix.example.org/{}",
            "x".repeat(MAX_BROWSER_PAGE_BYTES)
        ))
        .unwrap();
        assert!(!browser_page_allowed(&oversized));
    }

    #[test]
    fn account_switch_removal_or_shutdown_stops_work_before_and_after_it_runs() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("alice"));
            let selection = selections.for_account("alice").unwrap();
            let cancellation = DeviceSessionCancellation::new();
            let ran = guarded(&selection, &cancellation, READ_TIMEOUT, async { Ok(7) });
            assert_eq!(ran.await, Ok(7));

            // A wait that never completes ends as soon as the account changes.
            let waiting = guarded(&selection, &cancellation, READ_TIMEOUT, async {
                std::future::pending::<Result<()>>().await
            });
            let switch = async {
                tokio::task::yield_now().await;
                selections.replace(Some("bob"));
            };
            let (waited, ()) = tokio::join!(waiting, switch);
            assert_eq!(waited, Err(DeviceSessionError::AccountChanged));
            // Nothing new starts under the old selection.
            let refused = guarded(&selection, &cancellation, READ_TIMEOUT, async { Ok(1) });
            assert_eq!(refused.await, Err(DeviceSessionError::AccountChanged));

            // Shutdown (or leaving the page) cancels the view the same way.
            let selection = selections.for_account("bob").unwrap();
            let closing = DeviceSessionCancellation::new();
            closing.cancel();
            let closed = guarded(&selection, &closing, READ_TIMEOUT, async { Ok(1) });
            assert_eq!(closed.await, Err(DeviceSessionError::Cancelled));
            // Removing the account revokes its selection too.
            selections.replace(None);
            assert_eq!(
                scope_error(&selection, &DeviceSessionCancellation::new()),
                Err(DeviceSessionError::AccountChanged)
            );
        });
    }

    #[test]
    fn errors_never_carry_server_text_or_secrets() {
        let all = [
            DeviceSessionError::Cancelled,
            DeviceSessionError::AccountChanged,
            DeviceSessionError::Unavailable,
            DeviceSessionError::RateLimited,
            DeviceSessionError::TimedOut,
            DeviceSessionError::NotFound,
            DeviceSessionError::CurrentDevice,
            DeviceSessionError::InvalidName,
            DeviceSessionError::Forbidden,
            DeviceSessionError::Unsupported,
            DeviceSessionError::ThisDeviceUnverified,
            DeviceSessionError::NoKeys,
            DeviceSessionError::AlreadyVerified,
            DeviceSessionError::ChallengeMismatch,
        ];
        for error in all {
            let text = error.to_string();
            assert!(!text.is_empty() && !text.contains('{') && !text.contains("session="));
        }
    }
}
