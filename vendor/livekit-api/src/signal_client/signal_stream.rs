// Copyright 2025 LiveKit, Inc.
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

use bytes::Bytes;
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use livekit_protocol as proto;
use livekit_runtime::{JoinHandle, TcpStream};
use prost::Message as ProtoMessage;
use std::{env, io, time::Duration};

use tokio::sync::{mpsc, oneshot};

#[cfg(feature = "signal-client-tokio")]
use base64;

#[cfg(feature = "signal-client-tokio")]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream as TokioTcpStream,
};

#[cfg(feature = "signal-client-tokio")]
use tokio_tungstenite::{
    connect_async,
    tungstenite::client::IntoClientRequest,
    tungstenite::error::ProtocolError,
    tungstenite::http::{header::AUTHORIZATION, HeaderValue},
    tungstenite::{Error as WsError, Message},
    MaybeTlsStream, WebSocketStream,
};

#[cfg(feature = "__signal-client-async-compatible")]
use async_tungstenite::{
    async_std::connect_async,
    async_std::ClientStream as MaybeTlsStream,
    tungstenite::client::IntoClientRequest,
    tungstenite::error::ProtocolError,
    tungstenite::http::{header::AUTHORIZATION, HeaderValue},
    tungstenite::{Error as WsError, Message},
    WebSocketStream,
};

use super::{SignalError, SignalResult};

type WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug)]
enum InternalMessage {
    Signal {
        signal: proto::signal_request::Message,
        response_chn: oneshot::Sender<SignalResult<()>>,
    },
    Pong {
        ping_data: Bytes,
    },
    Close,
}

/// SignalStream hold the WebSocket connection
///
/// It is replaced by [SignalClient] at each reconnection.
#[derive(Debug)]
pub(super) struct SignalStream {
    internal_tx: mpsc::Sender<InternalMessage>,
    read_handle: JoinHandle<()>,
    write_handle: JoinHandle<()>,
}

#[cfg(feature = "signal-client-tokio")]
fn parse_proxy_url(raw: &str) -> Result<url::Url, WsError> {
    // Proxy URLs may carry HTTP basic-auth credentials. Never log the input.
    log::info!("using configured signaling proxy");
    let proxy = url::Url::parse(raw).map_err(|_| {
        WsError::Io(io::Error::new(io::ErrorKind::InvalidInput, "invalid proxy URL"))
    })?;
    // This CONNECT implementation opens plain TCP to the proxy. An HTTPS
    // proxy would require TLS *before* sending CONNECT or Proxy-Authorization.
    // Reject unsupported schemes before opening a socket; HTTPS_PROXY may
    // still contain a supported http:// proxy for a TLS-protected target.
    if proxy.scheme() != "http" || proxy.host_str().is_none() {
        return Err(WsError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported proxy URL: only HTTP CONNECT proxies are supported",
        )));
    }
    Ok(proxy)
}

#[cfg(feature = "signal-client-tokio")]
async fn connect_proxy(raw: &str) -> Result<(url::Url, TokioTcpStream), WsError> {
    let proxy = parse_proxy_url(raw)?;
    let host = proxy.host_str().expect("proxy host was validated");
    let port = proxy.port_or_known_default().unwrap_or(80);
    let stream = TokioTcpStream::connect(format!("{host}:{port}")).await.map_err(WsError::Io)?;
    Ok((proxy, stream))
}

#[cfg(feature = "signal-client-tokio")]
fn check_proxy_status(status_line: &str) -> io::Result<()> {
    let mut words = status_line.split_ascii_whitespace();
    if matches!(words.next(), Some("HTTP/1.0" | "HTTP/1.1")) && words.next() == Some("200") {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::ConnectionRefused, "proxy connection rejected"))
    }
}

impl SignalStream {
    /// Connect to livekit websocket.
    /// Return SignalError if the connections failed
    ///
    /// SignalStream will never try to reconnect if the connection has been
    /// closed.
    pub async fn connect(
        url: url::Url,
        token: &str,
        connect_timeout: Duration,
    ) -> SignalResult<(Self, mpsc::UnboundedReceiver<Box<proto::signal_response::Message>>)> {
        let connect_fut = Self::connect_inner(url, token);
        livekit_runtime::timeout(connect_timeout, connect_fut)
            .await
            .map_err(|_| SignalError::Timeout("signal connection timed out".into()))?
            .map_err(|error| match error {
                SignalError::WsError(WsError::Http(response)) => {
                    let status = response.status();
                    let sanitized = http::Response::builder().status(status).body(None).unwrap();
                    SignalError::WsError(WsError::Http(Box::new(sanitized)))
                }
                other => other,
            })
    }

