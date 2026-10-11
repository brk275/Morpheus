//! Account-scoped Matrix attachments. The SDK owns encryption, the media store,
//! and the persistent send queue; this module limits local input and keeps
//! encrypted media keys out of the public UI model.

use std::{
    borrow::Cow,
    fs::File,
    io::{self, Read},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    path::Path,
    time::Duration,
};

use futures_util::{Stream, StreamExt};
use matrix_sdk::{
    Client,
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo,
    },
    bytes::Bytes,
    reqwest::{self, StatusCode, dns, header},
    ruma::{
        UInt,
        api::{
            Metadata, OutgoingRequestExt,
            auth_scheme::SendAccessToken,
            client::{authenticated_media, media},
        },
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            room::{
                MediaSource,
                message::{MessageType, SyncRoomMessageEvent},
            },
        },
    },
    send_queue::{RoomSendQueueUpdate, SendHandle},
};
use matrix_sdk_crypto::AttachmentDecryptor;
use mime::Mime;
use serde::{Deserialize, Serialize};
use tokio::{sync::broadcast, task};
use url::{Host, Url};

use crate::{AccountManager, MatrixCoreError, QueuedSendOutcome, Result, account_room};

/// Limits one in-memory upload or download. Homeservers may impose a lower
/// upload limit; the SDK reports that independently.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
/// Explicit native image previews have a smaller input budget than Save.
pub const MAX_IMAGE_PREVIEW_BYTES: usize = 8 * 1024 * 1024;
const MEDIA_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_MEDIA_REDIRECTS: usize = 5;
const MAX_MATRIX_ERROR_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Video,
    Audio,
    File,
}

/// Safe to show in the UI. In particular, encrypted file keys and MXC sources
/// are never exposed by this type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMetadata {
    pub event_id: String,
    pub filename: String,
    pub mime_type: String,
    pub declared_size: Option<u64>,
    pub kind: AttachmentKind,
    pub encrypted: bool,
}

/// Plaintext bytes are intentionally neither serializable nor printable with
/// `Debug`. Callers should display or save them explicitly and discard promptly.
pub struct DownloadedAttachment {
    pub metadata: AttachmentMetadata,
    pub bytes: Vec<u8>,
}

/// Opaque, memory-only preview input. It contains media secrets and therefore
/// deliberately implements neither Debug nor Serialize. Consume it on the
/// caller's single bounded decoder worker; dropping a network future never
/// leaves a detached SDK decryption job behind.
pub struct ImagePreviewDownload {
    metadata: AttachmentMetadata,
    source: MediaSource,
    ciphertext: Vec<u8>,
}

