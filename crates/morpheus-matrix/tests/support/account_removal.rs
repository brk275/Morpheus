// Observe a fixture's already-requested logout; never authorize another delete.
use super::{AccountManager, LogoutResult, MatrixCoreError};
use std::{path::Path, time::Duration};

pub(crate) async fn finish_logout(
    manager: &AccountManager,
    id: &str,
    store: &Path,
    logout: Result<LogoutResult, MatrixCoreError>,
) -> Result<(), &'static str> {
    match logout {
        Ok(_) => {}
        Err(MatrixCoreError::AccountRemovalPending) => {
            eprintln!("MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=logout_pending");
            let removal = match tokio::time::timeout(
                Duration::from_secs(30),
                manager.finish_pending_account_removal(id),
            )
            .await
            {
                Ok(Ok(removal)) => removal,
                Ok(Err(_)) => {
                    eprintln!(
                        "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=existing_observation_error"
                    );
                    return Err("account removal observation failed");
                }
                Err(_) => {
                    eprintln!(
                        "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=existing_observation_timeout"
                    );
                    return Err("account removal observation timed out");
                }
            };
            eprintln!(
                "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=existing_finished complete={} credentials={} store={}",
                u8::from(removal.complete),
                u8::from(removal.credentials_removed),
                u8::from(removal.store_removed),
            );
            if removal.account_id != id {
                eprintln!(
                    "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=wrong_account complete={} credentials={} store={}",
                    u8::from(removal.complete),
                    u8::from(removal.credentials_removed),
                    u8::from(removal.store_removed),
                );
                return Err("account removal observed a different account");
            }
            if !removal.complete || !removal.credentials_removed || !removal.store_removed {
                return Err("account erasure incomplete");
            }
        }
        Err(_) => {
            eprintln!("MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=logout_error");
            return Err("account logout failed");
        }
    }
    if manager
        .accounts()
        .await
        .iter()
        .any(|account| account.id == id)
        || manager
            .app_password_status()
            .await
            .pending_removals
            .iter()
            .any(|pending| pending == id)
    {
        eprintln!("MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=registry_remaining");
        return Err("account removal remains registered");
    }
    match store.try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => {
            eprintln!("MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=store_remaining");
            Err("account store remains present")
        }
        Err(_) => {
            eprintln!("MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=store_observation_error");
            Err("account store observation failed")
        }
    }
}
