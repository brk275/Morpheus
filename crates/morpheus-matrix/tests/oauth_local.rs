//! Opt-in delegated OAuth protocol acceptance against real local MAS/Synapse.
//! The harness runs each phase in a separate process with a disposable Linux
//! Secret Service vault. HTTP form automation does not test an OS browser UI.
#![cfg(target_os = "linux")]

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use matrix_sdk::{Client, SessionTokens};
use morpheus_matrix::{AccountInfo, AccountManager};
use serde::Deserialize;
use tokio::time::{Instant, sleep, timeout};
use url::Url;
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, &'static str>;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const VAULT_TIMEOUT: Duration = Duration::from_secs(15);

const BROWSER_FAILURES: &[&str] = &[
    "browser_protocol",
    "browser_callback_fragment",
    "browser_callback_query",
    "browser_callback_state",
    "browser_callback_code",
    "browser_oauth_access_denied",
    "browser_oauth_login_required",
    "browser_oauth_consent_required",
    "browser_oauth_interaction_required",
    "browser_oauth_invalid_request",
    "browser_oauth_server_error",
    "browser_oauth_other",
    "browser_helper_exit",
    "browser_helper_output_size",
    "browser_helper_output_schema",
    "browser_callback_url",
    "browser_callback_match",
];

fn browser_failure_marker(line: &str) -> Option<&'static str> {
    let value = line.strip_prefix("MORPHEUS_OAUTH_FAILURE ")?;
    BROWSER_FAILURES
        .iter()
        .copied()
        .find(|known| *known == value)
}

#[derive(Clone, Copy)]
enum Stage {
    FixtureLoad,
    ProfileOpen,
    CancelBegin,
    CancelVerify,
    LoginBegin,
    BrowserForms,
    BrowserAuthorize,
    BrowserLogin,
    BrowserConsent,
    BrowserCallback,
    LoginFinish,
    LoginIdentity,
    CallbackReplay,
    ManualRefresh,
    VaultPersist,
    ProfileScan,
    Restore,
    RestoreIdentity,
    ExpiryWait,
    ExpiredTokenProbe,
    AutomaticRefresh,
    Logout,
    LogoutRemote,
    LogoutAccount,
    LogoutVault,
}

fn stage(value: Stage) {
    let name = match value {
        Stage::FixtureLoad => "fixture_load",
        Stage::ProfileOpen => "profile_open",
        Stage::CancelBegin => "cancel_begin",
        Stage::CancelVerify => "cancel_verify",
        Stage::LoginBegin => "login_begin",
        Stage::BrowserForms => "browser_forms",
        Stage::BrowserAuthorize => "browser_authorize",
        Stage::BrowserLogin => "browser_login",
        Stage::BrowserConsent => "browser_consent",
        Stage::BrowserCallback => "browser_callback",
        Stage::LoginFinish => "login_finish",
        Stage::LoginIdentity => "login_identity",
        Stage::CallbackReplay => "callback_replay",
        Stage::ManualRefresh => "manual_refresh",
        Stage::VaultPersist => "vault_persist",
        Stage::ProfileScan => "profile_scan",
        Stage::Restore => "restore",
        Stage::RestoreIdentity => "restore_identity",
        Stage::ExpiryWait => "expiry_wait",
        Stage::ExpiredTokenProbe => "expired_token_probe",
        Stage::AutomaticRefresh => "automatic_refresh",
        Stage::Logout => "logout",
        Stage::LogoutRemote => "logout_remote",
        Stage::LogoutAccount => "logout_account",
        Stage::LogoutVault => "logout_vault",
    };
    println!("\nMORPHEUS_OAUTH_STAGE {name}");
}

#[derive(Deserialize)]
struct Fixture {
    homeserver_url: String,
    issuer_url: String,
    username: String,
    password: String,
    user_id: String,
    access_token_ttl_seconds: u64,
}

fn private_path(path: &Path, directory: bool) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "fixture path is unavailable")?;
    if metadata.file_type().is_symlink()
        || metadata.is_dir() != directory
        || (!directory && !metadata.is_file())
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("fixture path is not private");
    }
    let path = path
        .canonicalize()
        .map_err(|_| "fixture path cannot be resolved")?;
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("checkout path is unavailable")?
        .canonicalize()
        .map_err(|_| "checkout path cannot be resolved")?;
    if path.starts_with(checkout) {
        return Err("fixture data must be outside the checkout");
    }
    Ok(path)
}

