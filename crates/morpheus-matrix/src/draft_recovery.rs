//! Durable send-recovery metadata carried inside the encrypted composer draft.
//!
//! The pinned SDK's typed `ComposerDraft` value is the only public draft
//! persistence path that serializes through the store cipher. Morpheus owns
//! that row's HTML representation, so the recovery envelope travels as one
//! attribute of the canonical draft span and is written atomically with the
//! live draft text in a single `INSERT OR REPLACE`. Nothing here is ever placed
//! in an outbound Matrix event: `ComposeText::encode_draft` is used only by
//! the draft store, while outbound formatting goes through `formatting`.
//!
//! Every pending record carries the complete typed content it was created
//! from (body, format and explicit mentions), so the text of a failed,
//! cancelled or unconfirmed send survives later edits, later submissions and
//! restarts, and can be reviewed, copied or restored one record at a time
//! without touching the newer live text. A record never adopts the live
//! generation; acknowledged text is dropped with its record and not restored.
//!
//! Encoding is strict and bounded: lowercase hex of canonical `serde_json`
//! output, re-encoded on read and compared byte for byte, unknown fields
//! rejected, every identifier validated. All size limits are derived below
//! from the supported message size before any allocation. A row this build
//! recognizes as Morpheus-written but cannot interpret is quarantined: the
//! complete original row is carried verbatim (bounded) until the user
//! explicitly discards it, or the scope stays read-only when it is too large
//! to carry, holds attachments or a draft kind this build does not write.
//! Quarantined state is never normalized into an ordinary envelope. The
//! destructive discard names an opaque per-process `RowRevision` minted for
//! the exact complete row (both text columns, kind and every attachment
//! value); the core re-reads and compares that complete row before it
//! releases anything, so no hash stands in for the data at this boundary.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use matrix_sdk::ruma::EventId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    TextFormat,
    compose::{
        ComposeText, MAX_COMPOSE_BYTES, MAX_MENTION_SPANS, MentionSpan, MentionTarget,
        RestoredCompose,
    },
};

/// Envelope schema version written by this build. Version 1 was never
/// released; rows claiming it are quarantined like any unknown version.
pub const ENVELOPE_VERSION: u32 = 2;
/// Pending send records tracked per draft scope, including kept text.
pub const MAX_PENDING_RECORDS: usize = 8;
/// Thread roots a room draft indexes for recovery discovery.
pub const MAX_THREAD_INDEX: usize = 32;
const MAX_TRANSACTION_ID_BYTES: usize = 64;
/// Longest Matrix identifier accepted in a record mention or thread index.
const MAX_MATRIX_ID_BYTES: usize = 255;
/// Worst-case JSON string expansion: every input byte escaped as `\u00XX`.
const JSON_ESCAPE_FACTOR: usize = 6;
/// Keys, kind and two decimal offsets of one serialized mention.
const MENTION_FIXED_JSON_BYTES: usize = 128;
/// Keys, two opaque ids, state, transaction id, format and punctuation.
const RECORD_FIXED_JSON_BYTES: usize = 512;
/// Serialized size ceiling of one record's text: a full supported body and
/// the maximum number of mentions with the longest valid user ids.
pub const MAX_RECORD_TEXT_JSON_BYTES: usize = MAX_COMPOSE_BYTES * JSON_ESCAPE_FACTOR
    + MAX_MENTION_SPANS * (MAX_MATRIX_ID_BYTES * JSON_ESCAPE_FACTOR + MENTION_FIXED_JSON_BYTES);
pub const MAX_RECORD_JSON_BYTES: usize = RECORD_FIXED_JSON_BYTES + MAX_RECORD_TEXT_JSON_BYTES;
const THREAD_INDEX_JSON_BYTES: usize =
    MAX_THREAD_INDEX * (MAX_MATRIX_ID_BYTES * JSON_ESCAPE_FACTOR + 8);
/// Largest original row (plain column plus HTML column) carried in
/// quarantine. Larger rows stay read-only in place. A row of this size holds
/// a full live text plus a full envelope without its own carry, so one level
/// of nesting is always carried.
pub const MAX_QUARANTINE_HTML_BYTES: usize = 2 * 1024 * 1024;
/// Longest draft kind code: `edit:` or `reply:` plus an event id.
const MAX_QUARANTINE_KIND_BYTES: usize = 6 + MAX_MATRIX_ID_BYTES;
/// The carried columns are hex (exactly twice their bytes) plus reason, kind,
/// fingerprint and keys.
const QUARANTINE_JSON_BYTES: usize =
    512 + MAX_QUARANTINE_KIND_BYTES + 2 * MAX_QUARANTINE_HTML_BYTES;
const ENVELOPE_FIXED_JSON_BYTES: usize = 256;
pub const MAX_ENVELOPE_JSON_BYTES: usize = ENVELOPE_FIXED_JSON_BYTES
    + MAX_PENDING_RECORDS * MAX_RECORD_JSON_BYTES
    + THREAD_INDEX_JSON_BYTES
    + QUARANTINE_JSON_BYTES;
/// Upper bound of the hex envelope attribute; checked before any allocation.
pub const MAX_ENVELOPE_HEX_BYTES: usize = 2 * MAX_ENVELOPE_JSON_BYTES;

const STATE_ENQUEUING: &str = "enqueuing";
const STATE_QUEUED: &str = "queued";
/// Text the user chose to keep after a send was cancelled or judged unsent
/// while the composer already held newer text. No request belongs to it.
const STATE_KEPT: &str = "kept";
const FORMAT_PLAIN: &str = "plain";
const FORMAT_MARKDOWN: &str = "markdown";
const MENTION_USER: &str = "user";
const MENTION_ROOM: &str = "room";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecoveryCodecError {
    #[error("The saved send status is too large to read.")]
    TooLarge,
    #[error("The saved send status is malformed.")]
    Malformed,
    #[error("The saved send status was written by an unsupported version.")]
    UnsupportedVersion,
    #[error("The saved send status is not in canonical form.")]
    NonCanonical,
}

/// Bounded pending-send limits, worded for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PendingLimitError {
    #[error("Too many messages are already waiting or kept in this conversation.")]
    TooManyPending,
    #[error("A reply in this thread is still waiting to send.")]
    ThreadBusy,
    #[error("Too many threads in this room have replies waiting to send.")]
    ThreadIndexFull,
    #[error("This message could not be recorded for sending. Your text is retained.")]
    InvalidText,
}

/// Why a saved row was quarantined. Shown to the user as fixed wording only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineReason {
    /// Written by a Morpheus version this build does not understand.
    UnsupportedVersion,
    /// Recognized as Morpheus-written but not in a valid canonical shape.
    Malformed,
    /// Larger than this build reads; the row stays untouched in place.
    TooLarge,
    /// Readable text stored with a draft kind or attachments this build does
    /// not write; the row stays untouched in place so neither is lost.
    UnsupportedFields,
}

impl QuarantineReason {
    fn code(self) -> &'static str {
        match self {
            Self::UnsupportedVersion => "version",
            Self::Malformed => "malformed",
            Self::TooLarge => "too-large",
            Self::UnsupportedFields => "fields",
        }
    }

    fn parse(code: &str) -> Option<Self> {
        Some(match code {
            "version" => Self::UnsupportedVersion,
            "malformed" => Self::Malformed,
            "too-large" => Self::TooLarge,
            "fields" => Self::UnsupportedFields,
            _ => return None,
        })
    }
}

impl From<RecoveryCodecError> for QuarantineReason {
    fn from(error: RecoveryCodecError) -> Self {
        match error {
            RecoveryCodecError::TooLarge => Self::TooLarge,
            RecoveryCodecError::UnsupportedVersion => Self::UnsupportedVersion,
            RecoveryCodecError::Malformed | RecoveryCodecError::NonCanonical => Self::Malformed,
        }
    }
}

/// Opaque draft generation and record ownership nonce.
///
/// A fresh value is assigned on every user edit, including an edit back to an
/// identical text, so equality never follows from equal text. Values are 32
/// lowercase hex characters and are compared only for identity.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Generation(String);

impl Generation {
    pub fn fresh() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }

    /// Re-admit an opaque value handed out through `as_key`; anything else
    /// is rejected. Used only to route a native control back to its record.
    pub fn from_key(key: &str) -> Option<Self> {
        let candidate = Self(key.to_owned());
        candidate.is_valid().then_some(candidate)
    }

    /// The opaque value for routing within the process. Never shown as text.
    pub fn as_key(&self) -> &str {
        &self.0
    }

    fn is_valid(&self) -> bool {
        self.0.len() == 32
            && self
                .0
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }
}

impl fmt::Debug for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Generation([opaque])")
    }
}

pub fn valid_transaction_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_TRANSACTION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordMention {
    start: usize,
    end: usize,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

/// The complete typed content of one record: body, format and explicit
/// mentions, exactly as submitted. Validated through `ComposeText` on both
/// encode and decode so a record can never restore an invalid composition.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordText {
    format: String,
    body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mentions: Vec<RecordMention>,
}

