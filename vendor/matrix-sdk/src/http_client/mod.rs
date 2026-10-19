// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    any::type_name,
    borrow::Cow,
    fmt::Debug,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use bytesize::ByteSize;
use eyeball::SharedObservable;
use http::Method;
use matrix_sdk_base::SendOutsideWasm;
use ruma::api::{
    OutgoingRequest, OutgoingRequestExt, SupportedVersions,
    auth_scheme::{self, AuthScheme, SendAccessToken},
    error::{FromHttpResponseError, IntoHttpError},
    path_builder,
};
use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::{Instrument, debug, error, field::debug, trace};

use crate::{HttpResult, config::RequestConfig, error::HttpError};

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod wasm;

#[cfg(not(target_family = "wasm"))]
pub(crate) use native::HttpSettings;

pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
struct MaybeSemaphore(Arc<Option<Semaphore>>);

#[allow(dead_code)] // false-positive lint: we never use it but only hold it for the drop
struct MaybeSemaphorePermit<'a>(Option<SemaphorePermit<'a>>);

impl MaybeSemaphore {
    fn new(max: Option<NonZeroUsize>) -> Self {
        let inner = max.map(|i| Semaphore::new(i.into()));
        MaybeSemaphore(Arc::new(inner))
    }

    async fn acquire(&self) -> MaybeSemaphorePermit<'_> {
        match self.0.as_ref() {
            Some(inner) => {
                // This can only ever error if the semaphore was closed, which
                // we never do, so we can safely ignore any error case
                MaybeSemaphorePermit(inner.acquire().await.ok())
            }
            None => MaybeSemaphorePermit(None),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: reqwest::Client,
    pub(crate) request_config: RequestConfig,
    concurrent_request_semaphore: MaybeSemaphore,
    next_request_id: Arc<AtomicU64>,
}

impl HttpClient {
    pub(crate) fn new(inner: reqwest::Client, request_config: RequestConfig) -> Self {
        HttpClient {
            inner,
            request_config,
            concurrent_request_semaphore: MaybeSemaphore::new(
                request_config.max_concurrent_requests,
            ),
            next_request_id: AtomicU64::new(0).into(),
        }
    }

    fn get_request_id(&self) -> String {
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        format!("REQ-{request_id}")
    }

    fn serialize_request<R>(
        &self,
        request: R,
        config: RequestConfig,
        homeserver: String,
        access_token: Option<&str>,
        path_builder_input: <R::PathBuilder as path_builder::PathBuilder>::Input<'_>,
    ) -> Result<http::Request<Bytes>, IntoHttpError>
    where
        R: OutgoingRequest + Debug,
        R::Authentication: SupportedAuthScheme,
    {
        trace!(request_type = type_name::<R>(), "Serializing request");

        let send_access_token = match access_token {
            Some(access_token) => match (config.force_auth, config.skip_auth) {
                (true, true) | (true, false) => SendAccessToken::Always(access_token),
                (false, true) => SendAccessToken::None,
                (false, false) => SendAccessToken::IfRequired(access_token),
            },
            None => SendAccessToken::None,
        };
        let authentication_input = R::Authentication::authentication_input(send_access_token);

        let request = request
            .try_into_http_request::<BytesMut>(
                &homeserver,
                authentication_input,
                path_builder_input,
            )?
            .map(|body| body.freeze());

        Ok(request)
    }

