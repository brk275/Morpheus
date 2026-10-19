//! Device rename and remote device sign-out for the signed-in account.
//!
//! Every call is bound to the account selection the user acted under: a
//! switch, logout or A → B → A sequence revokes it before the server is
//! contacted again. Remote sign-out never touches this device (the normal
//! logout flow owns that), never trusts or verifies anything, and only uses
//! authentication the server offers and this client can complete honestly:
//! the account password, the server's own browser page for single sign-on,
//! or the OAuth provider's account page. Passwords and UIA sessions are never
//! logged, serialized or stored.

use matrix_sdk::{
    Client,
    ruma::{
        OwnedDeviceId,
        api::client::{
            discovery::get_authorization_server_metadata::v1::{
                AccountManagementActionData, DeviceDeleteData,
            },
            uiaa::{
                AuthData, FallbackAcknowledgement, MatrixUserIdentifier, Password, UiaaInfo,
                UserIdentifier,
            },
        },
    },
};
use url::Url;

use crate::{AccountManager, AccountSelection, MatrixCoreError, Result, selected_loaded_for};

/// Longest device name Morpheus sends, in characters.
pub const MAX_DEVICE_NAME_CHARS: usize = 64;

const PASSWORD_STAGE: &str = "m.login.password";
const SSO_STAGE: &str = "m.login.sso";

/// What the person must do next to sign out a remote device. Deliberately
/// not `Debug`: a browser page URL carries the server's UIA session.
pub enum DeviceSignOutStep {
    /// The server signed the device out.
    Done,
    /// Enter this account's password to authorize the sign-out.
    Password(DeviceSignOutChallenge),
    /// Finish the server's page in the system browser, then continue here.
    Browser {
        challenge: DeviceSignOutChallenge,
        url: Url,
    },
    /// The account's OAuth provider manages devices; sign it out on this page.
    /// Morpheus cannot observe the result; a device-list refresh shows it.
    AccountManagement { url: Url },
    /// The server asks for authentication this client cannot complete.
    Unsupported(DeviceSignOutBlocker),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceSignOutBlocker {
    /// The OAuth provider advertises no account management page.
    NoAccountManagementPage,
    /// Only multi-stage or other authentication flows are offered.
    UnsupportedAuthentication,
    /// The page the server named is not HTTPS (or loopback development).
    InsecurePage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChallengeKind {
    Password,
    Browser,
}

/// One server UIA session for one remote device, bound to the account
/// selection that started it. Opaque to the app and never displayed.
pub struct DeviceSignOutChallenge {
    selection: AccountSelection,
    device_id: OwnedDeviceId,
    session: String,
    kind: ChallengeKind,
    rejected: bool,
}

impl std::fmt::Debug for DeviceSignOutChallenge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeviceSignOutChallenge { .. }")
    }
}

impl DeviceSignOutChallenge {
    pub fn account_id(&self) -> &str {
        self.selection.account_id()
    }

    pub fn device_id(&self) -> &str {
        self.device_id.as_str()
    }

    /// The server refused the previous attempt's credentials.
    pub fn rejected(&self) -> bool {
        self.rejected
    }
}

impl AccountManager {
    /// Rename one of this account's devices, this one included. The name is
    /// stripped of control and direction-override characters and bounded.
    pub async fn rename_device_for(
        &self,
        selection: &AccountSelection,
        device_id: &str,
        name: &str,
    ) -> Result<()> {
        let name = bounded_device_name(name)
            .ok_or_else(|| MatrixCoreError::Sdk("enter a device name".into()))?;
        let client = self.selected_client(selection).await?;
        let device_id: OwnedDeviceId = device_id.into();
        client
            .rename_device(&device_id, &name)
            .await
            .map_err(|_| MatrixCoreError::Sdk("the server did not rename the device".into()))?;
        Ok(())
    }