impl RecordText {
    pub fn from_compose(text: &ComposeText) -> Result<Self, RecoveryCodecError> {
        text.validate().map_err(|_| RecoveryCodecError::Malformed)?;
        Ok(Self {
            format: match text.format {
                TextFormat::Plain => FORMAT_PLAIN,
                TextFormat::Markdown => FORMAT_MARKDOWN,
            }
            .to_owned(),
            body: text.body.clone(),
            mentions: text
                .mentions
                .iter()
                .map(|span| RecordMention {
                    start: span.range.start,
                    end: span.range.end,
                    kind: match span.target {
                        MentionTarget::User(_) => MENTION_USER,
                        MentionTarget::Room => MENTION_ROOM,
                    }
                    .to_owned(),
                    id: match &span.target {
                        MentionTarget::User(id) => Some(id.clone()),
                        MentionTarget::Room => None,
                    },
                })
                .collect(),
        })
    }

    pub fn to_compose(&self) -> Result<ComposeText, RecoveryCodecError> {
        let format = match self.format.as_str() {
            FORMAT_PLAIN => TextFormat::Plain,
            FORMAT_MARKDOWN => TextFormat::Markdown,
            _ => return Err(RecoveryCodecError::Malformed),
        };
        if self.body.len() > MAX_COMPOSE_BYTES || self.mentions.len() > MAX_MENTION_SPANS {
            return Err(RecoveryCodecError::TooLarge);
        }
        let mut mentions = Vec::with_capacity(self.mentions.len());
        for mention in &self.mentions {
            let target = match (mention.kind.as_str(), &mention.id) {
                (MENTION_USER, Some(id)) if id.len() <= MAX_MATRIX_ID_BYTES => {
                    MentionTarget::User(id.clone())
                }
                (MENTION_ROOM, None) => MentionTarget::Room,
                _ => return Err(RecoveryCodecError::Malformed),
            };
            mentions.push(MentionSpan {
                range: mention.start..mention.end,
                target,
            });
        }
        let text = ComposeText {
            body: self.body.clone(),
            format,
            mentions,
        };
        text.validate().map_err(|_| RecoveryCodecError::Malformed)?;
        Ok(text)
    }

    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    fn is_valid(&self) -> bool {
        self.to_compose().is_ok()
    }
}

impl fmt::Debug for RecordText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordText")
            .field("bytes", &self.body.len())
            .field("mentions", &self.mentions.len())
            .finish()
    }
}

/// One send the user asked for in a draft scope, with its complete text.
///
/// `Enqueuing` is written before the SDK assigns a transaction id; `Queued`
/// after the exact handle returned; `Kept` holds text the user chose to keep
/// after the request ended without evidence of delivery. A record found after
/// restart without an exact SDK echo is unconfirmed, never adopted or sent.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRecord {
    nonce: Generation,
    generation: Generation,
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transaction_id: Option<String>,
    text: RecordText,
}

impl PendingRecord {
    pub fn enqueuing(nonce: Generation, generation: Generation, text: RecordText) -> Self {
        Self {
            nonce,
            generation,
            state: STATE_ENQUEUING.to_owned(),
            transaction_id: None,
            text,
        }
    }

    pub fn queued(
        nonce: Generation,
        generation: Generation,
        transaction_id: &str,
        text: RecordText,
    ) -> Result<Self, RecoveryCodecError> {
        if !valid_transaction_id(transaction_id) {
            return Err(RecoveryCodecError::Malformed);
        }
        Ok(Self {
            nonce,
            generation,
            state: STATE_QUEUED.to_owned(),
            transaction_id: Some(transaction_id.to_owned()),
            text,
        })
    }

    pub fn kept(nonce: Generation, generation: Generation, text: RecordText) -> Self {
        Self {
            nonce,
            generation,
            state: STATE_KEPT.to_owned(),
            transaction_id: None,
            text,
        }
    }

    pub fn nonce(&self) -> &Generation {
        &self.nonce
    }

    pub fn generation(&self) -> &Generation {
        &self.generation
    }

    pub fn transaction_id(&self) -> Option<&str> {
        self.transaction_id.as_deref()
    }

    pub fn text(&self) -> &RecordText {
        &self.text
    }

    pub fn is_queued(&self) -> bool {
        self.state == STATE_QUEUED
    }

    pub fn is_kept(&self) -> bool {
        self.state == STATE_KEPT
    }

    /// A record that still represents a send the SDK may hold or complete.
    pub fn awaits_send(&self) -> bool {
        !self.is_kept()
    }

    fn is_valid(&self) -> bool {
        self.nonce.is_valid()
            && self.generation.is_valid()
            && self.text.is_valid()
            && match self.state.as_str() {
                STATE_ENQUEUING | STATE_KEPT => self.transaction_id.is_none(),
                STATE_QUEUED => self
                    .transaction_id
                    .as_deref()
                    .is_some_and(valid_transaction_id),
                _ => false,
            }
    }
}

impl fmt::Debug for PendingRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingRecord")
            .field("state", &self.state)
            .field("has_transaction", &self.transaction_id.is_some())
            .field("text", &self.text)
            .finish()
    }
}

/// Integrity check of a carried row: a 64-bit FNV-1a over its plain column,
/// HTML column and draft kind, recomputed on read so a damaged carry is
/// rejected as malformed. It is never an identity: a discard or replacement is
/// authorized only by a `RowRevision` and an exact comparison of the complete
/// stored row.
fn row_checksum(plain: &str, html: Option<&str>, kind: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    };
    feed(plain.as_bytes());
    feed(html.unwrap_or("").as_bytes());
    feed(&[u8::from(html.is_some())]);
    feed(kind.as_bytes());
    hash
}

/// Opaque, process-local identity of one exact stored row this build will not
/// rewrite unreviewed. It is minted when the row is read and survives only
/// while every later read returns the identical complete row (both text
/// columns, the draft kind and every attachment value); a row that differs in
/// any of those gets a new revision. A discard names the revision the user
/// reviewed, and the core compares the complete row once more before it
/// releases anything, so equal text never implies the same identity and a
/// 64-bit hash never stands in for the data. Never persisted, never shown.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RowRevision(Generation);

impl RowRevision {
    pub fn fresh() -> Self {
        Self(Generation::fresh())
    }
}

impl fmt::Debug for RowRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RowRevision([opaque])")
    }
}

/// A row this build will not rewrite: too large to carry, holding attachments
/// or a draft kind this build does not write, it stays untouched in the store
/// until the user discards exactly this row (named by its `RowRevision`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadOnlyRow {
    pub reason: QuarantineReason,
}

/// The complete original stored draft this build could not interpret, carried
/// verbatim: both text columns and the draft kind (new message, reply or edit
/// target), plus an integrity checksum. Hex keeps the bound exact. Rows with
/// attachments are not carried; they stay read-only in place. Two carried
/// rows are the same original only when every carried column is equal.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantinedRow {
    reason: String,
    plain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    html: Option<String>,
    kind: String,
    fingerprint: u64,
}

impl QuarantinedRow {
    /// `None` when the row cannot be carried completely (too large, or it
    /// holds attachments); the caller keeps the scope read-only instead so the
    /// row stays untouched in the store.
    pub fn new(
        reason: QuarantineReason,
        plain: &str,
        html: Option<&str>,
        kind: &str,
        has_attachments: bool,
    ) -> Option<Self> {
        let bytes = plain.len() + html.map_or(0, str::len);
        if has_attachments
            || bytes == 0
            || bytes > MAX_QUARANTINE_HTML_BYTES
            || kind.is_empty()
            || kind.len() > MAX_QUARANTINE_KIND_BYTES
            || !kind.is_ascii()
        {
            return None;
        }
        Some(Self {
            reason: reason.code().to_owned(),
            plain: to_hex(plain.as_bytes()),
            html: html.map(|html| to_hex(html.as_bytes())),
            kind: kind.to_owned(),
            fingerprint: row_checksum(plain, html, kind),
        })
    }

    pub fn reason(&self) -> QuarantineReason {
        QuarantineReason::parse(&self.reason).unwrap_or(QuarantineReason::Malformed)
    }

    /// The verbatim original plain-text column.
    pub fn original_plain(&self) -> Option<String> {
        String::from_utf8(from_hex(&self.plain)?).ok()
    }

    /// The verbatim original HTML column, when the row had one.
    pub fn original_html(&self) -> Option<String> {
        String::from_utf8(from_hex(self.html.as_deref()?)?).ok()
    }

    /// The original draft kind: `new`, `reply:<event id>` or `edit:<event id>`.
    pub fn original_kind(&self) -> &str {
        &self.kind
    }

    fn is_valid(&self) -> bool {
        if QuarantineReason::parse(&self.reason).is_none()
            || self.kind.is_empty()
            || self.kind.len() > MAX_QUARANTINE_KIND_BYTES
            || !self.kind.is_ascii()
            || !is_lower_hex(&self.plain)
            || !self.html.as_deref().is_none_or(is_lower_hex)
            || self.plain.len() + self.html.as_deref().map_or(0, str::len)
                > 2 * MAX_QUARANTINE_HTML_BYTES
        {
            return false;
        }
        let (Some(plain), html) = (self.original_plain(), self.original_html()) else {
            return false;
        };
        if self.html.is_some() != html.is_some() || plain.is_empty() && html.is_none() {
            return false;
        }
        self.fingerprint == row_checksum(&plain, html.as_deref(), &self.kind)
    }
}

