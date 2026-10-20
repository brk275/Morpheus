use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Mutex as StdMutex, atomic::AtomicUsize},
};

use matrix_sdk::ruma::{event_id, room_id, user_id};
use serde_json::{Value, json};

use super::*;
use crate::history::tests::{fixture, runtime};

const ROOM: &str = "!media:example.org";
const ME: &str = "@casey:example.org";
const CAPTION: &str = "Secret caption";
const CIPHERTEXT: &str = "CIPHERTEXT-NEVER-SHOWN";

fn message(id: &str, room: &str, content: Value) -> Value {
    json!({
        "type": "m.room.message",
        "event_id": id,
        "room_id": room,
        "sender": "@ana:example.org",
        "origin_server_ts": 1_700_000_000_000_u64,
        "content": content,
    })
}

fn image(id: &str) -> Value {
    message(
        id,
        ROOM,
        json!({
            "msgtype": "m.image",
            "body": CAPTION,
            "filename": "holiday.png",
            "url": "mxc://example.org/holiday",
            "info": { "mimetype": "image/PNG", "size": 2048 },
        }),
    )
}

fn video(id: &str) -> Value {
    message(
        id,
        ROOM,
        json!({ "msgtype": "m.video", "body": "clip.mp4", "url": "mxc://example.org/clip" }),
    )
}

fn text(id: &str) -> Value {
    message(id, ROOM, json!({ "msgtype": "m.text", "body": CAPTION }))
}

fn encrypted(id: &str) -> Value {
    json!({
        "type": "m.room.encrypted",
        "event_id": id,
        "room_id": ROOM,
        "sender": "@ana:example.org",
        "origin_server_ts": 1_700_000_000_000_u64,
        "content": {
            "algorithm": "m.megolm.v1.aes-sha2",
            "ciphertext": CIPHERTEXT,
            "device_id": "ANADEVICE",
            "sender_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "session_id": "c2Vzc2lvbiBpZA",
        },
    })
}

fn raw(value: &Value) -> Raw<AnyTimelineEvent> {
    Raw::from_json(serde_json::value::to_raw_value(value).unwrap())
}

fn seen(value: Value, encrypted_room: bool) -> Seen {
    let raw = raw(&value);
    let id = raw.get_field::<OwnedEventId>("event_id").unwrap().unwrap();
    let event = TimelineEvent::from_plaintext(raw.cast());
    project(&id, &event, encrypted_room)
}

fn listed(value: Value) -> RoomMediaItem {
    match seen(value, false) {
        Seen::Media(item) => item,
        other => panic!("not media: {other:?}"),
    }
}

#[test]
fn only_declared_media_metadata_is_kept_never_the_body_or_the_source() {
    let mut threaded = image("$image:example.org");
    threaded["content"]["m.relates_to"] =
        json!({ "rel_type": "m.thread", "event_id": "$root:example.org" });
    let image_item = listed(threaded);
    assert_eq!(image_item.event_id, "$image:example.org");
    assert_eq!(image_item.kind, RoomMediaKind::Image);
    assert_eq!(image_item.filename, "holiday.png");
    assert_eq!(image_item.mime_type, "image/png");
    assert_eq!(image_item.size, Some(2048));
    assert_eq!(image_item.timestamp_ms, 1_700_000_000_000);
    assert_eq!(image_item.sender_id, "@ana:example.org");
    assert_eq!(image_item.thread_root.as_deref(), Some("$root:example.org"));
    assert!(!image_item.encrypted);
    // Sent without encryption in a room known to be unencrypted.
    assert!(!image_item.authenticity_warning);
    let shown = format!("{image_item:?}");
    assert!(!shown.contains(CAPTION), "{shown}");
    assert!(!shown.contains("mxc://"), "{shown}");
    // The same message in an encrypted room is not tied to its sender.
    let Seen::Media(warned) = seen(image("$image:example.org"), true) else {
        panic!("not media");
    };
    assert!(warned.authenticity_warning);
    let video = listed(video("$video:example.org"));
    assert_eq!(video.kind, RoomMediaKind::Video);
    assert_eq!(video.filename, "clip.mp4");
    assert_eq!(video.mime_type, "");
    assert_eq!(video.size, None);
}

