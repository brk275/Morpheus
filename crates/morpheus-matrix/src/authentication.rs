//! Native browser authentication. The SDK owns OAuth PKCE and token validation;
//! Morpheus owns the loopback callback, account store, and credential vault.

use super::*;
use matrix_sdk::{
    SessionMeta,
    authentication::{
        AuthSession,
        oauth::{
            ClientId, CsrfToken, OAuthSession, UserSession,
            registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
        },
    },
};

const AUTH_ATTEMPT_TTL: Duration = Duration::from_secs(180);
const PENDING_STORE_MARKER: &str = ".pending-browser-login";

// The untagged Matrix shape deliberately preserves existing version-1 vault
// entries. OAuth is wrapped so it cannot be mistaken for a Matrix session.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(super) enum StoredSession {
    OAuth { oauth: StoredOAuthSession },
    Matrix(MatrixSession),
}

#[derive(Serialize, Deserialize)]
pub(super) struct StoredOAuthSession {
    client_id: String,
    user: UserSession,
}

impl StoredSession {
    pub(super) fn from_client(client: &Client) -> Option<Self> {
        if let Some(session) = client.oauth().full_session() {
            Some(Self::OAuth {
                oauth: StoredOAuthSession {
                    client_id: session.client_id.to_string(),
                    user: session.user,
                },
            })
        } else {
            client.matrix_auth().session().map(Self::Matrix)
        }
    }

    pub(super) fn meta(&self) -> &SessionMeta {
        match self {
            Self::Matrix(session) => &session.meta,
            Self::OAuth { oauth } => &oauth.user.meta,
        }
    }

    pub(super) fn into_auth(self) -> AuthSession {
        match self {
            Self::Matrix(session) => session.into(),
            Self::OAuth { oauth } => OAuthSession {
                client_id: ClientId::new(oauth.client_id),
                user: oauth.user,
            }
            .into(),
        }
    }
}

pub(super) struct PendingSso {
    homeserver: String,
    redirect_url: Url,
    state_parameter: &'static str,
    created_at: Instant,
    oauth: Option<PendingOAuth>,
}

struct PendingOAuth {
    client: Client,
    passphrase: Zeroizing<String>,
    store: PendingStore,
}

/// An unsuccessful or cancelled login must not leave an unusable account store.
pub(super) struct PendingStore {
    pub(super) id: String,
    pub(super) path: PathBuf,
    account_dir: PathBuf,
    data_dir: PathBuf,
    marker: Option<fs::File>,
    keep: bool,
}

impl PendingStore {
    pub(super) fn new(manager: &AccountManager) -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        let path = manager.store_path(&id);
        let account_dir = path
            .parent()
            .ok_or(MatrixCoreError::InvalidSession)?
            .to_owned();
        let mut store = Self {
            path,
            account_dir,
            data_dir: manager.data_dir.clone(),
            marker: None,
            id,
            keep: false,
        };
        #[allow(unused_mut)]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&store.account_dir)?;
        builder.create(&store.path)?;
        let mut marker = fs::OpenOptions::new();
        marker.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            marker.mode(0o600);
        }
        let file = marker.open(store.account_dir.join(PENDING_STORE_MARKER))?;
        file.try_lock().map_err(std::io::Error::other)?;
        // Retain ownership even if writing this newly created marker fails.
        // Only a complete marker is eligible for cleanup by a later process.
        store.marker = Some(file);
        use std::io::Write as _;
        let file = store
            .marker
            .as_mut()
            .expect("owned marker was just installed");
        file.write_all(b"morpheus-pending-v1")?;
        file.sync_all()?;
        sync_registry_directory(&store.account_dir)?;
        sync_registry_directory(&manager.data_dir.join("accounts"))?;
        Ok(store)
    }

    pub(super) fn commit(&mut self) -> std::io::Result<()> {
        self.commit_with(|marker| fs::remove_file(marker))
    }

    fn commit_with(
        &mut self,
        remove_marker: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        // Final registry durability precedes this unlink. A leftover marker
        // must remain a blocked cleanup operation, never a successful login.
        self.marker.take();
        match remove_marker(&self.account_dir.join(PENDING_STORE_MARKER)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.keep = true;
        Ok(())
    }
}

impl Drop for PendingStore {
    fn drop(&mut self) {
        if !self.keep {
            if let Some(marker) = self.marker.take() {
                // Cancellation after a registry rename must preserve a saved
                // account. An ambiguous registry is never deletion consent.
                if matches!(
                    registered_account_on_disk(&self.data_dir, &self.id),
                    Ok(false)
                ) {
                    let _ = remove_pending_store(&self.account_dir, marker);
                }
            }
        }
    }
}

/// The marker is a retry record as well as an inter-process lock. Never
/// recursively remove its parent: Windows can delete the marker first and
/// then fail on a still-open SQLite file, making the leftover undiscoverable.
fn remove_pending_store(account_dir: &Path, marker: fs::File) -> std::io::Result<()> {
    remove_pending_store_with(account_dir, marker, |store| fs::remove_dir_all(store))
}