    async fn connect_inner(
        url: url::Url,
        token: &str,
    ) -> SignalResult<(Self, mpsc::UnboundedReceiver<Box<proto::signal_response::Message>>)> {
        // SDK query parameters may include encoded join/SDP information.
        log::info!("connecting to LiveKit signaling endpoint");
        let mut request = url.clone().into_client_request()?;
        let auth_header = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| SignalError::TokenFormat)?;
        request.headers_mut().insert(AUTHORIZATION, auth_header);

        #[cfg(feature = "signal-client-tokio")]
        let ws_stream = {
            // Check for HTTP_PROXY or HTTPS_PROXY environment variables
            let proxy_env = if crate::connection_policy::is_loopback(&url) {
                // Development credentials stay on loopback even when a user
                // configured a general HTTP proxy.
                Err(env::VarError::NotPresent)
            } else if url.scheme() == "wss" {
                env::var("HTTPS_PROXY").or_else(|_| env::var("https_proxy"))
            } else {
                env::var("HTTP_PROXY").or_else(|_| env::var("http_proxy"))
            };

            // Connect directly or through proxy
            let ws_stream = if let Ok(proxy_url) = proxy_env {
                if !proxy_url.is_empty() {
                    let host = url.host_str().ok_or_else(|| {
                        WsError::Io(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Target URL has no host",
                        ))
                    })?;

                    let port = url.port_or_known_default().ok_or_else(|| {
                        WsError::Io(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Target URL has no port and no default for scheme",
                        ))
                    })?;

                    let (proxy_url, mut proxy_stream) = connect_proxy(&proxy_url).await?;

                    let mut proxy_auth_header = None;
                    if let Some(password) = proxy_url.password() {
                        let auth = format!("{}:{}", proxy_url.username(), password);
                        let auth = format!("Basic {}", base64::encode(auth));
                        proxy_auth_header = Some(auth);
                    }

                    // Send CONNECT request
                    let target = format!("{}:{}", host, port);
                    let mut connect_req =
                        format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n", target, target);

                    // Add proxy authorization if needed
                    if let Some(auth) = proxy_auth_header {
                        connect_req.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
                    }

                    // Finalize request
                    connect_req.push_str("\r\n");

                    log::debug!("Sending CONNECT request to proxy");
                    proxy_stream.write_all(connect_req.as_bytes()).await.map_err(WsError::Io)?;

                    // Read and parse response
                    let mut response = Vec::new();
                    let mut buf = [0u8; 4096];
                    let mut headers_complete = false;

                    while !headers_complete {
                        let n = proxy_stream.read(&mut buf).await.map_err(WsError::Io)?;
                        if n == 0 {
                            return Err(WsError::Io(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "Proxy connection closed while reading response",
                            ))
                            .into());
                        }

                        if response.len().saturating_add(n) > 16 * 1024 {
                            return Err(WsError::Io(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "proxy response headers too large",
                            ))
                            .into());
                        }
                        response.extend_from_slice(&buf[..n]);