    pub fn send<R>(
        &self,
        request: R,
        config: Option<RequestConfig>,
        homeserver: String,
        access_token: Option<&str>,
        path_builder_input: <R::PathBuilder as path_builder::PathBuilder>::Input<'_>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> impl Future<Output = Result<R::IncomingResponse, HttpError>>
    where
        R: OutgoingRequest + Debug,
        R::Authentication: SupportedAuthScheme,
        HttpError: From<FromHttpResponseError<R::EndpointError>>,
    {
        // some functions split out so they only get compiled once, not
        // monomorphized per request type
        fn make_span(client: &HttpClient, config: &RequestConfig) -> tracing::Span {
            tracing::info_span!(
                "send",
                uri = tracing::field::Empty,
                ?config,
                method = tracing::field::Empty,
                request_id = client.get_request_id(),
                request_size = tracing::field::Empty,
                request_duration = tracing::field::Empty,
                status = tracing::field::Empty,
                response_size = tracing::field::Empty,
                sentry_event_id = tracing::field::Empty
            )
        }
        fn record_request_uri_and_size(request: &http::Request<Bytes>) {
            let method = request.method();

            let mut uri_parts = request.uri().clone().into_parts();

            // Erase the query parameters for the sake of secrecy (in case a
            // token is present).
            if let Some(path_and_query) = &mut uri_parts.path_and_query {
                *path_and_query =
                    path_and_query.path().try_into().expect("path is valid PathAndQuery");
            }

            let uri = http::Uri::from_parts(uri_parts).expect("created from valid URI");

            let span = tracing::Span::current();
            span.record("method", debug(method)).record("uri", uri.to_string());

            // POST, PUT, PATCH are the only methods that are reasonably used in
            // conjunction with request bodies
            if [Method::POST, Method::PUT, Method::PATCH].contains(method) {
                let request_size = request.body().len().try_into().unwrap_or(u64::MAX);
                span.record(
                    "request_size",
                    ByteSize(request_size).display().si_short().to_string(),
                );
            }
        }
        // these macros expand to a lot of code, also want to skip
        // monomorphization for them even though they might look super simple
        fn log_got_response() {
            debug!("Got response");
        }
        fn log_error(e: &HttpError) {
            error!("Error while sending request: {e:?}");
        }

        let config = match config {
            Some(config) => config,
            None => self.request_config,
        };

        async move {
            let request = self
                .serialize_request(request, config, homeserver, access_token, path_builder_input)
                .map_err(HttpError::IntoHttp)?;
            record_request_uri_and_size(&request);

            // will be automatically dropped at the end of this function
            let _handle = self.concurrent_request_semaphore.acquire().await;

            // There's a bunch of state in send_request, factor out a pinned
            // inner future to reduce the size of futures that await this
            // function.
            match Box::pin(self.send_request::<R>(request, config, send_progress)).await {
                Ok(response) => {
                    log_got_response();
                    Ok(response)
                }
                Err(e) => {
                    log_error(&e);
                    Err(e)
                }
            }
        }
        .instrument(make_span(self, &config))
    }
}

/// Progress of sending or receiving a payload.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransmissionProgress {
    /// How many bytes were already transferred.
    pub current: usize,
    /// How many bytes there are in total.
    pub total: usize,
}

/// Read a response into an [`http::Response`]. With `max_response_bytes`,
/// the body is read under that limit (see [`BoundedBody`]); without one it is
/// read whole, as upstream does.
async fn response_to_http_response(
    mut response: reqwest::Response,
    max_response_bytes: Option<u64>,
) -> Result<http::Response<Bytes>, HttpError> {
    let status = response.status();
    // Read before the headers move out: on wasm it comes from them.
    let declared = response.content_length();

    let mut http_builder = http::Response::builder().status(status);
    let headers = http_builder.headers_mut().expect("Can't get the response builder headers");

    for (k, v) in response.headers_mut().drain() {
        if let Some(key) = k {
            headers.insert(key, v);
        }
    }

    let body = match max_response_bytes {
        None => response.bytes().await?,
        Some(limit) => read_bounded_body(response, limit, declared).await?,
    };

    Ok(http_builder.body(body).expect("Can't construct a response using the given body"))
}

// Morpheus patch: bounded response bodies.

/// The first allocation for a body under a limit: a declared length is never
/// trusted for more than this before bytes arrive.
const BOUNDED_BODY_INITIAL_CAPACITY: usize = 64 * 1024;

/// A response body read under a per-request byte limit.
///
/// A declared length over the limit is refused before reading. Each chunk is
/// then counted with checked arithmetic before the buffer grows, and the
/// requested buffer capacity never exceeds the limit. Transport/decoder
/// buffers and the current chunk exist separately; allocator rounding may also
/// add overhead. The accumulated body length is bounded, not total memory.
/// Counting happens after content decoding, including chunked responses.
#[derive(Debug)]
struct BoundedBody {
    limit: u64,
    buffer: Vec<u8>,
}

impl BoundedBody {
    fn new(limit: u64, declared: Option<u64>) -> Result<Self, HttpError> {
        if declared.is_some_and(|declared| declared > limit) {
            return Err(HttpError::ResponseTooLarge { limit });
        }
        let initial = usize::try_from(declared.unwrap_or(0).min(limit))
            .unwrap_or(usize::MAX)
            .min(BOUNDED_BODY_INITIAL_CAPACITY);
        Ok(Self { limit, buffer: Vec::with_capacity(initial) })
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), HttpError> {
        let limit = self.limit;
        let too_large = move || HttpError::ResponseTooLarge { limit };
        let total = next_body_length(self.buffer.len(), chunk.len(), limit)
            .and_then(|total| usize::try_from(total).ok())
            .ok_or_else(too_large)?;
        if total > self.buffer.capacity() {
            // Grow geometrically for many small chunks, never past the limit.
            let most = usize::try_from(limit).unwrap_or(usize::MAX);
            let target = total.max(self.buffer.capacity().saturating_mul(2)).min(most);
            self.buffer.try_reserve_exact(target - self.buffer.len()).map_err(|_| too_large())?;
        }
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> Bytes {
        Bytes::from(self.buffer)
    }
}

/// The body length after one more chunk, or `None` when it would exceed
/// `limit` or overflow.
fn next_body_length(current: usize, chunk: usize, limit: u64) -> Option<u64> {
    checked_body_length(u64::try_from(current).ok()?, u64::try_from(chunk).ok()?, limit)
}

fn checked_body_length(current: u64, chunk: u64, limit: u64) -> Option<u64> {
    let total = current.checked_add(chunk)?;
    (total <= limit).then_some(total)
}

/// Read the decoded body chunk by chunk under `limit`. Returning an error
/// drops the response, which stops the transfer.
#[cfg(not(target_family = "wasm"))]
async fn read_bounded_body(
    mut response: reqwest::Response,
    limit: u64,
    declared: Option<u64>,
) -> Result<Bytes, HttpError> {
    let mut body = BoundedBody::new(limit, declared)?;
    while let Some(chunk) = response.chunk().await? {
        body.push(&chunk)?;
    }
    Ok(body.finish())
}

/// The pinned reqwest has no chunked reader on wasm without its `stream`
/// feature, which this build does not enable. A declared length over the
/// limit is still refused before reading, but a body without one is checked
/// only after the browser has read it: the same error and no-retry behavior,
/// not a memory bound.
#[cfg(target_family = "wasm")]
async fn read_bounded_body(
    response: reqwest::Response,
    limit: u64,
    declared: Option<u64>,
) -> Result<Bytes, HttpError> {
    let mut body = BoundedBody::new(limit, declared)?;
    body.push(&response.bytes().await?)?;
    Ok(body.finish())
}

/// Marker trait to identify the authentication schemes that the
/// [`Client`](crate::Client) supports.
///
/// This trait can also be implemented for custom [`AuthScheme`]s if necessary.
pub trait SupportedAuthScheme: AuthScheme {
    /// Get the [`AuthScheme::Input`] from the access token.
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_>;
}

impl SupportedAuthScheme for auth_scheme::NoAccessToken {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AccessToken {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AccessTokenOptional {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AppserviceToken {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AppserviceTokenOptional {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::NoAuthentication {
    fn authentication_input(_access_token: SendAccessToken<'_>) -> Self::Input<'_> {}
}

/// Marker trait to identify the path builders that the
/// [`Client`](crate::Client) supports.
///
/// This trait can also be implemented for custom
/// [`PathBuilder`](path_builder::PathBuilder)s if necessary.
pub trait SupportedPathBuilder: path_builder::PathBuilder {
    /// Get the [`PathBuilder::Input`](path_builder::PathBuilder::Input) from
    /// the [`Client`](crate::Client).
    fn get_path_builder_input(
        client: &crate::Client,
        skip_auth: bool,
    ) -> impl Future<Output = HttpResult<Self::Input<'static>>> + SendOutsideWasm;
}

impl SupportedPathBuilder for path_builder::VersionHistory {
    async fn get_path_builder_input(
        client: &crate::Client,
        skip_auth: bool,
    ) -> HttpResult<Cow<'static, SupportedVersions>> {
        // We always enable "failsafe" mode for the GET /versions requests in
        // this function. It disables trying to refresh the access token for
        // those requests, to avoid possible deadlocks.

