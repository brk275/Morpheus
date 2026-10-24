// Copyright Morpheus contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

// Real SDK requests over a disposable loopback/session/sync fixture. No native
// media, OS vault, external server or background per-connection/task owners.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use matrix_rtc_call_sdk::SdkMatrixBackend;
use matrix_rtc_core::MatrixBackend;
use matrix_sdk::{Client, config::SyncSettings, ruma::user_id};
use serde_json::{Value, json};
use tokio::sync::oneshot;

const ROOM: &str = "!slot-authority:example.test";
const USER: &str = "@alice:example.test";
const KEY: &str = "m.call#ROOM";
const STABLE: &str = "m.rtc.slot";
const UNSTABLE: &str = "org.matrix.msc4143.rtc.slot";
const TOKEN: &str = "synthetic-slot-fixture";
const REQUEST_LIMIT: usize = 64;
const HEADER_LIMIT: usize = 16 * 1024;
const BODY_LIMIT: usize = 64 * 1024;
const SOCKET_LIMIT: Duration = Duration::from_secs(2);
const OPERATION_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Request {
    method: String,
    target: String,
    body: String,
    authorized: bool,
}

struct Server {
    address: SocketAddr,
    stopped: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
    release: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<JoinHandle<Result<(), &'static str>>>,
    joined: Option<bool>,
}

impl Server {
    fn start(status: u16, body: String, hold: bool) -> (Self, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind synthetic loopback fixture");
        let address = listener.local_addr().expect("synthetic listener address");
        let stopped = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stopped);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut entered = Some(entered_tx);
            // One original worker handles at most 64 requests serially. No
            // request spawns another worker. Drop releases any held GET, wakes
            // this listener and actually joins this handle, including panic.
            for _ in 0..REQUEST_LIMIT {
                let (mut stream, peer) = listener.accept().map_err(|_| "fixture accept")?;
                if stopping.load(Ordering::SeqCst) {
                    return Ok(());
                }
                if !peer.ip().is_loopback() {
                    return Err("non-loopback fixture request");
                }
                stream
                    .set_write_timeout(Some(SOCKET_LIMIT))
                    .map_err(|_| "fixture write timeout")?;
                let request = read_request(&mut stream)?;
                let is_state = request.target.contains("/state/");
                if is_state {
                    recorded
                        .lock()
                        .map_err(|_| "fixture record lock")?
                        .push(request.clone());
                }
                let (answer_status, answer_body) =
                    if request.target.starts_with("/_matrix/client/versions") {
                        (200, json!({"versions":["v1.15"]}).to_string())
                    } else if request.target.starts_with("/_matrix/client/v3/sync") {
                        (200, joined_room().to_string())
                    } else if is_state && request.method == "GET" {
                        if let Some(entered) = entered.take() {
                            let _ = entered.send(());
                        }
                        if hold {
                            // The owning Server releases/disconnects this channel
                            // on every unwind/cancellation path before its join.
                            let _ = release_rx.recv();
                        }
                        if status == 0 {
                            continue; // Actual transport disconnect, not an SDK error model.
                        }
                        (status, body.clone())
                    } else if is_state && request.method == "PUT" {
                        (200, json!({"event_id":"$slot-written"}).to_string())
                    } else {
                        // Only generated session/crypto support traffic reaches
                        // this fallback; no owner credential is available here.
                        (200, "{}".to_owned())
                    };
                let response = format!(
                    "HTTP/1.1 {answer_status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer_body}",
                    answer_body.len()
                );
                // A client can drop its entered request at the test deadline.
                // The original worker still retires through the checked join.
                let _ = stream.write_all(response.as_bytes());
            }
            Err("fixture request limit")
        });
        (
            Self {
                address,
                stopped,
                requests,
                release: Some(release_tx),
                worker: Some(worker),
                joined: None,
            },
            entered_rx,
        )
    }

    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }

    fn finish(&mut self) -> bool {
        if let Some(joined) = self.joined {
            return joined;
        }
        self.release();
        self.stopped.store(true, Ordering::SeqCst);
        if self.worker.is_some() {
            // Bounded wake of this original loopback listener only. The socket
            // read has a whole-request deadline, so join cannot wait on a
            // partially sent header forever after this wake.
            let _ = TcpStream::connect_timeout(&self.address, SOCKET_LIMIT);
        }
        let joined = self
            .worker
            .take()
            .is_none_or(|worker| matches!(worker.join(), Ok(Ok(()))));
        self.joined = Some(joined);
        joined
    }

    fn observed(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, &'static str> {
    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            if end > HEADER_LIMIT {
                return Err("fixture header limit");
            }
            break end;
        }
        if bytes.len() > HEADER_LIMIT + 3 {
            return Err("fixture header limit");
        }
        read_chunk(stream, started, &mut bytes, &mut chunk)?;
    };
    let header = std::str::from_utf8(&bytes[..end]).map_err(|_| "fixture header encoding")?;
    let mut lines = header.split("\r\n");
    let mut first = lines
        .next()
        .ok_or("fixture request line")?
        .split_whitespace();
    let method = first.next().ok_or("fixture method")?.to_owned();
    let target = first.next().ok_or("fixture target")?.to_owned();
    if first.next() != Some("HTTP/1.1") || first.next().is_some() || !target.starts_with('/') {
        return Err("fixture request line");
    }
    let mut length = None;
    let mut authorized = false;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("fixture header")?;
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err("fixture duplicate length");
            }
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| "fixture content length")?;
            if parsed > BODY_LIMIT {
                return Err("fixture body limit");
            }
            length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("fixture unsupported transfer encoding");
        } else if name.eq_ignore_ascii_case("authorization") {
            authorized = value.trim() == format!("Bearer {TOKEN}");
        }
    }
    let total = end
        .checked_add(4)
        .and_then(|n| n.checked_add(length.unwrap_or(0)))
        .ok_or("fixture wire length")?;
    while bytes.len() < total {
        read_chunk(stream, started, &mut bytes, &mut chunk)?;
    }
    if bytes.len() != total {
        return Err("fixture unexpected pipelined request");
    }
    let body = std::str::from_utf8(&bytes[end + 4..total])
        .map_err(|_| "fixture body encoding")?
        .to_owned();
    Ok(Request {
        method,
        target,
        body,
        authorized,
    })
}

