// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.

//! LiveKit SFU authorisation service token exchange ([MSC4195]) — the IO half.
//!
//! The endpoint URLs, request bodies, and response decoding live in
//! [`matrix_rtc_transport::livekit::token`], where the web binding shares them; this
//! module only POSTs what that one builds. Obtaining the OpenID token itself is
//! a Client-Server API concern and belongs to the backend: the host
//! supplies one through
//! [`MatrixBackend`](matrix_rtc_core::MatrixBackend), so nothing here
//! is wired to a particular Matrix SDK.
//!
//! [MSC4195]: https://github.com/matrix-org/matrix-spec-proposals/pull/4195

use matrix_rtc_core::OpenIdToken;
pub use matrix_rtc_transport::livekit::token::{MemberClaims, SfuToken};
use matrix_rtc_transport::livekit::token::{get_token_request, legacy_token_request};

use crate::Error;

/// Exchange an OpenID token for a LiveKit SFU JWT via the authorisation
/// service's `/get_token` endpoint.
///
/// `livekit_service_url` is the `livekit_service_url` advertised by the
/// transport (e.g. `https://matrix-rtc.example.com/livekit/jwt`); the
/// `/get_token` path is appended to it.
pub async fn get_token(
    http: &reqwest::Client,
    livekit_service_url: &str,
    room_id: &str,
    slot_id: &str,
    member: &MemberClaims,
    openid_token: &OpenIdToken,
) -> Result<SfuToken, Error> {
    let service = livekit_api::connection_policy::authorization_url(livekit_service_url)
        .map_err(|_| Error::UnsafeEndpoint)?;
    let (endpoint, body) =
        get_token_request(service.as_str(), room_id, slot_id, member, openid_token);
    post_for_token(http, &endpoint, &body, &format!("{room_id}/{slot_id}")).await
}

/// Exchange an OpenID token for a LiveKit SFU JWT via the **pre-MSC4195**
/// `/sfu/get` endpoint.
///
/// For interoperating with Element Call builds older than MSC4354. There is
/// deliberately no fallback from `/get_token` to this on a 404 — see
/// [`matrix_rtc_transport::livekit::token::legacy_token_request`]. Temporary; see
/// [`matrix_rtc_core::compat`].
pub async fn get_legacy_token(
    http: &reqwest::Client,
    livekit_service_url: &str,
    room: &str,
    device_id: &str,
    openid_token: &OpenIdToken,
) -> Result<SfuToken, Error> {
    let service = livekit_api::connection_policy::authorization_url(livekit_service_url)
        .map_err(|_| Error::UnsafeEndpoint)?;
    let (endpoint, body) = legacy_token_request(service.as_str(), room, device_id, openid_token);
    post_for_token(http, &endpoint, &body, room).await
}

/// POST through a boundary-owned, redirect-free client. Do not log URLs,
/// tokens, response bodies, or the compatibility-only caller log tag.
async fn post_for_token(
    _http: &reqwest::Client,
    endpoint: &str,
    body: &serde_json::Value,
    _log_tag: &str,
) -> Result<SfuToken, Error> {
    let endpoint = livekit_api::connection_policy::authorization_url(endpoint)
        .map_err(|_| Error::UnsafeEndpoint)?;
    // A caller's reqwest Client can follow 307/308 redirects and replay the
    // OpenID body. Its policy cannot be inspected or overridden per request.
    // Keep the upstream parameter for source compatibility, but use a separate
    // bounded client whose TLS and redirect policy are owned by this boundary.
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30));
    if livekit_api::connection_policy::is_loopback(&endpoint) {
        builder = builder.no_proxy();
    }
    let http = builder.build().map_err(|_| Error::Network)?;
    let mut response = http
        .post(endpoint)
        .json(body)
        .send()
        .await
        .map_err(|_| Error::Network)?;
    if !response.status().is_success() {
        // Authorization services can echo request bodies. Never retain or log
        // their rejection body, Location header, or supplied URL.
        return Err(Error::Rejected(response.status().as_u16()));
    }
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(Error::InvalidResponse);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Network)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(Error::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut token: SfuToken = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)?;
    let url =
        livekit_api::connection_policy::sfu_url(&token.url).map_err(|_| Error::UnsafeEndpoint)?;
    if token.jwt.is_empty() {
        return Err(Error::InvalidResponse);
    }
    token.url = url.into();
    Ok(token)
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, timeout},
    };

    #[tokio::test]
    async fn token_post_never_replays_on_redirect_or_exposes_body() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        for status in [301, 302, 303, 307, 308] {
            let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin_url = format!("http://{}/get_token", origin.local_addr().unwrap());
            let target_url = format!("http://{}/stolen", target.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = origin.accept().await.unwrap();
                let mut request = [0; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status} Redirect\r\nLocation: {target_url}\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsecret-echo!"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            // Even a redirect-following caller client cannot weaken the policy.
            let error = post_for_token(
                &reqwest::Client::new(),
                &origin_url,
                &serde_json::json!({"openid_token": "secret-echo!"}),
                "test",
            )
            .await
            .err()
            .expect("redirect accepted");
            assert!(matches!(error, Error::Rejected(code) if code == status));
            assert!(!error.to_string().contains("secret-echo"));
            assert!(
                timeout(Duration::from_millis(30), target.accept())
                    .await
                    .is_err()
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn returned_sfu_must_be_secure_before_connect() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        for url in [
            "ws://remote.example.org",
            "wss://user:secret@remote.example.org",
            "wss://remote.example.org?token=secret",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/get_token", listener.local_addr().unwrap());
            let body = serde_json::json!({"jwt": "dummy-secret", "url": url}).to_string();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let error = post_for_token(
                &reqwest::Client::new(),
                &endpoint,
                &serde_json::json!({}),
                "test",
            )
            .await
            .err()
            .expect("unsafe SFU accepted");
            assert!(matches!(error, Error::UnsafeEndpoint));
            assert!(!error.to_string().contains("dummy-secret"));
            server.await.unwrap();
        }
    }
}
