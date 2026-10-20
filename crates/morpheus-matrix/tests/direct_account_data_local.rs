//! Opt-in regression for the shared `m.direct` writer against the
//! disposable local Synapse fixture used by `drafts_local.rs`. It writes
//! only the fixture account's own account data and restores it afterwards.
//! Covered: unknown fields and other entries kept by the merge, an existing
//! listing writing nothing, a wrong account refused with nothing written, a
//! guard that has ended refusing before anything is sent, and a guard ending
//! as the write starts reporting `Unknown` (never retried). Nothing here
//! prints identifiers or credentials.

use std::{
    collections::HashMap,
    fs,
    future::Future,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use matrix_sdk::ruma::{
    RoomId, UserId,
    api::client::config::{get_global_account_data, set_global_account_data},
    events::GlobalAccountDataEventType,
    serde::Raw,
};
use morpheus_matrix::{AccountManager, DirectListing, DirectWriteError, DirectWriteGuard, list_direct_room};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    accounts: Vec<TestAccount>,
    #[allow(dead_code)]
    rooms: HashMap<String, String>,
}

#[derive(Deserialize)]
struct TestAccount {
    label: String,
    username: String,
    user_id: String,
    homeserver_url: String,
    password: String,
}

/// Never stops.
struct Open;
impl DirectWriteGuard for Open {
    fn stopped(&self) -> bool {
        false
    }
    fn wait_stopped(&self) -> impl Future<Output = ()> + Send {
        std::future::pending()
    }
}

/// Already stopped before anything is sent.
struct Closed;
impl DirectWriteGuard for Closed {
    fn stopped(&self) -> bool {
        true
    }
    fn wait_stopped(&self) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }
}

/// Stops exactly as the write is admitted: the reads see an open guard; the
/// write's race sees it ended.
struct StopsAtWrite(AtomicUsize);
impl DirectWriteGuard for StopsAtWrite {
    fn stopped(&self) -> bool {
        false
    }
    fn wait_stopped(&self) -> impl Future<Output = ()> + Send {
        // Two reads, then the write (the third race).
        let call = self.0.fetch_add(1, Ordering::SeqCst);
        async move {
            if call < 2 {
                std::future::pending::<()>().await;
            }
        }
    }
}

async fn stored(client: &matrix_sdk::Client, own: &UserId) -> Option<Value> {
    let request = get_global_account_data::v3::Request::new(own.to_owned(), GlobalAccountDataEventType::Direct);
    client
        .send(request)
        .await
        .ok()
        .map(|response| serde_json::from_str(response.account_data.json().get()).unwrap())
}

async fn store(client: &matrix_sdk::Client, own: &UserId, content: &Value) {
    let raw = serde_json::value::to_raw_value(content).unwrap();
    let request = set_global_account_data::v3::Request::new_raw(
        own.to_owned(),
        GlobalAccountDataEventType::Direct,
        Raw::from_json(raw),
    );
    client.send(request).await.unwrap();
}

#[test]
fn direct_listing_merges_exactly_and_never_writes_for_another_account() {
    let Some(credentials_path) = std::env::var_os("MORPHEUS_MATRIX_TEST_ACCOUNTS") else {
        eprintln!("Skipped direct account data test; set MORPHEUS_MATRIX_TEST_ACCOUNTS to opt in.");
        return;
    };
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(PathBuf::from(credentials_path)).expect("Cannot read local fixture"),
    )
    .expect("Invalid local fixture");
    let root = std::env::temp_dir().join(format!("morpheus-direct-data-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("Cannot create account data directory");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Cannot start local test runtime");
    runtime.block_on(async {
        let alice = fixture.accounts.iter().find(|a| a.label == "a_primary").unwrap();
        let carol = fixture.accounts.iter().find(|a| a.label == "b_primary").unwrap();
        let manager = AccountManager::open(&root).expect("Cannot open account manager");
        manager
            .add_password_account(&alice.homeserver_url, &alice.username, &alice.password)
            .await
            .expect("Login failed");
        let client = manager.active_client().await.expect("Client missing");
        let own = UserId::parse(&alice.user_id).unwrap();
        let target = UserId::parse(&carol.user_id).unwrap();
        let marker = Uuid::new_v4().simple().to_string();
        let room = RoomId::parse(format!("!synthetic{marker}:example.org")).unwrap();
        let original = stored(&client, &own).await;

        // Seed: another entry and an unknown field, both to be kept.
        let seeded = json!({
            "@other-synthetic:example.org": ["!kept:example.org"],
            "org.example.synthetic": {"keep": marker},
        });
        store(&client, &own, &seeded).await;

        // A wrong account is refused and nothing is written.
        assert_eq!(
            list_direct_room(&client, &target, &target, &room, &Open).await,
            Err(DirectWriteError::NotSent)
        );
        assert_eq!(stored(&client, &own).await.as_ref(), Some(&seeded));

        // A guard that has ended sends nothing.
        assert_eq!(
            list_direct_room(&client, &own, &target, &room, &Closed).await,
            Err(DirectWriteError::NotSent)
        );
        assert_eq!(stored(&client, &own).await.as_ref(), Some(&seeded));

        // A guard ending as the write is admitted is unknown, never retried.
        assert_eq!(
            list_direct_room(&client, &own, &target, &room, &StopsAtWrite(AtomicUsize::new(0))).await,
            Err(DirectWriteError::Unknown)
        );

        // The merge keeps every other entry and the unknown field exactly.
        store(&client, &own, &seeded).await;
        assert_eq!(
            list_direct_room(&client, &own, &target, &room, &Open).await,
            Ok(DirectListing::Listed)
        );
        let after = stored(&client, &own).await.unwrap();
        assert_eq!(after["@other-synthetic:example.org"], seeded["@other-synthetic:example.org"]);
        assert_eq!(after["org.example.synthetic"], seeded["org.example.synthetic"]);
        assert_eq!(after[target.as_str()], json!([room.as_str()]));
        // Listing again writes nothing.
        assert_eq!(
            list_direct_room(&client, &own, &target, &room, &Open).await,
            Ok(DirectListing::AlreadyListed)
        );

        // Restore the fixture account's own listing.
        store(&client, &own, &original.unwrap_or_else(|| json!({}))).await;
    });
    let _ = fs::remove_dir_all(&root);
}