fn read_chunk(
    stream: &mut TcpStream,
    started: Instant,
    bytes: &mut Vec<u8>,
    chunk: &mut [u8; 2048],
) -> Result<(), &'static str> {
    let remaining = SOCKET_LIMIT
        .checked_sub(started.elapsed())
        .filter(|n| !n.is_zero())
        .ok_or("fixture request deadline")?;
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| "fixture read timeout")?;
    let size = stream.read(chunk).map_err(|_| "fixture read")?;
    if size == 0
        || bytes
            .len()
            .checked_add(size)
            .is_none_or(|n| n > HEADER_LIMIT + 4 + BODY_LIMIT)
    {
        return Err("fixture request wire limit");
    }
    bytes.extend_from_slice(&chunk[..size]);
    Ok(())
}

fn joined_room() -> Value {
    json!({"next_batch":"slot-fixture", "rooms":{"join":{ROOM:{
        "state":{"events":[
            {"type":"m.room.create","state_key":"","event_id":"$create","sender":USER,"origin_server_ts":1,
                "content":{"creator":USER,"room_version":"10"}},
            {"type":"m.room.member","state_key":USER,"event_id":"$joined","sender":USER,"origin_server_ts":1,
                "content":{"membership":"join"}}
        ]},
        "timeline":{"events":[],"limited":false}
    }}}})
}