        if !client.auth_ctx().has_valid_access_token() {
            // Try to get the value in the cache.
            if let Ok(Some(versions)) = client.supported_versions_cached().await {
                return Ok(Cow::Owned(versions));
            }

            // The request will skip auth so we might not get all the supported
            // features, so just fetch the supported versions and don't cache
            // them.
            let response = client.fetch_server_versions_inner(true, None).await?;

            Ok(Cow::Owned(response.as_supported_versions()))
        } else if skip_auth {
            let cached_versions = client.supported_versions_cached().await;

            let versions = if let Ok(Some(versions)) = cached_versions {
                versions
            } else {
                // If we're skipping auth we might not get all the supported
                // features, so just fetch the versions and don't cache them.
                let request_config = RequestConfig::default().retry_limit(5).skip_auth();
                let response =
                    client.fetch_server_versions_inner(true, Some(request_config)).await?;

                response.as_supported_versions()
            };

            Ok(Cow::Owned(versions))
        } else {
            client.supported_versions_inner(true).await.map(Cow::Owned)
        }
    }
}

impl SupportedPathBuilder for path_builder::SinglePath {
    async fn get_path_builder_input(_client: &crate::Client, _skip_auth: bool) -> HttpResult<()> {
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
        },
        time::Duration,
    };