impl fmt::Debug for QuarantinedRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuarantinedRow")
            .field("reason", &self.reason)
            .field(
                "bytes",
                &((self.plain.len() + self.html.as_deref().map_or(0, str::len)) / 2),
            )
            .finish()
    }
}

/// Metadata saved atomically with a draft scope's live text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftEnvelope {
    #[serde(rename = "v")]
    version: u32,
    generation: Generation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<PendingRecord>,
    /// Room-level index of thread roots whose own draft rows hold records,
    /// so discovery never has to enumerate hashed thread keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    threads: Vec<String>,
    /// A complete earlier row this build could not read, carried until the
    /// user explicitly discards it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quarantine: Option<QuarantinedRow>,
}

impl fmt::Debug for DraftEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraftEnvelope")
            .field("version", &self.version)
            .field("pending", &self.pending.len())
            .field("threads", &self.threads.len())
            .field("quarantine", &self.quarantine.is_some())
            .finish()
    }
}

impl DraftEnvelope {
    pub fn new(generation: Generation) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            generation,
            pending: Vec::new(),
            threads: Vec::new(),
            quarantine: None,
        }
    }

    pub fn fresh() -> Self {
        Self::new(Generation::fresh())
    }

    /// Rebuild an envelope from cached parts; rejected unless valid.
    pub fn restore(
        generation: Generation,
        pending: Vec<PendingRecord>,
        threads: Vec<String>,
        quarantine: Option<QuarantinedRow>,
    ) -> Result<Self, RecoveryCodecError> {
        let envelope = Self {
            version: ENVELOPE_VERSION,
            generation,
            pending,
            threads,
            quarantine,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    pub fn generation(&self) -> &Generation {
        &self.generation
    }

    pub fn set_generation(&mut self, generation: Generation) {
        self.generation = generation;
    }

    pub fn pending(&self) -> &[PendingRecord] {
        &self.pending
    }

    pub fn threads(&self) -> &[String] {
        &self.threads
    }

    pub fn quarantine(&self) -> Option<&QuarantinedRow> {
        self.quarantine.as_ref()
    }

    pub fn set_quarantine(&mut self, row: Option<QuarantinedRow>) {
        self.quarantine = row;
    }

    /// Explicit user discard of carried unreadable data.
    pub fn clear_quarantine(&mut self) -> bool {
        self.quarantine.take().is_some()
    }

    /// True when the row holds nothing but live text: no record of any
    /// state, no thread index and no quarantined original.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.threads.is_empty() && self.quarantine.is_none()
    }

    pub fn awaiting(&self) -> usize {
        self.pending
            .iter()
            .filter(|record| record.awaits_send())
            .count()
    }

    /// Record a send with its complete typed text before the SDK sees it.
    /// A thread scope accepts one awaiting reply; kept text does not block.
    pub fn begin_pending(
        &mut self,
        nonce: Generation,
        generation: Generation,
        text: &ComposeText,
        thread_scope: bool,
    ) -> Result<(), PendingLimitError> {
        if thread_scope && self.awaiting() > 0 {
            return Err(PendingLimitError::ThreadBusy);
        }
        if self.pending.len() >= MAX_PENDING_RECORDS
            || self.pending.iter().any(|record| record.nonce == nonce)
        {
            return Err(PendingLimitError::TooManyPending);
        }
        let text = RecordText::from_compose(text).map_err(|_| PendingLimitError::InvalidText)?;
        self.pending
            .push(PendingRecord::enqueuing(nonce, generation, text));
        Ok(())
    }

    /// Attach the exact SDK transaction id to an enqueuing record.
    pub fn mark_queued(&mut self, nonce: &Generation, transaction_id: &str) -> bool {
        if !valid_transaction_id(transaction_id)
            || self
                .pending
                .iter()
                .any(|record| record.transaction_id.as_deref() == Some(transaction_id))
        {
            return false;
        }
        let Some(record) = self
            .pending
            .iter_mut()
            .find(|record| record.nonce == *nonce && record.state == STATE_ENQUEUING)
        else {
            return false;
        };
        record.state = STATE_QUEUED.to_owned();
        record.transaction_id = Some(transaction_id.to_owned());
        true
    }

    pub fn find(&self, nonce: &Generation) -> Option<&PendingRecord> {
        self.pending.iter().find(|record| record.nonce == *nonce)
    }

    /// Remove exactly one record by nonce and exact transaction id (`None`
    /// for a record that never received one). Its text leaves with it.
    pub fn remove_pending(
        &mut self,
        nonce: &Generation,
        transaction_id: Option<&str>,
    ) -> Option<PendingRecord> {
        let index = self.pending.iter().position(|record| {
            record.nonce == *nonce && record.transaction_id.as_deref() == transaction_id
        })?;
        Some(self.pending.remove(index))
    }

    /// Turn an awaiting record into kept text: its request is over without
    /// evidence of delivery and the composer already holds newer text.
    pub fn keep_pending(&mut self, nonce: &Generation) -> bool {
        let Some(record) = self
            .pending
            .iter_mut()
            .find(|record| record.nonce == *nonce && record.awaits_send())
        else {
            return false;
        };
        record.state = STATE_KEPT.to_owned();
        record.transaction_id = None;
        true
    }

    pub fn index_thread(&mut self, root: &str) -> Result<bool, PendingLimitError> {
        if root.len() > MAX_MATRIX_ID_BYTES || EventId::parse(root).is_err() {
            return Err(PendingLimitError::ThreadIndexFull);
        }
        if self.threads.iter().any(|known| known == root) {
            return Ok(false);
        }
        if self.threads.len() >= MAX_THREAD_INDEX {
            return Err(PendingLimitError::ThreadIndexFull);
        }
        self.threads.push(root.to_owned());
        Ok(true)
    }

    pub fn unindex_thread(&mut self, root: &str) -> bool {
        let before = self.threads.len();
        self.threads.retain(|known| known != root);
        self.threads.len() != before
    }

    fn validate(&self) -> Result<(), RecoveryCodecError> {
        if self.version != ENVELOPE_VERSION {
            return Err(RecoveryCodecError::UnsupportedVersion);
        }
        if !self.generation.is_valid()
            || self.pending.len() > MAX_PENDING_RECORDS
            || self.threads.len() > MAX_THREAD_INDEX
        {
            return Err(RecoveryCodecError::Malformed);
        }
        for (index, record) in self.pending.iter().enumerate() {
            if !record.is_valid()
                || self.pending[..index].iter().any(|earlier| {
                    earlier.nonce == record.nonce
                        || (record.transaction_id.is_some()
                            && earlier.transaction_id == record.transaction_id)
                })
            {
                return Err(RecoveryCodecError::Malformed);
            }
        }
        for (index, root) in self.threads.iter().enumerate() {
            if root.len() > MAX_MATRIX_ID_BYTES
                || EventId::parse(root).is_err()
                || self.threads[..index].contains(root)
            {
                return Err(RecoveryCodecError::Malformed);
            }
        }
        if self.quarantine.as_ref().is_some_and(|row| !row.is_valid()) {
            return Err(RecoveryCodecError::Malformed);
        }
        Ok(())
    }

    /// Canonical lowercase hex of the JSON envelope.
    pub fn encode(&self) -> Result<String, RecoveryCodecError> {
        self.validate()?;
        let json = serde_json::to_vec(self).map_err(|_| RecoveryCodecError::Malformed)?;
        if json.len() > MAX_ENVELOPE_JSON_BYTES {
            return Err(RecoveryCodecError::TooLarge);
        }
        Ok(to_hex(&json))
    }

    /// Strict inverse of `encode`: bounded before allocation, lowercase hex
    /// only, unknown fields rejected, every identifier and text validated,
    /// and the result must re-encode to exactly the input.
    pub fn decode(hex: &str) -> Result<Self, RecoveryCodecError> {
        if hex.len() > MAX_ENVELOPE_HEX_BYTES {
            return Err(RecoveryCodecError::TooLarge);
        }
        let bytes = from_hex(hex).ok_or(RecoveryCodecError::Malformed)?;
        let envelope: Self =
            serde_json::from_slice(&bytes).map_err(|_| RecoveryCodecError::Malformed)?;
        if envelope.version != ENVELOPE_VERSION {
            return Err(RecoveryCodecError::UnsupportedVersion);
        }
        envelope.validate()?;
        if envelope.encode()? != hex {
            return Err(RecoveryCodecError::NonCanonical);
        }
        Ok(envelope)
    }
}

/// Why a scope refuses new sends and, when read-only, saves. The revision
/// names the exact original row so a discard applies to that row only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeBlock {
    /// Saved data this build cannot read is carried in the row.
    Quarantined { revision: RowRevision },
    /// The saved row cannot be carried (too large, attachments, or a draft
    /// kind this build does not write); it is never written.
    ReadOnly { revision: RowRevision },
}

impl ScopeBlock {
    pub fn revision(&self) -> &RowRevision {
        match self {
            Self::Quarantined { revision } | Self::ReadOnly { revision } => revision,
        }
    }
}

