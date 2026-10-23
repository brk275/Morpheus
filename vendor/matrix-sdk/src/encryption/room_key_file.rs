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
    async fn explicit_target_keeps_current_machine_and_imports_prepared_empty_export() {
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
}
