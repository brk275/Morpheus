// Copyright 2026 Morpheus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Opt-in bounded file bytes. Crypto/armor remain the pinned canonical helpers.
//! This module performs no filesystem IO, task spawning, logging or store writes.

use std::{cell::Cell, fmt, io::Write, marker::PhantomData};

use serde::de::{
    DeserializeOwned, DeserializeSeed, Deserializer as _, Error as _, SeqAccess, Visitor,
};
use thiserror::Error;
use zeroize::Zeroizing;

use super::{
    FOOTER, HEADER, KeyExportError, MINIMUM_PAYLOAD_BYTES, armor_payload, decrypt_bytes,
    encrypt_helper,
};
use crate::olm::ExportedRoomKey;

/// Maximum accepted/emitted armored file, including headers and whitespace.
pub const ROOM_KEY_FILE_MAX_BYTES: usize = 32 * 1024 * 1024;
/// Maximum complete keys in one file. Over-limit is an error, never truncation.
pub const ROOM_KEY_FILE_MAX_KEYS: usize = 20_000;
/// Maximum PBKDF2 work accepted from an untrusted file, checked before derivation.
pub const ROOM_KEY_FILE_MAX_ROUNDS: u32 = 1_000_000;
/// The canonical SDK export work factor. Import also accepts legacy weaker files.
pub const ROOM_KEY_EXPORT_ROUNDS: u32 = 500_000;
const MAX_PASSPHRASE_BYTES: usize = 1024;
// Reserve exact canonical framing, base64 expansion and the fixed binary layout.
const MAX_PLAINTEXT_BYTES: usize =
    (ROOM_KEY_FILE_MAX_BYTES - HEADER.len() - FOOTER.len() - 2) / 4 * 3 - MINIMUM_PAYLOAD_BYTES;

/// Fixed public categories: never retain decrypted bytes or parser/crypto errors.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum BoundedKeyExportError {
    /// Invalid armor, version, authentication, UTF-8 or key JSON.
    #[error("The encrypted room-key file or passphrase is invalid.")]
    InvalidFile,
    /// File, KDF, record count or metadata exceeds the supported bounds.
    #[error("The room-key file exceeds a supported resource limit.")]
    Limit,
    /// An import passphrase is empty/too long, or an export passphrase too short.
    #[error("The room-key file passphrase does not meet the length requirements.")]
    Passphrase,
}

/// Authenticate and decrypt complete bounded file bytes on the caller's owned
/// CPU worker. No internal task is spawned. Plaintext is RAII-zeroized across
/// UTF-8/JSON failures and unwinding; detailed parsing errors never escape.
pub fn decrypt_room_key_export_bounded(
    input: &[u8],
    passphrase: &str,
) -> Result<Vec<ExportedRoomKey>, BoundedKeyExportError> {
    if input.len() > ROOM_KEY_FILE_MAX_BYTES {
        return Err(BoundedKeyExportError::Limit);
    }
    if passphrase.is_empty() || passphrase.len() > MAX_PASSPHRASE_BYTES {
        return Err(BoundedKeyExportError::Passphrase);
    }
    let input = std::str::from_utf8(input).map_err(|_| BoundedKeyExportError::InvalidFile)?;
    let payload = armor_payload(input).map_err(|_| BoundedKeyExportError::InvalidFile)?;
    let plaintext =
        decrypt_bytes(&payload, passphrase, Some(ROOM_KEY_FILE_MAX_ROUNDS)).map_err(|error| {
            match error {
                KeyExportError::ResourceLimit => BoundedKeyExportError::Limit,
                _ => BoundedKeyExportError::InvalidFile,
            }
        })?;
    let text = std::str::from_utf8(&plaintext).map_err(|_| BoundedKeyExportError::InvalidFile)?;
    let keys = parse_records::<ExportedRoomKey>(text, ROOM_KEY_FILE_MAX_KEYS)?;
    for key in &keys {
        check_metadata(key)?;
    }
    Ok(keys)
}

