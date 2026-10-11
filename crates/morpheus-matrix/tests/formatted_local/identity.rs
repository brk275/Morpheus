//! Fixture-only trust of independently controlled disposable identities.
//! Never used by production account or composer code. Keys/passwords stay in
//! memory and every failure is a unit error, so callers can use a fixed label.

use super::TestAccount;
use matrix_sdk::{
    Client,
    ruma::{UserId, api::client::uiaa},
};
use std::time::Duration;

/// Both clients must be the exact users from the private fixture. Before a
/// manual test-only trust action, compare the observer's master/device keys to
/// the subject client's independently bootstrapped and verified own identity.
/// This is not interactive SAS/QR acceptance or a production trust shortcut.
pub(super) async fn verify_fixture_pair(
    left: &Client,
    left_account: &TestAccount,
    right: &Client,
    right_account: &TestAccount,
) -> Result<(), ()> {
    tokio::time::timeout(Duration::from_secs(45), async {
        if left.user_id() == right.user_id() {
            return Err(());
        }
        bootstrap(left, left_account).await?;
        bootstrap(right, right_account).await?;
        trust_peer(left, right).await?;
        trust_peer(right, left).await
    })
    .await
    .map_err(|_| ())?
}

async fn bootstrap(client: &Client, account: &TestAccount) -> Result<(), ()> {
    let expected = UserId::parse(&account.user_id).map_err(|_| ())?;
    let own = client.user_id().ok_or(())?;
    if own != expected {
        return Err(());
    }
    let encryption = client.encryption();
    encryption.wait_for_e2ee_initialization_tasks().await;
    let identity = encryption
        .request_user_identity(own)
        .await
        .map_err(|_| ())?;
    let device = encryption.get_own_device().await.map_err(|_| ())?;
    if identity.is_some_and(|identity| identity.is_verified())
        && device.is_some_and(|device| {
            device.is_verified_with_cross_signing()
                && !device.is_blacklisted()
                && !device.is_deleted()
        })
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
    if !identity.is_verified() {
        return Err(());
    }
    let device = encryption
        .get_own_device()
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if device.is_blacklisted() || device.is_deleted() {
        return Err(());
    }
    if !device.is_verified_with_cross_signing() {
        device.verify().await.map_err(|_| ())?;
        encryption
            .request_user_identity(own)
            .await
            .map_err(|_| ())?;
    }
    if encryption
        .get_own_device()
        .await
        .map_err(|_| ())?
        .is_some_and(|device| {
            device.is_verified_with_cross_signing()
                && !device.is_blacklisted()
                && !device.is_deleted()
        })
    {
        Ok(())
    } else {
        Err(())
    }
}

async fn trust_peer(observer: &Client, subject: &Client) -> Result<(), ()> {
    let user = subject.user_id().ok_or(())?;
    let device_id = subject.device_id().ok_or(())?;
    let own = subject
        .encryption()
        .request_user_identity(user)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if !own.is_verified() {
        return Err(());
    }
    let expected_master = own.master_key().get_first_key().ok_or(())?.to_base64();
    let own_device = subject
        .encryption()
        .get_own_device()
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if own_device.user_id() != user
        || own_device.device_id() != device_id
        || !own_device.is_verified_with_cross_signing()
        || own_device.is_blacklisted()
        || own_device.is_deleted()
    {
        return Err(());
    }
    let expected_curve = own_device.curve25519_key().ok_or(())?;
    let expected_ed = own_device.ed25519_key().ok_or(())?;
    let peer = observer
        .encryption()
        .request_user_identity(user)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if peer.user_id() != user
        || peer.master_key().get_first_key().ok_or(())?.to_base64() != expected_master
    {
        return Err(());
    }
    let peer_device = observer
        .encryption()
        .get_device(user, device_id)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if peer_device.curve25519_key() != Some(expected_curve)
        || peer_device.ed25519_key() != Some(expected_ed)
        || peer_device.is_blacklisted()
        || peer_device.is_deleted()
    {
        return Err(());
    }
    peer.verify().await.map_err(|_| ())?;
    let identity = observer
        .encryption()
        .request_user_identity(user)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    let device = observer
        .encryption()
        .get_device(user, device_id)
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if identity.is_verified()
        && identity.master_key().get_first_key().ok_or(())?.to_base64() == expected_master
        && device.is_verified_with_cross_signing()
        && !device.is_blacklisted()
        && !device.is_deleted()
        && device.curve25519_key() == Some(expected_curve)
        && device.ed25519_key() == Some(expected_ed)
    {
        Ok(())
    } else {
        Err(())
    }
}
