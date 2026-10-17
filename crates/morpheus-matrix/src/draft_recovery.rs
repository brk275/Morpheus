//! Durable send-recovery metadata carried inside the encrypted composer draft.
//!
//! The pinned SDK's typed `ComposerDraft` value is the only public draft
//! persistence path that serializes through the store cipher. Morpheus owns
//! that row's HTML representation, so the recovery envelope travels as one
//! attribute of the canonical draft span and is written atomically with the
//! draft text in a single `INSERT OR REPLACE`. Nothing here is ever placed in
//! an outbound Matrix event: `ComposeText::encode_draft` is used only by the
//! draft store, while outbound formatting goes through `formatting`.
//!
//! Encoding is strict and bounded: lowercase hex of canonical `serde_json`
//! output, re-encoded on read and compared byte for byte, unknown fields
//! rejected, every identifier validated. Unreadable metadata is carried
//! forward verbatim (bounded) and never silently dropped, so an explicit user
//! choice can discard it later without losing the saved text.

use std::fmt;

use matrix_sdk::ruma::EventId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Envelope schema version written by this build.
pub const ENVELOPE_VERSION: u32 = 1;
/// Queued room sends tracked per draft scope.
pub const MAX_PENDING_RECORDS: usize = 8;
/// Thread roots a room draft indexes for recovery discovery.
pub const MAX_THREAD_INDEX: usize = 32;
/// Upper bound of the hex envelope attribute, including carried unreadable data.
pub const MAX_ENVELOPE_HEX_BYTES: usize = 16 * 1024;
const MAX_TRANSACTION_ID_BYTES: usize = 64;
const STATE_ENQUEUING: &str = "enqueuing";
const STATE_QUEUED: &str = "queued";

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
    #[error("Too many messages are already waiting to send in this conversation.")]
    TooManyPending,
    #[error("A reply in this thread is still waiting to send.")]
    ThreadBusy,
    #[error("Too many threads in this room have replies waiting to send.")]
    ThreadIndexFull,
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

/// One send the user asked for in a draft scope.
///
/// `Enqueuing` is written before the SDK assigns a transaction id; `Queued`
/// after the exact handle returned. A record found after restart without an
/// exact SDK echo is unconfirmed, never adopted or treated as sent.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRecord {
    nonce: Generation,
    generation: Generation,
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transaction_id: Option<String>,
}

impl PendingRecord {
    pub fn enqueuing(nonce: Generation, generation: Generation) -> Self {
        Self {
            nonce,
            generation,
            state: STATE_ENQUEUING.to_owned(),
            transaction_id: None,
        }
    }

    pub fn queued(
        nonce: Generation,
        generation: Generation,
        transaction_id: &str,
    ) -> Result<Self, RecoveryCodecError> {
        if !valid_transaction_id(transaction_id) {
            return Err(RecoveryCodecError::Malformed);
        }
        Ok(Self {
            nonce,
            generation,
            state: STATE_QUEUED.to_owned(),
            transaction_id: Some(transaction_id.to_owned()),
        })
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

    pub fn is_queued(&self) -> bool {
        self.state == STATE_QUEUED
    }

    fn is_valid(&self) -> bool {
        self.nonce.is_valid()
            && self.generation.is_valid()
            && match self.state.as_str() {
                STATE_ENQUEUING => self.transaction_id.is_none(),
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
            .finish()
    }
}

/// Metadata saved atomically with a draft scope's text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftEnvelope {
    #[serde(rename = "v")]
    version: u32,
    generation: Generation,
    /// The saved text is the retained submission of this record and is hidden
    /// from the composer until the record resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retained_for: Option<Generation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<PendingRecord>,
    /// Room-level index of thread roots whose own draft rows hold pending
    /// records, so discovery never has to enumerate hashed thread keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    threads: Vec<String>,
    /// Metadata an earlier read could not interpret, carried verbatim until
    /// the user explicitly discards it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unreadable: Option<String>,
}

impl fmt::Debug for DraftEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraftEnvelope")
            .field("version", &self.version)
            .field("retained", &self.retained_for.is_some())
            .field("pending", &self.pending.len())
            .field("threads", &self.threads.len())
            .field("unreadable", &self.unreadable.is_some())
            .finish()
    }
}