fn loopback_endpoint(value: &str, scheme: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| "invalid fixture endpoint")?;
    if url.scheme() != scheme
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("fixture endpoints must use explicit IPv4 loopback ports");
    }
    Ok(url)
}

fn browser_callback(
    fixture: &Fixture,
    authorization_url: &str,
    redirect_url: &str,
) -> Result<String> {
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/local_matrix_auth.py");
    let payload = Zeroizing::new(
        serde_json::to_vec(&serde_json::json!({
            "authorization_url": authorization_url,
            "redirect_url": redirect_url,
            "issuer_url": fixture.issuer_url,
            "username": fixture.username,
            "password": fixture.password,
        }))
        .map_err(|_| "could not prepare browser form input")?,
    );
    let mut child = Command::new("python3")
        .arg(helper)
        .arg("browser")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "could not start browser form driver")?;
    let write = child
        .stdin
        .take()
        .ok_or("browser form input is unavailable")?
        .write_all(&payload);
    if write.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err("could not send browser form input");
    }
    // The helper enforces its own request/redirect budget. Its output, error
    // bodies and callback code are captured in memory and never formatted.
    let output = child
        .wait_with_output()
        .map_err(|_| "browser form driver failed")?;
    // The helper's stderr may also contain private diagnostics. Forward only
    // exact fixed markers, through our enum; never print a raw helper line.
    let mut helper_failure = None;
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        match line {
            "MORPHEUS_OAUTH_STAGE browser_authorize" => stage(Stage::BrowserAuthorize),
            "MORPHEUS_OAUTH_STAGE browser_login" => stage(Stage::BrowserLogin),
            "MORPHEUS_OAUTH_STAGE browser_consent" => stage(Stage::BrowserConsent),
            "MORPHEUS_OAUTH_STAGE browser_callback" => stage(Stage::BrowserCallback),
            _ => {
                if let Some(failure) = browser_failure_marker(line) {
                    helper_failure = Some(failure);
                }
            }
        }
    }
    if let Some(failure) = helper_failure {
        return Err(failure);
    }
    if !output.status.success() {
        return Err("browser_helper_exit");
    }
    if output.stdout.len() > 16_384 {
        return Err("browser_helper_output_size");
    }
    #[derive(Deserialize)]
    struct Callback {
        callback_url: String,
    }
    serde_json::from_slice::<Callback>(&output.stdout)
        .map(|result| result.callback_url)
        .map_err(|_| "browser_helper_output_schema")
}

async fn whoami(client: &Client, account: &AccountInfo) -> Result<()> {
    let identity = timeout(REQUEST_TIMEOUT, client.whoami())
        .await
        .map_err(|_| "authenticated request timed out")?
        .map_err(|_| "authenticated request failed")?;
    if identity.user_id.as_str() != account.user_id
        || identity.device_id.as_ref().map(|id| id.as_str()) != Some(account.device_id.as_str())
    {
        return Err("authenticated identity or device changed");
    }
    Ok(())
}

fn tokens(client: &Client) -> Result<SessionTokens> {
    if client.oauth().full_session().is_none() {
        return Err("login did not create a delegated OAuth session");
    }
    let tokens = client.session_tokens().ok_or("session has no tokens")?;
    if tokens.refresh_token.is_none() {
        return Err("delegated session has no refresh token");
    }
    Ok(tokens)
}