/// Emit a complete canonical Matrix encrypted export with a fresh caller-owned
/// passphrase. An oversized snapshot is refused rather than partially exported.
/// Only encrypted armor is returned. This method neither writes nor overwrites.
pub fn encrypt_room_key_export_bounded(
    keys: &[ExportedRoomKey],
    passphrase: &str,
) -> Result<String, BoundedKeyExportError> {
    if !(12..=MAX_PASSPHRASE_BYTES).contains(&passphrase.len()) {
        return Err(BoundedKeyExportError::Passphrase);
    }
    if keys.len() > ROOM_KEY_FILE_MAX_KEYS {
        return Err(BoundedKeyExportError::Limit);
    }
    for key in keys {
        check_metadata(key)?;
    }
    let mut writer = LimitedPlaintext {
        bytes: Zeroizing::new(Vec::new()),
        maximum: MAX_PLAINTEXT_BYTES,
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, keys).is_err() {
        return Err(if writer.exceeded {
            BoundedKeyExportError::Limit
        } else {
            BoundedKeyExportError::InvalidFile
        });
    }
    let ciphertext = encrypt_helper(&writer.bytes, passphrase, ROOM_KEY_EXPORT_ROUNDS);
    let output = [HEADER, &ciphertext, FOOTER].join("\n");
    if output.len() > ROOM_KEY_FILE_MAX_BYTES {
        return Err(BoundedKeyExportError::Limit);
    }
    Ok(output)
}

fn check_metadata(key: &ExportedRoomKey) -> Result<(), BoundedKeyExportError> {
    if key.room_id.as_str().len() > 255
        || key.session_id.len() > 512
        || key.algorithm.as_str().len() > 128
        || key.forwarding_curve25519_key_chain.len() > 32
        || key.sender_claimed_keys.iter().len() > 8
        || key.sender_claimed_keys.iter().any(|(name, _)| name.as_str().len() > 64)
    {
        return Err(BoundedKeyExportError::Limit);
    }
    Ok(())
}

// No size_hint reservation. A single key's decode allocation is bounded by the
// authenticated byte limit; accepted records/metadata have the additional caps.
fn parse_records<T: DeserializeOwned>(
    text: &str,
    maximum: usize,
) -> Result<Vec<T>, BoundedKeyExportError> {
    struct Records<'a, T> {
        maximum: usize,
        exceeded: &'a Cell<bool>,
        marker: PhantomData<T>,
    }
    struct RejectNext<'a>(&'a Cell<bool>);
    impl<'de> DeserializeSeed<'de> for RejectNext<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
            // SeqAccess invokes this only if another element exists. Reject
            // before invoking that key's Deserialize implementation/allocation.
            self.0.set(true);
            Err(D::Error::custom("Room-key record limit exceeded"))
        }
    }
    impl<'de, T: serde::Deserialize<'de>> Visitor<'de> for Records<'_, T> {
        type Value = Vec<T>;
        fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
            out.write_str("a room-key array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Vec<T>, A::Error> {
            let mut records = Vec::new();
            while records.len() < self.maximum {
                let Some(record) = sequence.next_element()? else {
                    return Ok(records);
                };
                records.push(record);
            }
            sequence.next_element_seed(RejectNext(self.exceeded))?;
            Ok(records)
        }
    }
    let exceeded = Cell::new(false);
    let mut json = serde_json::Deserializer::from_str(text);
    let result =
        (&mut json).deserialize_seq(Records { maximum, exceeded: &exceeded, marker: PhantomData });
    let records = result.map_err(|_| {
        if exceeded.get() {
            BoundedKeyExportError::Limit
        } else {
            BoundedKeyExportError::InvalidFile
        }
    })?;
    json.end().map_err(|_| BoundedKeyExportError::InvalidFile)?;
    Ok(records)
}

struct LimitedPlaintext {
    bytes: Zeroizing<Vec<u8>>,
    maximum: usize,
    exceeded: bool,
}
impl Write for LimitedPlaintext {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().checked_add(input.len()).is_none_or(|size| size > self.maximum) {
            self.exceeded = true;
            return Err(std::io::Error::other("Room-key byte limit exceeded"));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_encryption::key_export::{VERSION, base64_encode};

    fn armor(payload: &str) -> String {
        [HEADER, payload, FOOTER].join("\n")
    }