#[test]
fn hostile_names_and_types_become_bounded_plain_text() {
    let long = "n".repeat(1000);
    let cases = [
        (
            "../../etc/\u{202e}gpj.exe\nmore",
            "text/html; charset=utf-8",
            "text/html",
        ),
        (long.as_str(), "not a type", ""),
        ("", "application/pdf", "application/pdf"),
    ];
    for (name, mime, expected) in cases {
        let file = message(
            "$file:example.org",
            ROOM,
            json!({
                "msgtype": "m.file",
                "body": "",
                "filename": name,
                "url": "mxc://example.org/file",
                "info": { "mimetype": mime },
            }),
        );
        let file = listed(file);
        assert!(!file.filename.is_empty());
        assert!(file.filename.chars().count() <= MAX_MEDIA_NAME_CHARS);
        assert!(
            !file
                .filename
                .chars()
                .any(|c| c == '/' || c == '\\' || c == '\n' || c == '\u{202e}'),
            "{:?}",
            file.filename
        );
        assert_eq!(file.mime_type, expected);
    }
    let huge = format!("x/{}", "y".repeat(MAX_MEDIA_TYPE_CHARS));
    assert_eq!(media_type(&huge), "");
}

#[test]
fn edits_text_stickers_state_and_removed_messages_are_not_media() {
    let mut edit = image("$edit:example.org");
    edit["content"]["m.new_content"] = edit["content"].clone();
    edit["content"]["m.relates_to"] =
        json!({ "rel_type": "m.replace", "event_id": "$image:example.org" });
    assert_eq!(seen(edit, false), Seen::Other);
    assert_eq!(seen(text("$text:example.org"), false), Seen::Other);
    let sticker = json!({
        "type": "m.sticker",
        "event_id": "$sticker:example.org",
        "room_id": ROOM,
        "sender": "@ana:example.org",
        "origin_server_ts": 1,
        "content": { "body": "wave", "url": "mxc://example.org/wave", "info": {} },
    });
    assert_eq!(seen(sticker, false), Seen::Other);
    let mut removed = image("$removed:example.org");
    removed["content"] = json!({});
    removed["unsigned"] = json!({ "redacted_because": {
        "type": "m.room.redaction",
        "event_id": "$redaction:example.org",
        "sender": "@ana:example.org",
        "origin_server_ts": 2,
        "redacts": "$removed:example.org",
        "content": {},
    }});
    assert_eq!(seen(removed, false), Seen::Other);
    let state = json!({
        "type": "m.room.name",
        "state_key": "",
        "event_id": "$name:example.org",
        "room_id": ROOM,
        "sender": "@ana:example.org",
        "origin_server_ts": 1,
        "content": { "name": "Plans" },
    });
    assert_eq!(seen(state, false), Seen::Other);
}

#[test]
fn an_undecryptable_message_is_counted_and_shows_nothing_of_itself() {
    let secret = encrypted("$secret:example.org");
    assert_eq!(seen(secret, true), Seen::Undecryptable);
}

#[test]
fn an_event_that_is_not_what_it_was_admitted_as_is_skipped() {
    let raw = raw(&image("$image:example.org"));
    let event = TimelineEvent::from_plaintext(raw.cast());
    let other = event_id!("$other:example.org");
    assert_eq!(project(other, &event, false), Seen::Skipped);
    let mut large = image("$large:example.org");
    large["content"]["org.example.pad"] = json!("p".repeat(MAX_EVENT_BYTES));
    assert_eq!(seen(large, false), Seen::Skipped);
}