    /// Start signing out another device of this account. This device is
    /// refused: use the normal logout flow for it.
    pub async fn begin_device_sign_out_for(
        &self,
        selection: &AccountSelection,
        device_id: &str,
    ) -> Result<DeviceSignOutStep> {
        let client = self.selected_client(selection).await?;
        let device_id: OwnedDeviceId = device_id.into();
        if client.device_id() == Some(&*device_id) {
            return Err(MatrixCoreError::Sdk(
                "use Log out to sign out this device".into(),
            ));
        }
        if client.oauth().full_session().is_some() {
            return Ok(account_management_step(&client, &device_id).await);
        }
        let result = client.delete_devices(&[device_id.clone()], None).await;
        sign_out_result(&client, selection.clone(), device_id, result)
    }

    /// Continue a password challenge with the person's explicit input.
    pub async fn sign_out_device_with_password_for(
        &self,
        challenge: DeviceSignOutChallenge,
        password: &str,
    ) -> Result<DeviceSignOutStep> {
        if challenge.kind != ChallengeKind::Password {
            return Err(MatrixCoreError::Sdk(
                "this sign-out does not take a password".into(),
            ));
        }
        let client = self.selected_client(&challenge.selection).await?;
        let user_id = client
            .user_id()
            .ok_or(MatrixCoreError::NoActiveAccount)?
            .to_string();
        let identifier = UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id));
        let mut auth = Password::new(identifier, password.to_owned());
        auth.session = Some(challenge.session.clone());
        let devices = [challenge.device_id.clone()];
        let auth = AuthData::Password(auth);
        let result = client.delete_devices(&devices, Some(auth)).await;
        sign_out_result(&client, challenge.selection, challenge.device_id, result)
    }

    /// Continue after the person finished the server's browser page.
    pub async fn sign_out_device_after_browser_for(
        &self,
        challenge: DeviceSignOutChallenge,
    ) -> Result<DeviceSignOutStep> {
        if challenge.kind != ChallengeKind::Browser {
            return Err(MatrixCoreError::Sdk(
                "this sign-out does not use a browser page".into(),
            ));
        }
        let client = self.selected_client(&challenge.selection).await?;
        let acknowledgement = FallbackAcknowledgement::new(challenge.session.clone());
        let devices = [challenge.device_id.clone()];
        let auth = AuthData::FallbackAcknowledgement(acknowledgement);
        let result = client.delete_devices(&devices, Some(auth)).await;
        sign_out_result(&client, challenge.selection, challenge.device_id, result)
    }

    /// The selected account's client, only while `selection` is the current
    /// selection epoch of that account.
    async fn selected_client(&self, selection: &AccountSelection) -> Result<Client> {
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, selection.account_id())?;
        let current = self
            .account_selection(selection.account_id())
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        if !selection.is_current() || !selection.same_epoch(&current) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(loaded.client.clone())
    }
}

async fn account_management_step(client: &Client, device_id: &OwnedDeviceId) -> DeviceSignOutStep {
    let Ok(metadata) = client.oauth().server_metadata().await else {
        return DeviceSignOutStep::Unsupported(DeviceSignOutBlocker::NoAccountManagementPage);
    };
    let action = AccountManagementActionData::DeviceDelete(DeviceDeleteData::new(device_id));
    match metadata.account_management_url_with_action(action) {
        Some(url) if browser_page_allowed(&url) => DeviceSignOutStep::AccountManagement { url },
        Some(_) => DeviceSignOutStep::Unsupported(DeviceSignOutBlocker::InsecurePage),
        None => DeviceSignOutStep::Unsupported(DeviceSignOutBlocker::NoAccountManagementPage),
    }
}

fn sign_out_result<T>(
    client: &Client,
    selection: AccountSelection,
    device_id: OwnedDeviceId,
    result: matrix_sdk::HttpResult<T>,
) -> Result<DeviceSignOutStep> {
    match result {
        Ok(_) => Ok(DeviceSignOutStep::Done),
        Err(error) => match error.as_uiaa_response() {
            Some(info) => {
                let homeserver = client.homeserver();
                Ok(challenge_step(&homeserver, selection, device_id, info))
            }
            None => Err(MatrixCoreError::Sdk(
                "the server did not sign out the device".into(),
            )),
        },
    }
}