async fn require_vault_tokens(account: &AccountInfo, expected: &SessionTokens) -> Result<()> {
    stage(Stage::VaultPersist);
    let deadline = Instant::now() + VAULT_TIMEOUT;
    loop {
        let id = account.id.clone();
        let saved = tokio::task::spawn_blocking(move || {
            keyring::Entry::new("Morpheus Matrix", &id)
                .ok()
                .and_then(|entry| entry.get_password().ok())
                .map(Zeroizing::new)
        })
        .await
        .map_err(|_| "vault inspection task failed")?;
        if let Some(saved) = saved {
            let value: serde_json::Value =
                serde_json::from_str(&saved).map_err(|_| "vault session is not valid JSON")?;
            // matrix-sdk UserSession flattens SessionMeta and SessionTokens.
            let actual = &value["session"]["oauth"]["user"];
            if actual["access_token"].as_str() == Some(expected.access_token.as_str())
                && actual["refresh_token"].as_str() == expected.refresh_token.as_deref()
            {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("refreshed OAuth tokens were not saved to the OS vault");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn require_no_plaintext_session(profile: &Path, values: &[&str]) -> Result<()> {
    stage(Stage::ProfileScan);
    let mut directories = vec![profile.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).map_err(|_| "cannot inspect the account store")? {
            let entry = entry.map_err(|_| "cannot inspect an account store entry")?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "cannot inspect account store metadata")?;
            if metadata.file_type().is_symlink() {
                return Err("fresh OAuth account store contains a symbolic link");
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            } else if metadata.is_file() {
                if metadata.len() > 64 * 1024 * 1024 {
                    return Err("fresh OAuth test store exceeded its inspection budget");
                }
                let bytes =
                    fs::read(entry.path()).map_err(|_| "cannot inspect account store bytes")?;
                if values.iter().any(|value| {
                    !value.is_empty()
                        && bytes
                            .windows(value.len())
                            .any(|window| window == value.as_bytes())
                }) {
                    return Err("OAuth credential was written outside the OS vault");
                }
            }
        }
    }
    Ok(())
}

async fn login_and_refresh(
    manager: &AccountManager,
    fixture: &Fixture,
    profile: &Path,
) -> Result<()> {
    if !manager.accounts().await.is_empty() {
        return Err("OAuth fixture profile was not fresh");
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|_| "cannot reserve a loopback callback")?;
    let redirect = format!(
        "http://{}/callback",
        listener
            .local_addr()
            .map_err(|_| "callback address unavailable")?
    );

    stage(Stage::CancelBegin);
    let cancelled = timeout(
        REQUEST_TIMEOUT,
        manager.begin_sso(&fixture.homeserver_url, &redirect, None),
    )
    .await
    .map_err(|_| "cancelled OAuth start timed out")?
    .map_err(|_| "cancelled OAuth start failed")?;
    stage(Stage::CancelVerify);
    manager.cancel_sso(&cancelled.attempt_id).await;
    let mut cancelled_callback = Url::parse(&redirect).map_err(|_| "invalid callback")?;
    cancelled_callback
        .query_pairs_mut()
        .append_pair("state", &cancelled.attempt_id)
        .append_pair("code", "unused-fixture-code");
    if manager
        .finish_sso(&cancelled.attempt_id, cancelled_callback.as_str())
        .await
        .is_ok()
        || !manager.accounts().await.is_empty()
    {
        return Err("cancelled OAuth attempt created an account");
    }
    if fs::read_dir(profile.join("accounts"))
        .map_err(|_| "cannot inspect cancelled store")?
        .next()
        .is_some()
    {
        return Err("cancelled OAuth attempt retained its pending store");
    }
    println!("\nMORPHEUS_OAUTH_CHECK cancelled=true");

    stage(Stage::LoginBegin);
    let attempt = timeout(
        REQUEST_TIMEOUT,
        manager.begin_sso(&fixture.homeserver_url, &redirect, None),
    )
    .await
    .map_err(|_| "OAuth start timed out")?
    .map_err(|_| "OAuth start failed")?;
    let authorization =
        Url::parse(&attempt.browser_url).map_err(|_| "invalid authorization URL")?;
    let issuer = loopback_endpoint(&fixture.issuer_url, "https")?;
    if authorization.origin() != issuer.origin()
        || !authorization
            .query_pairs()
            .any(|(key, value)| key == "code_challenge_method" && value == "S256")
    {
        return Err("OAuth did not use the fixture issuer and S256 PKCE");
    }
    stage(Stage::BrowserForms);
    let callback = Zeroizing::new(browser_callback(
        fixture,
        &attempt.browser_url,
        &attempt.redirect_url,
    )?);
    if !attempt.accepts_callback(&Url::parse(&callback).map_err(|_| "browser_callback_url")?) {
        return Err("browser_callback_match");
    }
    stage(Stage::LoginFinish);
    let account = timeout(
        REQUEST_TIMEOUT,
        manager.finish_sso(&attempt.attempt_id, &callback),
    )
    .await
    .map_err(|_| "OAuth completion timed out")?
    .map_err(|_| "OAuth completion failed")?;
    if account.user_id != fixture.user_id {
        return Err("OAuth returned a different fixture account");
    }
    stage(Stage::CallbackReplay);
    if manager
        .finish_sso(&attempt.attempt_id, &callback)
        .await
        .is_ok()
    {
        return Err("OAuth callback was accepted twice");
    }
    stage(Stage::LoginIdentity);
    let client = manager
        .active_client()
        .await
        .map_err(|_| "OAuth client is unavailable")?;
    whoami(&client, &account).await?;
    println!("\nMORPHEUS_OAUTH_CHECK login=true");
    println!("\nMORPHEUS_OAUTH_CHECK callback_replay_rejected=true");
    manager.stop().await;
    let before = tokens(&client)?;
    stage(Stage::ManualRefresh);
    timeout(REQUEST_TIMEOUT, client.oauth().refresh_access_token())
        .await
        .map_err(|_| "OAuth refresh timed out")?
        .map_err(|_| "OAuth refresh failed")?;
    let after = tokens(&client)?;
    if before.access_token == after.access_token || before.refresh_token == after.refresh_token {
        return Err("MAS did not rotate both delegated tokens");
    }
    whoami(&client, &account).await?;
    require_vault_tokens(&account, &after).await?;
    require_no_plaintext_session(
        profile,
        &[
            &fixture.password,
            &before.access_token,
            before.refresh_token.as_deref().unwrap_or(""),
            &after.access_token,
            after.refresh_token.as_deref().unwrap_or(""),
        ],
    )?;
    println!("\nMORPHEUS_OAUTH_CHECK refresh=true");
    println!("\nMORPHEUS_OAUTH_CHECK vaulted=true");
    println!("\nMORPHEUS_OAUTH_CHECK no_plaintext_credentials=true");
    Ok(())
}

async fn restore_and_refresh(
    manager: &AccountManager,
    fixture: &Fixture,
    profile: &Path,
) -> Result<()> {
    stage(Stage::Restore);
    let expected = manager.accounts().await;
    if expected.len() != 1 {
        return Err("OAuth restore requires exactly one existing account");
    }
    let account = timeout(REQUEST_TIMEOUT, manager.restore_last_active())
        .await
        .map_err(|_| "vaulted OAuth restore timed out")?
        .map_err(|_| "vaulted OAuth restore failed")?
        .ok_or("no OAuth account was restored")?;
    if account.id != expected[0].id
        || account.device_id != expected[0].device_id
        || account.user_id != fixture.user_id
    {
        return Err("vaulted OAuth restore changed the account or device identity");
    }
    stage(Stage::RestoreIdentity);
    let client = manager
        .active_client()
        .await
        .map_err(|_| "restored client unavailable")?;
    whoami(&client, &account).await?;
    println!("\nMORPHEUS_OAUTH_CHECK restored=true");
    manager.stop().await;
    let before = tokens(&client)?;
    // Use real expiry, with the server's supported 60-second fixture TTL.
    // No database edits, mocked 401, forced clocks or manual SDK refresh here.
    stage(Stage::ExpiryWait);
    sleep(Duration::from_secs(fixture.access_token_ttl_seconds + 3)).await;
    stage(Stage::ExpiredTokenProbe);
    let stale = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| "expiry probe unavailable")?
        .get(format!(
            "{}/_matrix/client/v3/account/whoami",
            fixture.homeserver_url.trim_end_matches('/')
        ))
        .bearer_auth(&before.access_token)
        .send()
        .await
        .map_err(|_| "expiry probe failed")?;
    if stale.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Err("the original delegated access token did not expire");
    }
    stage(Stage::AutomaticRefresh);
    whoami(&client, &account).await?;
    let after = tokens(&client)?;
    if before.access_token == after.access_token || before.refresh_token == after.refresh_token {
        return Err("SDK did not automatically rotate expired delegated tokens");
    }
    require_vault_tokens(&account, &after).await?;
    require_no_plaintext_session(
        profile,
        &[
            &after.access_token,
            after.refresh_token.as_deref().unwrap_or(""),
        ],
    )?;
    println!("\nMORPHEUS_OAUTH_CHECK expired_token_rejected=true");
    println!("\nMORPHEUS_OAUTH_CHECK automatic_refresh=true");
    println!("\nMORPHEUS_OAUTH_CHECK vaulted=true");
    stage(Stage::Logout);
    let result = timeout(REQUEST_TIMEOUT, manager.logout_account(&account.id))
        .await
        .map_err(|_| "OAuth logout timed out")?
        .map_err(|_| "OAuth logout failed")?;
    stage(Stage::LogoutRemote);
    if !result.remote_logout_succeeded {
        return Err("OAuth remote logout failed");
    }
    println!("\nMORPHEUS_OAUTH_CHECK remote_logout=true");
    stage(Stage::LogoutAccount);
    if !manager.accounts().await.is_empty() {
        return Err("OAuth logout did not remove the account");
    }
    println!("\nMORPHEUS_OAUTH_CHECK account_removed=true");
    stage(Stage::LogoutVault);
    let entry = keyring::Entry::new("Morpheus Matrix", &account.id)
        .map_err(|_| "vault entry lookup failed")?;
    if !matches!(entry.get_password(), Err(keyring::Error::NoEntry)) {
        return Err("OAuth logout retained the vault credential");
    }
    println!("\nMORPHEUS_OAUTH_CHECK vault_removed=true");
    println!("\nMORPHEUS_OAUTH_CHECK logout=true");
    Ok(())
}

