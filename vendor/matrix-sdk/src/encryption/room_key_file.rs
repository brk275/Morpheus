// Copyright 2026 Morpheus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Opt-in prepared encrypted room-key bytes. No filesystem, UI, authorization
//! callback or hidden blocking task lives in this seam. Callers must retain the
//! actual CPU/IO owner and await admitted store work even after UI cancellation.

use matrix_sdk_base::crypto::{
    OlmMachine, decrypt_room_key_export_bounded, encrypt_room_key_export_bounded,
    olm::{ExportedRoomKey, InboundGroupSession},
};
use tokio::sync::RwLockReadGuard;

use super::Encryption;

pub use matrix_sdk_base::crypto::{
    BoundedKeyExportError, ROOM_KEY_FILE_MAX_BYTES, ROOM_KEY_FILE_MAX_KEYS,
};

/// Fixed categories, without decoder details, room identifiers or key material.
#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoomKeyFileError {
    /// Invalid, unauthenticated or unsupported encrypted bytes/passphrase.
    #[error(transparent)]
    File(#[from] BoundedKeyExportError),
    /// No current crypto store was available during target preparation.
    #[error("The encrypted account store is unavailable.")]
    StoreUnavailable,
    /// No export was produced; the existing store could not be read.
    #[error("The encrypted account keys could not be prepared for export.")]
    StoreRead,
    /// Store import was admitted and its completion failed. Partial/in-flight
    /// effects must not be presented as a confirmed unchanged store or retried.
    #[error("Room-key import completion could not be confirmed.")]
    ImportUncertain,
}

/// Authenticated decoded keys; intentionally not Clone, Debug or Serialize.
#[allow(missing_debug_implementations)]
pub struct PreparedRoomKeyImport {
    keys: Vec<ExportedRoomKey>,
}
impl PreparedRoomKeyImport {
    /// The complete bounded decoded count, before store-level validity/merging.
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }
}

/// Synchronous CPU preparation. Run on an actually retained worker, not an
/// unobserved spawn_blocking future. No key/store mutation occurs here.
pub fn prepare_room_key_import(
    bytes: &[u8],
    passphrase: &str,
) -> Result<PreparedRoomKeyImport, RoomKeyFileError> {
    let keys = decrypt_room_key_export_bounded(bytes, passphrase)?;
    // The existing store filters/logs unsupported conversions. Perform that
    // canonical, deterministic validation before admitting any store mutation,
    // without attaching raw identifiers or conversion errors to this result.
    for key in &keys {
        InboundGroupSession::from_export(key)
            .map_err(|_| RoomKeyFileError::File(BoundedKeyExportError::InvalidFile))?;
    }
    Ok(PreparedRoomKeyImport { keys })
}

/// A complete store snapshot awaiting encrypted serialization. Intentionally no
/// Clone/Debug/Serialize; no plaintext bytes or file paths are returned.
#[allow(missing_debug_implementations)]
pub struct PreparedRoomKeyExport {
    keys: Vec<ExportedRoomKey>,
}
impl PreparedRoomKeyExport {
    /// Number of keys in this complete snapshot. A too-large snapshot is refused.
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// Encrypt on the caller's retained CPU worker. Returns only canonical
    /// Matrix armor; caller owns fresh passphrase, final authority and file commit.
    pub fn encrypt(self, passphrase: &str) -> Result<String, RoomKeyFileError> {
        Ok(encrypt_room_key_export_bounded(&self.keys, passphrase)?)
    }
}

/// The exact current SDK machine guard, obtained by an explicit preparation
/// await. Holding it prevents SDK machine replacement before/during import.
/// No app account identifiers or authorization callbacks enter the SDK.
#[allow(missing_debug_implementations)]
pub struct RoomKeyImportStore<'a> {
    machine: RwLockReadGuard<'a, Option<OlmMachine>>,
    encryption: &'a Encryption,
}

/// Counts only: no key/session map is retained in the presentation result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomKeyFileImportSummary {
    /// Keys actually merged into the crypto store.
    pub imported_count: usize,
    /// SDK-valid sessions processed, after its existing conversion policy.
    pub total_count: usize,
    /// Complete bounded decoded file records (can exceed valid session count).
    pub file_key_count: usize,
}
impl RoomKeyImportStore<'_> {
    /// Consume prepared keys after the caller's final captured authority check.
    /// There is NO additional client/store lookup await before the existing
    /// store import handoff. Once admitted, callers must await this completion;
    /// dropping/select-cancelling this future does not prove no store effects.
    pub async fn import(
        self,
        prepared: PreparedRoomKeyImport,
    ) -> Result<RoomKeyFileImportSummary, RoomKeyFileError> {
        let machine = self.machine.as_ref().ok_or(RoomKeyFileError::StoreUnavailable)?;
        let file_key_count = prepared.keys.len();
        let result = machine
            .store()
            .import_exported_room_keys(prepared.keys, |_, _| {})
            .await
            .map_err(|_| RoomKeyFileError::ImportUncertain)?;
        // Existing SDK behavior. This is not a receipt of a remote backup upload.
        self.encryption.backups().maybe_trigger_backup();
        Ok(RoomKeyFileImportSummary {
            imported_count: result.imported_count,
            total_count: result.total_count,
            file_key_count,
        })
    }
}

