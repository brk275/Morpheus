// Copyright 2026 Morpheus contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Bounded CSAPI authorization through this account's own homeserver only.
//! Peer SFU locators and server names are JSON data, never HTTP authorities.

use std::time::Duration;

use matrix_rtc_core::{BackendError, LiveKitTokenRequest};
use matrix_sdk::Client;
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_RESPONSE: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
const TOKEN_PATH: &str = "/_matrix/client/unstable/io.element.msc4195/rtc/livekit/get_token";

fn refused() -> BackendError {
    BackendError::new("RTC homeserver authorization failed")
}

fn expired() -> BackendError {
    BackendError::with_matrix_error(
        Some("M_UNKNOWN_TOKEN".into()),
        Some(401),
        "RTC authorization awaits SDK-managed session refresh",
    )
}

fn soft_logout() -> BackendError {
    BackendError::with_matrix_error(
        Some("RTC_AUTH_SOFT_LOGOUT".into()),
        Some(401),
        "RTC authorization requires sign in",
    )
}

fn sign_in() -> BackendError {
    BackendError::with_matrix_error(
        Some("RTC_AUTH_SIGN_IN_REQUIRED".into()),
        Some(401),
        "sign in again before calling",
    )
}

#[derive(Clone, Copy)]
pub(super) enum RejectedAccess {
    Expired([u8; 32]),
    SignInRequired([u8; 32]),
    SoftLogout([u8; 32]),
}

impl RejectedAccess {
    fn fingerprint(self) -> [u8; 32] {
        match self {
            Self::Expired(hash) | Self::SignInRequired(hash) | Self::SoftLogout(hash) => hash,
        }
    }
    fn error(self) -> BackendError {
        match self {
            Self::Expired(_) => expired(),
            Self::SignInRequired(_) => sign_in(),
            Self::SoftLogout(_) => soft_logout(),
        }
    }
}

fn requires_sign_in(error: &matrix_sdk::RefreshTokenError) -> bool {
    use matrix_sdk::RefreshTokenError;
    use matrix_sdk::authentication::oauth::error::{
        BasicErrorResponseType, OAuthError, RequestTokenError,
    };
    use matrix_sdk::ruma::api::error::ErrorKind;
    match error {
        RefreshTokenError::RefreshTokenRequired => true,
        RefreshTokenError::MatrixAuth(error) => matches!(
            error.client_api_error_kind(),
            Some(ErrorKind::UnknownToken(_) | ErrorKind::Forbidden)
        ),
        RefreshTokenError::OAuth(error) => match error.as_ref() {
            OAuthError::NotAuthenticated
            | OAuthError::NotRegistered
            | OAuthError::SessionMismatch => true,
            OAuthError::RefreshToken(RequestTokenError::ServerResponse(error)) => matches!(
                error.error(),
                BasicErrorResponseType::InvalidGrant
                    | BasicErrorResponseType::InvalidClient
                    | BasicErrorResponseType::UnauthorizedClient
            ),
            _ => false,
        },
    }
}

fn fingerprint(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn authority(mut homeserver: Url) -> Result<Url, BackendError> {
    if !homeserver.has_host()
        || !homeserver.username().is_empty()
        || homeserver.password().is_some()
        || homeserver.query().is_some()
        || homeserver.fragment().is_some()
        || (homeserver.scheme() != "https"
            && !(homeserver.scheme() == "http" && loopback(&homeserver)))
    {
        return Err(refused());
    }
    // Preserve legitimate delegated homeserver base paths. Never join against
    // a peer-controlled absolute URL or replace the configured authority.
    let path = format!("{}{}", homeserver.path().trim_end_matches('/'), TOKEN_PATH);
    homeserver.set_path(&path);
    Ok(homeserver)
}

fn same_account(client: &Client, request: &LiveKitTokenRequest) -> bool {
    client
        .user_id()
        .is_some_and(|id| id.as_str() == request.own_user_id)
        && client
            .device_id()
            .is_some_and(|id| id.as_str() == request.own_device_id)
}

enum Answer {
    Token(String),
    Expired,
    SoftLogout,
}

async fn post_once(
    http: &reqwest::Client,
    endpoint: Url,
    token: &str,
    body: &Value,
) -> Result<Answer, BackendError> {
    let mut response = http
        .post(endpoint)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|_| refused())?;
    let status = response.status();
    // Redirects and errors are not parsed into SDK errors that can retain or
    // log reflected credentials, response bodies, or Location headers.
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE as u64)
    {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| refused())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
            return Err(refused());
        }
        bytes.extend_from_slice(&chunk);
    }
    if status.as_u16() == 401 {
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| refused())?;
        if value.get("errcode").and_then(Value::as_str) == Some("M_UNKNOWN_TOKEN") {
            let soft_logout = match value.get("soft_logout") {
                None => false,
                Some(value) => value.as_bool().ok_or_else(refused)?,
            };
            return Ok(if soft_logout {
                Answer::SoftLogout
            } else {
                Answer::Expired
            });
        }
    }
    if !status.is_success() {
        return Err(refused());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| refused())?;
    let jwt = value
        .get("jwt")
        .and_then(Value::as_str)
        .filter(|jwt| !jwt.is_empty())
        .ok_or_else(refused)?;
    Ok(Answer::Token(jwt.to_owned()))
}