#[test]
fn a_chunk_is_bounded_to_one_page_of_this_rooms_events_once_each() {
    let room = room_id!("!media:example.org");
    let elsewhere = message("$elsewhere:example.org", "!other:example.org", json!({}));
    let mut chunk = vec![
        raw(&elsewhere),
        raw(&image("$twice:example.org")),
        raw(&image("$twice:example.org")),
    ];
    let mut large = image("$large:example.org");
    large["content"]["org.example.pad"] = json!("p".repeat(MAX_EVENT_BYTES));
    chunk.push(raw(&large));
    for index in 0..MEDIA_PAGE_EVENTS as usize {
        chunk.push(raw(&text(&format!("$text{index}:example.org"))));
    }
    let mut seen = HashSet::new();
    let mut page = RoomMediaPage::default();
    let admitted = admit_chunk(
        room,
        chunk,
        MEDIA_PAGE_EVENTS as usize,
        &mut seen,
        &mut page,
    );
    // A longer page than asked for is not read further.
    assert!(page.truncated);
    assert_eq!(page.examined, MEDIA_PAGE_EVENTS as usize);
    // Another room's event and the oversized one, never parsed.
    assert_eq!(page.skipped, 2);
    // The duplicate is one message, not a skipped one.
    assert_eq!(admitted.len(), MEDIA_PAGE_EVENTS as usize - 3);
    assert_eq!(admitted[0].0.as_str(), "$twice:example.org");
    // An event seen earlier in the load is not admitted again.
    let twice = vec![raw(&image("$twice:example.org"))];
    let again = admit_chunk(
        room,
        twice,
        MEDIA_PAGE_EVENTS as usize,
        &mut seen,
        &mut page,
    );
    assert!(again.is_empty());
}

#[test]
fn paging_continues_only_from_a_new_usable_token() {
    assert_eq!(next_step(None, None), Next::Start);
    assert_eq!(next_step(Some("t1"), None), Next::Start);
    let t1 = || Some("t1".to_owned());
    assert_eq!(next_step(None, t1()), Next::Continue("t1".into()));
    let t2 = Some("t2".to_owned());
    assert_eq!(next_step(Some("t1"), t2), Next::Continue("t2".into()));
    assert_eq!(next_step(Some("t1"), t1()), Next::Stuck);
    assert_eq!(next_step(None, Some(String::new())), Next::Stuck);
    assert_eq!(next_step(None, Some("t\n1".into())), Next::Stuck);
}

#[test]
fn only_a_room_known_to_be_unencrypted_is_filtered_by_media_address() {
    let open = media_filter(false);
    assert_eq!(open.types, Some(vec!["m.room.message".to_owned()]));
    assert!(matches!(open.url_filter, Some(UrlFilter::EventsWithUrl)));
    let closed = media_filter(true);
    let types = vec!["m.room.message".to_owned(), "m.room.encrypted".to_owned()];
    assert_eq!(closed.types, Some(types));
    assert!(closed.url_filter.is_none());
}

#[test]
fn failures_are_fixed_sentences_and_only_transient_ones_are_retryable() {
    let too_large = HttpError::ResponseTooLarge {
        limit: MAX_PAGE_RESPONSE_BYTES,
    };
    assert_eq!(media_failure(&too_large, false), RoomMediaError::TooLarge);
    let retryable = [
        RoomMediaError::RateLimited,
        RoomMediaError::TimedOut,
        RoomMediaError::Unavailable,
    ];
    for error in [
        RoomMediaError::AccountChanged,
        RoomMediaError::Cancelled,
        RoomMediaError::InvalidRoom,
        RoomMediaError::InvalidToken,
        RoomMediaError::NotJoined,
        RoomMediaError::Forbidden,
        RoomMediaError::RateLimited,
        RoomMediaError::TooLarge,
        RoomMediaError::ReadLimit,
        RoomMediaError::TimedOut,
        RoomMediaError::Unavailable,
    ] {
        assert!(!error.text().is_empty());
        assert_eq!(error.retryable(), retryable.contains(&error), "{error:?}");
    }
}

// A loopback homeserver: one joined room of one account with a restored
// synthetic session. No real account, network, vault or media.
#[derive(Clone, Default)]
struct Served {
    syncs: Arc<StdMutex<VecDeque<Value>>>,
    /// `/messages` answers in order: status and body.
    pages: Arc<StdMutex<VecDeque<(u16, String)>>>,
    /// Each `/messages` request target (path and query) as received.
    requests: Arc<StdMutex<Vec<String>>>,
    received: Arc<AtomicUsize>,
    refreshes: Arc<AtomicUsize>,
    /// A `/messages` request taken while held is never answered.
    hold: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

struct MediaServer {
    url: String,
    served: Served,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl MediaServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = Served::default();
        let serving = served.clone();
        let worker = std::thread::spawn(move || {
            while !serving.stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let serving = serving.clone();
                        std::thread::spawn(move || answer(stream, &serving));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            served,
            worker: Some(worker),
        }
    }