impl Encryption {
    /// Explicit preparation await. Recheck original account/security/operation
    /// authority AFTER this completes and AFTER CPU preparation, before import.
    pub async fn room_key_import_store(&self) -> Result<RoomKeyImportStore<'_>, RoomKeyFileError> {
        let machine = self.client.olm_machine().await;
        if machine.is_none() {
            return Err(RoomKeyFileError::StoreUnavailable);
        }
        Ok(RoomKeyImportStore { machine, encryption: self })
    }

    /// Prepare ALL current account room keys, never a truncated recovery export.
    /// The existing SDK store first reads its whole session snapshot; the file
    /// output bounds do not claim a constant-memory store-read implementation.
    pub async fn prepare_room_key_export(&self) -> Result<PreparedRoomKeyExport, RoomKeyFileError> {
        let machine = self.client.olm_machine().await;
        let machine = machine.as_ref().ok_or(RoomKeyFileError::StoreUnavailable)?;
        let mut accepted = 0usize;
        let mut overflow = false;
        let keys = machine
            .store()
            .export_room_keys(|_| {
                if accepted == ROOM_KEY_FILE_MAX_KEYS {
                    overflow = true;
                    false
                } else {
                    accepted += 1;
                    true
                }
            })
            .await
            .map_err(|_| RoomKeyFileError::StoreRead)?;
        if overflow {
            return Err(BoundedKeyExportError::Limit.into());
        }
        Ok(PreparedRoomKeyExport { keys })
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use crate::test_utils::logged_in_client;
    use matrix_sdk_test::async_test;

    #[test]
    fn invalid_preparation_has_fixed_non_secret_error() {
        let error = match prepare_room_key_import(b"PRIVATE-KEY-MARKER", "passphrase") {
            Err(error) => error,
            Ok(_) => panic!("Invalid room-key bytes were admitted"),
        };
        assert_eq!(error, RoomKeyFileError::File(BoundedKeyExportError::InvalidFile));
        assert!(!format!("{error:?} {error}").contains("PRIVATE-KEY-MARKER"));
    }

    #[test]
    fn public_byte_reader_refuses_layout_and_kdf_before_store_preparation() {
        use matrix_sdk_base::crypto::{ROOM_KEY_FILE_MAX_ROUNDS, vodozemac::base64_encode};
        let armor = |packet| {
            format!(
                "-----BEGIN MEGOLM SESSION DATA-----\n{}\n-----END MEGOLM SESSION DATA-----",
                base64_encode(packet)
            )
        };
        // Complete fixed header is 37 bytes and MAC 32. No short input reaches
        // slicing, derivation or any client/store operation.
        for length in 0..69 {
            let bytes = armor(vec![0u8; length]);
            assert!(matches!(
                prepare_room_key_import(bytes.as_bytes(), "test"),
                Err(RoomKeyFileError::File(BoundedKeyExportError::InvalidFile))
            ));
        }
        for rounds in [0u32, ROOM_KEY_FILE_MAX_ROUNDS + 1, u32::MAX] {
            let mut packet = vec![0u8; 69];
            packet[0] = 1;
            packet[33..37].copy_from_slice(&rounds.to_be_bytes());
            let bytes = armor(packet);
            assert!(matches!(
                prepare_room_key_import(bytes.as_bytes(), "test"),
                Err(RoomKeyFileError::File(BoundedKeyExportError::Limit))
            ));
        }
        assert!(matches!(
            prepare_room_key_import(&vec![0; ROOM_KEY_FILE_MAX_BYTES + 1], "test"),
            Err(RoomKeyFileError::File(BoundedKeyExportError::Limit))
        ));
    }

    #[async_test]
    async fn test_explicit_target_keeps_current_machine_and_imports_prepared_empty_export() {
        let client = logged_in_client(None).await;
        let encryption = client.encryption();
        let snapshot = encryption.prepare_room_key_export().await.unwrap();
        assert_eq!(snapshot.key_count(), 0);
        let bytes = snapshot.encrypt("fresh-test-passphrase").unwrap();
        assert!(matches!(
            prepare_room_key_import(bytes.as_bytes(), "wrong-passphrase"),
            Err(RoomKeyFileError::File(BoundedKeyExportError::InvalidFile))
        ));
        let mut tampered = bytes.as_bytes().to_vec();
        let payload_end = bytes.rfind("\n-----END").unwrap();
        let byte = &mut tampered[payload_end - 2];
        *byte = if *byte == b'A' { b'B' } else { b'A' };
        assert!(matches!(
            prepare_room_key_import(&tampered, "fresh-test-passphrase"),
            Err(RoomKeyFileError::File(BoundedKeyExportError::InvalidFile))
        ));
        let prepared = prepare_room_key_import(bytes.as_bytes(), "fresh-test-passphrase").unwrap();
        assert_eq!(prepared.key_count(), 0);
        let target = encryption.room_key_import_store().await.unwrap();
        let result = target.import(prepared).await.unwrap();
        assert_eq!(
            result,
            RoomKeyFileImportSummary { imported_count: 0, total_count: 0, file_key_count: 0 }
        );
    }

    // New ephemeral native keys only: no copied secret fixture, server, vault or
    // account data. These cases run through the SDK public byte/store API even
    // though the path-patched crypto package is excluded from workspace tests.
    fn generated_key() -> ExportedRoomKey {
        use matrix_sdk_base::crypto::{
            types::{EventEncryptionAlgorithm, SigningKeys},
            vodozemac::{
                megolm::{GroupSession, InboundGroupSession as NativeInbound, SessionConfig},
                olm::Account,
            },
        };
        use ruma::{DeviceKeyAlgorithm, room_id};

        let account = Account::new();
        let identity = account.identity_keys();
        let group = GroupSession::new(SessionConfig::version_1());
        let inbound = NativeInbound::new(&group.session_key(), SessionConfig::version_1());
        let mut signing = SigningKeys::new();
        signing.insert(DeviceKeyAlgorithm::Ed25519, identity.ed25519.into());
        ExportedRoomKey {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id: room_id!("!room-key-fixture:example.org").to_owned(),
            sender_key: identity.curve25519,
            session_id: group.session_id(),
            session_key: inbound.export_at_first_known_index(),
            sender_claimed_keys: signing,
            forwarding_curve25519_key_chain: Vec::new(),
            shared_history: false,
        }
    }

    #[async_test]
    async fn test_nonempty_legacy_import_reexports_to_an_independent_store() {
        use matrix_sdk_base::crypto::{decrypt_room_key_export_bounded, encrypt_room_key_export};

        let key = generated_key();
        let expected_room = key.room_id.clone();
        let expected_session = key.session_id.clone();
        let legacy = encrypt_room_key_export(&[key], "legacy-test-passphrase", 1)
            .expect("Synthetic legacy export serializes");
        let prepared = prepare_room_key_import(legacy.as_bytes(), "legacy-test-passphrase")
            .unwrap_or_else(|_| panic!("Generated nonempty export must authenticate"));
        assert_eq!(prepared.key_count(), 1);
        let first_client = logged_in_client(None).await;
        let first = first_client.encryption();
        let result = first.room_key_import_store().await.unwrap().import(prepared).await.unwrap();
        assert_eq!(
            result,
            RoomKeyFileImportSummary { imported_count: 1, total_count: 1, file_key_count: 1 }
        );
        let snapshot = first.prepare_room_key_export().await.unwrap();
        assert_eq!(snapshot.key_count(), 1);
        let encrypted = snapshot.encrypt("fresh-export-passphrase").unwrap();
        let decoded =
            decrypt_room_key_export_bounded(encrypted.as_bytes(), "fresh-export-passphrase")
                .unwrap_or_else(|_| panic!("Reexport must authenticate"));
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].room_id, expected_room);
        assert_eq!(decoded[0].session_id, expected_session);
        drop(decoded);

        let second_client = logged_in_client(None).await;
        let second = second_client.encryption();
        assert_eq!(second.prepare_room_key_export().await.unwrap().key_count(), 0);
        let restored = prepare_room_key_import(encrypted.as_bytes(), "fresh-export-passphrase")
            .unwrap_or_else(|_| panic!("Fresh export must prepare for a separate store"));
        let result = second.room_key_import_store().await.unwrap().import(restored).await.unwrap();
        assert_eq!(
            result,
            RoomKeyFileImportSummary { imported_count: 1, total_count: 1, file_key_count: 1 }
        );
        assert_eq!(second.prepare_room_key_export().await.unwrap().key_count(), 1);
    }

    #[test]
    fn public_api_refuses_authenticated_over_count_before_store_admission() {
        use matrix_sdk_base::crypto::{encrypt_room_key_export, encrypt_room_key_export_bounded};

        let key = generated_key();
        let encoded =
            zeroize::Zeroizing::new(serde_json::to_string(&key).expect("Synthetic key serializes"));
        let keys: Vec<ExportedRoomKey> = (0..=ROOM_KEY_FILE_MAX_KEYS)
            .map(|_| {
                serde_json::from_str(&encoded)
                    .unwrap_or_else(|_| panic!("Generated synthetic key must decode"))
            })
            .collect();
        // Writer rejects the complete over-limit snapshot, rather than emitting
        // a truncated backup or doing expensive export KDF work first.
        assert!(matches!(
            encrypt_room_key_export_bounded(&keys, "test-export-passphrase"),
            Err(BoundedKeyExportError::Limit)
        ));
        // Legacy public API produces an authenticated over-count input. Its low
        // work factor is deliberate fixture setup; import retains compatibility.
        let bytes = encrypt_room_key_export(&keys, "legacy-test-passphrase", 1)
            .expect("Synthetic over-count legacy file serializes");
        assert!(bytes.len() <= ROOM_KEY_FILE_MAX_BYTES, "Count must be the refusing limit");
        assert!(matches!(
            prepare_room_key_import(bytes.as_bytes(), "legacy-test-passphrase"),
            Err(RoomKeyFileError::File(BoundedKeyExportError::Limit))
        ));
    }
}