impl DraftEnvelope {
    pub fn new(generation: Generation) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            generation,
            retained_for: None,
            pending: Vec::new(),
            threads: Vec::new(),
            unreadable: None,
        }
    }

    pub fn fresh() -> Self {
        Self::new(Generation::fresh())
    }

    /// Rebuild an envelope from cached parts; every bound and identifier is
    /// validated so a cache can never persist what the decoder would reject.
    pub fn restore(
        generation: Generation,
        retained_for: Option<Generation>,
        pending: Vec<PendingRecord>,
        threads: Vec<String>,
        unreadable: Option<String>,
    ) -> Result<Self, RecoveryCodecError> {
        let envelope = Self {
            version: ENVELOPE_VERSION,
            generation,
            retained_for,
            pending,
            threads,
            unreadable,
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

    pub fn retained_for(&self) -> Option<&Generation> {
        self.retained_for.as_ref()
    }

    pub fn pending(&self) -> &[PendingRecord] {
        &self.pending
    }

    pub fn threads(&self) -> &[String] {
        &self.threads
    }

    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }

    /// Keep metadata this build cannot read. Oversized data cannot be ours and
    /// is not kept; the caller still reports it as unreadable.
    pub fn carry_unreadable(&mut self, raw: &str) -> bool {
        if raw.len() > MAX_ENVELOPE_HEX_BYTES {
            return false;
        }
        self.unreadable = Some(raw.to_owned());
        true
    }

    /// True when the row holds nothing but text: no pending send, no thread
    /// index, no retained submission and no carried unreadable data.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
            && self.threads.is_empty()
            && self.retained_for.is_none()
            && self.unreadable.is_none()
    }

    pub fn begin_pending(
        &mut self,
        nonce: Generation,
        generation: Generation,
        thread_scope: bool,
    ) -> Result<(), PendingLimitError> {
        if thread_scope && !self.pending.is_empty() {
            return Err(PendingLimitError::ThreadBusy);
        }
        if self.pending.len() >= MAX_PENDING_RECORDS
            || self.pending.iter().any(|record| record.nonce == nonce)
        {
            return Err(PendingLimitError::TooManyPending);
        }
        self.pending
            .push(PendingRecord::enqueuing(nonce, generation));
        Ok(())
    }

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
            .find(|record| record.nonce == *nonce && !record.is_queued())
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

    /// Remove exactly one owned record. A queued record requires its exact
    /// transaction id; an enqueuing record requires that none is claimed.
    pub fn remove_pending(
        &mut self,
        nonce: &Generation,
        transaction_id: Option<&str>,
    ) -> Option<PendingRecord> {
        let index = self.pending.iter().position(|record| {
            record.nonce == *nonce && record.transaction_id.as_deref() == transaction_id
        })?;
        let record = self.pending.remove(index);
        if self.retained_for.as_ref() == Some(nonce) {
            self.retained_for = None;
        }
        Some(record)
    }

    pub fn set_retained_for(&mut self, nonce: Option<Generation>) {
        self.retained_for = nonce;
    }

    pub fn index_thread(&mut self, root: &str) -> Result<bool, PendingLimitError> {
        if EventId::parse(root).is_err() {
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

    pub fn clear_unreadable(&mut self) -> bool {
        self.unreadable.take().is_some()
    }

    fn validate(&self) -> Result<(), RecoveryCodecError> {
        if self.version != ENVELOPE_VERSION {
            return Err(RecoveryCodecError::UnsupportedVersion);
        }
        if !self.generation.is_valid()
            || self
                .retained_for
                .as_ref()
                .is_some_and(|nonce| !nonce.is_valid())
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
        if let Some(nonce) = &self.retained_for
            && !self.pending.iter().any(|record| record.nonce == *nonce)
        {
            return Err(RecoveryCodecError::Malformed);
        }
        for (index, root) in self.threads.iter().enumerate() {
            if EventId::parse(root).is_err() || self.threads[..index].contains(root) {
                return Err(RecoveryCodecError::Malformed);
            }
        }
        if let Some(unreadable) = &self.unreadable
            && (unreadable.is_empty()
                || unreadable.len() > MAX_ENVELOPE_HEX_BYTES
                || !is_lower_hex(unreadable))
        {
            return Err(RecoveryCodecError::Malformed);
        }
        Ok(())
    }

    /// Canonical lowercase hex of the JSON envelope.
    pub fn encode(&self) -> Result<String, RecoveryCodecError> {
        self.validate()?;
        let json = serde_json::to_vec(self).map_err(|_| RecoveryCodecError::Malformed)?;
        let hex = to_hex(&json);
        if hex.len() > MAX_ENVELOPE_HEX_BYTES {
            return Err(RecoveryCodecError::TooLarge);
        }
        Ok(hex)
    }

    /// Strict inverse of `encode`: bounded before allocation, lowercase hex
    /// only, unknown fields rejected, every identifier validated, and the
    /// result must re-encode to exactly the input.
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

    fn queued_envelope() -> (DraftEnvelope, Generation) {
        let mut envelope = DraftEnvelope::fresh();
        let nonce = Generation::fresh();
        envelope
            .begin_pending(nonce.clone(), envelope.generation().clone(), false)
            .unwrap();
        assert!(envelope.mark_queued(&nonce, "0123456789abcdef0123456789abcdef"));
        envelope.set_retained_for(Some(nonce.clone()));
        (envelope, nonce)
    }

    #[test]
    fn generations_are_opaque_fresh_and_never_equal_by_text() {
        let first = Generation::fresh();
        let second = Generation::fresh();
        assert_ne!(first, second);
        assert!(first.is_valid());
        assert_eq!(format!("{first:?}"), "Generation([opaque])");
        assert!(!Generation("ABCDEF".repeat(5) + "ab").is_valid());
        assert!(!Generation("0".repeat(31)).is_valid());
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
        // Same JSON with a trailing space is valid JSON but not canonical.
        let mut json = from_hex(&hex).unwrap();
        json.push(b' ');
        assert_eq!(
            DraftEnvelope::decode(&to_hex(&json)),
            Err(RecoveryCodecError::NonCanonical)
        );
        let unknown = to_hex(br#"{"v":1,"generation":"00000000000000000000000000000000","extra":1}"#);
        assert_eq!(
            DraftEnvelope::decode(&unknown),
            Err(RecoveryCodecError::Malformed)
        );
        let future = to_hex(br#"{"v":2,"generation":"00000000000000000000000000000000"}"#);
        assert_eq!(
            DraftEnvelope::decode(&future),
            Err(RecoveryCodecError::UnsupportedVersion)
        );
        let oversized = "00".repeat(MAX_ENVELOPE_HEX_BYTES / 2 + 1);
        assert_eq!(
            DraftEnvelope::decode(&oversized),
            Err(RecoveryCodecError::TooLarge)
        );
    }

    #[test]
    fn pending_records_are_bounded_unique_and_removed_only_by_exact_identity() {
        let mut envelope = DraftEnvelope::fresh();
        let generation = envelope.generation().clone();
        let mut nonces = Vec::new();
        for _ in 0..MAX_PENDING_RECORDS {
            let nonce = Generation::fresh();
            envelope
                .begin_pending(nonce.clone(), generation.clone(), false)
                .unwrap();
            nonces.push(nonce);
        }
        assert_eq!(
            envelope.begin_pending(Generation::fresh(), generation.clone(), false),
            Err(PendingLimitError::TooManyPending)
        );
        assert!(envelope.mark_queued(&nonces[0], "aaaa"));
        assert!(!envelope.mark_queued(&nonces[1], "aaaa"), "duplicate transaction");
        assert!(!envelope.mark_queued(&nonces[0], "bbbb"), "already queued");
        assert!(!envelope.mark_queued(&nonces[1], "bad id!"));
        assert!(envelope.remove_pending(&nonces[0], None).is_none());
        assert!(envelope.remove_pending(&nonces[0], Some("other")).is_none());
        assert!(envelope.remove_pending(&nonces[1], Some("aaaa")).is_none());
        assert!(envelope.remove_pending(&nonces[0], Some("aaaa")).is_some());
        assert!(envelope.remove_pending(&nonces[1], None).is_some());
        assert_eq!(envelope.pending().len(), MAX_PENDING_RECORDS - 2);
        let mut thread = DraftEnvelope::fresh();
        thread
            .begin_pending(Generation::fresh(), generation.clone(), true)
            .unwrap();
        assert_eq!(
            thread.begin_pending(Generation::fresh(), generation, true),
            Err(PendingLimitError::ThreadBusy)
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
    fn retained_text_requires_its_record_and_unreadable_data_is_carried_bounded() {
        let (mut envelope, nonce) = queued_envelope();
        assert!(!envelope.is_empty());
        envelope.remove_pending(&nonce, Some("0123456789abcdef0123456789abcdef"));
        assert!(envelope.retained_for().is_none());
        assert!(envelope.is_empty());
        let dangling = DraftEnvelope::restore(
            Generation::fresh(),
            Some(Generation::fresh()),
            Vec::new(),
            Vec::new(),
            None,
        );
        assert_eq!(dangling, Err(RecoveryCodecError::Malformed));
        assert!(envelope.carry_unreadable("abcd"));
        assert!(!envelope.is_empty());
        let hex = envelope.encode().unwrap();
        assert_eq!(DraftEnvelope::decode(&hex).unwrap().unreadable(), Some("abcd"));
        assert!(!envelope.carry_unreadable(&"ab".repeat(MAX_ENVELOPE_HEX_BYTES)));
        assert!(envelope.clear_unreadable());
        assert!(envelope.is_empty());
        assert!(
            DraftEnvelope::restore(Generation::fresh(), None, Vec::new(), Vec::new(), Some("xyz".into()))
                .is_err()
        );
    }

    #[test]
    fn debug_output_carries_no_identifiers() {
        let (envelope, _) = queued_envelope();
        let rendered = format!("{envelope:?} {:?}", envelope.pending()[0]);
        assert!(!rendered.contains("0123456789abcdef"));
        assert!(!rendered.contains(&envelope.generation().0));
    }
}