/// Why a save was refused: it would have erased or rewritten metadata the
/// caller did not own. The row keeps its last valid data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveRejected {
    ReadOnly,
    QuarantineDropped,
    RecordDropped,
    RecordRegressed,
    RecordReplayed,
}

/// Where a record this process created stands with respect to its single
/// enqueue capability. Only a record that has not crossed the SDK boundary
/// may be abandoned without an exact release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FreshState {
    /// Saved as enqueuing; no capability issued yet.
    Saved,
    /// The capability was issued to the controller and not yet used.
    Granted,
    /// The capability was consumed at the SDK boundary: a request may exist
    /// even though no transaction id is in the row yet.
    Consumed,
}

/// A record this process created: the selection epoch it belongs to and the
/// state of its one enqueue capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FreshIntent {
    epoch: u64,
    state: FreshState,
}

/// Authoritative per-scope recovery state, mirrored from the last encrypted
/// row read or written. Every save is checked against it: a record leaves or
/// changes state only through an exact, authorized operation of this process,
/// never through a stale snapshot or a replayed restored record; a carried
/// original is replaced only by the exact same original or after the user
/// discarded exactly it.
#[derive(Clone, Default)]
pub struct ScopeRecovery {
    records: Vec<PendingRecord>,
    threads: Vec<String>,
    quarantine: Option<QuarantinedRow>,
    block: Option<ScopeBlock>,
    /// Records an exact resolution released; the next save may drop or keep
    /// them. Pruned once they leave the row.
    released: HashSet<Generation>,
    /// Records created by this process under a selection epoch. Only these
    /// may receive a transaction id, and each gets exactly one enqueue
    /// capability; restored records never do. Only one whose capability was
    /// never consumed may be abandoned without a resolution: once the SDK
    /// boundary was crossed, a request may exist and the record stays until
    /// its exact release.
    fresh: HashMap<Generation, FreshIntent>,
    /// Issued, not yet consumed enqueue capabilities.
    grants: HashSet<Generation>,
    /// The user explicitly discarded the carried or read-only original.
    quarantine_released: bool,
}

impl fmt::Debug for ScopeRecovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeRecovery")
            .field("records", &self.records.len())
            .field("threads", &self.threads.len())
            .field("block", &self.block)
            .field("released", &self.released.len())
            .field("fresh", &self.fresh.len())
            .field("grants", &self.grants.len())
            .finish()
    }
}

impl ScopeRecovery {
    pub fn from_restored(restored: &RestoredCompose) -> Self {
        let mut scope = Self::default();
        scope.apply_loaded(restored);
        scope
    }

    pub fn records(&self) -> &[PendingRecord] {
        &self.records
    }

    pub fn threads(&self) -> &[String] {
        &self.threads
    }

    pub fn block(&self) -> Option<ScopeBlock> {
        self.block.clone()
    }

    /// The carried original, when the block is a carried quarantine.
    pub fn quarantine(&self) -> Option<&QuarantinedRow> {
        self.quarantine.as_ref()
    }

    pub fn find(&self, nonce: &Generation) -> Option<&PendingRecord> {
        self.records.iter().find(|record| record.nonce == *nonce)
    }

    pub fn awaiting(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.awaits_send())
            .count()
    }

    /// Nothing worth keeping in the registry: no record, no index, no block.
    pub fn is_idle(&self) -> bool {
        self.records.is_empty() && self.threads.is_empty() && self.block.is_none()
    }

    /// The row was read: it is the truth for records, index, carry and block.
    /// Process-local ownership survives only for records still present. A
    /// carried original that is exactly the one already held keeps its
    /// revision, so a navigation reload does not invalidate a review in
    /// progress; any other carry, and every read-only row, takes the revision
    /// the caller minted or reconciled for the complete row it read.
    pub fn apply_loaded(&mut self, restored: &RestoredCompose) {
        self.records = restored.envelope.pending().to_vec();
        self.threads = restored.envelope.threads().to_vec();
        let carry = restored.envelope.quarantine().cloned();
        let same_carry = carry.is_some()
            && carry == self.quarantine
            && matches!(self.block, Some(ScopeBlock::Quarantined { .. }));
        let revision = || restored.revision.clone().unwrap_or_else(RowRevision::fresh);
        self.block = match (restored.read_only, &carry) {
            (Some(_), _) => Some(ScopeBlock::ReadOnly {
                revision: revision(),
            }),
            (None, Some(_)) if same_carry => self.block.clone(),
            (None, Some(_)) => Some(ScopeBlock::Quarantined {
                revision: revision(),
            }),
            (None, None) => None,
        };
        self.quarantine = carry;
        self.quarantine_released = false;
        self.prune();
    }

    /// Thread roots the incoming snapshot would drop from the room index. The
    /// caller must prove each dropped thread holds no record, kept text or
    /// unreadable data before writing; otherwise the thread row is orphaned
    /// from restart discovery.
    pub fn dropped_threads(&self, incoming: &DraftEnvelope) -> Vec<String> {
        self.threads
            .iter()
            .filter(|root| !incoming.threads().contains(root))
            .cloned()
            .collect()
    }

    /// Would writing `incoming` over the current row erase or rewrite
    /// anything the caller does not own?
    pub fn check_save(&self, incoming: &DraftEnvelope) -> Result<(), SaveRejected> {
        match self.block {
            Some(ScopeBlock::ReadOnly { .. }) if !self.quarantine_released => {
                return Err(SaveRejected::ReadOnly);
            }
            Some(ScopeBlock::Quarantined { .. })
                if incoming.quarantine() != self.quarantine.as_ref()
                    && !(self.quarantine_released && incoming.quarantine().is_none()) =>
            {
                // Only the exact carried original may ride along; a different
                // or stale carry would replace unreadable data unreviewed, and
                // after the explicit discard the carry may only be dropped.
                return Err(SaveRejected::QuarantineDropped);
            }
            None if incoming.quarantine().is_some() => {
                return Err(SaveRejected::QuarantineDropped);
            }
            _ => {}
        }
        for current in &self.records {
            let Some(next) = incoming.find(&current.nonce) else {
                // A fresh enqueuing record may be abandoned only while its
                // capability was never consumed; after the SDK boundary the
                // request may exist and only an exact release lets it go.
                let abandoned_fresh = !current.is_queued()
                    && !current.is_kept()
                    && self
                        .fresh
                        .get(&current.nonce)
                        .is_some_and(|intent| intent.state != FreshState::Consumed);
                if self.released.contains(&current.nonce) || abandoned_fresh {
                    continue;
                }
                return Err(SaveRejected::RecordDropped);
            };
            if next.text != current.text || next.generation != current.generation {
                return Err(SaveRejected::RecordRegressed);
            }
            let same_state = next.state == current.state;
            let same_transaction = next.transaction_id == current.transaction_id;
            if same_state && same_transaction {
                continue;
            }
            if current.is_kept() {
                return Err(SaveRejected::RecordRegressed);
            }
            if next.is_kept() {
                if self.released.contains(&current.nonce) {
                    continue;
                }
                return Err(SaveRejected::RecordDropped);
            }
            if !current.is_queued() && next.is_queued() {
                if self.fresh.contains_key(&current.nonce) {
                    continue;
                }
                return Err(SaveRejected::RecordReplayed);
            }
            return Err(SaveRejected::RecordRegressed);
        }
        for next in incoming.pending() {
            if self.find(&next.nonce).is_none() && (next.is_queued() || next.is_kept()) {
                return Err(SaveRejected::RecordReplayed);
            }
        }
        Ok(())
    }

    /// The row was written with `incoming` after `check_save` passed, under
    /// the given selection epoch.
    pub fn apply_saved(&mut self, incoming: &DraftEnvelope, epoch: u64) {
        for next in incoming.pending() {
            if self.find(&next.nonce).is_none() {
                self.fresh.insert(
                    next.nonce.clone(),
                    FreshIntent {
                        epoch,
                        state: FreshState::Saved,
                    },
                );
            }
        }
        self.records = incoming.pending().to_vec();
        self.threads = incoming.threads().to_vec();
        // `check_save` admits only the exact held carry or, after the discard,
        // none: the carried original and its revision are unchanged by a save
        // of the live text around it.
        let carry = incoming.quarantine().cloned();
        self.block = match (&carry, &self.block) {
            (None, _) => None,
            (Some(next), Some(ScopeBlock::Quarantined { revision }))
                if Some(next) == self.quarantine.as_ref() =>
            {
                Some(ScopeBlock::Quarantined {
                    revision: revision.clone(),
                })
            }
            (Some(_), _) => Some(ScopeBlock::Quarantined {
                revision: RowRevision::fresh(),
            }),
        };
        self.quarantine = carry;
        self.quarantine_released = false;
        self.prune();
    }

    /// The row was deleted.
    pub fn apply_cleared(&mut self) {
        self.records.clear();
        self.threads.clear();
        self.quarantine = None;
        self.block = None;
        self.quarantine_released = false;
        self.prune();
    }

    /// An exact, authorized resolution: the record may now leave the row or
    /// become kept text. Exact nonce and transaction id (none for a record
    /// that never received one) are both required.
    pub fn release(&mut self, nonce: &Generation, transaction_id: Option<&str>) -> bool {
        let owned = self
            .find(nonce)
            .is_some_and(|record| record.transaction_id.as_deref() == transaction_id);
        if owned {
            self.released.insert(nonce.clone());
        }
        owned
    }

    /// The user explicitly discarded exactly this original, named by the
    /// revision they reviewed. The caller has compared the complete stored
    /// row against that original first; a different revision releases nothing.
    pub fn release_quarantine(&mut self, revision: &RowRevision) -> bool {
        if self.block.as_ref().map(ScopeBlock::revision) != Some(revision) {
            return false;
        }
        self.quarantine_released = true;
        true
    }

    /// Issue the single enqueue capability for a record this process created
    /// under the same selection epoch and already saved as enqueuing. It is
    /// issued once; restored records and other epochs never qualify.
    pub fn grant(&mut self, nonce: &Generation, epoch: u64) -> bool {
        let enqueuing = self
            .find(nonce)
            .is_some_and(|record| !record.is_queued() && !record.is_kept());
        let Some(intent) = self.fresh.get_mut(nonce) else {
            return false;
        };
        if !enqueuing || intent.epoch != epoch || intent.state != FreshState::Saved {
            return false;
        }
        intent.state = FreshState::Granted;
        self.grants.insert(nonce.clone());
        true
    }

    /// Consume the capability at the SDK boundary under the same selection
    /// epoch. False means no fresh intent of this selection owns this enqueue.
    /// From here the record cannot leave the row without an exact release,
    /// even before its transaction id is persisted.
    pub fn consume_grant(&mut self, nonce: &Generation, epoch: u64) -> bool {
        let Some(intent) = self.fresh.get_mut(nonce) else {
            return false;
        };
        if intent.epoch != epoch || !self.grants.remove(nonce) {
            return false;
        }
        intent.state = FreshState::Consumed;
        true
    }

    /// A new selection epoch: every capability and every fresh ownership of
    /// the old one dies. Its records stay as restored records would.
    pub fn revoke_process_ownership(&mut self) {
        self.grants.clear();
        self.fresh.clear();
    }

    fn prune(&mut self) {
        let present: HashSet<Generation> = self
            .records
            .iter()
            .map(|record| record.nonce.clone())
            .collect();
        self.released.retain(|nonce| present.contains(nonce));
        self.fresh.retain(|nonce, _| present.contains(nonce));
        self.grants.retain(|nonce| {
            self.records
                .iter()
                .any(|record| record.nonce == *nonce && !record.is_queued() && !record.is_kept())
        });
    }
}

