// Morpheus security patch, 2026. Licensed under Apache-2.0.
//! Credential-bearing endpoints must use TLS outside exact loopback hosts.
//! Validation errors deliberately omit the supplied URL.

use url::{Host, Url};

const INVALID: &str = "unsafe credential transport URL";

/// Authorization service base URL. Query, fragment, and userinfo are forbidden.
pub fn authorization_url(raw: &str) -> Result<Url, &'static str> {
    validate(raw, "https", "http", false)
}

/// SFU URL returned by the authorization service.
pub fn sfu_url(raw: &str) -> Result<Url, &'static str> {
    validate(raw, "wss", "ws", false)
}

/// LiveKit's public connection API also accepts the equivalent HTTP schemes.
/// Validate before the SDK adds its own query parameters, on every connection.
pub fn signaling_url(raw: &str) -> Result<Url, &'static str> {
    let mut url = Url::parse(raw).map_err(|_| INVALID)?;
    match url.scheme() {
        "https" => {
            url.set_scheme("wss").map_err(|_| INVALID)?;
        }
        "http" => {
            url.set_scheme("ws").map_err(|_| INVALID)?;
        }
        _ => {}
    }
    sfu_url(url.as_str())
}

/// Internal validation/region HTTP requests may contain SDK query parameters.
pub(crate) fn authenticated_http_url(raw: &str) -> Result<Url, &'static str> {
    validate(raw, "https", "http", true)
}

pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    }
}

fn validate(raw: &str, secure: &str, local: &str, allow_query: bool) -> Result<Url, &'static str> {
    if raw.chars().any(char::is_control) {
        return Err(INVALID);
    }
    let mut url = Url::parse(raw).map_err(|_| INVALID)?;
    if !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || (!allow_query && url.query().is_some())
    {
        return Err(INVALID);
    }
    if url.host_str() == Some("localhost") {
        url.set_host(Some("127.0.0.1")).map_err(|_| INVALID)?;
    }
    if url.scheme() != secure && !(url.scheme() == local && is_loopback(&url)) {
        return Err(INVALID);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_url_policy() {
        assert_eq!(
            authorization_url("https://rtc.example.org/prefix/").unwrap().path(),
            "/prefix/"
        );
        assert_eq!(
            authorization_url("http://localhost:18010").unwrap().host_str(),
            Some("127.0.0.1")
        );
        assert!(authorization_url("http://127.0.0.1:18010").is_ok());
        assert!(authorization_url("http://[::1]:18010").is_ok());
        for url in [
            "http://rtc.example.org",
            "http://192.168.1.2",
            "http://localhost.example.org",
            "http://rtc.localhost",
            "https://user:secret@example.org",
            "https://rtc.example.org?secret=hidden",
            "https://rtc.example.org#secret",
            "ftp://rtc.example.org",
            "https://rtc.example.org\n",
        ] {
            let error = authorization_url(url).unwrap_err();
            assert_eq!(error, INVALID);
            assert!(!error.contains("secret"));
        }
        assert!(sfu_url("wss://rtc.example.org/prefix").is_ok());
        assert_eq!(sfu_url("ws://localhost:18011").unwrap().host_str(), Some("127.0.0.1"));
        assert!(sfu_url("ws://rtc.example.org").is_err());
        assert!(sfu_url("https://rtc.example.org").is_err());
        assert!(sfu_url("wss://rtc.example.org?access_token=hidden").is_err());
        assert_eq!(signaling_url("https://rtc.example.org").unwrap().scheme(), "wss");
        assert!(signaling_url("http://rtc.example.org").is_err());
    }
}