fn remove_pending_store_with(
    account_dir: &Path,
    marker: fs::File,
    remove_payload: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let reject = || std::io::Error::other("unfinished browser store contains unrecognized entries");
    let account_metadata = fs::symlink_metadata(account_dir)?;
    if !account_metadata.is_dir() || account_metadata.file_type().is_symlink() {
        return Err(reject());
    }
    // Only the two entries created by PendingStore belong to this cleanup.
    // Leave any unknown sibling or symlink untouched, including the payload.
    for entry in fs::read_dir(account_dir)? {
        let entry = entry?;
        if entry.file_name() != "store" && entry.file_name() != PENDING_STORE_MARKER {
            return Err(reject());
        }
    }
    let marker_path = account_dir.join(PENDING_STORE_MARKER);
    if !fs::symlink_metadata(&marker_path)?.is_file() {
        return Err(reject());
    }
    let store = account_dir.join("store");
    match fs::symlink_metadata(&store) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            // Keep marker's lock/handle alive until every payload file is gone.
            // A deletion failure returns with the marker still on disk; Drop
            // releases only its handle so a later cleanup can retry safely.
            remove_payload(&store)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err(reject()),
        Err(error) => return Err(error),
    }
    drop(marker);
    fs::remove_file(marker_path)?;
    // Nonrecursive removal cannot erase an unrelated file added meanwhile.
    fs::remove_dir(account_dir)
}

/// A valid owned marker on a registered account means its login commit or
/// marker cleanup was interrupted. Block it before any startup activation.
/// This recovery reads only files; explicit removal owns all vault cleanup.
pub(super) fn recover_pending_login_rows(data_dir: &Path, registry: &mut Registry) -> Result<()> {
    let mut recovered = registry.clone();
    let mut markers = Vec::new();
    for account in &registry.accounts {
        let account_dir = data_dir.join("accounts").join(&account.id);
        let marker_path = account_dir.join(PENDING_STORE_MARKER);
        match fs::symlink_metadata(&marker_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            _ => return Err(MatrixCoreError::AppPasswordUnavailable),
        }
        let directory = fs::symlink_metadata(&account_dir)?;
        if !directory.is_dir() || directory.file_type().is_symlink() {
            return Err(MatrixCoreError::AppPasswordUnavailable);
        }
        let mut marker = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&marker_path)?;
        marker
            .try_lock()
            .map_err(|_| MatrixCoreError::AccountAccessBusy)?;
        use std::io::Read as _;
        let mut version = [0; 19];
        if marker.metadata()?.len() != 19
            || marker.read_exact(&mut version).is_err()
            || &version != b"morpheus-pending-v1"
        {
            return Err(MatrixCoreError::AppPasswordUnavailable);
        }
        markers.push(marker);
        if !recovered.pending_removals.contains(&account.id) {
            recovered.pending_removals.push(account.id.clone());
        }
        if recovered.active_account_id.as_deref() == Some(account.id.as_str()) {
            recovered.active_account_id = None;
        }
    }
    if !markers.is_empty() {
        save_registry(data_dir, &recovered)?;
        // Even if directory sync fails, the caller must retain the blocked
        // in-memory row and refuse startup instead of assuming no rename.
        *registry = recovered;
        sync_registry_directory(data_dir)?;
    }
    Ok(())
}