    #[test]
    fn minimum_layout_and_kdf_limits_are_checked_before_derivation() {
        for length in 0..MINIMUM_PAYLOAD_BYTES {
            let bytes = armor(&base64_encode(vec![0; length]));
            assert!(matches!(
                decrypt_room_key_export_bounded(bytes.as_bytes(), "test"),
                Err(BoundedKeyExportError::InvalidFile)
            ));
        }
        for rounds in [0u32, ROOM_KEY_FILE_MAX_ROUNDS + 1, u32::MAX] {
            let mut packet = vec![0u8; MINIMUM_PAYLOAD_BYTES];
            packet[0] = VERSION;
            packet[33..37].copy_from_slice(&rounds.to_be_bytes());
            let bytes = armor(&base64_encode(packet));
            assert!(matches!(
                decrypt_room_key_export_bounded(bytes.as_bytes(), "test"),
                Err(BoundedKeyExportError::Limit)
            ));
        }
    }

    #[test]
    fn bounded_count_rejects_next_record_before_decoding_it() {
        assert_eq!(parse_records::<u8>("[1,2]", 2).unwrap(), vec![1, 2]);
        // The third record is not a u8. Limit proves its decoder was not run.
        assert_eq!(parse_records::<u8>("[1,2,{}]", 2).unwrap_err(), BoundedKeyExportError::Limit);
        assert_eq!(
            parse_records::<u8>("[1,{}]", 2).unwrap_err(),
            BoundedKeyExportError::InvalidFile
        );
        assert_eq!(
            parse_records::<u8>("[] {}", 2).unwrap_err(),
            BoundedKeyExportError::InvalidFile
        );
    }

    #[test]
    fn invalid_utf8_and_json_never_escape_with_authenticated_plaintext() {
        for plaintext in [&b"\xffPRIVATE-KEY-MARKER"[..], &b"{PRIVATE-KEY-MARKER"[..]] {
            let bytes = armor(&encrypt_helper(plaintext, "test", 1));
            let error = match decrypt_room_key_export_bounded(bytes.as_bytes(), "test") {
                Err(error) => error,
                Ok(_) => panic!("Invalid plaintext was admitted"),
            };
            assert_eq!(error, BoundedKeyExportError::InvalidFile);
            assert!(!format!("{error:?} {error}").contains("PRIVATE-KEY-MARKER"));
        }
    }

    #[test]
    fn authentication_precedes_plaintext_parsing() {
        let payload = encrypt_helper(b"[]", "test", 1);
        let mut packet = super::super::base64_decode(&payload).unwrap();
        let original = armor(&payload);
        assert!(matches!(
            decrypt_room_key_export_bounded(original.as_bytes(), "wrong"),
            Err(BoundedKeyExportError::InvalidFile)
        ));
        packet[37] ^= 1;
        let bytes = armor(&base64_encode(packet));
        assert!(matches!(
            decrypt_room_key_export_bounded(bytes.as_bytes(), "test"),
            Err(BoundedKeyExportError::InvalidFile)
        ));
    }

    #[test]
    fn limited_plaintext_never_appends_over_limit() {
        let mut output =
            LimitedPlaintext { bytes: Zeroizing::new(Vec::new()), maximum: 4, exceeded: false };
        output.write_all(b"1234").unwrap();
        assert!(output.write_all(b"5").is_err());
        assert_eq!(&*output.bytes, b"1234");
        assert!(output.exceeded);
    }

    #[test]
    fn complete_empty_export_roundtrips_and_passphrases_are_bounded() {
        let export = encrypt_room_key_export_bounded(&[], "fresh-passphrase").unwrap();
        assert!(
            decrypt_room_key_export_bounded(export.as_bytes(), "fresh-passphrase")
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            encrypt_room_key_export_bounded(&[], "short"),
            Err(BoundedKeyExportError::Passphrase)
        ));
        assert!(matches!(
            decrypt_room_key_export_bounded(export.as_bytes(), ""),
            Err(BoundedKeyExportError::Passphrase)
        ));
        assert!(matches!(
            decrypt_room_key_export_bounded(&vec![0; ROOM_KEY_FILE_MAX_BYTES + 1], "test"),
            Err(BoundedKeyExportError::Limit)
        ));
    }
}
