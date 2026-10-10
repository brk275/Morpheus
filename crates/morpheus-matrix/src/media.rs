//! Account-scoped Matrix attachments. The SDK owns encryption, the media store,
//! and the persistent send queue; this module limits local input and keeps
//! encrypted media keys out of the public UI model.

use std::{fs::File, io::Read, path::Path, time::Duration};

use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo,
    },
    media::{MediaFormat, MediaRequestParameters},
    ruma::{
        UInt,
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
use mime::Mime;
use serde::{Deserialize, Serialize};
use tokio::{sync::broadcast, task};

use crate::{AccountManager, MatrixCoreError, QueuedSendOutcome, Result, account_room};

/// Limits one in-memory upload or download. Homeservers may impose a lower
/// upload limit; the SDK reports that independently.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MEDIA_TIMEOUT: Duration = Duration::from_secs(300);

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

    /// Download and decrypt an attachment for the selected account. Missing
    /// or excessive event size metadata is rejected before calling the SDK.
    /// The pinned SDK currently buffers the HTTP response internally, so a
    /// malicious server that lies about size can exceed this cap transiently;
    /// the returned plaintext is checked again and no download is cached.
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
        let size = resolved
            .metadata
            .declared_size
            .ok_or(MatrixCoreError::MissingMediaSize)?;
        check_size(size)?;
        let request = MediaRequestParameters {
            source: resolved.source,
            format: MediaFormat::File,
        };
        let bytes = tokio::time::timeout(
            MEDIA_TIMEOUT,
            room.client().media().get_media_content(&request, false),
        )
        .await
        .map_err(|_| MatrixCoreError::Sdk("media download timed out".into()))?
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
        check_size(bytes.len() as u64)?;
        self.assert_active(expected_account_id).await?;
        Ok(DownloadedAttachment {
            metadata: resolved.metadata,
            bytes,
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

async fn resolve_attachment(
    room: &matrix_sdk::room::Room,
    event_id: &str,
) -> Result<ResolvedAttachment> {
    let id = super::parse_event_id(event_id)?;
    let event = room
        .load_or_fetch_event(&id, None)
        .await
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    let raw = event
        .raw()
        .deserialize()
        .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
    match raw {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(event),
        )) => from_message(event_id, &event.content.msgtype),
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
    if size > MAX_ATTACHMENT_BYTES as u64 {
        Err(MatrixCoreError::MediaTooLarge {
            maximum: MAX_ATTACHMENT_BYTES as u64,
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
