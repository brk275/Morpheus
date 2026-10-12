//! Real OS-vault and encrypted-store account access checks. Reuses the already
//! logged-in disposable devices; no extra Matrix login or service is created.
use super::*;
use morpheus_matrix::AccountInfo;
use zeroize::Zeroizing;

async fn activate(manager: &AccountManager, id: &str, password: &Zeroizing<String>) {
    let challenge = manager
        .begin_activation(id)
        .await
        .expect("Cannot request protected activation");
    let grant = manager
        .authorize_activation(challenge, password.clone())
        .await
        .expect("App password verification failed");
    manager
        .activate(grant)
        .await
        .expect("Protected activation failed");
}

async fn remove(manager: &AccountManager, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if !manager.account_access_busy() {
            let result = manager
                .remove_account(id)
                .await
                .expect("Local account removal failed");
            if result.complete {
                assert!(result.credentials_removed && result.store_removed);
                let entry = keyring::Entry::new("Morpheus Matrix", id)
                    .expect("Cannot inspect removed fixture credential");
                assert!(
                    matches!(entry.get_password(), Err(keyring::Error::NoEntry)),
                    "Removed account retained a vaulted session"
                );
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Local account cleanup did not finish"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn verify_existing_accounts(
    manager: AccountManager,
    store: &std::path::Path,
    first: &AccountInfo,
    second: &AccountInfo,
    first_room: &str,
    second_room: &str,
    first_body: &str,
) -> AccountManager {
    let password = Zeroizing::new(format!("Disposable app access {}", Uuid::new_v4()));
    assert_eq!(
        manager
            .active_account()
            .await
            .as_ref()
            .map(|a| a.id.as_str()),
        Some(first.id.as_str())
    );
    let original_selection = manager
        .account_selection(&first.id)
        .expect("Missing original selection");
    phase!(password_enable);
    manager
        .set_app_password(None, password.clone())
        .await
        .expect("Cannot enable app password in OS vault");
    assert!(manager.app_password_status().await.enabled);
    assert!(matches!(
        manager.reset_app_password_after_removal().await,
        Err(MatrixCoreError::AccountRemovalPending)
    ));
    phase!(password_rejected_switches);
    assert!(matches!(
        manager.switch_to(&second.id).await,
        Err(MatrixCoreError::AppPasswordRequired)
    ));
    let wrong = manager
        .begin_activation(&second.id)
        .await
        .expect("Cannot request activation");
    assert!(matches!(
        manager
            .authorize_activation(wrong, Zeroizing::new("incorrect fixture password".into()))
            .await,
        Err(MatrixCoreError::AppPasswordIncorrect)
    ));
    let cancelled = manager
        .begin_activation(&second.id)
        .await
        .expect("Cannot request activation");
    cancelled.cancellation().cancel();
    assert!(matches!(
        manager
            .authorize_activation(cancelled, password.clone())
            .await,
        Err(MatrixCoreError::ActivationExpired)
    ));
    let challenge = manager
        .begin_activation(&second.id)
        .await
        .expect("Cannot request activation");
    let cancellation = challenge.cancellation();
    let grant = manager
        .authorize_activation(challenge, password.clone())
        .await
        .expect("Cannot authorize fixture");
    cancellation.cancel();
    assert!(matches!(
        manager.activate(grant).await,
        Err(MatrixCoreError::ActivationExpired)
    ));
    assert!(original_selection.is_current());
    assert_eq!(
        manager
            .active_account()
            .await
            .as_ref()
            .map(|a| a.id.as_str()),
        Some(first.id.as_str())
    );
    phase!(password_cached_switch);
    activate(&manager, &second.id, &password).await;
    assert!(!original_selection.is_current());
    let rooms = wait_for_room(&manager, second_room).await;
    assert!(!rooms.iter().any(|room| room.room_id == first_room));
    activate(&manager, &first.id, &password).await;
    assert_cached_feed_contains(&manager, first_room, first_body).await;
    phase!(password_restart);
    manager.stop().await;
    drop(manager);
    // No intervening sleep: stop must drain vault writers before profile reopen.
    let manager = AccountManager::open(store).expect("Cannot reopen protected profile after stop");
    assert!(manager.active_account().await.is_none());
    assert!(matches!(
        manager.active_client().await,
        Err(MatrixCoreError::NoActiveAccount)
    ));
    assert!(matches!(
        manager.restore_last_active().await,
        Err(MatrixCoreError::AppPasswordRequired)
    ));
    activate(&manager, &first.id, &password).await;
    assert_cached_feed_contains(&manager, first_room, first_body).await;
    phase!(remove_inactive);
    remove(&manager, &second.id).await;
    assert!(!store.join("accounts").join(&second.id).exists());
    assert_eq!(
        manager
            .active_account()
            .await
            .as_ref()
            .map(|a| a.id.as_str()),
        Some(first.id.as_str())
    );
    phase!(remove_active);
    remove(&manager, &first.id).await;
    assert!(!store.join("accounts").join(&first.id).exists());
    assert!(manager.active_account().await.is_none());
    assert!(manager.subscribe_active_account().borrow().is_none());
    assert!(manager.accounts().await.is_empty());
    assert!(
        manager
            .app_password_status()
            .await
            .pending_removals
            .is_empty()
    );
    phase!(password_reset);
    manager
        .reset_app_password_after_removal()
        .await
        .expect("Cannot reset after verified complete removal");
    assert!(!manager.app_password_status().await.enabled);
    manager
}

pub(super) async fn prepare_protected_login(manager: &AccountManager) -> Zeroizing<String> {
    phase!(password_classic_enable);
    let password = Zeroizing::new(format!("Disposable classic access {}", Uuid::new_v4()));
    manager
        .set_app_password(None, password.clone())
        .await
        .expect("Cannot protect classic test profile");
    password
}

pub(super) async fn verify_new_login_paused(
    manager: &AccountManager,
    account: &AccountInfo,
    password: &Zeroizing<String>,
) {
    phase!(protected_login_paused);
    assert!(
        manager
            .accounts()
            .await
            .iter()
            .any(|saved| saved.id == account.id)
    );
    assert!(manager.active_account().await.is_none());
    assert!(manager.account_selection(&account.id).is_none());
    assert!(matches!(
        manager.rooms_snapshot().await,
        Err(MatrixCoreError::NoActiveAccount)
    ));
    assert!(matches!(
        manager.switch_to(&account.id).await,
        Err(MatrixCoreError::AppPasswordRequired)
    ));
    activate(manager, &account.id, password).await;
}