fn challenge_step(
    homeserver: &Url,
    selection: AccountSelection,
    device_id: OwnedDeviceId,
    info: &UiaaInfo,
) -> DeviceSignOutStep {
    let Some(session) = info.session.clone() else {
        return DeviceSignOutStep::Unsupported(DeviceSignOutBlocker::UnsupportedAuthentication);
    };
    let rejected = info.auth_error.is_some();
    let offers = |stage: &str| {
        info.flows
            .iter()
            .any(|flow| flow.stages.len() == 1 && flow.stages[0].as_str() == stage)
    };
    let challenge = |kind| DeviceSignOutChallenge {
        selection: selection.clone(),
        device_id: device_id.clone(),
        session: session.clone(),
        kind,
        rejected,
    };
    if offers(PASSWORD_STAGE) {
        return DeviceSignOutStep::Password(challenge(ChallengeKind::Password));
    }
    if offers(SSO_STAGE) {
        return match fallback_page(homeserver, SSO_STAGE, &session) {
            Some(url) => DeviceSignOutStep::Browser {
                challenge: challenge(ChallengeKind::Browser),
                url,
            },
            None => DeviceSignOutStep::Unsupported(DeviceSignOutBlocker::InsecurePage),
        };
    }
    DeviceSignOutStep::Unsupported(DeviceSignOutBlocker::UnsupportedAuthentication)
}

/// The spec's UIA fallback page for one stage of one session on this
/// account's homeserver, the same origin the client already trusts.
fn fallback_page(homeserver: &Url, stage: &str, session: &str) -> Option<Url> {
    let mut url = homeserver.clone();
    url.set_query(None);
    url.set_fragment(None);
    let segments = ["_matrix", "client", "v3", "auth", stage, "fallback", "web"];
    {
        let mut path = url.path_segments_mut().ok()?;
        path.pop_if_empty().extend(segments);
    }
    url.query_pairs_mut().append_pair("session", session);
    browser_page_allowed(&url).then_some(url)
}

/// HTTPS without embedded credentials, or plain HTTP only for loopback
/// development servers, matching homeserver validation.
fn browser_page_allowed(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => return false,
    };
    url.scheme() == "https" || (url.scheme() == "http" && loopback)
}

/// Trimmed, bounded and free of control or direction-override characters;
/// `None` when nothing printable remains.
pub fn bounded_device_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                )
        })
        .collect();
    let bounded: String = cleaned
        .trim()
        .chars()
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    let bounded = bounded.trim_end();
    (!bounded.is_empty()).then(|| bounded.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_are_trimmed_bounded_and_stripped_of_spoofing_characters() {
        assert_eq!(bounded_device_name("  Laptop  "), Some("Laptop".into()));
        assert_eq!(
            bounded_device_name("Work\u{202e}pc\n"),
            Some("Workpc".into())
        );
        assert_eq!(bounded_device_name(" \u{200b}\t "), None);
        let long = "x".repeat(200);
        assert_eq!(
            bounded_device_name(&long).map(|name| name.chars().count()),
            Some(MAX_DEVICE_NAME_CHARS)
        );
    }

    #[test]
    fn fallback_pages_stay_on_the_trusted_homeserver_origin() {
        let homeserver = Url::parse("https://matrix.example.org/").unwrap();
        let page = fallback_page(&homeserver, SSO_STAGE, "abc").unwrap();
        assert_eq!(
            page.as_str(),
            "https://matrix.example.org/_matrix/client/v3/auth/m.login.sso/fallback/web?session=abc"
        );
        let insecure = Url::parse("http://matrix.example.org/").unwrap();
        assert_eq!(fallback_page(&insecure, SSO_STAGE, "abc"), None);
        let loopback = Url::parse("http://127.0.0.1:8008/").unwrap();
        assert!(fallback_page(&loopback, SSO_STAGE, "abc").is_some());
        let credentials = Url::parse("https://user:secret@matrix.example.org/").unwrap();
        assert!(!browser_page_allowed(&credentials));
    }
}