pub(super) async fn get(
    client: &Client,
    request: LiveKitTokenRequest,
    expired_access: &std::sync::Mutex<Option<RejectedAccess>>,
    gate: &tokio::sync::Mutex<()>,
) -> Result<String, BackendError> {
    // Validate the fixed configured authority BEFORE reading account tokens.
    let endpoint = authority(client.homeserver())?;
    if !same_account(client, &request)
        || [
            &request.url,
            &request.server_name,
            &request.room_id,
            &request.slot_id,
            &request.member_id,
        ]
        .iter()
        .any(|field| field.is_empty() || field.len() > 1024 || field.chars().any(char::is_control))
    {
        return Err(refused());
    }
    // The transport boundary also validates these before calling us. Keep the
    // backend safe when independently called through its public trait.
    let locator = Url::parse(&request.url).map_err(|_| refused())?;
    if !locator.has_host()
        || !locator.username().is_empty()
        || locator.password().is_some()
        || locator.query().is_some()
        || locator.fragment().is_some()
        || (locator.scheme() != "wss" && !(locator.scheme() == "ws" && loopback(&locator)))
    {
        return Err(refused());
    }
    matrix_sdk::ruma::ServerName::parse(&request.server_name).map_err(|_| refused())?;
    let body = json!({"url":&request.url,"server_name":&request.server_name,
        "room_id":&request.room_id,"slot_id":&request.slot_id,"member_id":&request.member_id});
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DEADLINE);
    if loopback(&endpoint) {
        builder = builder.no_proxy();
    }
    let http = builder.build().map_err(|_| refused())?;
    let exchange = async {
        // One account-backend authorization at a time, across its clones.
        // Waiting for this gate is included in the overall deadline.
        let _authorization = gate.lock().await;
        if !same_account(client, &request) || authority(client.homeserver())? != endpoint {
            return Err(refused());
        }
        let first = client
            .access_token()
            .filter(|token| !token.is_empty())
            .ok_or_else(sign_in)?;
        let first_fingerprint = fingerprint(&first);
        {
            let mut refused = expired_access
                .lock()
                .expect("RTC auth state mutex poisoned");
            if let Some(marker) = *refused
                && marker.fingerprint() == first_fingerprint
            {
                return Err(marker.error());
            }
            *refused = None;
        }
        match post_once(&http, endpoint.clone(), &first, &body).await? {
            Answer::Token(jwt) => {
                if !same_account(client, &request)
                    || authority(client.homeserver())? != endpoint
                    || client.access_token().is_none()
                {
                    return Err(refused());
                }
                Ok(jwt)
            }
            Answer::SoftLogout => {
                *expired_access
                    .lock()
                    .expect("RTC auth state mutex poisoned") =
                    Some(RejectedAccess::SoftLogout(first_fingerprint));
                Err(soft_logout())
            }
            Answer::Expired => {
                *expired_access
                    .lock()
                    .expect("RTC auth state mutex poisoned") =
                    Some(RejectedAccess::Expired(first_fingerprint));
                if !same_account(client, &request) || authority(client.homeserver())? != endpoint {
                    return Err(refused());
                }
                // The SDK owns auth-mode selection, refresh serialization,
                // token rotation, persistence callbacks and session notices.
                // No refresh credential is read or persisted by this module.
                if let Err(error) = client.refresh_access_token().await {
                    if requires_sign_in(&error) {
                        *expired_access
                            .lock()
                            .expect("RTC auth state mutex poisoned") =
                            Some(RejectedAccess::SignInRequired(first_fingerprint));
                        return Err(sign_in());
                    }
                    return Err(expired());
                }
                if !same_account(client, &request) || authority(client.homeserver())? != endpoint {
                    return Err(refused());
                }
                let fresh = client
                    .access_token()
                    .filter(|token| !token.is_empty() && token != &first)
                    .ok_or_else(expired)?;
                // Exactly one retry, using the new snapshot only. A second
                // rejection never refreshes or replays the expired token.
                match post_once(&http, endpoint.clone(), &fresh, &body).await? {
                    Answer::Token(jwt)
                        if same_account(client, &request)
                            && authority(client.homeserver())? == endpoint
                            && client.access_token().is_some() =>
                    {
                        Ok(jwt)
                    }
                    Answer::Token(_) => Err(refused()),
                    Answer::SoftLogout => {
                        *expired_access
                            .lock()
                            .expect("RTC auth state mutex poisoned") =
                            Some(RejectedAccess::SoftLogout(fingerprint(&fresh)));
                        Err(soft_logout())
                    }
                    Answer::Expired => {
                        *expired_access
                            .lock()
                            .expect("RTC auth state mutex poisoned") =
                            Some(RejectedAccess::Expired(fingerprint(&fresh)));
                        Err(expired())
                    }
                }
            }
        }
    };
    // Includes refresh and both POSTs. Dropping a call's future drops our HTTP
    // operations; the SDK retains its own intentional refresh completion rules.
    tokio::time::timeout(DEADLINE, exchange)
        .await
        .map_err(|_| expired())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn get(client: &Client, request: LiveKitTokenRequest) -> Result<String, BackendError> {
        super::get(
            client,
            request,
            &std::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(()),
        )
        .await
    }

    fn crypto() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    async fn request_bytes(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0 && bytes.len() + n <= 16 * 1024);
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:").map(str::trim))
                    .unwrap_or("0")
                    .parse()
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    return bytes;
                }
            }
        }
    }

    async fn client(listener: &TcpListener, refresh: bool) -> Client {
        crypto();
        let origin = listener.local_addr().unwrap();
        let endpoint = format!("http://{origin}");
        let client = Client::builder()
            .homeserver_url(&endpoint)
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .request_config(
                matrix_sdk::config::RequestConfig::new()
                    .disable_retry()
                    .timeout(Duration::from_secs(2)),
            )
            .build()
            .await
            .unwrap();
        client
            .restore_session(matrix_sdk::authentication::matrix::MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: matrix_sdk::ruma::owned_user_id!("@alice:own.example.org"),
                    device_id: matrix_sdk::ruma::owned_device_id!("ALICE"),
                },
                tokens: matrix_sdk::SessionTokens {
                    access_token: "synthetic-old-token".into(),
                    refresh_token: refresh.then(|| "synthetic-refresh-token".into()),
                },
            })
            .await
            .unwrap();
        // The pinned SDK's restore_session spawns E2EE recovery setup, which
        // reads this account-data event even with automatic backups disabled.
        // Its independent backup-state watcher also emits the initial Unknown
        // state and makes the same request. Complete exactly these two known
        // requests; never ignore extra connections during the actual check.
        let bootstrap = async {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let bytes = request_bytes(&mut stream).await;
                let request = std::str::from_utf8(&bytes).expect("Invalid SDK bootstrap HTTP");
                let (headers, body) = request.split_once("\r\n\r\n").unwrap();
                let headers = headers.to_ascii_lowercase();
                let expected_request = concat!(
                    "get /_matrix/client/v3/user/@alice:own.example.org/",
                    "account_data/m.secret_storage.default_key http/1.1"
                );
                assert!(headers.lines().next() == Some(expected_request));
                assert!(
                    headers
                        .lines()
                        .filter(|line| line.starts_with("authorization:"))
                        .eq(["authorization: bearer synthetic-old-token"])
                );
                let expected_host = format!("host: {origin}");
                assert!(
                    headers
                        .lines()
                        .filter(|line| line.starts_with("host:"))
                        .eq([expected_host.as_str()])
                );
                assert!(body.is_empty() && !headers.contains("transfer-encoding:"));
                answer(
                    &mut stream,
                    404,
                    r#"{"errcode":"M_NOT_FOUND","error":"No synthetic account data"}"#,
                )
                .await;
            }
        };
        let initialized = async {
            client
                .encryption()
                .wait_for_e2ee_initialization_tasks()
                .await
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(bootstrap, initialized)
        })
        .await
        .expect("SDK bootstrap did not finish within its existing request deadline");
        assert!(client.access_token().as_deref() == Some("synthetic-old-token"));
        client
    }

    fn token_request() -> LiveKitTokenRequest {
        LiveKitTokenRequest {
            url: "wss://media.foreign.example.org".into(),
            server_name: "foreign.example.org".into(),
            room_id: "!room:own.example.org".into(),
            slot_id: "m.call#ROOM".into(),
            member_id: "participation".into(),
            own_user_id: "@alice:own.example.org".into(),
            own_device_id: "ALICE".into(),
        }
    }

    async fn answer(stream: &mut tokio::net::TcpStream, status: u16, body: &str) {
        stream.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    }

    #[test]
    fn authority_preserves_delegated_base_path_and_rejects_credential_destinations() {
        let endpoint =
            authority(Url::parse("https://delegated.example.org/matrix/base/").unwrap()).unwrap();
        assert_eq!(
            endpoint.as_str(),
            format!("https://delegated.example.org/matrix/base{TOKEN_PATH}")
        );
        for url in [
            "http://foreign.example.org",
            "https://user:secret@example.org",
            "https://example.org?token=secret",
            "https://example.org#secret",
            "file:///tmp/token",
        ] {
            assert!(authority(Url::parse(url).unwrap()).is_err());
        }
        assert!(authority(Url::parse("http://127.0.0.1:1234/base").unwrap()).is_ok());
        assert!(authority(Url::parse("http://[::1]:1234/base").unwrap()).is_ok());
    }

    #[tokio::test]
    async fn actual_account_client_refreshes_once_and_only_retries_with_the_fresh_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(&listener, true).await;
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let bytes = request_bytes(&mut stream).await;
                let received = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                match index {
                    0 => {
                        assert!(received.starts_with(&format!("post {TOKEN_PATH} ")));
                        assert!(received.contains("authorization: bearer synthetic-old-token"));
                        assert!(received.contains("\"server_name\":\"foreign.example.org\""));
                        answer(
                            &mut stream,
                            401,
                            r#"{"errcode":"M_UNKNOWN_TOKEN","soft_logout":false}"#,
                        )
                        .await;
                    }
                    1 => {
                        assert!(received.starts_with("post /_matrix/client/v3/refresh "));
                        answer(&mut stream, 200, r#"{"access_token":"synthetic-fresh-token","refresh_token":"synthetic-fresh-refresh"}"#).await;
                    }
                    _ => {
                        assert!(received.starts_with(&format!("post {TOKEN_PATH} ")));
                        assert!(received.contains("authorization: bearer synthetic-fresh-token"));
                        assert!(!received.contains("synthetic-old-token"));
                        answer(&mut stream, 200, r#"{"jwt":"synthetic-sfu-jwt"}"#).await;
                    }
                }
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        assert_eq!(
            get(&client, token_request()).await.unwrap(),
            "synthetic-sfu-jwt"
        );
        assert_eq!(
            client.access_token().as_deref(),
            Some("synthetic-fresh-token")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn soft_logout_and_malformed_soft_logout_never_request_a_refresh() {
        for response in [
            r#"{"errcode":"M_UNKNOWN_TOKEN","soft_logout":true}"#,
            r#"{"errcode":"M_UNKNOWN_TOKEN","soft_logout":"false"}"#,
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = client(&listener, true).await;
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                request_bytes(&mut stream).await;
                answer(&mut stream, 401, response).await;
                drop(stream);
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err()
                );
            });
            assert!(get(&client, token_request()).await.is_err());
            assert_eq!(
                client.access_token().as_deref(),
                Some("synthetic-old-token")
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn mismatched_account_is_refused_before_any_authorized_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(&listener, true).await;
        let mut request = token_request();
        request.own_device_id = "OTHER-ACCOUNT-DEVICE".into();
        assert!(get(&client, request).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_rejected_snapshot_is_not_replayed_by_another_backend_clone() {
        use matrix_rtc_core::MatrixBackend;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(&listener, false).await;
        let backend = crate::SdkMatrixBackend::new(client);
        let other = backend.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request_bytes(&mut stream).await;
            answer(
                &mut stream,
                401,
                r#"{"errcode":"M_UNKNOWN_TOKEN","soft_logout":false}"#,
            )
            .await;
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        let (first, second) = tokio::join!(
            backend.livekit_token(token_request()),
            other.livekit_token(token_request())
        );
        let first = first.unwrap_err();
        assert_eq!(first.errcode.as_deref(), Some("RTC_AUTH_SIGN_IN_REQUIRED"));
        let second = second.unwrap_err();
        assert_eq!(second.errcode.as_deref(), Some("RTC_AUTH_SIGN_IN_REQUIRED"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_sdk_refresh_requires_sign_in_without_replaying_the_account_token() {
        use matrix_rtc_core::MatrixBackend;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(&listener, true).await;
        let backend = crate::SdkMatrixBackend::new(client);
        let other = backend.clone();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let bytes = request_bytes(&mut stream).await;
                let received = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                let expected = if index == 0 {
                    format!("post {TOKEN_PATH} ")
                } else {
                    "post /_matrix/client/v3/refresh ".to_owned()
                };
                assert!(received.starts_with(&expected));
                answer(
                    &mut stream,
                    401,
                    r#"{"errcode":"M_UNKNOWN_TOKEN","error":"expired","soft_logout":false}"#,
                )
                .await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        for client in [&backend, &other] {
            let error = client.livekit_token(token_request()).await.unwrap_err();
            assert_eq!(error.errcode.as_deref(), Some("RTC_AUTH_SIGN_IN_REQUIRED"));
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_authorization_does_not_return_a_late_token_or_start_a_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client(&listener, true).await;
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request_bytes(&mut stream).await;
            entered.send(()).unwrap();
            released.await.unwrap();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 28\r\nConnection: close\r\n\r\n{\"jwt\":\"synthetic-late-jwt\"}").await;
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        let task = tokio::spawn(async move { get(&client, token_request()).await });
        waiting.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn authenticated_post_cannot_replay_a_bearer_to_a_redirect_target() {
        crypto();
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let foreign = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            authority(Url::parse(&format!("http://{}", origin.local_addr().unwrap())).unwrap())
                .unwrap();
        let redirect = format!("http://{}/stolen", foreign.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            let received = request_bytes(&mut stream).await;
            let request = String::from_utf8_lossy(&received).to_ascii_lowercase();
            assert!(request.starts_with(&format!("post {TOKEN_PATH} ")));
            assert!(request.contains("authorization: bearer synthetic-account-token"));
            stream.write_all(format!("HTTP/1.1 307 Redirect\r\nLocation: {redirect}\r\nContent-Length: 16\r\nConnection: close\r\n\r\ncredential-echo!").as_bytes()).await.unwrap();
        });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap();
        let error = post_once(
            &http,
            endpoint,
            "synthetic-account-token",
            &json!({"url":"wss://foreign.example.org", "server_name":"foreign.example.org"}),
        )
        .await
        .err()
        .unwrap();
        assert!(!error.to_string().contains("credential"));
        assert!(!error.to_string().contains("synthetic-account-token"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), foreign.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_stream_is_bounded_before_json_and_reflected_errors_are_discarded() {
        crypto();
        for response in [
            format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}", "x".repeat(MAX_RESPONSE + 1)),
            "HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n{\"error\":\"synthetic-account-token\"}".to_owned(),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = authority(Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap()).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut received = [0u8; 8192];
                let _ = stream.read(&mut received).await.unwrap();
                let _ = stream.write_all(response.as_bytes()).await;
            });
            let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().build().unwrap();
            let error = post_once(&http, endpoint, "synthetic-account-token", &json!({})).await.err().unwrap();
            assert_eq!(error.message, "RTC homeserver authorization failed");
            assert!(!format!("{error:?}").contains("synthetic-account-token"));
            server.await.unwrap();
        }
    }
}