                        // Check if we've received the end of headers (double CRLF)
                        if response.windows(4).any(|w| w == b"\r\n\r\n") {
                            headers_complete = true;
                        }
                    }

                    // Parse status line
                    let response_str = String::from_utf8_lossy(&response);
                    let status_line = response_str.lines().next().ok_or_else(|| {
                        WsError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Invalid proxy response",
                        ))
                    })?;

                    // Do not echo the proxy's reason text, which can contain
                    // reflected Proxy-Authorization credentials.
                    check_proxy_status(status_line).map_err(WsError::Io)?;

                    log::debug!("Proxy connection established to {}", target);

                    // Create MaybeTlsStream based on original URL scheme
                    let stream = if url.scheme() == "wss" {
                        // Only enable proxy TLS support when rustls-tls-native-roots is enabled
                        #[cfg(feature = "rustls-tls-native-roots")]
                        {
                            // For WSS, we need to establish TLS over the proxy connection
                            use std::sync::Arc;
                            use tokio_rustls::{
                                rustls::{self, pki_types::ServerName},
                                TlsConnector,
                            };

                            // Load native root certificates
                            let mut root_store = rustls::RootCertStore::empty();
                            let cert_result = rustls_native_certs::load_native_certs();
                            if !cert_result.errors.is_empty() {
                                log::warn!(
                                    "Native root CA certificate loading errors: {:?}",
                                    cert_result.errors
                                );
                            }
                            if cert_result.certs.is_empty() {
                                return Err(WsError::Io(io::Error::new(
                                    io::ErrorKind::Other,
                                    format!(
                                        "Could not load any native root certificates: {:?}",
                                        cert_result.errors
                                    ),
                                ))
                                .into());
                            }
                            let total = cert_result.certs.len();
                            let (added, ignored) =
                                root_store.add_parsable_certificates(cert_result.certs);
                            log::debug!(
                                "Added {added}/{total} native root certificates ({ignored} ignored)"
                            );

                            let tls_config = rustls::ClientConfig::builder()
                                .with_root_certificates(root_store)
                                .with_no_client_auth();

                            let server_name =
                                ServerName::try_from(host.to_owned()).map_err(|_| {
                                    WsError::Io(io::Error::new(
                                        io::ErrorKind::InvalidInput,
                                        format!("Invalid DNS name: {}", host),
                                    ))
                                })?;

                            let connector = TlsConnector::from(Arc::new(tls_config));
                            let tls_stream = connector
                                .connect(server_name, proxy_stream)
                                .await
                                .map_err(|e| {
                                    WsError::Io(io::Error::new(
                                        io::ErrorKind::Other,
                                        format!("TLS connection error: {}", e),
                                    ))
                                })?;

                            MaybeTlsStream::Rustls(tls_stream)
                        }

                        #[cfg(not(feature = "rustls-tls-native-roots"))]
                        {
                            // For non-rustls-tls-native-roots builds, don't support proxy for WSS
                            return Err(WsError::Io(io::Error::new(
                                io::ErrorKind::Other,
                                "WSS over proxy requires rustls-tls-native-roots feature",
                            ))
                            .into());
                        }
                    } else {
                        // For plain WS, just use the proxy stream directly
                        MaybeTlsStream::Plain(proxy_stream)
                    };

                    // Now perform WebSocket handshake over the established connection
                    let (ws_stream, _) =
                        tokio_tungstenite::client_async_with_config(request, stream, None).await?;
                    ws_stream
                } else {
                    // No proxy specified, connect directly
                    let (ws_stream, _) = connect_async(request).await?;
                    ws_stream
                }
            } else {
                // Non-tokio build or no proxy - connect directly
                let (ws_stream, _) = connect_async(request).await?;
                ws_stream
            };

            ws_stream
        };

        #[cfg(not(feature = "signal-client-tokio"))]
        let (ws_stream, _) = connect_async(request).await?;
        let (ws_writer, ws_reader) = ws_stream.split();

        let (emitter, events) = mpsc::unbounded_channel();
        let (internal_tx, internal_rx) = mpsc::channel::<InternalMessage>(8);
        let write_handle = livekit_runtime::spawn(Self::write_task(internal_rx, ws_writer));
        let read_handle =
            livekit_runtime::spawn(Self::read_task(internal_tx.clone(), ws_reader, emitter));

        Ok((Self { internal_tx, read_handle, write_handle }, events))
    }

    /// Close the websocket
    /// It sends a CloseFrame to the server before closing
    pub async fn close(self, notify_close: bool) {
        if notify_close {
            let _ = self.internal_tx.send(InternalMessage::Close).await;
        }
        let _ = self.write_handle.await;
        let _ = self.read_handle.await;
    }

    /// Send a SignalRequest to the websocket
    /// It also waits for the message to be sent
    pub async fn send(&self, signal: proto::signal_request::Message) -> SignalResult<()> {
        let (send, recv) = oneshot::channel();
        let msg = InternalMessage::Signal { signal, response_chn: send };
        let _ = self.internal_tx.send(msg).await;
        recv.await.map_err(|_| SignalError::SendError)?
    }

    /// This task is used to send messages to the websocket
    /// It is also responsible for closing the connection
    async fn write_task(
        mut internal_rx: mpsc::Receiver<InternalMessage>,
        mut ws_writer: SplitSink<WebSocket, Message>,
    ) {
        while let Some(msg) = internal_rx.recv().await {
            match msg {
                InternalMessage::Signal { signal, response_chn } => {
                    let data = proto::SignalRequest { message: Some(signal) }.encode_to_vec();

                    if let Err(err) = ws_writer.send(Message::Binary(data.into())).await {
                        let _ = response_chn.send(Err(err.into()));
                        break;
                    }

                    let _ = response_chn.send(Ok(()));
                }
                InternalMessage::Pong { ping_data } => {
                    if ws_writer.send(Message::Pong(ping_data)).await.is_err() {
                        log::error!("failed to send pong message");
                    }
                }
                InternalMessage::Close => break,
            }
        }

        let _ = ws_writer.close().await;
    }

    /// This task is used to read incoming messages from the websocket
    /// and dispatch them through the EventEmitter.
    ///
    /// It can also send messages to [handle_write] task ( Used e.g. answer to pings )
    async fn read_task(
        internal_tx: mpsc::Sender<InternalMessage>,
        mut ws_reader: SplitStream<WebSocket>,
        emitter: mpsc::UnboundedSender<Box<proto::signal_response::Message>>,
    ) {
        while let Some(msg) = ws_reader.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    let Ok(res) = proto::SignalResponse::decode(data.as_ref()) else {
                        log::error!("invalid signaling response");
                        break;
                    };

                    if let Some(msg) = res.message {
                        let _ = emitter.send(Box::new(msg));
                    }
                }
                Ok(Message::Ping(data)) => {
                    let _ = internal_tx.send(InternalMessage::Pong { ping_data: data }).await;
                    continue;
                }
                Ok(Message::Close(close)) => {
                    log::info!(
                        "server closed the connection (code {:?})",
                        close.map(|frame| u16::from(frame.code))
                    );
                    break;
                }
                Ok(Message::Frame(_)) => {}
                Err(WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => {
                    break; // Ignore
                }
                _ => {
                    log::error!("unexpected websocket message or read failure");
                    break;
                }
            }
        }

        let _ = internal_tx.send(InternalMessage::Close).await;
    }
}