async fn client(server: &mut Server) -> Client {
    super::install_crypto_provider();
    let setup = async {
        let client = Client::builder()
            .homeserver_url(format!("http://{}", server.address))
            .request_config(
                matrix_sdk::config::RequestConfig::new()
                    .timeout(SOCKET_LIMIT)
                    .retry_limit(0),
            )
            .build()
            .await
            .map_err(|_| "fixture client build")?;
        client.send_queue().set_enabled(false).await;
        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: user_id!("@alice:example.test").to_owned(),
                device_id: "SLOT-FIXTURE".into(),
            },
            tokens: matrix_sdk::SessionTokens {
                access_token: TOKEN.into(),
                refresh_token: None,
            },
        };
        client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await
            .map_err(|_| "fixture session restore")?;
        client
            .sync_once(SyncSettings::default())
            .await
            .map_err(|_| "fixture initial sync")?;
        Ok::<_, &'static str>(client)
    };
    match tokio::time::timeout(OPERATION_LIMIT, setup).await {
        Ok(Ok(client)) => client,
        _ => {
            let joined = server.finish();
            assert!(joined, "failed setup must join its original fixture worker");
            panic!("synthetic SDK session setup failed or exceeded its deadline");
        }
    }
}

fn open_content() -> Value {
    json!({"status":"open","application":{"type":"m.call"},"encryption":{"type":"m.per_member"}})
}

fn target_is(request: &Request, kind: &str, key: &str) -> bool {
    let Some(path) = request.target.strip_prefix("/_matrix/client/v3/rooms/") else {
        return false;
    };
    let Some((room, state)) = path.split_once("/state/") else {
        return false;
    };
    let mut decoded_room = Vec::new();
    let mut encoded = room.bytes();
    while let Some(byte) = encoded.next() {
        if byte == b'%' {
            let Some(high) = encoded.next().and_then(|n| (n as char).to_digit(16)) else {
                return false;
            };
            let Some(low) = encoded.next().and_then(|n| (n as char).to_digit(16)) else {
                return false;
            };
            decoded_room.push((high * 16 + low) as u8);
        } else {
            decoded_room.push(byte);
        }
    }
    decoded_room == ROOM.as_bytes() && state == format!("{kind}/{key}")
}

#[tokio::test]
async fn actual_sdk_slot_http_preserves_present_authority_and_refuses_unproven_absence() {
    let cases = [
        (200, open_content().to_string(), Some(STABLE)),
        (200, json!({"status":"closed"}).to_string(), Some(STABLE)),
        (200, "{}".to_owned(), Some(STABLE)),
        (200, "[]".to_owned(), Some(STABLE)),
        (
            200,
            json!({"status":"unknown","application":7}).to_string(),
            Some(STABLE),
        ),
        (
            404,
            json!({"errcode":"M_NOT_FOUND","error":"synthetic absent slot"}).to_string(),
            Some(UNSTABLE),
        ),
        (
            401,
            json!({"errcode":"M_UNKNOWN_TOKEN","error":"private reflected text"}).to_string(),
            None,
        ),
        (
            403,
            json!({"errcode":"M_FORBIDDEN","error":"private reflected text"}).to_string(),
            None,
        ),
        (
            404,
            json!({"errcode":"M_FORBIDDEN","error":"private reflected text"}).to_string(),
            None,
        ),
        (
            500,
            json!({"errcode":"M_NOT_FOUND","error":"private reflected text"}).to_string(),
            None,
        ),
        (200, "{".to_owned(), None),
        (
            200,
            json!({"padding":"x".repeat(BODY_LIMIT)}).to_string(),
            None,
        ),
        (0, String::new(), None),
    ];
    // Either input spelling must preserve the actual stable authority. This
    // also proves that requesting stable on confirmed absence selects unstable.
    for requested in [STABLE, UNSTABLE] {
        // The exact body is forwarded for both close and reopen requests.
        let content = if requested == STABLE {
            json!({"status":"closed"})
        } else {
            open_content()
        };
        for (status, body, selected) in &cases {
            let (mut server, _entered) = Server::start(*status, body.clone(), false);
            let client = client(&mut server).await;
            let backend = SdkMatrixBackend::new(client.clone());
            let result = tokio::time::timeout(
                OPERATION_LIMIT,
                backend.send_state_event(
                    ROOM.into(),
                    requested.into(),
                    KEY.into(),
                    content.clone(),
                ),
            )
            .await;
            drop(backend);
            drop(client);
            let joined = server.finish();
            let requests = server.observed();
            let gets: Vec<_> = requests.iter().filter(|r| r.method == "GET").collect();
            let puts: Vec<_> = requests.iter().filter(|r| r.method == "PUT").collect();
            assert!(joined, "fixture worker must be joined before assertions");
            assert!(
                requests.iter().all(|r| r.authorized),
                "only the fixed disposable SDK token is used"
            );
            assert_eq!(
                gets.len(),
                1,
                "one original stable authority read; no alternate read retry"
            );
            assert!(target_is(gets[0], STABLE, "m.call%23ROOM"));
            assert!(gets[0].body.is_empty());
            match selected {
                Some(kind) => {
                    assert!(
                        matches!(result, Ok(Ok(_))),
                        "present/confirmed-absent authority write must succeed"
                    );
                    assert_eq!(puts.len(), 1);
                    assert!(target_is(puts[0], kind, "m.call%23ROOM"));
                    assert!(
                        serde_json::from_str::<Value>(&puts[0].body)
                            .is_ok_and(|body| body == content)
                    );
                }
                None => {
                    assert!(
                        matches!(result, Ok(Err(error)) if error.to_string() == "failed to send event: RTC slot authority read failed"),
                        "authority read failures must be fixed and must not reflect server text"
                    );
                    assert!(puts.is_empty());
                }
            }
        }
    }
}