    fn queue(&self, status: u16, body: String) {
        self.served.pages.lock().unwrap().push_back((status, body));
    }

    fn requests(&self) -> Vec<String> {
        self.served.requests.lock().unwrap().clone()
    }

    async fn wait_for_requests(&self, count: usize) {
        while self.served.received.load(Ordering::SeqCst) < count {
            time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for MediaServer {
    fn drop(&mut self) {
        self.served.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn page(events: Vec<Value>, end: Option<&str>) -> String {
    let mut body = json!({ "start": "s0", "chunk": events });
    if let Some(end) = end {
        body["end"] = json!(end);
    }
    body.to_string()
}

fn joined_room() -> Value {
    let state = |kind: &str, key: &str, content: Value| {
        json!({
            "type": kind,
            "state_key": key,
            "sender": ME,
            "event_id": format!("$state-{kind}"),
            "room_id": ROOM,
            "origin_server_ts": 1,
            "content": content,
        })
    };
    let create = json!({ "room_version": "10", "creator": ME });
    let events = vec![
        state("m.room.create", "", create),
        state("m.room.member", ME, json!({ "membership": "join" })),
    ];
    let mut join = serde_json::Map::new();
    let room = json!({
        "state": { "events": events },
        "timeline": { "events": [], "limited": false },
    });
    join.insert(ROOM.to_owned(), room);
    json!({ "next_batch": "s1", "rooms": { "join": join } })
}

fn answer(mut stream: TcpStream, served: &Served) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let line = read_request(&mut stream);
    let target = line.split(' ').nth(1).unwrap_or("").to_owned();
    let unrecognized = json!({ "errcode": "M_UNRECOGNIZED", "error": "x" }).to_string();
    let (status, reply) = if target.starts_with("/_matrix/client/versions") {
        (200, json!({ "versions": ["v1.15"] }).to_string())
    } else if target.contains("/sync") {
        let next = served.syncs.lock().unwrap().pop_front();
        let idle = json!({ "next_batch": "idle" });
        (200, next.unwrap_or(idle).to_string())
    } else if target.ends_with("/refresh") {
        served.refreshes.fetch_add(1, Ordering::SeqCst);
        (
            200,
            json!({ "access_token": "synthetic replacement token" }).to_string(),
        )
    } else if target.contains("/messages") {
        served.requests.lock().unwrap().push(target);
        served.received.fetch_add(1, Ordering::SeqCst);
        if served.hold.load(Ordering::SeqCst) {
            while !served.stopped.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            return;
        }
        let next = served.pages.lock().unwrap().pop_front();
        next.unwrap_or_else(|| (200, page(Vec::new(), None)))
    } else {
        (404, unrecognized)
    };
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The request line of one HTTP/1.1 request; its body is read and dropped.
fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(size) = stream.read(&mut chunk) {
        if size == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..size]);
        let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]).to_string();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                let length = name.eq_ignore_ascii_case("content-length");
                length.then(|| value.trim().parse::<usize>().ok()).flatten()
            })
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            return headers.lines().next().unwrap_or("").to_owned();
        }
    }
    String::new()
}