fn is_lower_hex(value: &str) -> bool {
    value.len() % 2 == 0
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn from_hex(value: &str) -> Option<Vec<u8>> {
    if !is_lower_hex(value) {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("checked by is_lower_hex"),
    };
    Some(
        value
            .as_bytes()
            .chunks(2)
            .map(|pair| (digit(pair[0]) << 4) | digit(pair[1]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXN: &str = "0123456789abcdef0123456789abcdef";

    fn typed(body: &str) -> ComposeText {
        ComposeText::plain(body)
    }

    fn mentioned() -> ComposeText {
        ComposeText::plain("ping @casey:example.org now")
            .insert_mention(5..23, MentionTarget::User("@casey:example.org".into()))
            .unwrap()
            .0
            .with_format(TextFormat::Markdown)
    }

    fn queued_envelope() -> (DraftEnvelope, Generation) {
        let mut envelope = DraftEnvelope::fresh();
        let nonce = Generation::fresh();
        envelope
            .begin_pending(
                nonce.clone(),
                envelope.generation().clone(),
                &mentioned(),
                false,
            )
            .unwrap();
        assert!(envelope.mark_queued(&nonce, TXN));
        (envelope, nonce)
    }

    #[test]
    fn generations_are_opaque_fresh_and_never_equal_by_text() {
        let first = Generation::fresh();
        let second = Generation::fresh();
        assert_ne!(first, second);
        assert!(first.is_valid());
        assert_eq!(format!("{first:?}"), "Generation([opaque])");
        assert_eq!(Generation::from_key(first.as_key()), Some(first));
        assert!(Generation::from_key(&"ABCDEF".repeat(5)).is_none());
        assert!(Generation::from_key(&"0".repeat(31)).is_none());
    }

    #[test]
    fn envelope_round_trips_canonically_and_rejects_noncanonical_forms() {
        let (envelope, _) = queued_envelope();
        let hex = envelope.encode().unwrap();
        assert!(hex.len() <= MAX_ENVELOPE_HEX_BYTES);
        assert_eq!(DraftEnvelope::decode(&hex).unwrap(), envelope);
        assert_eq!(
            DraftEnvelope::decode(&hex.to_uppercase()),
            Err(RecoveryCodecError::Malformed)
        );
        assert_eq!(
            DraftEnvelope::decode(&hex[..hex.len() - 1]),
            Err(RecoveryCodecError::Malformed)
        );
        let mut json = from_hex(&hex).unwrap();
        json.push(b' ');
        assert_eq!(
            DraftEnvelope::decode(&to_hex(&json)),
            Err(RecoveryCodecError::NonCanonical)
        );
        let unknown =
            to_hex(br#"{"v":2,"generation":"00000000000000000000000000000000","extra":1}"#);
        assert_eq!(
            DraftEnvelope::decode(&unknown),
            Err(RecoveryCodecError::Malformed)
        );
        for version in [1, 3] {
            let other = to_hex(
                format!(r#"{{"v":{version},"generation":"00000000000000000000000000000000"}}"#)
                    .as_bytes(),
            );
            assert_eq!(
                DraftEnvelope::decode(&other),
                Err(RecoveryCodecError::UnsupportedVersion)
            );
        }
        let oversized = "00".repeat(MAX_ENVELOPE_HEX_BYTES / 2 + 1);
        assert_eq!(
            DraftEnvelope::decode(&oversized),
            Err(RecoveryCodecError::TooLarge)
        );
        // A record whose text would not validate as a composition is rejected
        // even when the JSON is well formed.
        let bad_mention = to_hex(
            format!(
                r#"{{"v":2,"generation":"{g}","pending":[{{"nonce":"{g}","generation":"{g}","state":"kept","text":{{"format":"plain","body":"hi","mentions":[{{"start":0,"end":9,"kind":"user","id":"@a:b.c"}}]}}}}]}}"#,
                g = "0".repeat(32)
            )
            .as_bytes(),
        );
        assert_eq!(
            DraftEnvelope::decode(&bad_mention),
            Err(RecoveryCodecError::Malformed)
        );
    }

    #[test]
    fn every_record_keeps_its_own_typed_text_through_later_records_and_decoding() {
        let mut envelope = DraftEnvelope::fresh();
        let first = Generation::fresh();
        let second = Generation::fresh();
        let first_generation = envelope.generation().clone();
        envelope
            .begin_pending(first.clone(), first_generation.clone(), &mentioned(), false)
            .unwrap();
        assert!(envelope.mark_queued(&first, TXN));
        // A later edit to the same text is a new generation; the record keeps
        // the generation and text it was created from.
        envelope.set_generation(Generation::fresh());
        envelope
            .begin_pending(
                second.clone(),
                envelope.generation().clone(),
                &typed("second"),
                false,
            )
            .unwrap();
        let decoded = DraftEnvelope::decode(&envelope.encode().unwrap()).unwrap();
        let records = decoded.pending();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].text().to_compose().unwrap(), mentioned());
        assert_eq!(records[0].generation(), &first_generation);
        assert_ne!(records[0].generation(), decoded.generation());
        assert_eq!(records[1].text().to_compose().unwrap(), typed("second"));
        // Out-of-order resolution: the newer record leaves first; the older
        // one, with its text, stays and can become kept text.
        let mut decoded = decoded;
        assert!(decoded.remove_pending(&second, None).is_some());
        assert!(decoded.remove_pending(&first, None).is_none());
        assert!(decoded.keep_pending(&first));
        assert!(!decoded.keep_pending(&first), "kept text is not a send");
        let kept = decoded.find(&first).unwrap();
        assert!(kept.is_kept() && !kept.awaits_send() && kept.transaction_id().is_none());
        assert_eq!(kept.text().to_compose().unwrap(), mentioned());
        assert_eq!(decoded.awaiting(), 0);
        // Kept text does not block a thread's one awaiting reply.
        let mut thread = DraftEnvelope::fresh();
        let reply = Generation::fresh();
        thread
            .begin_pending(
                reply.clone(),
                thread.generation().clone(),
                &typed("a"),
                true,
            )
            .unwrap();
        assert_eq!(
            thread.begin_pending(
                Generation::fresh(),
                thread.generation().clone(),
                &typed("b"),
                true
            ),
            Err(PendingLimitError::ThreadBusy)
        );
        assert!(thread.keep_pending(&reply));
        thread
            .begin_pending(
                Generation::fresh(),
                thread.generation().clone(),
                &typed("b"),
                true,
            )
            .unwrap();
        assert_eq!(
            DraftEnvelope::decode(&thread.encode().unwrap()).unwrap(),
            thread
        );
    }

    #[test]
    fn pending_records_are_bounded_unique_and_removed_only_by_exact_identity() {
        let mut envelope = DraftEnvelope::fresh();
        let generation = envelope.generation().clone();
        let mut nonces = Vec::new();
        for index in 0..MAX_PENDING_RECORDS {
            let nonce = Generation::fresh();
            envelope
                .begin_pending(
                    nonce.clone(),
                    generation.clone(),
                    &typed(&format!("m{index}")),
                    false,
                )
                .unwrap();
            nonces.push(nonce);
        }
        assert_eq!(
            envelope.begin_pending(Generation::fresh(), generation.clone(), &typed("x"), false),
            Err(PendingLimitError::TooManyPending)
        );
        assert!(envelope.mark_queued(&nonces[0], "aaaa"));
        assert!(
            !envelope.mark_queued(&nonces[1], "aaaa"),
            "duplicate transaction"
        );
        assert!(!envelope.mark_queued(&nonces[0], "bbbb"), "already queued");
        assert!(!envelope.mark_queued(&nonces[1], "bad id!"));
        assert!(envelope.remove_pending(&nonces[0], None).is_none());
        assert!(envelope.remove_pending(&nonces[0], Some("other")).is_none());
        assert!(envelope.remove_pending(&nonces[1], Some("aaaa")).is_none());
        assert!(envelope.remove_pending(&nonces[0], Some("aaaa")).is_some());
        assert!(envelope.remove_pending(&nonces[1], None).is_some());
        assert_eq!(envelope.pending().len(), MAX_PENDING_RECORDS - 2);
        let oversized = ComposeText::plain("x".repeat(MAX_COMPOSE_BYTES + 1));
        assert_eq!(
            envelope.begin_pending(Generation::fresh(), generation, &oversized, false),
            Err(PendingLimitError::InvalidText)
        );
    }

    #[test]
    fn size_limits_are_derived_from_the_supported_message_and_hold_at_the_extremes() {
        // Largest record: a full body whose every byte expands sixfold in
        // JSON, plus the maximum number of mentions with the longest user ids.
        let user = format!("@{}:example.org", "a".repeat(MAX_MATRIX_ID_BYTES - 13));
        assert_eq!(user.len(), MAX_MATRIX_ID_BYTES);
        let mut body = String::new();
        let mut spans = Vec::new();
        for _ in 0..MAX_MENTION_SPANS {
            let start = body.len();
            body.push_str(&user);
            spans.push(start..body.len());
            body.push('\n');
        }
        while body.len() < MAX_COMPOSE_BYTES {
            body.push('\u{1}');
        }
        let mut text = ComposeText::plain(body);
        for range in spans {
            text = text
                .insert_mention(range, MentionTarget::User(user.clone()))
                .unwrap()
                .0;
        }
        assert_eq!(text.body.len(), MAX_COMPOSE_BYTES);
        let record = RecordText::from_compose(&text).unwrap();
        let encoded = serde_json::to_vec(&PendingRecord::kept(
            Generation::fresh(),
            Generation::fresh(),
            record,
        ))
        .unwrap();
        assert!(encoded.len() <= MAX_RECORD_JSON_BYTES, "{}", encoded.len());
        // Largest envelope: every record at that size, a full thread index and
        // the largest carried quarantine still encode within the bounds.
        let mut envelope = DraftEnvelope::fresh();
        for _ in 0..MAX_PENDING_RECORDS {
            envelope
                .begin_pending(
                    Generation::fresh(),
                    envelope.generation().clone(),
                    &text,
                    false,
                )
                .unwrap();
        }
        for index in 0..MAX_THREAD_INDEX {
            let suffix = format!("{index:02}:example.org");
            let root = format!(
                "${}{suffix}",
                "r".repeat(MAX_MATRIX_ID_BYTES - 1 - suffix.len())
            );
            assert_eq!(root.len(), MAX_MATRIX_ID_BYTES);
            envelope.index_thread(&root).unwrap();
        }
        let carried = "\u{1}".repeat(MAX_QUARANTINE_HTML_BYTES / 2);
        let kind = format!(
            "reply:${}:example.org",
            "e".repeat(MAX_MATRIX_ID_BYTES - 13)
        );
        envelope.set_quarantine(QuarantinedRow::new(
            QuarantineReason::Malformed,
            &carried,
            Some(&carried),
            &kind,
            false,
        ));
        assert!(envelope.quarantine().is_some());
        let hex = envelope.encode().unwrap();
        assert!(hex.len() <= MAX_ENVELOPE_HEX_BYTES, "{}", hex.len());
        assert_eq!(DraftEnvelope::decode(&hex).unwrap(), envelope);
        let too_big = "x".repeat(MAX_QUARANTINE_HTML_BYTES + 1);
        assert!(
            QuarantinedRow::new(QuarantineReason::TooLarge, &too_big, None, "new", false).is_none()
        );
    }

    #[test]
    fn thread_index_is_bounded_validated_and_deduplicated() {
        let mut envelope = DraftEnvelope::fresh();
        assert!(envelope.index_thread("$root-one:example.org").unwrap());
        assert!(!envelope.index_thread("$root-one:example.org").unwrap());
        assert_eq!(
            envelope.index_thread("not an event id"),
            Err(PendingLimitError::ThreadIndexFull)
        );
        for index in 1..MAX_THREAD_INDEX {
            envelope
                .index_thread(&format!("$root-{index}:example.org"))
                .unwrap();
        }
        assert_eq!(
            envelope.index_thread("$overflow:example.org"),
            Err(PendingLimitError::ThreadIndexFull)
        );
        assert!(envelope.unindex_thread("$root-one:example.org"));
        assert!(!envelope.unindex_thread("$root-one:example.org"));
        assert!(!envelope.is_empty());
        let hex = envelope.encode().unwrap();
        assert_eq!(DraftEnvelope::decode(&hex).unwrap(), envelope);
    }

    #[test]
    fn quarantined_rows_are_carried_verbatim_and_cleared_only_explicitly() {
        let original =
            "<span data-morpheus-compose=\"7\" data-format=\"plain\">kept \u{1F600} text</span>";
        let row = QuarantinedRow::new(
            QuarantineReason::UnsupportedVersion,
            "kept text",
            Some(original),
            "new",
            false,
        )
        .unwrap();
        assert_eq!(row.reason(), QuarantineReason::UnsupportedVersion);
        assert_eq!(row.original_html().as_deref(), Some(original));
        assert_eq!(row.original_plain().as_deref(), Some("kept text"));
        let mut envelope = DraftEnvelope::fresh();
        envelope.set_quarantine(Some(row.clone()));
        assert!(!envelope.is_empty());
        let hex = envelope.encode().unwrap();
        let decoded = DraftEnvelope::decode(&hex).unwrap();
        assert_eq!(decoded.quarantine(), Some(&row));
        assert_eq!(
            decoded.quarantine().unwrap().original_html().as_deref(),
            Some(original)
        );
        // Records and the index coexist with the carry; a later generation
        // keeps it.
        let mut decoded = decoded;
        decoded.set_generation(Generation::fresh());
        decoded
            .begin_pending(
                Generation::fresh(),
                decoded.generation().clone(),
                &typed("new"),
                false,
            )
            .unwrap();
        let again = DraftEnvelope::decode(&decoded.encode().unwrap()).unwrap();
        assert_eq!(again.quarantine(), Some(&row));
        assert!(decoded.clear_quarantine());
        assert!(!decoded.clear_quarantine());
        // Invalid carries never pass validation.
        let forged = to_hex(
            format!(
                r#"{{"v":2,"generation":"{g}","quarantine":{{"reason":"other","plain":"ab","kind":"new","fingerprint":1}}}}"#,
                g = "0".repeat(32)
            )
            .as_bytes(),
        );
        assert_eq!(
            DraftEnvelope::decode(&forged),
            Err(RecoveryCodecError::Malformed)
        );
        let odd_hex = to_hex(
            format!(
                r#"{{"v":2,"generation":"{g}","quarantine":{{"reason":"malformed","plain":"abc","kind":"new","fingerprint":1}}}}"#,
                g = "0".repeat(32)
            )
            .as_bytes(),
        );
        assert_eq!(
            DraftEnvelope::decode(&odd_hex),
            Err(RecoveryCodecError::Malformed)
        );
    }

    fn restored_with(envelope: DraftEnvelope, read_only: Option<ReadOnlyRow>) -> RestoredCompose {
        let blocked = read_only.is_some() || envelope.quarantine().is_some();
        RestoredCompose {
            text: typed("live"),
            discarded_rich_intent: false,
            envelope,
            read_only,
            revision: blocked.then(RowRevision::fresh),
        }
    }

    const EPOCH: u64 = 7;

    #[test]
    fn saves_cannot_drop_or_rewrite_records_without_an_exact_resolution() {
        // A row from an earlier process: one queued and one enqueuing record.
        let mut row = DraftEnvelope::fresh();
        let queued = Generation::fresh();
        let lost = Generation::fresh();
        row.begin_pending(queued.clone(), row.generation().clone(), &typed("q"), false)
            .unwrap();
        assert!(row.mark_queued(&queued, TXN));
        row.begin_pending(lost.clone(), row.generation().clone(), &typed("l"), false)
            .unwrap();
        let mut scope = ScopeRecovery::from_restored(&restored_with(row.clone(), None));
        // A plain text save or clear that keeps every record is fine.
        let mut edited = row.clone();
        edited.set_generation(Generation::fresh());
        assert_eq!(scope.check_save(&edited), Ok(()));
        // A stale snapshot without the queued record is refused.
        let mut stale = row.clone();
        assert!(stale.remove_pending(&queued, Some(TXN)).is_some());
        assert_eq!(scope.check_save(&stale), Err(SaveRejected::RecordDropped));
        // Dropping a restored enqueuing record is refused too: it is not fresh.
        let mut dropped = row.clone();
        assert!(dropped.remove_pending(&lost, None).is_some());
        assert_eq!(scope.check_save(&dropped), Err(SaveRejected::RecordDropped));
        // A restored enqueuing record can never be given a transaction id.
        let mut replay = row.clone();
        assert!(replay.mark_queued(&lost, "ffff"));
        assert_eq!(scope.check_save(&replay), Err(SaveRejected::RecordReplayed));
        assert!(
            !scope.grant(&lost, EPOCH),
            "restored records get no enqueue grant"
        );
        // Keeping a record is a resolution and needs the release as well.
        let mut kept = row.clone();
        assert!(kept.keep_pending(&queued));
        assert_eq!(scope.check_save(&kept), Err(SaveRejected::RecordDropped));
        // A release needs the exact transaction id.
        assert!(!scope.release(&queued, None));
        assert!(!scope.release(&queued, Some("other")));
        assert!(scope.release(&queued, Some(TXN)));
        assert_eq!(scope.check_save(&kept), Ok(()));
        assert_eq!(scope.check_save(&stale), Ok(()));
        // Text and generation of an existing record are immutable.
        let mut rewritten = DraftEnvelope::fresh();
        rewritten
            .begin_pending(queued.clone(), Generation::fresh(), &typed("q"), false)
            .unwrap();
        assert!(rewritten.mark_queued(&queued, TXN));
        rewritten
            .begin_pending(
                lost.clone(),
                row.pending()[1].generation().clone(),
                &typed("l"),
                false,
            )
            .unwrap();
        assert_eq!(
            scope.check_save(&rewritten),
            Err(SaveRejected::RecordRegressed)
        );
        // After the save the released record is gone and the release pruned.
        scope.apply_saved(&stale, EPOCH);
        assert_eq!(scope.records().len(), 1);
        assert!(!scope.release(&queued, Some(TXN)));
        // Kept text never goes back to a send.
        let mut kept_scope = ScopeRecovery::from_restored(&restored_with(kept.clone(), None));
        let mut revived = kept.clone();
        revived.remove_pending(&queued, None);
        revived
            .begin_pending(
                queued.clone(),
                kept.pending()[0].generation().clone(),
                &typed("q"),
                false,
            )
            .unwrap();
        assert_eq!(
            kept_scope.check_save(&revived),
            Err(SaveRejected::RecordRegressed)
        );
        assert!(
            kept_scope.release(&queued, None),
            "kept text releases with no transaction"
        );
        let mut gone = kept.clone();
        gone.remove_pending(&queued, None);
        assert_eq!(kept_scope.check_save(&gone), Ok(()));
    }

    #[test]
    fn fresh_records_follow_the_one_use_grant_lifecycle_bound_to_the_selection_epoch() {
        let mut scope = ScopeRecovery::default();
        let mut row = DraftEnvelope::fresh();
        let nonce = Generation::fresh();
        row.begin_pending(
            nonce.clone(),
            row.generation().clone(),
            &typed("new"),
            false,
        )
        .unwrap();
        // A new enqueuing record is a fresh intent; a new queued or kept one
        // appearing from nowhere is not.
        assert_eq!(scope.check_save(&row), Ok(()));
        let mut forged = DraftEnvelope::fresh();
        forged
            .begin_pending(
                Generation::fresh(),
                forged.generation().clone(),
                &typed("x"),
                false,
            )
            .unwrap();
        let forged_nonce = forged.pending()[0].nonce().clone();
        assert!(forged.mark_queued(&forged_nonce, "aaaa"));
        assert_eq!(scope.check_save(&forged), Err(SaveRejected::RecordReplayed));
        scope.apply_saved(&row, EPOCH);
        assert!(
            !scope.consume_grant(&nonce, EPOCH),
            "no grant before it is issued"
        );
        assert!(
            !scope.grant(&nonce, EPOCH + 1),
            "another selection epoch cannot ask"
        );
        assert!(scope.grant(&nonce, EPOCH));
        assert!(!scope.grant(&nonce, EPOCH), "issued once");
        assert!(
            !scope.consume_grant(&nonce, EPOCH + 1),
            "another epoch cannot consume"
        );
        assert!(scope.consume_grant(&nonce, EPOCH));
        assert!(!scope.consume_grant(&nonce, EPOCH), "single use");
        // Consumed is consumed: the record is still enqueuing (the id is not
        // yet persisted) and still cannot be granted again.
        assert!(
            !scope.grant(&nonce, EPOCH),
            "no regrant while the record stays enqueuing"
        );
        // The fresh record may now take its transaction id, once.
        let mut queued = row.clone();
        assert!(queued.mark_queued(&nonce, TXN));
        assert_eq!(scope.check_save(&queued), Ok(()));
        scope.apply_saved(&queued, EPOCH);
        assert!(
            !scope.grant(&nonce, EPOCH),
            "a queued record needs no grant"
        );
        assert_eq!(
            scope.check_save(&row),
            Err(SaveRejected::RecordRegressed),
            "a queued record cannot go back to enqueuing"
        );
        // Abandoning is allowed only while the fresh record never crossed the
        // SDK boundary: before the grant, and after the grant while it is
        // unused. Once consumed, the record stays until its exact release,
        // even though no transaction id is in the row yet.
        let empty = DraftEnvelope::fresh();
        let mut before_grant = ScopeRecovery::default();
        before_grant.apply_saved(&row, EPOCH);
        assert_eq!(
            before_grant.check_save(&empty),
            Ok(()),
            "never granted: the caller may abandon"
        );
        let mut granted = ScopeRecovery::default();
        granted.apply_saved(&row, EPOCH);
        assert!(granted.grant(&nonce, EPOCH));
        assert_eq!(
            granted.check_save(&empty),
            Ok(()),
            "granted but unused: preparation may still fail and abandon"
        );
        let mut consumed = ScopeRecovery::default();
        consumed.apply_saved(&row, EPOCH);
        assert!(consumed.grant(&nonce, EPOCH));
        assert!(consumed.consume_grant(&nonce, EPOCH));
        assert_eq!(
            consumed.check_save(&empty),
            Err(SaveRejected::RecordDropped),
            "consumed: a save before the id is persisted cannot drop the record"
        );
        let mut stale_without_record = DraftEnvelope::fresh();
        stale_without_record.set_generation(Generation::fresh());
        assert_eq!(
            consumed.check_save(&stale_without_record),
            Err(SaveRejected::RecordDropped)
        );
        // The exact queued assignment is still the one allowed transition,
        // and the exact release (no id yet) is the one way out.
        assert_eq!(consumed.check_save(&queued), Ok(()));
        assert!(
            !consumed.release(&nonce, Some(TXN)),
            "a release must name the record's exact id, here none"
        );
        assert!(consumed.release(&nonce, None));
        assert_eq!(consumed.check_save(&empty), Ok(()));
        // Wrong epoch and revocation: the consumed record is as protected as
        // a restored one.
        let mut other_epoch = ScopeRecovery::default();
        other_epoch.apply_saved(&row, EPOCH);
        assert!(other_epoch.grant(&nonce, EPOCH));
        assert!(!other_epoch.consume_grant(&nonce, EPOCH + 1));
        assert_eq!(
            other_epoch.check_save(&empty),
            Ok(()),
            "an unconsumed grant still allows abandonment"
        );
        assert!(other_epoch.consume_grant(&nonce, EPOCH));
        other_epoch.revoke_process_ownership();
        assert_eq!(
            other_epoch.check_save(&empty),
            Err(SaveRejected::RecordDropped)
        );
        assert_eq!(
            other_epoch.check_save(&queued),
            Err(SaveRejected::RecordReplayed)
        );
        assert_eq!(scope.check_save(&empty), Err(SaveRejected::RecordDropped));
        // Account A -> B -> A: the new selection epoch revokes the old fresh
        // ownership. The old enqueuing record can be neither granted nor
        // abandoned; it is now a restored record awaiting review.
        let mut aba = ScopeRecovery::default();
        aba.apply_saved(&row, EPOCH);
        assert!(aba.grant(&nonce, EPOCH));
        aba.revoke_process_ownership();
        assert!(!aba.consume_grant(&nonce, EPOCH));
        assert!(!aba.grant(&nonce, EPOCH));
        assert!(!aba.grant(&nonce, EPOCH + 1));
        assert_eq!(aba.check_save(&empty), Err(SaveRejected::RecordDropped));
        assert_eq!(aba.check_save(&queued), Err(SaveRejected::RecordReplayed));
        // A restart turns the same row into restored records: no fresh
        // ownership, no grants, no abandonment without resolution.
        let restored = ScopeRecovery::from_restored(&restored_with(queued.clone(), None));
        assert_eq!(
            restored.check_save(&empty),
            Err(SaveRejected::RecordDropped)
        );
        let mut restored = restored;
        assert!(!restored.grant(&nonce, EPOCH));
        assert!(!restored.consume_grant(&nonce, EPOCH));
    }

    #[test]
    fn dropped_thread_index_entries_are_named_for_the_caller_to_prove_idle() {
        let mut row = DraftEnvelope::fresh();
        row.index_thread("$one:example.org").unwrap();
        row.index_thread("$two:example.org").unwrap();
        let mut scope = ScopeRecovery::from_restored(&restored_with(row.clone(), None));
        assert!(!scope.is_idle(), "an index alone keeps the scope alive");
        // A stale snapshot taken before the index existed drops both roots.
        let stale = DraftEnvelope::fresh();
        assert_eq!(
            scope.dropped_threads(&stale),
            vec!["$one:example.org", "$two:example.org"]
        );
        let mut partial = row.clone();
        assert!(partial.unindex_thread("$two:example.org"));
        assert_eq!(scope.dropped_threads(&partial), vec!["$two:example.org"]);
        let mut grown = row.clone();
        grown.index_thread("$three:example.org").unwrap();
        assert!(scope.dropped_threads(&grown).is_empty());
        scope.apply_saved(&partial, EPOCH);
        assert_eq!(scope.threads(), ["$one:example.org"]);
        scope.apply_cleared();
        assert!(scope.is_idle());
    }

    #[test]
    fn quarantine_and_read_only_blocks_hold_until_the_exact_explicit_release() {
        let original_html = "<span data-morpheus-compose=\"9\" data-format=\"plain\">x</span>";
        let row = QuarantinedRow::new(
            QuarantineReason::UnsupportedVersion,
            "plain column differs",
            Some(original_html),
            "reply:$target:example.org",
            false,
        )
        .unwrap();
        assert_eq!(
            row.original_plain().as_deref(),
            Some("plain column differs")
        );
        assert_eq!(row.original_html().as_deref(), Some(original_html));
        assert_eq!(row.original_kind(), "reply:$target:example.org");
        assert_eq!(
            row.fingerprint,
            row_checksum(
                "plain column differs",
                Some(original_html),
                "reply:$target:example.org"
            )
        );
        let mut carried = DraftEnvelope::fresh();
        carried.set_quarantine(Some(row.clone()));
        let loaded = restored_with(carried.clone(), None);
        let revision = loaded.revision.clone().unwrap();
        let mut scope = ScopeRecovery::from_restored(&loaded);
        assert_eq!(
            scope.block(),
            Some(ScopeBlock::Quarantined {
                revision: revision.clone()
            })
        );
        assert_eq!(scope.quarantine(), Some(&row));
        let mut edit = carried.clone();
        edit.set_generation(Generation::fresh());
        assert_eq!(
            scope.check_save(&edit),
            Ok(()),
            "edits that keep the exact carry are fine"
        );
        let plain = DraftEnvelope::fresh();
        assert_eq!(
            scope.check_save(&plain),
            Err(SaveRejected::QuarantineDropped)
        );
        // A different or stale carry cannot replace the unreadable original.
        let other =
            QuarantinedRow::new(QuarantineReason::Malformed, "other", None, "new", false).unwrap();
        let mut swapped = carried.clone();
        swapped.set_quarantine(Some(other.clone()));
        assert_eq!(
            scope.check_save(&swapped),
            Err(SaveRejected::QuarantineDropped)
        );
        // A save of the live text around the exact carry keeps the revision:
        // the original the user is reviewing has not changed.
        scope.apply_saved(&edit, EPOCH);
        assert_eq!(
            scope.block(),
            Some(ScopeBlock::Quarantined {
                revision: revision.clone()
            })
        );
        // A reload that returns the identical carry keeps it too; a reload
        // whose carry differs is another original with another identity.
        scope.apply_loaded(&restored_with(edit.clone(), None));
        assert_eq!(
            scope.block(),
            Some(ScopeBlock::Quarantined {
                revision: revision.clone()
            })
        );
        let mut other_scope = scope.clone();
        other_scope.apply_loaded(&restored_with(swapped.clone(), None));
        assert_ne!(
            other_scope.block(),
            Some(ScopeBlock::Quarantined {
                revision: revision.clone()
            })
        );
        assert_eq!(other_scope.quarantine(), Some(&other));
        // A discard names the exact original; a stale revision is refused.
        assert!(!scope.release_quarantine(&RowRevision::fresh()));
        assert!(!scope.release_quarantine(other_scope.block().unwrap().revision()));
        assert!(scope.release_quarantine(&revision));
        // After the discard a different carry still cannot slip in; the carry
        // may only be dropped or kept exactly.
        assert_eq!(
            scope.check_save(&swapped),
            Err(SaveRejected::QuarantineDropped)
        );
        assert_eq!(scope.check_save(&edit), Ok(()));
        assert_eq!(scope.check_save(&plain), Ok(()));
        scope.apply_saved(&plain, EPOCH);
        assert_eq!(scope.block(), None);
        assert!(
            !scope.release_quarantine(&revision),
            "nothing left to release"
        );
        // Without a block, nothing may introduce a carry out of nowhere.
        assert_eq!(
            scope.check_save(&swapped),
            Err(SaveRejected::QuarantineDropped)
        );
        // Read-only: no save at all until released, then the first save wins.
        // The identity is whatever the caller minted for the complete row it
        // read (text columns, kind and attachment values); the registry never
        // derives it from text.
        let frozen_row = ReadOnlyRow {
            reason: QuarantineReason::TooLarge,
        };
        let frozen_loaded = restored_with(DraftEnvelope::fresh(), Some(frozen_row));
        let frozen_revision = frozen_loaded.revision.clone().unwrap();
        let mut frozen = ScopeRecovery::from_restored(&frozen_loaded);
        assert_eq!(
            frozen.block(),
            Some(ScopeBlock::ReadOnly {
                revision: frozen_revision.clone()
            })
        );
        assert_eq!(frozen.check_save(&plain), Err(SaveRejected::ReadOnly));
        assert_eq!(frozen.check_save(&carried), Err(SaveRejected::ReadOnly));
        // The same text read again as a different complete row (an attachment
        // value or the kind changed underneath) is a different identity.
        let mut changed_underneath = frozen.clone();
        changed_underneath.apply_loaded(&restored_with(DraftEnvelope::fresh(), Some(frozen_row)));
        assert_ne!(
            changed_underneath.block(),
            Some(ScopeBlock::ReadOnly {
                revision: frozen_revision.clone()
            })
        );
        assert!(
            !changed_underneath.release_quarantine(&frozen_revision),
            "a stale review discards nothing"
        );
        assert!(!frozen.release_quarantine(&RowRevision::fresh()));
        assert!(frozen.release_quarantine(&frozen_revision));
        assert_eq!(frozen.check_save(&plain), Ok(()));
        // Reloading the row re-arms the block; it was not normalized away.
        frozen.apply_loaded(&restored_with(carried.clone(), None));
        assert!(matches!(
            frozen.block(),
            Some(ScopeBlock::Quarantined { .. })
        ));
        assert_eq!(
            frozen.check_save(&plain),
            Err(SaveRejected::QuarantineDropped)
        );
        assert!(!format!("{frozen:?}").contains("data-morpheus"));
        // Unsupported stored fields are a read-only reason of their own.
        assert_eq!(
            QuarantineReason::parse("fields"),
            Some(QuarantineReason::UnsupportedFields)
        );
        assert_eq!(QuarantineReason::UnsupportedFields.code(), "fields");
        // Rows with attachments or nothing to carry are not carried.
        assert!(QuarantinedRow::new(QuarantineReason::Malformed, "x", None, "new", true).is_none());
        assert!(QuarantinedRow::new(QuarantineReason::Malformed, "", None, "new", false).is_none());
        // A damaged carry (checksum not matching its columns) is invalid.
        let mut tampered = DraftEnvelope::fresh();
        tampered.set_quarantine(Some(row.clone()));
        let hex = tampered.encode().unwrap();
        let json = String::from_utf8(from_hex(&hex).unwrap()).unwrap();
        let forged = json.replace(
            &row.fingerprint.to_string(),
            &(row.fingerprint ^ 1).to_string(),
        );
        assert_eq!(
            DraftEnvelope::decode(&to_hex(forged.as_bytes())),
            Err(RecoveryCodecError::Malformed)
        );
    }

    #[test]
    fn debug_output_carries_no_identifiers_or_text() {
        let (envelope, _) = queued_envelope();
        let rendered = format!("{envelope:?} {:?}", envelope.pending()[0]);
        assert!(!rendered.contains(TXN));
        assert!(!rendered.contains(envelope.generation().as_key()));
        assert!(!rendered.contains("casey"));
        let row = QuarantinedRow::new(
            QuarantineReason::Malformed,
            "secret",
            Some("<b>secret</b>"),
            "new",
            false,
        )
        .unwrap();
        assert!(!format!("{row:?}").contains("secret"));
    }
}