#[tokio::test]
async fn actual_sdk_entered_slot_read_cannot_authorize_a_write_after_original_admission_closes() {
    for status in [200, 404] {
        let body = if status == 200 {
            "{}".to_owned()
        } else {
            json!({"errcode":"M_NOT_FOUND","error":"synthetic absent slot"}).to_string()
        };
        let (mut server, entered) = Server::start(status, body, true);
        let client = client(&mut server).await;
        let original = Arc::new(AtomicBool::new(true));
        let captured = Arc::clone(&original);
        let backend = SdkMatrixBackend::new(client.clone())
            .with_slot_write_admission(Arc::new(move || captured.load(Ordering::SeqCst)));
        // Both branches are polled in this original future, with no spawned
        // task/JoinHandle that an outer cancellation or panic could detach.
        let outcome = tokio::time::timeout(OPERATION_LIMIT, async {
            tokio::join!(
                backend.send_state_event(ROOM.into(), UNSTABLE.into(), KEY.into(), open_content()),
                async {
                    let entered = entered.await;
                    original.store(false, Ordering::SeqCst);
                    server.release();
                    entered
                }
            )
        })
        .await;
        // Dropping a timed-out direct future cannot leave an app-owned task;
        // finish releases any still-held response and joins the server worker.
        drop(backend);
        drop(client);
        let joined = server.finish();
        let requests = server.observed();
        assert!(joined, "entered GET worker must retire before assertions");
        assert!(
            matches!(outcome, Ok((Err(error), Ok(()))) if error.to_string() == "failed to send event: RTC slot write admission closed"),
            "actual entered GET must finish under the closed original predicate"
        );
        assert!(requests.iter().all(|r| r.authorized));
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(target_is(&requests[0], STABLE, "m.call%23ROOM"));
    }
}

#[tokio::test]
async fn actual_sdk_non_slot_state_write_keeps_original_path_without_slot_probe() {
    let (mut server, _entered) = Server::start(403, "{}".to_owned(), false);
    let client = client(&mut server).await;
    let backend =
        SdkMatrixBackend::new(client.clone()).with_slot_write_admission(Arc::new(|| false));
    let content = json!({"application":"m.call","scope":"m.room"});
    let result = tokio::time::timeout(
        OPERATION_LIMIT,
        backend.send_state_event(
            ROOM.into(),
            "org.matrix.msc3401.call.member".into(),
            "fixture-member".into(),
            content.clone(),
        ),
    )
    .await;
    drop(backend);
    drop(client);
    let joined = server.finish();
    let requests = server.observed();
    assert!(joined);
    assert!(
        matches!(result, Ok(Ok(_))),
        "non-slot writes must preserve their original path"
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PUT");
    assert!(requests[0].authorized);
    assert!(target_is(
        &requests[0],
        "org.matrix.msc3401.call.member",
        "fixture-member"
    ));
    assert!(serde_json::from_str::<Value>(&requests[0].body).is_ok_and(|body| body == content));
}