/// A fresh manager whose selected account is signed in with a synthetic
/// session against `server` and synced once into the joined room.
async fn signed_in(
    server: &MediaServer,
) -> (std::path::PathBuf, Arc<AccountManager>, crate::AccountInfo) {
    let account = crate::AccountInfo {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: ME.into(),
        device_id: "MEDIA-FIXTURE".into(),
        homeserver: server.url.clone(),
    };
    let (directory, manager, account) = fixture(Some(account)).await;
    // Match production token-refresh policy so the no-replay test would
    // actually refresh/replay without the feature's explicit opt-out.
    let client = Client::builder()
        .homeserver_url(&server.url)
        .handle_refresh_tokens()
        .build()
        .await
        .unwrap();
    client.send_queue().set_enabled(false).await;
    {
        let mut state = manager.state.lock().await;
        let loaded = Arc::get_mut(state.loaded.get_mut(&account.id).unwrap())
            .expect("fresh synthetic fixture has one loaded-account owner");
        loaded.client = client.clone();
    };
    let session = matrix_sdk::authentication::matrix::MatrixSession {
        meta: matrix_sdk::SessionMeta {
            user_id: user_id!("@casey:example.org").to_owned(),
            device_id: "MEDIA-FIXTURE".into(),
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
    (directory, manager, account)
}

fn assert_private(page: &RoomMediaPage) {
    let shown = format!("{page:?}");
    for secret in [CAPTION, CIPHERTEXT, "mxc://"] {
        assert!(!shown.contains(secret), "{secret} reached the page");
    }
}

#[test]
fn a_load_reads_back_to_the_start_and_lists_this_rooms_media_once() {
    runtime().block_on(async {
        let server = MediaServer::start();
        let (directory, manager, account) = signed_in(&server).await;
        let selection = manager.account_selection(&account.id).unwrap();
        let hostile = message(
            "$file:example.org",
            ROOM,
            json!({
                "msgtype": "m.file",
                "body": "notes",
                "filename": "../../\u{202e}txt.exe",
                "url": "mxc://example.org/notes",
            }),
        );
        let first = vec![
            image("$image:example.org"),
            text("$text:example.org"),
            message("$elsewhere:example.org", "!other:example.org", json!({})),
            image("$image:example.org"),
            video("$video:example.org"),
        ];
        server.queue(200, page(first, Some("t1")));
        let second = vec![hostile, encrypted("$secret:example.org")];
        server.queue(200, page(second, None));
        let cancellation = RoomMediaCancellation::new();
        let loaded = manager
            .room_media_for(&selection, ROOM, None, &cancellation)
            .await
            .unwrap();
        let ids: Vec<_> = loaded
            .items
            .iter()
            .map(|item| item.event_id.as_str())
            .collect();
        let expected = [
            "$image:example.org",
            "$video:example.org",
            "$file:example.org",
        ];
        assert_eq!(ids, expected);
        assert!(loaded.reached_start);
        assert_eq!(loaded.next, None);
        assert!(!loaded.truncated);
        assert_eq!(loaded.examined, 7);
        assert_eq!(loaded.skipped, 1);
        assert_eq!(loaded.undecryptable, 1);
        assert!(!loaded.items[2].filename.contains('/'));
        assert!(!loaded.items[2].filename.contains('\u{202e}'));
        assert_private(&loaded);
        // Two bounded backward pages, the second from the first's end.
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert!(request.contains("dir=b"), "{request}");
            assert!(request.contains("limit=50"), "{request}");
            assert!(request.contains("m.room.message"), "{request}");
        }
        assert!(!requests[0].contains("from="));
        assert!(requests[1].contains("from=t1"));
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn a_held_page_ends_with_the_cancellation_or_the_account_and_holds_no_lock() {
    runtime().block_on(async {
        let server = MediaServer::start();
        let (directory, manager, account) = signed_in(&server).await;
        let selection = manager.account_selection(&account.id).unwrap();
        server.served.hold.store(true, Ordering::SeqCst);
        let cancellation = RoomMediaCancellation::new();
        let stop = async {
            server.wait_for_requests(1).await;
            // The request is with the homeserver: the manager is not locked.
            assert!(manager.state.try_lock().is_ok());
            cancellation.cancel();
        };
        let load = manager.room_media_for(&selection, ROOM, None, &cancellation);
        let (result, ()) = tokio::join!(load, stop);
        assert_eq!(result, Err(RoomMediaError::Cancelled));
        // A cancelled browser starts nothing more.
        let again = manager.room_media_for(&selection, ROOM, None, &cancellation);
        assert_eq!(again.await, Err(RoomMediaError::Cancelled));
        assert_eq!(server.requests().len(), 1);
        // The account changes while the next page is held.
        let cancellation = RoomMediaCancellation::new();
        let switch = async {
            server.wait_for_requests(2).await;
            manager.publish_active_account(None);
        };
        let load = manager.room_media_for(&selection, ROOM, None, &cancellation);
        let (result, ()) = tokio::join!(load, switch);
        assert_eq!(result, Err(RoomMediaError::AccountChanged));
        let after = manager.room_media_for(&selection, ROOM, None, &cancellation);
        assert_eq!(after.await, Err(RoomMediaError::AccountChanged));
        assert_eq!(server.requests().len(), 2);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn a_refused_or_oversized_page_is_a_fixed_error_and_a_bad_token_sends_nothing() {
    runtime().block_on(async {
        let server = MediaServer::start();
        let (directory, manager, account) = signed_in(&server).await;
        let selection = manager.account_selection(&account.id).unwrap();
        let error = |code: &str| json!({ "errcode": code, "error": "homeserver text" }).to_string();
        let mut huge = text("$huge:example.org");
        huge["content"]["org.example.pad"] = json!("p".repeat(MAX_PAGE_RESPONSE_BYTES as usize));
        server.queue(429, error("M_LIMIT_EXCEEDED"));
        server.queue(403, error("M_FORBIDDEN"));
        server.queue(200, page(vec![huge], None));
        server.queue(400, error("M_INVALID_PARAM"));
        let cancellation = RoomMediaCancellation::new();
        let expected = [
            (None, RoomMediaError::RateLimited),
            (None, RoomMediaError::Forbidden),
            (None, RoomMediaError::TooLarge),
            (Some("t9"), RoomMediaError::InvalidToken),
        ];
        for (from, expected) in expected {
            let result = manager.room_media_for(&selection, ROOM, from, &cancellation);
            let result = result.await;
            assert_eq!(result, Err(expected));
            assert!(!expected.text().contains("homeserver text"));
        }
        assert_eq!(server.requests().len(), 4);
        // An unusable token or room is refused before anything is sent.
        let bad = manager.room_media_for(&selection, ROOM, Some("bad\ntoken"), &cancellation);
        assert_eq!(bad.await, Err(RoomMediaError::InvalidToken));
        let room = manager.room_media_for(&selection, "not a room", None, &cancellation);
        assert_eq!(room.await, Err(RoomMediaError::InvalidRoom));
        assert_eq!(server.requests().len(), 4);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn a_load_stops_at_its_request_budget_and_says_where_to_continue() {
    runtime().block_on(async {
        let server = MediaServer::start();
        let (directory, manager, account) = signed_in(&server).await;
        let selection = manager.account_selection(&account.id).unwrap();
        for index in 1..=MEDIA_LOAD_REQUESTS {
            let events = vec![text(&format!("$text{index}:example.org"))];
            server.queue(200, page(events, Some(&format!("t{index}"))));
        }
        let cancellation = RoomMediaCancellation::new();
        let loaded = manager
            .room_media_for(&selection, ROOM, None, &cancellation)
            .await
            .unwrap();
        assert!(loaded.items.is_empty());
        assert_eq!(loaded.examined, MEDIA_LOAD_REQUESTS);
        assert_eq!(loaded.next, Some(format!("t{MEDIA_LOAD_REQUESTS}")));
        // A load that ran out of requests did not reach the start.
        assert!(!loaded.reached_start);
        assert!(!loaded.truncated);
        assert_eq!(server.requests().len(), MEDIA_LOAD_REQUESTS);
        // A server that answers with the token it was given cannot be paged.
        let from = loaded.next.unwrap();
        server.queue(200, page(Vec::new(), Some(&from)));
        let stuck = manager
            .room_media_for(&selection, ROOM, Some(&from), &cancellation)
            .await
            .unwrap();
        assert!(stuck.truncated);
        assert!(!stuck.reached_start);
        assert_eq!(stuck.next, None);
        assert_eq!(server.requests().len(), MEDIA_LOAD_REQUESTS + 1);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn an_overlong_page_stops_at_the_gap_even_if_the_server_reports_the_start() {
    runtime().block_on(async {
        for end in [Some("past-unread-tail"), None] {
            let server = MediaServer::start();
            let (directory, manager, account) = signed_in(&server).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let events = (0..=MEDIA_PAGE_EVENTS)
                .map(|index| image(&format!("$image-{index}:example.org")))
                .collect();
            server.queue(200, page(events, end));
            server.queue(200, page(vec![image("$older:example.org")], None));
            let loaded = manager
                .room_media_for(&selection, ROOM, None, &RoomMediaCancellation::new())
                .await
                .unwrap();
            assert_eq!(server.requests().len(), 1);
            assert_eq!(loaded.examined, MEDIA_PAGE_EVENTS as usize);
            assert!(loaded.truncated && !loaded.reached_start);
            assert_eq!(loaded.next, None);
            assert!(
                !loaded
                    .items
                    .iter()
                    .any(|item| item.event_id == "$older:example.org")
            );
            drop(manager);
            let _ = std::fs::remove_dir_all(directory);
        }
    });
}

#[test]
fn a_final_one_event_budget_is_sent_as_one_and_zero_sends_nothing() {
    runtime().block_on(async {
        let server = MediaServer::start();
        let (directory, manager, account) = signed_in(&server).await;
        let selection = manager.account_selection(&account.id).unwrap();
        server.queue(200, page(vec![image("$last:example.org")], Some("older")));
        let cancellation = RoomMediaCancellation::new();
        let loaded = manager
            .room_media_for_budget(&selection, ROOM, None, 1, &cancellation)
            .await
            .unwrap();
        assert_eq!(loaded.examined, 1);
        assert_eq!(loaded.next.as_deref(), Some("older"));
        assert!(!loaded.truncated && !loaded.reached_start);
        assert_eq!(server.requests().len(), 1);
        let requests = server.requests();
        let query = requests[0].split_once('?').unwrap().1;
        assert!(query.split('&').any(|part| part == "limit=1"), "{query}");
        let refused = manager
            .room_media_for_budget(&selection, ROOM, Some("older"), 0, &cancellation)
            .await;
        assert_eq!(refused, Err(RoomMediaError::ReadLimit));
        assert_eq!(server.requests().len(), 1);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn page_admission_rechecks_joined_membership_after_the_previous_wait() {
    runtime().block_on(async {
        let server = MediaServer::start();
        let (directory, manager, account) = signed_in(&server).await;
        let selection = manager.account_selection(&account.id).unwrap();
        let client = manager.hierarchy_client_for(&selection).await.unwrap();
        let room = client.get_room(room_id!("!media:example.org")).unwrap();
        assert!(page_request(&room, None, true, 50).is_ok());
        let left = json!({
            "type": "m.room.member", "state_key": ME, "sender": ME,
            "event_id": "$left:example.org", "origin_server_ts": 2,
            "content": { "membership": "leave" },
        });
        let mut leave = serde_json::Map::new();
        leave.insert(ROOM.into(), json!({ "timeline": { "events": [left] } }));
        server.served.syncs.lock().unwrap().push_back(json!({
            "next_batch": "s2", "rooms": { "leave": leave },
        }));
        client
            .sync_once(matrix_sdk::config::SyncSettings::default())
            .await
            .unwrap();
        assert_eq!(room.state(), RoomState::Left);
        assert!(matches!(
            page_request(&room, Some("next".into()), true, 50),
            Err(RoomMediaError::NotJoined)
        ));
        let read = manager
            .room_media_for(&selection, ROOM, None, &RoomMediaCancellation::new())
            .await;
        assert_eq!(read, Err(RoomMediaError::NotJoined));
        assert!(server.requests().is_empty());
        drop(room);
        drop(client);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn an_unknown_token_never_refreshes_or_replays_the_history_page() {
    runtime().block_on(async {
        let server = MediaServer::start();
        let (directory, manager, account) = signed_in(&server).await;
        let selection = manager.account_selection(&account.id).unwrap();
        server.queue(
            401,
            json!({ "errcode": "M_UNKNOWN_TOKEN", "error": "expired" }).to_string(),
        );
        server.queue(200, page(vec![image("$replayed:example.org")], None));
        let result = manager
            .room_media_for(&selection, ROOM, None, &RoomMediaCancellation::new())
            .await;
        assert_eq!(result, Err(RoomMediaError::Unavailable));
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.served.refreshes.load(Ordering::SeqCst), 0);
        drop(manager);
        let _ = std::fs::remove_dir_all(directory);
    });
}
