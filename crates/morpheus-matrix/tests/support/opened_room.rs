//! Positive fixture setup corresponding to opening a joined room in the app.
//! Merely receiving a room in sync does not authorize its paused send queue.

use morpheus_matrix::AccountManager;
use std::time::Duration;

pub async fn scan(manager: &AccountManager, room_id: &str) -> Result<(), &'static str> {
    let account = manager
        .active_account()
        .await
        .ok_or("Fixture has no active account")?;
    let selection = manager
        .account_selection(&account.id)
        .ok_or("Fixture account changed")?;
    tokio::time::timeout(
        Duration::from_secs(30),
        manager.recover_room_for(&selection, room_id),
    )
    .await
    .map_err(|_| "Fixture room recovery timed out")?
    .map_err(|_| "Fixture room recovery failed")?;
    Ok(())
}
