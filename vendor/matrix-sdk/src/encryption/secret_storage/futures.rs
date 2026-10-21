// Copyright 2023 The Matrix.org Foundation C.I.C.
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
    fmt,
    future::IntoFuture,
    pin::Pin,
    sync::{Arc, Mutex},
};

use futures_core::Future;
use matrix_sdk_base::crypto::secret_storage::SecretStorageKey;
use ruma::events::secret_storage::default_key::SecretStorageDefaultKeyEventContent;
use zeroize::Zeroizing;

use super::{Result, SecretStorage, SecretStore};

/// Future returned by [`SecretStorage::create_secret_store()`].
#[derive(Debug)]
pub struct CreateStore<'a> {
    pub(super) secret_storage: &'a SecretStorage,
    pub(super) passphrase: Option<&'a str>,
    // MORPHEUS PATCH (generated key handoff).
    pub(super) key_handoff: Option<GeneratedKeyHandoff>,
}

/// MORPHEUS PATCH (generated key handoff): receives the key a new
/// [`SecretStore`] is protected with, encoded like
/// [`SecretStore::secret_storage_key()`], as soon as the key is generated and
/// before anything about it is sent to the homeserver.
///
/// Creating a store uploads the key's description, then the secrets
/// encrypted with it, then makes it the default key. Each of those requests
/// can take effect on the server and still end in an error, after which the
/// creation fails and drops the key. A caller holding a handoff still has
/// the key in that case. The key is held in zeroizing memory; this type's
/// `Debug` output never contains it.
#[derive(Clone, Default)]
pub struct GeneratedKeyHandoff(Arc<Mutex<Option<Zeroizing<String>>>>);

impl GeneratedKeyHandoff {
    /// An empty handoff.
    pub fn new() -> Self {
        Self::default()
    }

    /// The generated key, taken out of the handoff. `None` if no key was
    /// generated yet, or it was already taken.
    pub fn take(&self) -> Option<Zeroizing<String>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).take()
    }

    fn put(&self, key: Zeroizing<String>) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(key);
    }
}

impl fmt::Debug for GeneratedKeyHandoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedKeyHandoff").finish_non_exhaustive()
    }
}

impl<'a> CreateStore<'a> {
    /// Set the passphrase for the new [`SecretStore`].
    ///
    /// See the documentation for the [`SecretStorage::create_secret_store()`]
    /// method for more info.
    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }

    /// MORPHEUS PATCH (generated key handoff): put the new key into
    /// `handoff` right after it is generated, before the first request.
    ///
    /// See [`GeneratedKeyHandoff`].
    pub fn hand_key_to(mut self, handoff: GeneratedKeyHandoff) -> Self {
        self.key_handoff = Some(handoff);

        self
    }
}

impl<'a> IntoFuture for CreateStore<'a> {
    type Output = Result<SecretStore>;
    #[cfg(target_family = "wasm")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_family = "wasm"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { secret_storage, passphrase, key_handoff } = self;

        Box::pin(async move {
            // Prevent multiple simultaneous calls to this method.
            //
            // See the documentation for the lock in the `store_secret` method
            // for more info.
            let client_copy = secret_storage.client.to_owned();
            let _guard = client_copy.locks().open_secret_store_lock.lock().await;

            let new_key = if let Some(passphrase) = passphrase {
                SecretStorageKey::new_from_passphrase(passphrase)
            } else {
                SecretStorageKey::new()
            };

            // MORPHEUS PATCH (generated key handoff): from here on any request
            // may take effect on the server before an error is returned, so
            // the key leaves this future first.
            if let Some(handoff) = &key_handoff {
                handoff.put(Zeroizing::new(new_key.to_base58()));
            }

            let content = new_key.event_content().to_owned();

            secret_storage.client.account().set_account_data(content).await?;

            let store = SecretStore { client: secret_storage.client.to_owned(), key: new_key };
            store.export_secrets().await?;

            let default_key_content =
                SecretStorageDefaultKeyEventContent::new(store.key.key_id().to_owned());

            store.client.account().set_account_data(default_key_content).await?;

            Ok(store)
        })
    }
}