#[test]
fn delegated_oauth_success_refresh_and_vaulted_restart() {
    let Some(fixture_path) = std::env::var_os("MORPHEUS_MATRIX_OAUTH_FIXTURE") else {
        eprintln!("Skipped delegated OAuth acceptance: no disposable fixture configured");
        return;
    };
    let result: Result<()> = (|| {
        stage(Stage::FixtureLoad);
        let fixture_path = private_path(Path::new(&fixture_path), false)?;
        let fixture_bytes =
            Zeroizing::new(fs::read(fixture_path).map_err(|_| "cannot read OAuth fixture")?);
        let fixture: Fixture =
            serde_json::from_slice(&fixture_bytes).map_err(|_| "invalid OAuth fixture")?;
        loopback_endpoint(&fixture.homeserver_url, "http")?;
        // RFC 7009 requires HTTPS even for loopback token revocation. Use the
        // runner's installed disposable CA through the ordinary SDK TLS client.
        loopback_endpoint(&fixture.issuer_url, "https")?;
        if fixture.access_token_ttl_seconds != 60 {
            return Err("OAuth fixture must use the supported 60-second TTL");
        }
        let profile =
            std::env::var_os("MORPHEUS_MATRIX_OAUTH_PROFILE").ok_or("OAuth profile unavailable")?;
        let profile = private_path(Path::new(&profile), true)?;
        let phase =
            std::env::var("MORPHEUS_MATRIX_OAUTH_PHASE").map_err(|_| "OAuth phase unavailable")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "OAuth runtime unavailable")?;
        runtime.block_on(async {
            stage(Stage::ProfileOpen);
            let manager =
                AccountManager::open(&profile).map_err(|_| "cannot open private OAuth profile")?;
            let outcome = match phase.as_str() {
                "login_refresh" => login_and_refresh(&manager, &fixture, &profile).await,
                "restore_refresh" => restore_and_refresh(&manager, &fixture, &profile).await,
                _ => Err("unsupported OAuth acceptance phase"),
            };
            manager.stop().await;
            if outcome.is_err() {
                for account in manager.accounts().await {
                    let _ = timeout(REQUEST_TIMEOUT, manager.logout_account(&account.id)).await;
                }
            }
            outcome
        })
    })();
    if let Err(error) = result {
        let failure = match error {
            value if BROWSER_FAILURES.contains(&value) => value,
            "OAuth logout timed out" => "logout_timeout",
            "OAuth logout failed" => "logout_operation",
            "OAuth remote logout failed" => "logout_remote",
            "OAuth logout did not remove the account" => "logout_account",
            "vault entry lookup failed" => "logout_vault_lookup",
            "OAuth logout retained the vault credential" => "logout_vault_retained",
            _ => "rust_acceptance",
        };
        println!("\nMORPHEUS_OAUTH_FAILURE {failure}");
    }
    // Only our static stage identifiers can reach a panic/public test output.
    assert!(
        result.is_ok(),
        "Delegated OAuth acceptance failed: {result:?}"
    );
}

#[test]
fn browser_failure_diagnostics_only_forward_exact_fixed_codes() {
    assert_eq!(
        browser_failure_marker("MORPHEUS_OAUTH_FAILURE browser_callback_state"),
        Some("browser_callback_state")
    );
    for line in [
        "MORPHEUS_OAUTH_FAILURE browser_callback_state private-value",
        "MORPHEUS_OAUTH_FAILURE unknown_provider_error",
        "MORPHEUS_OAUTH_FAILURE https://example.org/callback?code=private",
    ] {
        assert!(browser_failure_marker(line).is_none());
    }
}