    use assert_matches::assert_matches;
    use bytes::Bytes;
    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::{async_test, test_json};
    use ruma::api::client::account::whoami;
    use wiremock::{
        Mock, Request, ResponseTemplate,
        matchers::{method, path},
    };

    use super::{BoundedBody, checked_body_length, next_body_length};
    use crate::{
        HttpError,
        http_client::RequestConfig,
        test_utils::{set_client_session, test_client_builder_with_server},
    };

    /// A valid `whoami` response body padded with whitespace to `len` bytes.
    fn padded_whoami(len: usize) -> Vec<u8> {
        let mut body = br#"{"user_id":"@example:localhost"}"#.to_vec();
        assert!(len >= body.len());
        body.resize(len, b' ');
        body
    }

    #[test]
    fn test_bounded_body_accepts_up_to_the_limit() {
        // Under and exactly at the bound, with or without a declared length.
        let mut body = BoundedBody::new(8, Some(8)).unwrap();
        body.push(b"abc").unwrap();
        body.push(b"defgh").unwrap();
        assert_eq!(body.finish(), Bytes::from_static(b"abcdefgh"));
        let mut body = BoundedBody::new(8, None).unwrap();
        body.push(b"abcdefg").unwrap();
        assert_eq!(body.finish().len(), 7);
    }

    #[test]
    fn test_bounded_body_refuses_more_than_the_limit() {
        // A declared length over the bound is refused before anything is read.
        assert_matches!(
            BoundedBody::new(8, Some(9)),
            Err(HttpError::ResponseTooLarge { limit: 8 })
        );
        // An unknown or understated length (a chunked body, or one that grows
        // when decoded) is counted as it arrives: the chunk crossing the bound
        // is refused before it is stored, and nothing past the bound was ever
        // reserved.
        let mut body = BoundedBody::new(8, Some(2)).unwrap();
        body.push(b"abcdef").unwrap();
        assert_matches!(body.push(b"ghi"), Err(HttpError::ResponseTooLarge { limit: 8 }));
        assert_eq!(body.buffer.len(), 6);
        assert!(body.buffer.capacity() <= 8);
        let mut body = BoundedBody::new(0, None).unwrap();
        body.push(b"").unwrap();
        assert_matches!(body.push(b"x"), Err(HttpError::ResponseTooLarge { limit: 0 }));
    }

    #[test]
    fn test_bounded_body_length_is_checked_without_extra_allocation() {
        assert_eq!(next_body_length(3, 5, 8), Some(8));
        assert_eq!(next_body_length(3, 6, 8), None);
        assert_eq!(checked_body_length(u64::MAX, 1, u64::MAX), None);
        assert_eq!(checked_body_length(u64::MAX - 1, 1, u64::MAX), Some(u64::MAX));
        // A chunk larger than the bound is refused without reserving for it.
        let mut body = BoundedBody::new(16, None).unwrap();
        let reserved = body.buffer.capacity();
        assert_matches!(body.push(&[0; 17]), Err(HttpError::ResponseTooLarge { limit: 16 }));
        assert_eq!(body.buffer.capacity(), reserved);
        assert!(body.buffer.is_empty());
        // A huge declared length that fits the bound is not trusted for more
        // than the initial allocation.
        let body = BoundedBody::new(u64::MAX, Some(u64::MAX)).unwrap();
        assert!(body.buffer.capacity() <= super::BOUNDED_BODY_INITIAL_CAPACITY);
    }

