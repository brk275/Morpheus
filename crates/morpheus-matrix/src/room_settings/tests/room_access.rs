//! History visibility, guest access and the directory listing through the
//! real session, client and transport, against the loopback homeserver.

use super::*;

fn put(server: &SettingsServer, kind: &str, content: &str) {
    let entry = (kind.to_owned(), String::new());
    let state = &server.served.state;
    state.lock().unwrap().insert(entry, content.to_owned());
}

fn list(server: &SettingsServer, listing: Option<&str>) {
    *server.served.directory.lock().unwrap() = listing.map(str::to_owned);
}

#[test]
fn each_access_change_is_one_request_that_keeps_every_other_byte() {
    use GuestAccessChoice as Guests;
    use HistoryVisibilityChoice as History;
    use RoomAccessSetting as Setting;
    history_runtime().block_on(async {
        let server = SettingsServer::start();
        let stored = r#"{"org.example.rate":1.10,"history_visibility":"shared"}"#;
        put(&server, "m.room.history_visibility", stored);
        list(&server, Some("private"));
        let (directory, manager, account) = signed_in(&server).await;
        let gate = Arc::new(AtomicBool::new(true));
        let form = RoomSettingsCancellation::new();
        let session = open(&manager, &account, &gate, form).await;
        let reviewed = session.load_access().await.unwrap();
        assert_eq!(reviewed.history, Setting::Set(History::Shared));
        assert_eq!(reviewed.guests, Setting::Default(Guests::Forbidden));
        assert_eq!(reviewed.directory, DirectoryListing::NotListed);
        assert!(reviewed.history_editable() && reviewed.guests_editable());
        assert!(reviewed.directory_editable());
        // Reading sends nothing.
        assert!(server.writes().is_empty());
        // Each later change still checks what was reviewed, not the
        // previous change's read-back.
        let joined = History::Joined;
        let saved = session.set_history_visibility(&reviewed, joined).await;
        assert_eq!(saved.unwrap().history, Setting::Set(joined));
        let allowed = Guests::CanJoin;
        session.set_guest_access(&reviewed, allowed).await.unwrap();
        let listed = session.set_directory_listing(&reviewed, true).await;
        let listed = listed.unwrap();
        assert_eq!(listed.directory, DirectoryListing::Listed);
        assert_eq!(listed.guests, Setting::Set(allowed));
        let writes = server.writes();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].0, "m.room.history_visibility");
        assert_eq!(writes[0].1, stored.replace("\"shared\"", "\"joined\""));
        assert_eq!(writes[1].0, "m.room.guest_access");
        assert_eq!(writes[1].1, r#"{"guest_access":"can_join"}"#);
        assert_eq!(writes[2].0, "directory");
        assert_eq!(writes[2].1, r#"{"visibility":"public"}"#);
        // A setting the room already has sends nothing.
        session.set_directory_listing(&listed, true).await.unwrap();
        session.set_guest_access(&listed, allowed).await.unwrap();
        assert_eq!(server.writes().len(), 3);
        drop(session);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn a_changed_unknown_or_unpermitted_access_setting_sends_nothing() {
    use RoomSettingsError as Error;
    history_runtime().block_on(async {
        let server = SettingsServer::start();
        let joined = r#"{"history_visibility":"joined","org.example.x":1.10}"#;
        put(&server, "m.room.history_visibility", joined);
        list(&server, Some("private"));
        let (directory, manager, account) = signed_in(&server).await;
        let gate = Arc::new(AtomicBool::new(true));
        let form = RoomSettingsCancellation::new();
        let session = open(&manager, &account, &gate, form).await;
        let reviewed = session.load_access().await.unwrap();
        // Another client respells only a number: a different stored text.
        let respelled = joined.replace("1.10", "1.1");
        put(&server, "m.room.history_visibility", &respelled);
        let shared = HistoryVisibilityChoice::Shared;
        let refused = session.set_history_visibility(&reviewed, shared).await;
        assert_eq!(refused.err(), Some(Error::BaselineChanged));
        // The listing changed elsewhere since it was read.
        list(&server, Some("public"));
        let refused = session.set_directory_listing(&reviewed, false).await;
        assert_eq!(refused.err(), Some(Error::BaselineChanged));
        // A value Morpheus does not know is shown, never rewritten.
        let custom = r#"{"history_visibility":"org.example.members"}"#;
        put(&server, "m.room.history_visibility", custom);
        let reviewed = session.load_access().await.unwrap();
        assert_eq!(reviewed.history, RoomAccessSetting::Unrecognized);
        assert!(!reviewed.history_editable());
        let refused = session.set_history_visibility(&reviewed, shared).await;
        assert_eq!(refused.err(), Some(Error::ReadOnly));
        // No directory, or no usable answer: nothing to change.
        list(&server, None);
        let reviewed = session.load_access().await.unwrap();
        assert_eq!(reviewed.directory, DirectoryListing::Unsupported);
        let refused = session.set_directory_listing(&reviewed, true).await;
        assert_eq!(refused.err(), Some(Error::ReadOnly));
        // A known value past the read cap is not taken.
        let pad = Some("x".repeat(MAX_STATE_BYTES));
        *server.served.directory_padding.lock().unwrap() = pad;
        list(&server, Some("public"));
        let reviewed = session.load_access().await.unwrap();
        assert_eq!(reviewed.directory, DirectoryListing::Unknown);
        assert!(!reviewed.directory_editable());
        *server.served.directory_padding.lock().unwrap() = None;
        list(&server, Some("org.example.partly"));
        let reviewed = session.load_access().await.unwrap();
        assert_eq!(reviewed.directory, DirectoryListing::Unknown);
        let refused = session.set_directory_listing(&reviewed, true).await;
        assert_eq!(refused.err(), Some(Error::Unavailable));
        // The room's power levels allow guest access only; the listing
        // needs the alias permission.
        let levels = json!({
            "users": {ME: 50},
            "events": {"m.room.history_visibility": 100, "m.room.canonical_alias": 75}
        });
        put(&server, "m.room.power_levels", &levels.to_string());
        list(&server, Some("private"));
        let reviewed = session.load_access().await.unwrap();
        let allowed = reviewed.permissions;
        assert!(!allowed.history && allowed.guests && !allowed.directory);
        let refused = session.set_directory_listing(&reviewed, true).await;
        assert_eq!(refused.err(), Some(Error::PermissionDenied));
        // A room version without known rules grants nothing.
        let unknown = json!({"creator": ME, "room_version": "org.example.v1"});
        put(&server, "m.room.create", &unknown.to_string());
        let reviewed = session.load_access().await.unwrap();
        assert_eq!(reviewed.permissions, RoomAccessPermissions::default());
        let allow = GuestAccessChoice::CanJoin;
        let refused = session.set_guest_access(&reviewed, allow).await;
        assert_eq!(refused.err(), Some(Error::PermissionDenied));
        assert!(server.writes().is_empty());
        drop(session);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn an_access_change_handed_over_without_an_answer_is_unknown_and_never_resent() {
    use RoomSettingsError as Error;
    history_runtime().block_on(async {
        let server = SettingsServer::start();
        list(&server, Some("private"));
        let (directory, manager, account) = signed_in(&server).await;
        let gate = Arc::new(AtomicBool::new(true));
        // The security gate closes during the fresh permission read: the
        // final check refuses and nothing leaves.
        let form = RoomSettingsCancellation::new();
        let session = open(&manager, &account, &gate, form).await;
        let reviewed = session.load_access().await.unwrap();
        server.served.hold_power.store(true, Ordering::SeqCst);
        let reads = server.power_reads();
        let close = async {
            server.wait_for_power_reads(reads + 1).await;
            gate.store(false, Ordering::SeqCst);
            server.served.hold_power.store(false, Ordering::SeqCst);
        };
        let publish = session.set_directory_listing(&reviewed, true);
        let (result, ()) = tokio::join!(publish, close);
        assert_eq!(result.err(), Some(Error::SecurityBusy));
        assert!(server.writes().is_empty());
        gate.store(true, Ordering::SeqCst);
        // The form closes while the listing change is with the homeserver.
        server.served.hold_writes.store(true, Ordering::SeqCst);
        let form = RoomSettingsCancellation::new();
        let session = open(&manager, &account, &gate, form.clone()).await;
        let reviewed = session.load_access().await.unwrap();
        let close = async {
            server.wait_for_writes(1).await;
            form.cancel();
        };
        let publish = session.set_directory_listing(&reviewed, true);
        let (result, ()) = tokio::join!(publish, close);
        assert_eq!(result.err(), Some(Error::Unknown));
        // The account changes while a state change is with the homeserver.
        let form = RoomSettingsCancellation::new();
        let session = open(&manager, &account, &gate, form).await;
        let reviewed = session.load_access().await.unwrap();
        let switch = async {
            server.wait_for_writes(2).await;
            manager.publish_active_account(None);
        };
        let allow = GuestAccessChoice::CanJoin;
        let (result, ()) = tokio::join!(session.set_guest_access(&reviewed, allow), switch);
        assert_eq!(result.err(), Some(Error::Unknown));
        manager.publish_active_account(Some(account.clone()));
        // No answer within the request's timeout.
        let form = RoomSettingsCancellation::new();
        let session = open(&manager, &account, &gate, form).await;
        let reviewed = session.load_access().await.unwrap();
        let publish = session.set_directory_listing(&reviewed, true);
        let unanswered = time::timeout(Duration::from_secs(30), publish);
        assert_eq!(unanswered.await.unwrap().err(), Some(Error::Unknown));
        // None of them was sent again, and none was applied.
        assert_eq!(server.writes().len(), 3);
        server.served.hold_writes.store(false, Ordering::SeqCst);
        // Answers: a 4xx changed nothing; anything else may have. The
        // unknown-token answer comes last: it marks the token expired.
        let error = |code: &str| json!({"errcode": code, "error": "homeserver text"});
        let answers = [
            (403, error("M_FORBIDDEN"), Error::PermissionDenied),
            (429, error("M_LIMIT_EXCEEDED"), Error::RateLimited),
            (500, error("M_UNKNOWN"), Error::Unknown),
            (502, json!("not json"), Error::Unknown),
            (401, error("M_UNKNOWN_TOKEN"), Error::SignInExpired),
        ];
        for (sent, (status, body, expected)) in answers.into_iter().enumerate() {
            *server.served.write_reply.lock().unwrap() = Some((status, body));
            let result = session.set_directory_listing(&reviewed, true).await;
            assert_eq!(result.err(), Some(expected), "status {status}");
            assert_eq!(server.writes().len(), 3 + sent + 1, "status {status}");
            assert!(!expected.to_string().contains("homeserver text"));
        }
        let listing = server.served.directory.lock().unwrap().clone();
        assert_eq!(listing.as_deref(), Some("private"));
        drop(session);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

/// A client of the loopback homeserver that, like Morpheus's own clients
/// (`build_client`), renews its access token with a refresh token when a
/// request is rejected, synced once into the room.
async fn refreshing_client(server: &SettingsServer) -> matrix_sdk::Client {
    let client = matrix_sdk::Client::builder()
        .homeserver_url(&server.url)
        .handle_refresh_tokens()
        .build()
        .await
        .unwrap();
    let session = matrix_sdk::authentication::matrix::MatrixSession {
        meta: matrix_sdk::SessionMeta {
            user_id: user_id!("@casey:example.org").to_owned(),
            device_id: "SETTINGS-FIXTURE".into(),
        },
        tokens: matrix_sdk::SessionTokens {
            access_token: "synthetic access token".into(),
            refresh_token: Some("synthetic refresh token".into()),
        },
    };
    let restored = client
        .matrix_auth()
        .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
        .await;
    restored.unwrap();
    server.served.syncs.lock().unwrap().push_back(joined_room());
    let settings = matrix_sdk::config::SyncSettings::default();
    client.sync_once(settings).await.unwrap();
    client
}

fn reject(server: &SettingsServer, kind: &str) {
    let expired = json!({"errcode": "M_UNKNOWN_TOKEN", "error": "homeserver text"});
    *server.served.read_reply.lock().unwrap() = Some((kind.to_owned(), 401, expired));
}

/// RAC44-02: the creation, membership and power-level reads behind every
/// access load and change never renew and replay a rejected token: one
/// request, no renewal, and a fixed result. The same client does renew
/// and replay a read sent with an ordinary SDK config, so it really would;
/// the shared `read_config` itself opts out too, so the positive control
/// uses an explicitly ordinary `RequestConfig` of its own. An
/// oversized answer still gets the fixed error; the transport cap itself is
/// checked on the config (`the_access_reads_opt_out_of_authentication_refresh`).
#[test]
fn the_access_reads_never_refresh_or_replay_a_rejected_token() {
    history_runtime().block_on(async {
        let server = SettingsServer::start();
        let client = refreshing_client(&server).await;
        let room = client.get_room(room_id!("!settings:example.org")).unwrap();
        let own = user_id!("@casey:example.org").to_owned();
        let reads = |kind: &str| {
            let reads = server.served.state_reads.lock().unwrap();
            reads.iter().filter(|read| *read == kind).count()
        };
        let refreshes = || server.served.refresh_reads.load(Ordering::SeqCst);
        // Positive control only: an explicitly ordinary SDK config (not
        // `read_config` or any production config, which all opt out) renews
        // the token and replays the read.
        reject(&server, "m.room.create");
        use matrix_sdk::ruma::api::client::state::get_state_event_for_key::v3 as get_state;
        let mut ordinary = get_state::Request::new(
            room.room_id().to_owned(),
            StateEventType::RoomCreate,
            String::new(),
        );
        ordinary.format = get_state::StateEventFormat::Event;
        let ordinary_config = matrix_sdk::config::RequestConfig::new();
        let replayed = room
            .client()
            .send(ordinary)
            .with_request_config(ordinary_config)
            .await;
        assert!(replayed.is_ok(), "read again after the renewal");
        assert_eq!((refreshes(), reads("m.room.create")), (1, 2));
        // Each authority read of the access path: one request, no renewal.
        for kind in ["m.room.create", "m.room.member", "m.room.power_levels"] {
            let before = reads(kind);
            reject(&server, kind);
            let result = room_access_settings::access_authority(&room, &own).await;
            let failed = result.err();
            assert_eq!(failed, Some(RoomSettingsError::Unavailable), "{kind}");
            assert_eq!(reads(kind), before + 1, "{kind}: one request, no replay");
            assert_eq!(refreshes(), 1, "{kind}: no renewal");
        }
        // Without a rejection the same path reads the room.
        let read = room_access_settings::access_authority(&room, &own).await;
        assert!(read.is_ok());
        // An oversized answer gets the fixed error. The parser's own size
        // check gives the same, so this does not show where it was refused;
        // the transport cap is checked on the config itself.
        let padding = "x".repeat(MAX_STATE_BYTES);
        let levels = json!({"users": {ME: 100}, "org.example.pad": padding});
        put(&server, "m.room.power_levels", &levels.to_string());
        let capped = room_access_settings::access_authority(&room, &own).await;
        assert_eq!(capped.err(), Some(RoomSettingsError::InvalidState));
        assert_eq!(refreshes(), 1);
        drop(client);
    });
}