impl ImagePreviewDownload {
    pub fn into_plaintext(self) -> Result<DownloadedAttachment> {
        let bytes = decrypt_media(self.source, self.ciphertext, MAX_IMAGE_PREVIEW_BYTES)?;
        Ok(DownloadedAttachment {
            metadata: self.metadata,
            bytes,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentSendUpdate {
    /// Progress uses SDK abstract units, which can differ from byte counts
    /// when client-side encryption is involved.
    Progress {
        current: usize,
        total: usize,
    },
    Sent {
        event_id: String,
    },
    /// The SDK kept this request. Recoverable errors pause the room queue;
    /// unrecoverable ones wedge this request until retry or cancellation.
    Retained {
        recoverable: bool,
    },
    Cancelled,
    Unknown,
}

/// Handle to a queued upload. Dropping it does not cancel the persistent
/// queue; call `abort` when the user explicitly cancels an attachment.
pub struct QueuedAttachment {
    transaction_id: String,
    handle: SendHandle,
    updates: broadcast::Receiver<RoomSendQueueUpdate>,
    finished: bool,
}

impl QueuedAttachment {
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Returns false if the event was already sent. The SDK also handles the
    /// upload already being in flight and redacts a late event if necessary.
    pub async fn abort(&self) -> Result<bool> {
        self.handle
            .abort()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
    }

    /// Retry a wedged upload without enqueueing a duplicate attachment.
    pub async fn retry_wedged(&self) -> Result<()> {
        self.handle
            .unwedge()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
    }

    pub async fn next_update(&mut self) -> AttachmentSendUpdate {
        if self.finished {
            return AttachmentSendUpdate::Unknown;
        }
        loop {
            match self.updates.recv().await {
                Ok(RoomSendQueueUpdate::MediaUpload {
                    related_to,
                    progress,
                    ..
                }) if related_to.as_str() == self.transaction_id => {
                    return AttachmentSendUpdate::Progress {
                        current: progress.current,
                        total: progress.total,
                    };
                }
                Ok(RoomSendQueueUpdate::SentEvent {
                    transaction_id,
                    event_id,
                }) if transaction_id.as_str() == self.transaction_id => {
                    self.finished = true;
                    return AttachmentSendUpdate::Sent {
                        event_id: event_id.to_string(),
                    };
                }
                Ok(RoomSendQueueUpdate::SendError {
                    transaction_id,
                    is_recoverable,
                    ..
                }) if transaction_id.as_str() == self.transaction_id => {
                    return AttachmentSendUpdate::Retained {
                        recoverable: is_recoverable,
                    };
                }
                Ok(RoomSendQueueUpdate::CancelledLocalEvent { transaction_id })
                    if transaction_id.as_str() == self.transaction_id =>
                {
                    self.finished = true;
                    return AttachmentSendUpdate::Cancelled;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    self.finished = true;
                    return AttachmentSendUpdate::Unknown;
                }
            }
        }
    }

    pub async fn wait_outcome(mut self) -> QueuedSendOutcome {
        loop {
            match self.next_update().await {
                AttachmentSendUpdate::Progress { .. } => {}
                AttachmentSendUpdate::Sent { .. } => return QueuedSendOutcome::Sent,
                AttachmentSendUpdate::Retained { .. } => {}
                AttachmentSendUpdate::Cancelled => return QueuedSendOutcome::Failed,
                AttachmentSendUpdate::Unknown => return QueuedSendOutcome::Unknown,
            }
        }
    }
}

struct ResolvedAttachment {
    metadata: AttachmentMetadata,
    source: MediaSource,
    // Message encryption is independent of the attachment's own MediaSource
    // and current room state. Any encrypted context requires preview opt-in.
    event_was_encrypted: bool,
}

impl AccountManager {
    /// Queue a local file for the account currently shown to the user. The
    /// entire input is read with a strict 25 MiB cap before it enters the SDK's
    /// encrypted per-account media store and persistent room send queue.
    pub async fn send_attachment_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        path: impl AsRef<Path>,
        mime_type: &str,
    ) -> Result<QueuedAttachment> {
        let mime_type = parse_mime(mime_type)?;
        let path = path.as_ref().to_path_buf();
        let (filename, bytes) = task::spawn_blocking(move || read_bounded_file(&path))
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))??;
        let size = UInt::try_from(bytes.len() as u64)
            .map_err(|_| MatrixCoreError::InvalidMedia("attachment size is invalid".into()))?;
        let info = attachment_info(&mime_type, size);

        // Hold the switch lock through queueing. If the user changed accounts
        // while the file was read, no upload can be directed to the old room.
        let state = self.state.lock().await;
        if state.registry.active_account_id.as_deref() != Some(expected_account_id) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let room = account_room(&state, expected_account_id, room_id)?;
        let queue = room.send_queue();
        let (_, updates) = queue
            .subscribe()
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        let handle = queue
            .send_attachment(
                filename,
                mime_type,
                bytes,
                AttachmentConfig {
                    info: Some(info),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        Ok(QueuedAttachment {
            transaction_id: handle.transaction_id().to_string(),
            handle,
            updates,
            finished: false,
        })
    }

    /// Inspect media metadata from an event without handing its encrypted
    /// source or decryption key to the UI.
    pub async fn attachment_metadata_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<AttachmentMetadata> {
        let room = {
            let state = self.state.lock().await;
            if state.registry.active_account_id.as_deref() != Some(expected_account_id) {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            account_room(&state, expected_account_id, room_id)?
        };
        let resolved = resolve_attachment(&room, event_id).await?;
        self.assert_active(expected_account_id).await?;
        Ok(resolved.metadata)
    }

    /// Download and decrypt an attachment for the selected account. The Matrix
    /// SDK's media convenience API buffers an unbounded HTTP body, so use a
    /// dedicated redirect-checked client and Ruma's endpoint construction.
    /// Each incoming chunk is checked before allocation exceeds the limit.
    pub async fn download_attachment_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<DownloadedAttachment> {
        let room = {
            let state = self.state.lock().await;
            if state.registry.active_account_id.as_deref() != Some(expected_account_id) {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            account_room(&state, expected_account_id, room_id)?
        };
        let resolved = resolve_attachment(&room, event_id).await?;
        if let Some(size) = resolved.metadata.declared_size {
            check_size(size)?;
        }
        let bytes = download_bounded_media(&room.client(), &resolved.source).await?;
        check_size(bytes.len() as u64)?;
        self.assert_active(expected_account_id).await?;
        Ok(DownloadedAttachment {
            metadata: resolved.metadata,
            bytes,
        })
    }

    /// Resolve only a real Matrix image event in the selected account. The
    /// caller owns cancellation/deadlines and current privacy-policy checks.
    /// No source URL or decryption key crosses this opaque API.
    pub async fn download_image_preview_for(
        &self,
        selection: &crate::AccountSelection,
        room_id: &str,
        event_id: &str,
        allow_encrypted: bool,
    ) -> Result<ImagePreviewDownload> {
        let room = {
            let state = self.state.lock().await;
            if !selection.is_current()
                || state.registry.active_account_id.as_deref() != Some(selection.account_id())
            {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            account_room(&state, selection.account_id(), room_id)?
        };
        let encryption = room
            .latest_encryption_state()
            .await
            .map_err(|_| MatrixCoreError::InvalidMedia("Cannot check room encryption".into()))?;
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        if encryption.is_unknown() {
            return Err(MatrixCoreError::InvalidMedia(
                "Cannot check room encryption".into(),
            ));
        }
        let encrypted_room = encryption.is_encrypted();
        if encrypted_room && !allow_encrypted {
            return Err(MatrixCoreError::InvalidMedia(
                "Encrypted image previews are disabled".into(),
            ));
        }
        let resolved = resolve_attachment(&room, event_id).await?;
        validate_preview_metadata(
            &resolved.metadata,
            encrypted_room,
            resolved.event_was_encrypted,
            allow_encrypted,
        )?;
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let ciphertext =
            fetch_bounded_media(&room.client(), &resolved.source, MAX_IMAGE_PREVIEW_BYTES).await?;
        if !selection.is_current() {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(ImagePreviewDownload {
            metadata: resolved.metadata,
            source: resolved.source,
            ciphertext,
        })
    }

    async fn assert_active(&self, expected_account_id: &str) -> Result<()> {
        if self
            .state
            .lock()
            .await
            .registry
            .active_account_id
            .as_deref()
            == Some(expected_account_id)
        {
            Ok(())
        } else {
            Err(MatrixCoreError::SelectedAccountChanged)
        }
    }
}

async fn fetch_bounded_media(
    client: &Client,
    source: &MediaSource,
    maximum: usize,
) -> Result<Vec<u8>> {
    let uri = match source {
        MediaSource::Plain(uri) => uri,
        MediaSource::Encrypted(file) => &file.url,
    };
    let supported = client
        .supported_versions()
        .await
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    let base_url = client.homeserver();
    let authenticated =
        authenticated_media::get_content::v1::Request::PATH_BUILDER.is_supported(&supported);
    // The SDK's reqwest client follows redirects automatically. A final-URL
    // check is too late to prevent a request to an unsafe target. Use separate
    // clients so the trusted homeserver retains normal proxy support while
    // every off-origin CDN connection uses only DNS-vetted public addresses.
    let first_party_http = reqwest::Client::builder()
        .user_agent("matrix-rust-sdk")
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    let public_cdn_http = reqwest::Client::builder()
        .user_agent("matrix-rust-sdk")
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .redirect(reqwest::redirect::Policy::none())
        // A system/environment proxy resolves the CDN hostname itself and
        // would bypass our resolver. Fail closed for CDN fetches instead.
        .no_proxy()
        .dns_resolver(PublicMediaResolver)
        .build()
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    let mut refresh_attempt = 0;
    let response = 'retry_after_refresh: loop {
        let request = if authenticated {
            let token = client.access_token().ok_or_else(|| {
                MatrixCoreError::Sdk("authenticated media requires a session".into())
            })?;
            authenticated_media::get_content::v1::Request::from_uri(uri)
                .map_err(|error| MatrixCoreError::InvalidMedia(error.to_string()))?
                .try_into_http_request::<Vec<u8>>(
                    base_url.as_str(),
                    SendAccessToken::IfRequired(&token),
                    Cow::Borrowed(&supported),
                )
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
        } else {
            #[allow(deprecated)]
            let request = media::get_content::v3::Request::from_url(uri)
                .map_err(|error| MatrixCoreError::InvalidMedia(error.to_string()))?;
            request
                .try_into_http_request::<Vec<u8>>(
                    base_url.as_str(),
                    SendAccessToken::None,
                    Cow::Borrowed(&supported),
                )
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
        };
        let mut url = Url::parse(&request.uri().to_string())
            .map_err(|error| MatrixCoreError::InvalidMedia(error.to_string()))?;
        let initial_url = url.clone();
        let mut crossed_origin = false;
        for redirects in 0..=MAX_MEDIA_REDIRECTS {
            // Never forward an access token to a CDN. The unauthenticated CDN
            // URL is supplied by the homeserver's 307/308 Location header.
            let media_http = if crossed_origin {
                &public_cdn_http
            } else {
                &first_party_http
            };
            let mut get = media_http.get(url.clone()).timeout(MEDIA_TIMEOUT);
            if !crossed_origin && url.origin() == initial_url.origin() {
                get = get.headers(request.headers().clone());
            }
            let response = get.send().await.map_err(|error| {
                // reqwest may include a signed CDN URL in its Display
                // output. Never surface its query string in UI or logs.
                let detail = if error.is_timeout() {
                    "media request timed out"
                } else if error.is_connect() {
                    "media connection failed"
                } else {
                    "media network request failed"
                };
                MatrixCoreError::Sdk(detail.into())
            })?;
            if authenticated
                && refresh_attempt == 0
                && redirects == 0
                && response.status() == StatusCode::UNAUTHORIZED
            {
                if is_unknown_token(response).await {
                    client
                        .refresh_access_token()
                        .await
                        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
                    refresh_attempt += 1;
                    continue 'retry_after_refresh;
                }
                return Err(MatrixCoreError::Sdk(
                    "media download was rejected with HTTP 401".into(),
                ));
            }
            if matches!(
                response.status(),
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
            ) {
                if !authenticated {
                    return Err(MatrixCoreError::InvalidMedia(
                        "legacy media endpoint redirected despite allow_redirect=false".into(),
                    ));
                }
                if redirects == MAX_MEDIA_REDIRECTS {
                    return Err(MatrixCoreError::InvalidMedia(
                        "too many media redirects".into(),
                    ));
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .ok_or_else(|| {
                        MatrixCoreError::InvalidMedia("media redirect has no Location".into())
                    })?
                    .to_str()
                    .map_err(|_| {
                        MatrixCoreError::InvalidMedia("invalid media redirect Location".into())
                    })?;
                let next = url
                    .join(location)
                    .map_err(|error| MatrixCoreError::InvalidMedia(error.to_string()))?;
                validate_media_redirect(&initial_url, &next)?;
                crossed_origin |= next.origin() != initial_url.origin();
                url = next;
                continue;
            }
            if response.status().is_redirection() {
                return Err(MatrixCoreError::InvalidMedia(
                    "unsupported media redirect response".into(),
                ));
            }
            if !response.status().is_success() {
                return Err(MatrixCoreError::Sdk(format!(
                    "media server returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            break 'retry_after_refresh response;
        }
        unreachable!("bounded media redirect loop always returns or errors")
    };
    if let Some(length) = response.content_length() {
        check_size_limit(length, maximum)?;
    }
    collect_bounded_stream_limit(response.bytes_stream(), maximum).await
}

async fn download_bounded_media(client: &Client, source: &MediaSource) -> Result<Vec<u8>> {
    let ciphertext = fetch_bounded_media(client, source, MAX_ATTACHMENT_BYTES).await?;
    if matches!(source, MediaSource::Plain(_)) {
        return Ok(ciphertext);
    }
    let source = source.clone();
    task::spawn_blocking(move || decrypt_media(source, ciphertext, MAX_ATTACHMENT_BYTES))
        .await
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
}

fn decrypt_media(source: MediaSource, ciphertext: Vec<u8>, maximum: usize) -> Result<Vec<u8>> {
    check_size_limit(ciphertext.len() as u64, maximum)?;
    match source {
        MediaSource::Plain(_) => Ok(ciphertext),
        MediaSource::Encrypted(file) => {
            let mut source = std::io::Cursor::new(ciphertext);
            let decryptor = AttachmentDecryptor::new(&mut source, file.as_ref().clone().into())
                .map_err(|_| MatrixCoreError::InvalidMedia("Cannot decrypt media".into()))?;
            let mut plaintext = Vec::new();
            decryptor
                .take(maximum as u64 + 1)
                .read_to_end(&mut plaintext)?;
            check_size_limit(plaintext.len() as u64, maximum)?;
            Ok(plaintext)
        }
    }
}

fn validate_preview_metadata(
    metadata: &AttachmentMetadata,
    encrypted_room: bool,
    encrypted_event: bool,
    allow_encrypted: bool,
) -> Result<()> {
    if metadata.kind != AttachmentKind::Image {
        return Err(MatrixCoreError::InvalidMedia(
            "Only image messages can be previewed".into(),
        ));
    }
    if (encrypted_room || encrypted_event || metadata.encrypted) && !allow_encrypted {
        return Err(MatrixCoreError::InvalidMedia(
            "Encrypted image previews are disabled".into(),
        ));
    }
    if let Some(size) = metadata.declared_size {
        check_size_limit(size, MAX_IMAGE_PREVIEW_BYTES)?;
    }
    Ok(())
}

/// A cross-origin redirect is only useful for a public HTTPS CDN. The URL
/// check rejects obvious local targets; PublicMediaResolver also checks the
/// addresses returned for that hostname at the point of connection.
fn validate_media_redirect(initial: &Url, next: &Url) -> Result<()> {
    if next.username() != "" || next.password().is_some() || next.fragment().is_some() {
        return Err(MatrixCoreError::InvalidMedia(
            "unsafe media redirect URL".into(),
        ));
    }
    if next.origin() == initial.origin() {
        return Ok(());
    }
    if next.scheme() != "https" {
        return Err(MatrixCoreError::InvalidMedia(
            "media redirect must use HTTPS".into(),
        ));
    }
    let Some(Host::Domain(domain)) = next.host() else {
        return Err(MatrixCoreError::InvalidMedia(
            "media redirect cannot target an IP address".into(),
        ));
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if !domain.contains('.')
        || [
            "localhost",
            "local",
            "lan",
            "internal",
            "test",
            "invalid",
            "example",
            "onion",
        ]
        .iter()
        .any(|suffix| domain == *suffix || domain.ends_with(&format!(".{suffix}")))
    {
        return Err(MatrixCoreError::InvalidMedia(
            "media redirect targets a local host".into(),
        ));
    }
    Ok(())
}

/// Reqwest's connector uses these exact SocketAddrs to establish a connection,
/// so a hostname cannot be resolved again after the check. A fresh resolution
/// on a later connection is checked again, including after a DNS rebinding.
struct PublicMediaResolver;

impl dns::Resolve for PublicMediaResolver {
    fn resolve(&self, name: dns::Name) -> dns::Resolving {
        Box::pin(resolve_public_media_host(name.as_str().to_owned()))
    }
}

async fn resolve_public_media_host(
    hostname: String,
) -> std::result::Result<dns::Addrs, Box<dyn std::error::Error + Send + Sync>> {
    // Match reqwest's default getaddrinfo resolver without blocking Tokio's
    // executor. Hyper connects to the returned addresses, not to `hostname`.
    let addresses: Vec<SocketAddr> = task::spawn_blocking(move || {
        (hostname.as_str(), 0)
            .to_socket_addrs()
            .map(Iterator::collect)
    })
    .await??;
    Ok(Box::new(
        checked_public_media_addresses(addresses)?.into_iter(),
    ))
}

fn checked_public_media_addresses(addresses: Vec<SocketAddr>) -> io::Result<Vec<SocketAddr>> {
    if addresses.is_empty() || addresses.iter().any(|addr| !is_public_media_ip(addr.ip())) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "media CDN DNS returned a non-public address",
        ));
    }
    Ok(addresses)
}

fn is_public_media_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_media_ipv4(address),
        IpAddr::V6(address) => is_public_media_ipv6(address),
    }
}

fn is_public_media_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !matches!(
        (a, b, c),
        // Unspecified, RFC1918, carrier-grade NAT, loopback, link-local.
        (0 | 10 | 127, _, _)
            | (100, 64..=127, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 168, _)
            // IANA protocol assignments, documentation, relay and anycast
            // special-purpose blocks, benchmarking, multicast and reserved.
            | (192, 0, 0 | 2)
            | (192, 31, 196)
            | (192, 52, 193)
            | (192, 88, 99)
            | (192, 175, 48)
            | (198, 18 | 19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn is_public_media_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    // Conservative allowlist of current RIR-allocated global-unicast space.
    // The rest of 2000::/3 is still reserved by IANA. In particular this
    // excludes IPv4-mapped/NAT64, ULA, link-local, multicast, 6to4, and
    // documentation addresses. Update when IANA allocates new prefixes:
    // https://www.iana.org/assignments/ipv6-unicast-address-assignments/
    match segments[0] {
        0x2001 => {
            matches!(
                segments[1],
                0x0200..=0x0fff | 0x1200..=0x4dff | 0x5000..=0x5fff | 0x8000..=0xbfff
            ) && segments[1] != 0x0db8
        }
        0x2003 => segments[1] < 0x4000,
        0x2400..=0x241f
        | 0x2600..=0x260f
        | 0x2630..=0x263f
        | 0x2800..=0x280f
        | 0x2a00..=0x2a1f
        | 0x2c00..=0x2c0f => true,
        0x2610 | 0x2620 => segments[1] <= 0x01ff,
        _ => false,
    }
}

async fn is_unknown_token(response: reqwest::Response) -> bool {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return false;
        };
        if body.len().saturating_add(chunk.len()) > MAX_MATRIX_ERROR_BYTES {
            return false;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| json.get("errcode")?.as_str().map(str::to_owned))
        .is_some_and(|code| code == "M_UNKNOWN_TOKEN")
}

#[cfg(test)]
async fn collect_bounded_stream<S, E>(stream: S) -> Result<Vec<u8>>
where
    S: Stream<Item = std::result::Result<Bytes, E>>,
{
    collect_bounded_stream_limit(stream, MAX_ATTACHMENT_BYTES).await
}

async fn collect_bounded_stream_limit<S, E>(stream: S, maximum: usize) -> Result<Vec<u8>>
where
    S: Stream<Item = std::result::Result<Bytes, E>>,
{
    futures_util::pin_mut!(stream);
    let mut result = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| MatrixCoreError::Sdk("media body transfer failed".into()))?;
        let next_size = result.len().saturating_add(chunk.len());
        check_size_limit(next_size as u64, maximum)?;
        result
            .try_reserve_exact(chunk.len())
            .map_err(|_| MatrixCoreError::InvalidMedia("Media allocation failed".into()))?;
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

async fn resolve_attachment(
    room: &matrix_sdk::room::Room,
    event_id: &str,
) -> Result<ResolvedAttachment> {
    let id = super::parse_event_id(event_id)?;
    let event = room
        .load_or_fetch_event(&id, None)
        .await
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    let event_was_encrypted = event.encryption_info().is_some();
    let raw = event
        .raw()
        .deserialize()
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    match raw {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(event),
        )) => {
            let mut resolved = from_message(event_id, &event.content.msgtype)?;
            resolved.event_was_encrypted = event_was_encrypted;
            Ok(resolved)
        }
        _ => Err(MatrixCoreError::NotMediaAttachment),
    }
}

fn from_message(event_id: &str, message: &MessageType) -> Result<ResolvedAttachment> {
    let (kind, filename, mime_type, declared_size, source) = match message {
        MessageType::Image(content) => (
            AttachmentKind::Image,
            content.filename(),
            content
                .info
                .as_ref()
                .and_then(|info| info.mimetype.as_deref()),
            content.info.as_ref().and_then(|info| info.size),
            &content.source,
        ),
        MessageType::Video(content) => (
            AttachmentKind::Video,
            content.filename(),
            content
                .info
                .as_ref()
                .and_then(|info| info.mimetype.as_deref()),
            content.info.as_ref().and_then(|info| info.size),
            &content.source,
        ),
        MessageType::Audio(content) => (
            AttachmentKind::Audio,
            content.filename(),
            content
                .info
                .as_ref()
                .and_then(|info| info.mimetype.as_deref()),
            content.info.as_ref().and_then(|info| info.size),
            &content.source,
        ),
        MessageType::File(content) => (
            AttachmentKind::File,
            content.filename(),
            content
                .info
                .as_ref()
                .and_then(|info| info.mimetype.as_deref()),
            content.info.as_ref().and_then(|info| info.size),
            &content.source,
        ),
        _ => return Err(MatrixCoreError::NotMediaAttachment),
    };
    Ok(ResolvedAttachment {
        event_was_encrypted: false,
        metadata: AttachmentMetadata {
            event_id: event_id.into(),
            filename: safe_filename(filename),
            mime_type: mime_type
                .and_then(|value| parse_mime(value).ok())
                .map_or_else(
                    || "application/octet-stream".into(),
                    |value| value.to_string(),
                ),
            declared_size: declared_size.map(u64::from),
            kind,
            encrypted: matches!(source, MediaSource::Encrypted(_)),
        },
        source: source.clone(),
    })
}

fn parse_mime(raw: &str) -> Result<Mime> {
    if raw.is_empty() || raw.len() > 128 {
        return Err(MatrixCoreError::InvalidMedia("invalid MIME type".into()));
    }
    raw.parse::<Mime>()
        .map_err(|_| MatrixCoreError::InvalidMedia("invalid MIME type".into()))
}

fn attachment_info(mime_type: &Mime, size: UInt) -> AttachmentInfo {
    match mime_type.type_().as_str() {
        "image" => AttachmentInfo::Image(BaseImageInfo {
            size: Some(size),
            ..Default::default()
        }),
        "video" => AttachmentInfo::Video(BaseVideoInfo {
            size: Some(size),
            ..Default::default()
        }),
        "audio" => AttachmentInfo::Audio(BaseAudioInfo {
            size: Some(size),
            ..Default::default()
        }),
        _ => AttachmentInfo::File(BaseFileInfo { size: Some(size) }),
    }
}

fn check_size(size: u64) -> Result<()> {
    check_size_limit(size, MAX_ATTACHMENT_BYTES)
}

fn check_size_limit(size: u64, maximum: usize) -> Result<()> {
    if size > maximum as u64 {
        Err(MatrixCoreError::MediaTooLarge {
            maximum: maximum as u64,
            actual: size,
        })
    } else {
        Ok(())
    }
}

fn read_bounded_file(path: &Path) -> Result<(String, Vec<u8>)> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| MatrixCoreError::InvalidMedia("attachment needs a UTF-8 filename".into()))?;
    let filename = safe_filename(filename);
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(MatrixCoreError::InvalidMedia(
            "attachment is not a regular file".into(),
        ));
    }
    check_size(metadata.len())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    check_size(bytes.len() as u64)?;
    Ok((filename, bytes))
}

pub(crate) fn attachment_name(message: &MessageType) -> Option<String> {
    let name = match message {
        MessageType::Image(content) => content.filename(),
        MessageType::Video(content) => content.filename(),
        MessageType::Audio(content) => content.filename(),
        MessageType::File(content) => content.filename(),
        _ => return None,
    };
    Some(safe_filename(name))
}

fn safe_filename(raw: &str) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let safe = basename
        .chars()
        .take(200)
        .map(|c| {
            if c.is_control()
                || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')
                || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
            {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    let safe = safe.trim_matches([' ', '.']);
    if safe.is_empty() {
        "attachment".into()
    } else if is_windows_reserved_name(safe) {
        format!("_{safe}")
    } else {
        safe.into()
    }
}

fn is_windows_reserved_name(filename: &str) -> bool {
    // Windows reserves these device basenames even when followed by an
    // extension. Prefixing also makes a filename safe across platforms when
    // it is chosen on one OS and downloaded on another.
    let stem = filename.split('.').next().unwrap_or(filename).trim_end();
    if ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return true;
    }
    let upper = stem.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    bytes.len() == 4
        && (&bytes[..3] == b"COM" || &bytes[..3] == b"LPT")
        && (b'1'..=b'9').contains(&bytes[3])
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[test]
    fn media_redirects_validate_targets_before_request() {
        let initial =
            Url::parse("https://homeserver.example.org/_matrix/client/v1/media/download/a/b")
                .unwrap();
        assert!(
            validate_media_redirect(
                &initial,
                &Url::parse("https://homeserver.example.org/media-proxy/a").unwrap(),
            )
            .is_ok()
        );
        assert!(
            validate_media_redirect(
                &initial,
                &Url::parse("https://cdn.example.net/object/a").unwrap(),
            )
            .is_ok()
        );
        for target in [
            "http://cdn.example.net/object/a",
            "https://127.0.0.1/object/a",
            "https://[::1]/object/a",
            "https://localhost/object/a",
            "https://gateway.internal/object/a",
            "https://user:password@cdn.example.net/object/a",
            "https://cdn.example.net/object/a#fragment",
        ] {
            assert!(
                validate_media_redirect(&initial, &Url::parse(target).unwrap()).is_err(),
                "{target}"
            );
        }
    }

    #[test]
    fn cdn_dns_accepts_only_public_addresses() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "2a00:1450:4009::200e",
        ] {
            assert!(is_public_media_ip(address.parse().unwrap()), "{address}");
        }
        for address in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.5",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.5",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::c0a8:0101",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002:c0a8:101::1",
            "3000::1",
            "3fff::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(!is_public_media_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[tokio::test]
    async fn cdn_resolver_rejects_loopback_at_connection_resolution() {
        assert!(resolve_public_media_host("localhost".into()).await.is_err());
        assert!(resolve_public_media_host("127.0.0.1".into()).await.is_err());
        let public = "1.1.1.1:0".parse().unwrap();
        let private = "127.0.0.1:0".parse().unwrap();
        assert_eq!(
            checked_public_media_addresses(vec![public]).unwrap(),
            vec![public]
        );
        assert!(checked_public_media_addresses(vec![public, private]).is_err());
        assert!(checked_public_media_addresses(vec![]).is_err());
    }

    #[tokio::test]
    async fn guarded_cdn_client_never_connects_to_loopback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(PublicMediaResolver)
            .build()
            .unwrap();
        assert!(
            client
                .get(format!(
                    "http://localhost:{}/",
                    listener.local_addr().unwrap().port()
                ))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_err()
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn streamed_download_rejects_an_oversized_body() {
        let chunks = [
            Ok::<Bytes, std::io::Error>(Bytes::from(vec![0x41; MAX_ATTACHMENT_BYTES])),
            Ok(Bytes::from_static(b"extra")),
        ];
        assert!(matches!(
            collect_bounded_stream(stream::iter(chunks)).await,
            Err(MatrixCoreError::MediaTooLarge { actual, .. }) if actual == (MAX_ATTACHMENT_BYTES + 5) as u64
        ));
        assert_eq!(
            collect_bounded_stream(stream::iter([Ok::<Bytes, std::io::Error>(
                Bytes::from_static(b"within limit"),
            )]))
            .await
            .unwrap(),
            b"within limit"
        );
    }

    #[test]
    fn file_read_rejects_oversized_input() {
        let path = std::env::temp_dir().join(format!("morpheus-media-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"small attachment").unwrap();
        let (name, bytes) = read_bounded_file(&path).unwrap();
        assert!(name.starts_with("morpheus-media-"));
        assert_eq!(bytes, b"small attachment");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES as u64 + 1)
            .unwrap();
        assert!(matches!(
            read_bounded_file(&path),
            Err(MatrixCoreError::MediaTooLarge { .. })
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn filenames_and_mime_types_are_validated() {
        assert_eq!(safe_filename("../secret\\image?.png"), "image_.png");
        assert_eq!(safe_filename(".."), "attachment");
        assert_eq!(safe_filename("file\u{202e}gnp.exe"), "file_gnp.exe");
        assert_eq!(safe_filename("co\u{200b}n.txt"), "co_n.txt");
        assert_eq!(safe_filename("\u{061c}report\u{feff}.pdf"), "_report_.pdf");
        assert_eq!(safe_filename("CON.txt"), "_CON.txt");
        assert_eq!(safe_filename("lpt9"), "_lpt9");
        assert_eq!(safe_filename("com0.txt"), "com0.txt");
        assert!(parse_mime("image/png").is_ok());
        assert!(parse_mime("not a MIME type").is_err());
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn preview_requires_image_and_each_encryption_context_opt_in() {
        let mut metadata = AttachmentMetadata {
            event_id: "$fixture".into(),
            filename: "image.png".into(),
            mime_type: "image/png".into(),
            declared_size: None,
            kind: AttachmentKind::Image,
            encrypted: false,
        };
        assert!(validate_preview_metadata(&metadata, false, false, false).is_ok());
        // Decrypted message metadata cannot lose privacy protection merely
        // because room state and the embedded MXC source are both cleartext.
        assert!(validate_preview_metadata(&metadata, false, true, false).is_err());
        assert!(validate_preview_metadata(&metadata, false, true, true).is_ok());
        assert!(validate_preview_metadata(&metadata, true, false, false).is_err());
        metadata.encrypted = true;
        assert!(validate_preview_metadata(&metadata, false, false, false).is_err());
        assert!(validate_preview_metadata(&metadata, true, true, true).is_ok());
        metadata.kind = AttachmentKind::File;
        assert!(validate_preview_metadata(&metadata, true, true, true).is_err());
    }
    #[test]
    fn preview_decryption_consumes_eof_hash_and_rejects_tampering() {
        use matrix_sdk::ruma::{events::room::EncryptedFile, mxc_uri};
        use matrix_sdk_crypto::AttachmentEncryptor;
        let mut input = std::io::Cursor::new(b"bounded private preview");
        let mut encryptor = AttachmentEncryptor::new(&mut input);
        let mut ciphertext = Vec::new();
        encryptor.read_to_end(&mut ciphertext).unwrap();
        let info = encryptor.finish();
        let source = MediaSource::Encrypted(Box::new(EncryptedFile::new(
            mxc_uri!("mxc://example.org/preview-fixture").to_owned(),
            info.encryption_info,
            info.hashes,
        )));
        let plaintext =
            decrypt_media(source.clone(), ciphertext.clone(), ciphertext.len()).unwrap();
        assert_eq!(plaintext, b"bounded private preview");
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 1;
        assert!(decrypt_media(source.clone(), ciphertext.clone(), ciphertext.len()).is_err());
        assert!(matches!(
            decrypt_media(source, ciphertext, last),
            Err(MatrixCoreError::MediaTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn preview_stream_enforces_smaller_limit_without_declared_size() {
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"abcd")),
            Ok(Bytes::from_static(b"ef")),
        ]);
        assert!(matches!(
            collect_bounded_stream_limit(chunks, 5).await,
            Err(MatrixCoreError::MediaTooLarge {
                maximum: 5,
                actual: 6
            })
        ));
        assert_eq!(
            collect_bounded_stream_limit(
                futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"abcd"))]),
                4
            )
            .await
            .unwrap(),
            b"abcd"
        );
        assert!(
            check_size_limit(MAX_IMAGE_PREVIEW_BYTES as u64 + 1, MAX_IMAGE_PREVIEW_BYTES).is_err()
        );
        assert!(check_size(MAX_IMAGE_PREVIEW_BYTES as u64 + 1).is_ok());
    }
}