    #[test]
    fn test_oversized_responses_are_never_retried() {
        assert!(matches!(
            HttpError::ResponseTooLarge { limit: 1 }.retry_kind(),
            crate::error::RetryKind::Permanent
        ));
        // The message names only the limit: no body, URL or token.
        assert_eq!(
            HttpError::ResponseTooLarge { limit: 8 }.to_string(),
            "the response exceeded the 8-byte limit set for this request"
        );
    }

    #[async_test]
    async fn test_response_limit_applies_per_request_and_is_terminal() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder.build().await.unwrap();
        set_client_session(&client).await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;
        // Under, at and without the bound succeed; over the bound fails once.
        // A retried request would exceed this count.
        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(padded_whoami(256), "application/json"),
            )
            .expect(4)
            .mount(&server)
            .await;

        let send = |limit: Option<u64>| {
            let config = RequestConfig::default().retry_limit(3).max_response_bytes(limit);
            client.send(whoami::v3::Request::new()).with_request_config(config)
        };
        send(Some(257)).await.unwrap();
        send(Some(256)).await.unwrap();
        // The default reads a body of any size.
        send(None).await.unwrap();
        assert_matches!(send(Some(255)).await, Err(HttpError::ResponseTooLarge { limit: 255 }));
        server.verify().await;
    }

    #[cfg(not(target_family = "wasm"))]
    #[async_test]
    async fn test_response_limit_counts_chunked_transport_before_appending() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        // A real HTTP body without Content-Length exercises reqwest's native
        // chunk reader, rather than only feeding the accumulator directly.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() <= 4096);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n"
            ).await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}/synthetic-chunks"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.content_length(), None);
        assert_matches!(
            super::response_to_http_response(response, Some(7)).await,
            Err(HttpError::ResponseTooLarge { limit: 7 })
        );
        server.await.unwrap();
    }

    #[async_test]
    async fn test_response_limit_request_stops_when_dropped() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder.build().await.unwrap();
        set_client_session(&client).await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(padded_whoami(64), "application/json")
                    .set_delay(Duration::from_secs(60)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let config = RequestConfig::default().retry_limit(3).max_response_bytes(16_u64);
        let request = client.send(whoami::v3::Request::new()).with_request_config(config);
        // Dropping the request stops it; nothing continues or retries later.
        assert!(tokio::time::timeout(Duration::from_millis(200), request).await.is_err());
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.verify().await;
    }

    #[async_test]
    async fn test_ensure_concurrent_request_limit_is_observed() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder
            .request_config(RequestConfig::default().max_concurrent_requests(NonZeroUsize::new(5)))
            .build()
            .await
            .unwrap();

        set_client_session(&client).await;

        let counter = Arc::new(AtomicU8::new(0));
        let inner_counter = counter.clone();

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(move |_req: &Request| {
                inner_counter.fetch_add(1, Ordering::SeqCst);
                // we stall the requests
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
            })
            .mount(&server)
            .await;

        let bg_task = spawn(async move {
            futures_util::future::join_all((0..10).map(|_| client.whoami())).await
        });

        // give it some time to issue the requests
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            5,
            "More requests passed than the limit we configured"
        );
        bg_task.abort();
    }

    #[async_test]
    async fn test_ensure_no_max_concurrent_request_does_not_limit() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder
            .request_config(RequestConfig::default().max_concurrent_requests(None))
            .build()
            .await
            .unwrap();

        set_client_session(&client).await;

        let counter = Arc::new(AtomicU8::new(0));
        let inner_counter = counter.clone();

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(move |_req: &Request| {
                inner_counter.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
            })
            .mount(&server)
            .await;

        let bg_task = spawn(async move {
            futures_util::future::join_all((0..254).map(|_| client.whoami())).await
        });

        // give it some time to issue the requests
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 254, "Not all requests passed through");
        bg_task.abort();
    }
}
