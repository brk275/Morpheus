//! System-browser Matrix SSO. The browser returns to a short-lived loopback
//! listener; no WebView or custom protocol handler is involved.

use std::sync::Arc;
use std::time::Duration;

use morpheus_matrix::{AccountInfo, AccountManager, SsoAttemptInfo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, timeout_at};
use url::Url;

const CALLBACK_PATH: &str = "/callback";
const AUTH_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_REQUEST_BYTES: usize = 8192;

struct CancelAttempt {
    manager: Arc<AccountManager>,
    attempt_id: Option<String>,
}

impl Drop for CancelAttempt {
    fn drop(&mut self) {
        if let Some(attempt_id) = self.attempt_id.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let manager = self.manager.clone();
            runtime.spawn(async move { manager.cancel_sso(&attempt_id).await });
        }
        // During runtime shutdown the manager's pending entries drop instead;
        // unfinished encrypted stores are also cleaned on the next launch.
    }
}

pub async fn sign_in(
    manager: Arc<AccountManager>,
    homeserver: String,
) -> Result<AccountInfo, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("Could not start browser sign-in callback: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("Could not read browser callback port: {error}"))?;
    let redirect = format!("http://{address}{CALLBACK_PATH}");
    let attempt = manager
        .begin_sso(&homeserver, &redirect, None)
        .await
        .map_err(|error| error.to_string())?;

    let mut cancellation = CancelAttempt {
        manager: manager.clone(),
        attempt_id: Some(attempt.attempt_id.clone()),
    };
    let result = complete_browser_sign_in(&manager, &attempt, listener, address).await;
    manager.cancel_sso(&attempt.attempt_id).await;
    cancellation.attempt_id = None;
    result
}

async fn complete_browser_sign_in(
    manager: &Arc<AccountManager>,
    attempt: &SsoAttemptInfo,
    listener: TcpListener,
    address: std::net::SocketAddr,
) -> Result<AccountInfo, String> {
    let login_url = Url::parse(&attempt.browser_url)
        .map_err(|_| "Homeserver returned an invalid browser sign-in URL".to_owned())?;
    let is_loopback = login_url
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]"));
    if login_url.scheme() != "https" && !(login_url.scheme() == "http" && is_loopback) {
        return Err("Browser sign-in requires an HTTPS URL".into());
    }
    webbrowser::open(login_url.as_str())
        .map_err(|error| format!("Could not open the system browser: {error}"))?;

    let deadline = Instant::now() + AUTH_TIMEOUT;
    loop {
        let (mut stream, peer) = timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| "Browser sign-in timed out".to_owned())?
            .map_err(|error| format!("Browser callback failed: {error}"))?;
        if !peer.ip().is_loopback() {
            respond(
                &mut stream,
                403,
                "This callback only accepts local requests",
            )
            .await;
            continue;
        }
        let target = match read_callback_target(&mut stream, deadline).await {
            Ok(Some(target)) => target,
            Ok(None) => {
                respond(&mut stream, 400, "Invalid callback request").await;
                continue;
            }
            Err(error) => {
                respond(&mut stream, 400, "Invalid callback request").await;
                return Err(error);
            }
        };
        let callback = format!("http://{address}{target}");
        let url = Url::parse(&callback).map_err(|_| "Invalid callback URL".to_owned())?;
        if !attempt.accepts_callback(&url) {
            respond(&mut stream, 400, "Invalid sign-in state").await;
            continue;
        }
        // Once accepted, finish credential-vault and registry persistence even
        // if the UI task is cancelled. Dropping halfway through a vault write
        // can otherwise leave a saved credential without its account registry.
        let manager = manager.clone();
        let attempt_id = attempt.attempt_id.clone();
        let result = tokio::spawn(async move { manager.finish_sso(&attempt_id, &callback).await })
            .await
            .map_err(|_| "Browser sign-in could not be completed".to_owned())?;
        match &result {
            Ok(_) => respond(&mut stream, 200, "Sign-in complete. Return to Morpheus.").await,
            Err(_) => respond(&mut stream, 400, "Sign-in failed. Return to Morpheus.").await,
        }
        return result.map_err(|error| error.to_string());
    }
}

async fn read_callback_target(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<Option<String>, String> {
    let mut bytes = [0u8; MAX_REQUEST_BYTES];
    let mut size = 0;
    while size < bytes.len() && !bytes[..size].contains(&b'\n') {
        let count = timeout_at(deadline, stream.read(&mut bytes[size..]))
            .await
            .map_err(|_| "Browser callback timed out".to_owned())?
            .map_err(|error| format!("Could not read browser callback: {error}"))?;
        if count == 0 {
            break;
        }
        size += count;
    }
    let request = std::str::from_utf8(&bytes[..size]).ok();
    let Some(line) = request.and_then(|request| request.lines().next()) else {
        return Ok(None);
    };
    let mut words = line.split_ascii_whitespace();
    let (Some("GET"), Some(target), Some("HTTP/1.1" | "HTTP/1.0"), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Ok(None);
    };
    if !target.starts_with(&format!("{CALLBACK_PATH}?"))
        || target.contains('#')
        || target.chars().any(char::is_control)
    {
        return Ok(None);
    }
    Ok(Some(target.to_owned()))
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Morpheus</title><body style=\"font:16px sans-serif;padding:3rem;background:#10131b;color:white\">{message}</body>"
    );
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}