/// SQLite's worker may release its last Windows file handle after the Client
/// drops. Retry interrupted cleanup on the next launch, without touching any
/// registered account, unrelated directory, or symlink.
pub(super) fn cleanup_pending_stores(data_dir: &Path, registry: &Registry) -> Result<()> {
    for entry in fs::read_dir(data_dir.join("accounts"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str() else { continue };
        if Uuid::parse_str(id).is_err()
            || registry.accounts.iter().any(|account| account.id == id)
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let marker = entry.path().join(PENDING_STORE_MARKER);
        if !fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Ok(mut file) = fs::OpenOptions::new().read(true).write(true).open(&marker) else {
            continue;
        };
        // Another Morpheus process may currently own this unfinished login.
        if file.try_lock().is_err() {
            continue;
        }
        use std::io::Read as _;
        let mut version = [0; 19];
        if file.read(&mut version).ok() != Some(19) || &version != b"morpheus-pending-v1" {
            continue;
        }
        // The caller's registry snapshot can predate another process's login.
        // That writer holds this marker lock through saving accounts.json, so
        // reading again while we own the lock closes commit's unlock/unlink
        // window. An unreadable or invalid registry is never deletion consent.
        match registered_account_on_disk(data_dir, id) {
            Ok(false) => {}
            Ok(true) => continue,
            Err(_) => {
                warn!("could not validate account registry; unfinished store cleanup deferred");
                continue;
            }
        }
        if remove_pending_store(&entry.path(), file).is_err() {
            warn!("could not remove unfinished browser sign-in store; retrying next launch");
        }
    }
    Ok(())
}

fn registered_account_on_disk(data_dir: &Path, id: &str) -> Result<bool> {
    let registry: Registry = match fs::read(data_dir.join("accounts.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry::default(),
        Err(error) => return Err(error.into()),
    };
    registry.validate()?;
    Ok(registry.accounts.iter().any(|account| account.id == id))
}

pub(super) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // The SDK uses this client for OAuth token exchange and refresh too.
        // A 307/308 must never forward credentials to a redirected endpoint.
        .redirect(reqwest::redirect::Policy::none())
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("Morpheus/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| MatrixCoreError::Sdk("could not initialize secure HTTP transport".into()))
}

fn client_metadata(redirect_url: &Url) -> Result<Raw<ClientMetadata>> {
    // RFC 8252 permits an ephemeral port for an IP loopback redirect. Register
    // the actual callback path and IP, with the port omitted.
    let mut registered_redirect = redirect_url.clone();
    registered_redirect
        .set_port(None)
        .map_err(|_| MatrixCoreError::InvalidSession)?;
    let metadata = ClientMetadata {
        client_name: Some(Localized::new("Morpheus".to_owned(), [])),
        ..ClientMetadata::new(
            ApplicationType::Native,
            vec![OAuthGrantType::AuthorizationCode {
                redirect_uris: vec![registered_redirect],
            }],
            Localized::new(
                Url::parse("https://github.com/brk275/Morpheus").unwrap(),
                [],
            ),
        )
    };
    Raw::new(&metadata).map_err(Into::into)
}

fn validate_callback(
    expected: &Url,
    state_parameter: &str,
    attempt_id: &str,
    callback: &Url,
) -> Result<()> {
    if callback.scheme() != expected.scheme()
        || callback.host_str() != expected.host_str()
        || callback.port_or_known_default() != expected.port_or_known_default()
        || callback.path() != expected.path()
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
    {
        return Err(MatrixCoreError::InvalidSsoCallback(
            "unexpected redirect origin or path".into(),
        ));
    }
    let mut states = callback
        .query_pairs()
        .filter(|(key, _)| key == state_parameter);
    if states.next().is_none_or(|(_, value)| value != attempt_id) || states.next().is_some() {
        return Err(MatrixCoreError::InvalidSsoCallback(
            "missing or mismatched state".into(),
        ));
    }
    Ok(())
}

impl SsoAttemptInfo {
    /// Validate an incoming callback before consuming the one-use attempt.
    /// Unrelated local requests must not cancel a browser login in progress.
    pub fn accepts_callback(&self, callback: &Url) -> bool {
        Url::parse(&self.redirect_url).is_ok_and(|redirect| {
            validate_callback(&redirect, self.state_parameter, &self.attempt_id, callback).is_ok()
        })
    }
}

impl AccountManager {
    /// Prefer delegated OAuth when advertised. Only an unsupported discovery
    /// endpoint falls back to legacy Matrix SSO; discovery failures stay errors.
    pub async fn begin_sso(
        &self,
        homeserver: &str,
        loopback_redirect_url: &str,
        identity_provider_id: Option<&str>,
    ) -> Result<SsoAttemptInfo> {
        let homeserver = validate_homeserver(homeserver)?;
        let mut redirect_url = validate_loopback_redirect(loopback_redirect_url)?;
        let discovery = Client::builder()
            .http_client(http_client()?)
            .homeserver_url(&homeserver)
            .build()
            .await
            .map_err(|_| {
                MatrixCoreError::Sdk("could not discover browser authentication".into())
            })?;
        let oauth_supported = match discovery.oauth().server_metadata().await {
            Ok(_) => true,
            Err(error) if error.is_not_supported() => false,
            Err(_) => {
                return Err(MatrixCoreError::Sdk(
                    "could not validate OAuth server metadata".into(),
                ));
            }
        };
        let (attempt_id, browser_url, state_parameter, oauth) = if oauth_supported {
            let store = PendingStore::new(self)?;
            let passphrase = Zeroizing::new(format!(
                "{}{}",
                Uuid::new_v4().simple(),
                Uuid::new_v4().simple()
            ));
            let client = build_client(&homeserver, &store.path, &passphrase).await?;
            // Pinned SDK owns S256 PKCE, CSRF state, requested Matrix device
            // scope, issuer validation, and registration as a public client.
            let authorization = client.oauth()
                .login(redirect_url.clone(), None, Some(client_metadata(&redirect_url)?.into()), None)
                .build().await
                .map_err(|_| MatrixCoreError::Sdk("OAuth browser sign-in could not start; the server must support native dynamic client registration".into()))?;
            (
                authorization.state.secret().clone(),
                authorization.url.to_string(),
                "state",
                Some(PendingOAuth {
                    client,
                    passphrase,
                    store,
                }),
            )
        } else {
            let attempt_id = Uuid::new_v4().to_string();
            redirect_url
                .query_pairs_mut()
                .append_pair("morpheus_state", &attempt_id);
            let browser_url = discovery
                .matrix_auth()
                .get_sso_login_url(redirect_url.as_str(), identity_provider_id)
                .await
                .map_err(|_| {
                    MatrixCoreError::Sdk("could not start Matrix browser sign-in".into())
                })?;
            (attempt_id, browser_url, "morpheus_state", None)
        };
        let mut state = self.state.lock().await;
        state
            .pending_sso
            .retain(|_, attempt| attempt.created_at.elapsed() < AUTH_ATTEMPT_TTL);
        state.pending_sso.insert(
            attempt_id.clone(),
            PendingSso {
                homeserver,
                redirect_url: redirect_url.clone(),
                state_parameter,
                created_at: Instant::now(),
                oauth,
            },
        );
        Ok(SsoAttemptInfo {
            attempt_id,
            browser_url,
            redirect_url: redirect_url.to_string(),
            state_parameter,
        })
    }

    /// Discard the PKCE verifier and unfinished store when the browser fails,
    /// the callback times out, or the user cancels sign-in.
    pub async fn cancel_sso(&self, attempt_id: &str) {
        let pending = self.state.lock().await.pending_sso.remove(attempt_id);
        if let Some(PendingSso {
            oauth: Some(pending),
            ..
        }) = pending
        {
            pending
                .client
                .oauth()
                .abort_login(&CsrfToken::new(attempt_id.to_owned()))
                .await;
        }
    }

    pub async fn finish_sso(&self, attempt_id: &str, callback_url: &str) -> Result<AccountInfo> {
        let callback = Url::parse(callback_url)
            .map_err(|_| MatrixCoreError::InvalidSsoCallback("invalid callback URL".into()))?;
        let mut state = self.state.lock().await;
        let pending = state
            .pending_sso
            .get(attempt_id)
            .ok_or(MatrixCoreError::InvalidSession)?;
        validate_callback(
            &pending.redirect_url,
            pending.state_parameter,
            attempt_id,
            &callback,
        )?;
        let pending = state
            .pending_sso
            .remove(attempt_id)
            .ok_or(MatrixCoreError::InvalidSession)?;
        if pending.created_at.elapsed() >= AUTH_ATTEMPT_TTL {
            return Err(MatrixCoreError::InvalidSsoCallback(
                "browser sign-in expired".into(),
            ));
        }
        // Keep this bundle intact: Rust drops fields in declaration order, so
        // the Client and its SQLite owner are released before the store guard.
        let mut login = if let Some(login) = pending.oauth {
            if login
                .client
                .oauth()
                .finish_login(callback.into())
                .await
                .is_err()
            {
                // Token exchange may have succeeded before /whoami or crypto
                // initialization failed. Revoke that partial remote session.
                if login.client.session_tokens().is_some() {
                    let _ =
                        tokio::time::timeout(Duration::from_secs(15), login.client.logout()).await;
                }
                login
                    .client
                    .oauth()
                    .abort_login(&CsrfToken::new(attempt_id.to_owned()))
                    .await;
                return Err(MatrixCoreError::Sdk(
                    "OAuth sign-in was cancelled or could not be completed".into(),
                ));
            }
            login
        } else {
            let mut tokens = callback
                .query_pairs()
                .filter(|(key, _)| key == "loginToken");
            let token = match tokens.next() {
                Some((_, token)) if !token.is_empty() && tokens.next().is_none() => {
                    Zeroizing::new(token.into_owned())
                }
                _ => {
                    return Err(MatrixCoreError::InvalidSsoCallback(
                        "missing or repeated login token".into(),
                    ));
                }
            };
            let store = PendingStore::new(self)?;
            let passphrase = Zeroizing::new(format!(
                "{}{}",
                Uuid::new_v4().simple(),
                Uuid::new_v4().simple()
            ));
            let client = build_client(&pending.homeserver, &store.path, &passphrase).await?;
            let login = PendingOAuth {
                client,
                passphrase,
                store,
            };
            login
                .client
                .matrix_auth()
                .login_token(&*token)
                .initial_device_display_name("Morpheus Desktop")
                .send()
                .await
                .map_err(|_| {
                    MatrixCoreError::Sdk("Matrix browser sign-in could not be completed".into())
                })?;
            login
        };
        let client = &login.client;
        let passphrase = &login.passphrase;
        let store = &mut login.store;
        let session = StoredSession::from_client(client).ok_or(MatrixCoreError::InvalidSession)?;
        let account = AccountInfo {
            id: store.id.clone(),
            user_id: session.meta().user_id.to_string(),
            device_id: session.meta().device_id.to_string(),
            homeserver: pending.homeserver,
        };
        let result = async {
            if state
                .registry
                .accounts
                .iter()
                .any(|a| a.user_id == account.user_id && a.device_id == account.device_id)
            {
                return Err(MatrixCoreError::DuplicateAccount);
            }
            let permit = self.access_permit()?;
            state.access = account_access::AccessFence::read(&self.data_dir)?;
            start_event_cache(client)?;
            self.persist_new_account(
                &mut state,
                &account,
                store,
                StoredSecret {
                    version: SECRET_VERSION,
                    store_passphrase: passphrase.to_string(),
                    session,
                },
                permit.clone(),
            )
            .await?;
            if state.access.enabled {
                return Ok(());
            }
            self.selections.replace(None);
            stop_sync(&mut state).await;
            let loaded = Arc::new(LoadedAccount::new(
                (*client).clone(),
                store.id.clone(),
                passphrase.to_string(),
                self._access_lock.clone(),
            ));
            state.loaded.insert(store.id.clone(), loaded.clone());
            state.active_account_id = Some(account.id.clone());
            self.publish_active_account(Some(account.clone()));
            self.start_sync(&mut state, loaded.client.clone()).await;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::time::timeout(Duration::from_secs(15), client.logout()).await;
            return Err(error);
        }
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        thread,
    };

    /// A loopback protocol fixture, with no real users, credentials or browser.
    /// It exercises the pinned SDK's discovery/registration and error callbacks.
    struct AuthServer {
        url: String,
        stopped: Arc<AtomicBool>,
        redirects_followed: Arc<AtomicUsize>,
        password_logins: Arc<AtomicUsize>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl AuthServer {
        fn start(discovery_status: u16) -> Self {
            Self::start_with_response_modes(discovery_status, &["query", "fragment"])
        }

        fn start_with_response_modes(discovery_status: u16, response_modes: &[&str]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let origin = url.clone();
            let response_modes: Vec<String> =
                response_modes.iter().map(|mode| (*mode).into()).collect();
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let redirects_followed = Arc::new(AtomicUsize::new(0));
            let redirects = redirects_followed.clone();
            let password_logins = Arc::new(AtomicUsize::new(0));
            let observed_password_logins = password_logins.clone();
            let worker = thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        Err(_) => break,
                    };
                    // Darwin inherits O_NONBLOCK from the listener. Reading an
                    // accepted socket before its request arrives must wait,
                    // rather than treating WouldBlock as an unknown endpoint.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let Ok(size) = stream.read(&mut chunk) else {
                            break;
                        };
                        if size == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..size]);
                        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                        if request.len() > 65536 {
                            break;
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("");
                    let (status, body, extra) = if path == "/_matrix/client/versions" {
                        (
                            200,
                            serde_json::json!({"versions":["v1.15"],"unstable_features":{"org.matrix.msc2965":true}}),
                            String::new(),
                        )
                    } else if path.ends_with("/auth_metadata") {
                        if discovery_status == 200 {
                            (
                                200,
                                serde_json::json!({
                                    "issuer": origin, "authorization_endpoint": format!("{origin}/authorize"),
                                    "token_endpoint": format!("{origin}/token"),
                                    "registration_endpoint": format!("{origin}/register"),
                                    "revocation_endpoint": format!("{origin}/revoke"),
                                    "response_types_supported": ["code"], "response_modes_supported": response_modes,
                                    "grant_types_supported": ["authorization_code", "refresh_token"],
                                    "code_challenge_methods_supported": ["S256"]
                                }),
                                String::new(),
                            )
                        } else {
                            (
                                discovery_status,
                                serde_json::json!({"errcode": if discovery_status == 404 { "M_UNRECOGNIZED" } else { "M_FORBIDDEN" }, "error":"fixture response"}),
                                String::new(),
                            )
                        }
                    } else if path.ends_with("/login") && request.starts_with("POST ") {
                        observed_password_logins.fetch_add(1, Ordering::SeqCst);
                        (
                            403,
                            serde_json::json!({"errcode":"M_FORBIDDEN", "error":"private-reflected-authentication-detail"}),
                            String::new(),
                        )
                    } else if path == "/register" {
                        (
                            201,
                            serde_json::json!({"client_id":"synthetic-native-client"}),
                            String::new(),
                        )
                    } else if path == "/redirect" {
                        (
                            307,
                            serde_json::json!({}),
                            format!("Location: {origin}/sink\r\n"),
                        )
                    } else if path == "/sink" {
                        redirects.fetch_add(1, Ordering::SeqCst);
                        (200, serde_json::json!({}), String::new())
                    } else {
                        (
                            404,
                            serde_json::json!({"errcode":"M_UNRECOGNIZED","error":"fixture endpoint unavailable"}),
                            String::new(),
                        )
                    };
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Self {
                url,
                stopped,
                redirects_followed,
                password_logins,
                worker: Some(worker),
            }
        }
    }

    impl Drop for AuthServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    #[test]
    fn auth_fixture_waits_for_delayed_and_split_requests() {
        let server = AuthServer::start(200);
        let mut stream = TcpStream::connect(server.url.trim_start_matches("http://")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        stream.write_all(b"GET /_matrix/client/v1/auth_").unwrap();
        thread::sleep(Duration::from_millis(50));
        stream
            .write_all(b"metadata HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 "));
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        // Exercise the pinned SDK's metadata type, including the required
        // response modes, before using this server for an OAuth flow.
        let metadata: matrix_sdk::ruma::api::client::discovery::get_authorization_server_metadata::v1::AuthorizationServerMetadata =
            serde_json::from_str(body).unwrap();
        metadata.insecure_validate_urls().unwrap();
    }

    struct PrivateTestDir(PathBuf);
    impl PrivateTestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("morpheus-auth-test-{}", Uuid::new_v4()));
            #[allow(unused_mut)]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for PrivateTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn assert_pending_removed(data_dir: &Path, path: &Path) {
        // SQLite workers may still be releasing Windows handles. Exercise the
        // same cleanup used on next launch rather than relying on unlink-open
        // semantics available only on Unix.
        for _ in 0..50 {
            cleanup_pending_stores(data_dir, &Registry::default()).unwrap();
            if !path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("unfinished browser store was not cleaned");
    }

    #[test]
    fn failed_password_login_sanitizes_response_and_leaves_no_reset_blocking_parent() {
        let directory = PrivateTestDir::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let manager = AccountManager::open(&directory.0).unwrap();
                manager
                    .set_app_password(None, Zeroizing::new("synthetic local password".into()))
                    .await
                    .unwrap();
                let server = AuthServer::start(404);
                let error = manager
                    .add_password_account(
                        &server.url,
                        "synthetic-user",
                        "incorrect fixture password",
                    )
                    .await
                    .unwrap_err();
                assert_eq!(server.password_logins.load(Ordering::SeqCst), 1);
                assert!(
                    !error
                        .to_string()
                        .contains("private-reflected-authentication-detail")
                );
                assert!(
                    error
                        .to_string()
                        .contains("Check your homeserver and credentials")
                );
                assert!(manager.accounts().await.is_empty());
                for attempt in 0..50 {
                    cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
                    if fs::read_dir(directory.0.join("accounts"))
                        .unwrap()
                        .next()
                        .is_none()
                    {
                        break;
                    }
                    assert!(
                        attempt < 49,
                        "failed password login left an account directory"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                manager.reset_app_password_after_removal().await.unwrap();
                assert!(!manager.app_password_status().await.enabled);
            });
    }

    #[test]
    fn failed_marker_commit_preserves_the_registered_store_and_recovery_record() {
        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let mut pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        save_registry(
            &directory.0,
            &Registry {
                accounts: vec![AccountInfo {
                    id: pending.id.clone(),
                    user_id: "@fixture:example.org".into(),
                    device_id: "FIXTURE".into(),
                    homeserver: "https://example.org".into(),
                }],
                ..Registry::default()
            },
        )
        .unwrap();
        assert!(
            pending
                .commit_with(|_| Err(std::io::Error::other("injected marker unlink failure")))
                .is_err()
        );
        drop(pending);
        assert!(account_dir.join("store").is_dir());
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());
    }

    #[test]
    fn pending_guard_drop_preserves_registered_or_unreadable_registry_data() {
        for invalid_registry in [false, true] {
            let directory = PrivateTestDir::new();
            let manager = AccountManager::open(&directory.0).unwrap();
            let pending = PendingStore::new(&manager).unwrap();
            let account_dir = pending.account_dir.clone();
            fs::write(pending.path.join("keep"), b"registered account payload").unwrap();
            if invalid_registry {
                fs::write(directory.0.join("accounts.json"), b"unreadable registry").unwrap();
            } else {
                save_registry(
                    &directory.0,
                    &Registry {
                        accounts: vec![AccountInfo {
                            id: pending.id.clone(),
                            user_id: "@fixture:example.org".into(),
                            device_id: "FIXTURE".into(),
                            homeserver: "https://example.org".into(),
                        }],
                        ..Registry::default()
                    },
                )
                .unwrap();
            }
            drop(pending);
            assert_eq!(
                fs::read(account_dir.join("store").join("keep")).unwrap(),
                b"registered account payload"
            );
            assert!(account_dir.join(PENDING_STORE_MARKER).is_file());
        }
    }

    #[test]
    fn startup_recovers_registered_marker_but_refuses_locked_or_malformed_marker() {
        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        let id = pending.id.clone();
        let registry = Registry {
            accounts: vec![AccountInfo {
                id: id.clone(),
                user_id: "@fixture:example.org".into(),
                device_id: "FIXTURE".into(),
                homeserver: "https://example.org".into(),
            }],
            active_account_id: Some(id.clone()),
            ..Registry::default()
        };
        save_registry(&directory.0, &registry).unwrap();
        let unknown = directory.0.join("accounts").join("unrelated");
        fs::create_dir(&unknown).unwrap();
        fs::write(unknown.join("keep"), b"unrelated payload").unwrap();
        drop(manager);
        assert!(matches!(
            AccountManager::open(&directory.0),
            Err(MatrixCoreError::AccountAccessBusy)
        ));
        drop(pending);
        let manager = AccountManager::open(&directory.0).unwrap();
        let recovered: Registry =
            serde_json::from_slice(&fs::read(directory.0.join("accounts.json")).unwrap()).unwrap();
        assert_eq!(recovered.pending_removals, [id.clone()]);
        assert!(recovered.active_account_id.is_none());
        assert!(account_dir.join("store").is_dir());
        assert_eq!(
            fs::read(unknown.join("keep")).unwrap(),
            b"unrelated payload"
        );
        drop(manager);
        fs::write(account_dir.join(PENDING_STORE_MARKER), b"malformed marker").unwrap();
        assert!(matches!(
            AccountManager::open(&directory.0),
            Err(MatrixCoreError::AppPasswordUnavailable)
        ));
        assert!(account_dir.join("store").is_dir());
        assert_eq!(
            fs::read(unknown.join("keep")).unwrap(),
            b"unrelated payload"
        );
    }

    #[test]
    fn pending_store_cleanup_preserves_active_and_registered_accounts() {
        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let mut pending = PendingStore::new(&manager).unwrap();
        assert!(pending.path.is_dir());
        assert!(pending.account_dir.join(PENDING_STORE_MARKER).is_file());
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(
            pending.path.is_dir(),
            "another process's active login must survive"
        );
        let id = pending.id.clone();
        let path = pending.path.clone();
        // Simulate a process that exited without removing its pending store.
        pending.keep = true;
        drop(pending);
        let registry = Registry {
            accounts: vec![AccountInfo {
                id,
                user_id: "@fixture:example.org".into(),
                device_id: "TESTDEVICE".into(),
                homeserver: "https://example.org".into(),
            }],
            ..Registry::default()
        };
        cleanup_pending_stores(&directory.0, &registry).unwrap();
        assert!(
            path.is_dir(),
            "a saved account must survive a leftover marker"
        );
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn pending_store_cleanup_rechecks_disk_registry_after_observing_stale_snapshot() {
        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let stale_registry = Registry::default();
        let mut pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        let registry = Registry {
            accounts: vec![AccountInfo {
                id: pending.id.clone(),
                user_id: "@fixture:example.org".into(),
                device_id: "TESTDEVICE".into(),
                homeserver: "https://example.org".into(),
            }],
            ..Registry::default()
        };
        // Match finish_sso's ordering: publish the registry under the marker
        // lock, then release it. Retain the marker to simulate unlink failure
        // or a cleanup that opened it just before commit could remove it.
        save_registry(&directory.0, &registry).unwrap();
        pending.keep = true;
        drop(pending);
        cleanup_pending_stores(&directory.0, &stale_registry).unwrap();
        assert!(account_dir.join("store").is_dir());
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());
    }

    #[test]
    fn pending_store_cleanup_defers_when_current_registry_cannot_be_validated() {
        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let mut pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        pending.keep = true;
        drop(pending);
        let registry_path = directory.0.join("accounts.json");

        fs::write(&registry_path, b"invalid registry JSON").unwrap();
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(account_dir.join("store").is_dir());
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());

        let mut unsupported = Registry::default();
        unsupported.version = REGISTRY_VERSION.wrapping_add(1);
        fs::write(&registry_path, serde_json::to_vec(&unsupported).unwrap()).unwrap();
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(account_dir.join("store").is_dir());
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());

        fs::remove_file(&registry_path).unwrap();
        fs::create_dir(&registry_path).unwrap();
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(account_dir.join("store").is_dir());
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());

        fs::remove_dir(&registry_path).unwrap();
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(!account_dir.exists());
    }

    #[test]
    fn pending_store_cleanup_keeps_locked_marker_when_payload_removal_fails() {
        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let mut pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        let payload = pending.path.join("database-fixture");
        fs::write(&payload, b"unfinished store fixture").unwrap();
        let marker = pending.marker.take().unwrap();
        pending.keep = true;
        drop(pending);
        let result = remove_pending_store_with(&account_dir, marker, |store| {
            assert_eq!(store, account_dir.join("store"));
            let contender = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(account_dir.join(PENDING_STORE_MARKER))
                .unwrap();
            assert!(
                matches!(contender.try_lock(), Err(fs::TryLockError::WouldBlock)),
                "the marker must stay locked throughout payload deletion"
            );
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected locked database file",
            ))
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read(account_dir.join(PENDING_STORE_MARKER)).unwrap(),
            b"morpheus-pending-v1"
        );
        assert_eq!(fs::read(&payload).unwrap(), b"unfinished store fixture");
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(
            !account_dir.exists(),
            "a later cleanup must still discover the store"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pending_store_windows_open_file_preserves_marker_until_retry_succeeds() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        let payload = pending.path.join("locked-sqlite-fixture");
        fs::write(&payload, b"unfinished store fixture").unwrap();
        // Windows denies deletion while this handle omits FILE_SHARE_DELETE.
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&payload)
            .unwrap();
        drop(pending);
        assert!(payload.exists());
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());
        drop(locked);
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(!account_dir.exists());
    }

    #[test]
    fn pending_store_cleanup_preserves_unknown_sibling_files() {
        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        let unknown = account_dir.join("owner-note");
        fs::write(&unknown, b"keep this unrelated file").unwrap();
        drop(pending);
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert_eq!(fs::read(&unknown).unwrap(), b"keep this unrelated file");
        assert!(account_dir.join("store").is_dir());
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());
        fs::remove_file(unknown).unwrap();
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(!account_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pending_store_cleanup_preserves_symlink_payload_and_target() {
        use std::os::unix::fs::symlink;

        let directory = PrivateTestDir::new();
        let manager = AccountManager::open(&directory.0).unwrap();
        let pending = PendingStore::new(&manager).unwrap();
        let account_dir = pending.account_dir.clone();
        let outside = directory.0.join("unrelated-directory");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"never follow the symlink").unwrap();
        fs::remove_dir(&pending.path).unwrap();
        symlink(&outside, &pending.path).unwrap();
        drop(pending);
        cleanup_pending_stores(&directory.0, &Registry::default()).unwrap();
        assert!(
            fs::symlink_metadata(account_dir.join("store"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(account_dir.join(PENDING_STORE_MARKER).is_file());
        assert_eq!(
            fs::read(outside.join("sentinel")).unwrap(),
            b"never follow the symlink"
        );
    }

    #[test]
    fn sdk_oauth_pkce_callback_denial_cancel_and_expiry() {
        let server = AuthServer::start(200);
        let directory = PrivateTestDir::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let manager = AccountManager::open(&directory.0).unwrap();
                let attempt = manager
                    .begin_sso(&server.url, "http://127.0.0.1:45678/callback", None)
                    .await
                    .unwrap();
                let authorization = Url::parse(&attempt.browser_url).unwrap();
                assert_eq!(
                    attempt.state_parameter, "state",
                    "OAuth fixture unexpectedly selected legacy SSO"
                );
                let parameters: HashMap<_, _> = authorization.query_pairs().into_owned().collect();
                assert!(
                    parameters
                        .get("state")
                        .is_some_and(|state| state == &attempt.attempt_id),
                    "SDK OAuth state was absent or mismatched"
                );
                assert_eq!(parameters["code_challenge_method"], "S256");
                assert!(!parameters["code_challenge"].is_empty());
                assert!(!parameters.contains_key("code_verifier"));
                assert_eq!(parameters["redirect_uri"], attempt.redirect_url);
                assert_eq!(attempt.state_parameter, "state");
                let path = manager.state.lock().await.pending_sso[&attempt.attempt_id]
                    .oauth
                    .as_ref()
                    .unwrap()
                    .store
                    .path
                    .clone();
                assert!(path.is_dir());

                assert!(
                    manager
                        .finish_sso(
                            &attempt.attempt_id,
                            "http://127.0.0.1:45678/callback?state=wrong&code=synthetic"
                        )
                        .await
                        .is_err()
                );
                assert!(
                    manager
                        .state
                        .lock()
                        .await
                        .pending_sso
                        .contains_key(&attempt.attempt_id)
                );
                let callback = format!(
                    "{}?state={}&error=access_denied&error_description=private-provider-detail",
                    attempt.redirect_url, attempt.attempt_id
                );
                let error = manager
                    .finish_sso(&attempt.attempt_id, &callback)
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(!error.contains("private-provider-detail"));
                assert_pending_removed(&directory.0, &path).await;
                assert!(
                    manager
                        .finish_sso(&attempt.attempt_id, &callback)
                        .await
                        .is_err()
                );

                let cancelled = manager
                    .begin_sso(&server.url, "http://127.0.0.1:45678/callback", None)
                    .await
                    .unwrap();
                let path = manager.state.lock().await.pending_sso[&cancelled.attempt_id]
                    .oauth
                    .as_ref()
                    .unwrap()
                    .store
                    .path
                    .clone();
                manager.cancel_sso(&cancelled.attempt_id).await;
                assert_pending_removed(&directory.0, &path).await;
                assert!(manager.state.lock().await.pending_sso.is_empty());

                let expired = manager
                    .begin_sso(&server.url, "http://127.0.0.1:45678/callback", None)
                    .await
                    .unwrap();
                manager
                    .state
                    .lock()
                    .await
                    .pending_sso
                    .get_mut(&expired.attempt_id)
                    .unwrap()
                    .created_at = Instant::now() - AUTH_ATTEMPT_TTL;
                let callback = format!(
                    "{}?state={}&code=synthetic",
                    expired.redirect_url, expired.attempt_id
                );
                assert!(
                    manager
                        .finish_sso(&expired.attempt_id, &callback)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("expired")
                );
                assert!(manager.accounts().await.is_empty());
            });
    }

    #[test]
    fn sdk_falls_back_only_for_unsupported_oauth_and_never_follows_token_redirects() {
        let directory = PrivateTestDir::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let manager = AccountManager::open(&directory.0).unwrap();
                let legacy = AuthServer::start(404);
                let attempt = manager
                    .begin_sso(&legacy.url, "http://127.0.0.1:45678/callback", None)
                    .await
                    .unwrap();
                assert_eq!(attempt.state_parameter, "morpheus_state");
                assert!(attempt.browser_url.contains("/login/sso/redirect"));
                manager.cancel_sso(&attempt.attempt_id).await;
                let failure = AuthServer::start(403);
                assert!(
                    manager
                        .begin_sso(&failure.url, "http://127.0.0.1:45678/callback", None)
                        .await
                        .is_err()
                );
                // A malformed successful discovery response must remain an
                // error, even when a legacy SSO endpoint could be constructed.
                let invalid = AuthServer::start_with_response_modes(200, &["query"]);
                assert!(
                    manager
                        .begin_sso(&invalid.url, "http://127.0.0.1:45678/callback", None)
                        .await
                        .is_err()
                );
                assert!(manager.state.lock().await.pending_sso.is_empty());
                let response = http_client()
                    .unwrap()
                    .post(format!("{}/redirect", legacy.url))
                    .body("synthetic-grant")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), 307);
                assert_eq!(legacy.redirects_followed.load(Ordering::SeqCst), 0);
            });
    }

    #[test]
    fn callback_requires_exact_origin_path_and_one_state_for_both_protocols() {
        let expected = Url::parse("http://127.0.0.1:45678/callback").unwrap();
        for name in ["state", "morpheus_state"] {
            let valid = format!("{expected}?{name}=test-state&code=test-code");
            assert!(
                validate_callback(&expected, name, "test-state", &Url::parse(&valid).unwrap())
                    .is_ok()
            );
            for invalid in [
                valid.replace("127.0.0.1", "localhost"),
                valid.replace(":45678", ":45679"),
                valid.replace("/callback", "/other"),
                valid.replace("test-state", "wrong-state"),
                format!("{valid}&{name}=test-state"),
                format!("{valid}#fragment"),
                valid.replace("http://", "http://user@"),
                expected.to_string(),
            ] {
                assert!(
                    validate_callback(
                        &expected,
                        name,
                        "test-state",
                        &Url::parse(&invalid).unwrap()
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn native_registration_preserves_callback_path_with_ephemeral_port() {
        let metadata =
            client_metadata(&Url::parse("http://127.0.0.1:45678/callback").unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(metadata.json().get()).unwrap();
        assert_eq!(value["application_type"], "native");
        assert_eq!(value["token_endpoint_auth_method"], "none");
        assert_eq!(
            value["redirect_uris"],
            serde_json::json!(["http://127.0.0.1/callback"])
        );
        assert!(
            value["grant_types"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "refresh_token")
        );
    }

    #[test]
    fn vault_legacy_shape_remains_readable_and_oauth_keeps_client_registration() {
        // Synthetic markers never authenticate to any server.
        let legacy = serde_json::json!({
            "version": 1, "store_passphrase": "synthetic-passphrase",
            "session": { "user_id": "@fixture:example.org", "device_id": "TESTDEVICE",
                "access_token": "synthetic-access", "refresh_token": "synthetic-refresh" }
        });
        let stored: StoredSecret = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(&stored).unwrap(), legacy);
        assert!(matches!(stored.session.into_auth(), AuthSession::Matrix(_)));

        let oauth = serde_json::json!({
            "version": 1, "store_passphrase": "synthetic-passphrase",
            "session": {"oauth": {"client_id": "synthetic-client", "user": legacy["session"]}}
        });
        let stored: StoredSecret = serde_json::from_value(oauth.clone()).unwrap();
        assert_eq!(serde_json::to_value(&stored).unwrap(), oauth);
        assert_eq!(
            stored.session.meta().user_id.as_str(),
            "@fixture:example.org"
        );
        let AuthSession::OAuth(session) = stored.session.into_auth() else {
            panic!("OAuth session lost")
        };
        assert_eq!(session.client_id.as_str(), "synthetic-client");
        assert_eq!(
            session.user.tokens.refresh_token.as_deref(),
            Some("synthetic-refresh")
        );
    }
}