#[cfg(all(test, feature = "signal-client-tokio"))]
mod redaction_tests {
    use super::*;
    use std::sync::{Mutex, Once};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{
        accept_async,
        tungstenite::protocol::{frame::coding::CloseCode, CloseFrame},
    };

    struct CapturedLogs(Mutex<Vec<String>>);
    static LOGS: CapturedLogs = CapturedLogs(Mutex::new(Vec::new()));
    static LOGGER: Once = Once::new();
    impl log::Log for CapturedLogs {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            // Include tungstenite's logger as well as our adapter. A regression
            // in the static level cap must expose the serialized auth header.
            self.0.lock().unwrap().push(record.args().to_string());
        }
        fn flush(&self) {}
    }

    #[tokio::test]
    async fn unsupported_proxy_schemes_never_open_a_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        const MARKER: &str = "morpheus-dummy-proxy-password";
        for scheme in ["https", "socks5", "wss", "ftp"] {
            let raw = format!("{scheme}://test:{MARKER}@{address}");
            let error = connect_proxy(&raw).await.unwrap_err();
            assert!(!error.to_string().contains(MARKER));
            assert!(!error.to_string().contains(&raw));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err(),
            "unsupported proxy scheme opened a socket"
        );

        // HTTPS_PROXY selects the proxy for a secure target; its URL may
        // legitimately be http://. Keep that existing CONNECT route working.
        let (_, stream) = connect_proxy(&format!("http://test:{MARKER}@{address}")).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), listener.accept()).await.is_ok());
        drop(stream);
    }

    #[tokio::test]
    async fn proxy_credentials_and_reflected_peer_text_never_enter_logs() {
        LOGGER.call_once(|| {
            log::set_logger(&LOGS).expect("test logger already installed");
            log::set_max_level(log::LevelFilter::Trace);
        });
        assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Info);
        const MARKER: &str = "morpheus-dummy-credential-should-not-be-logged";
        let proxy = format!("http://test:{MARKER}@127.0.0.1:1234");
        assert!(parse_proxy_url(&proxy).is_ok());
        let rejected = check_proxy_status(&format!("HTTP/1.1 407 {MARKER}")).unwrap_err();
        assert!(!rejected.to_string().contains(MARKER));
        assert!(check_proxy_status("HTTP/1.1 500 reflected 200").is_err());
        for message in [
            Message::Text(MARKER.into()),
            Message::Close(Some(CloseFrame { code: CloseCode::Normal, reason: MARKER.into() })),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url =
                url::Url::parse(&format!("ws://{}/rtc", listener.local_addr().unwrap())).unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(socket).await.unwrap();
                websocket.send(message).await.unwrap();
                let _ = websocket.next().await;
            });
            let (stream, mut events) =
                SignalStream::connect(url, MARKER, Duration::from_secs(2)).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .is_none());
            stream.close(false).await;
            server.await.unwrap();
        }
        let logs = LOGS.0.lock().unwrap().join("\n");
        assert!(logs.contains("using configured signaling proxy"));
        assert!(logs.contains("unexpected websocket message"));
        assert!(logs.contains("server closed the connection"));
        assert!(!logs.contains(MARKER), "credential was logged");
        assert!(!logs.contains(&proxy), "proxy URL was logged");
    }
}
