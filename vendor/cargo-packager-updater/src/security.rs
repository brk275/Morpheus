// Morpheus security boundary for cargo-packager-updater 0.2.3.
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::{io::Read, time::Duration};

use reqwest::blocking::Client;
use url::Url;

use crate::{Error, Result};

pub(crate) const MAX_FEED_BYTES: u64 = 64 * 1024;
pub(crate) const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) fn validate_url(url: &Url) -> Result<()> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Security(
            "updates require an HTTPS URL without userinfo or fragments",
        ));
    }
    Ok(())
}

pub(crate) fn validate_public_key(key: &str) -> Result<()> {
    if key.len() > 4096 {
        return Err(Error::Security("invalid update public key"));
    }
    crate::base64_to_string(key)
        .and_then(|decoded| minisign_verify::PublicKey::decode(&decoded).map_err(Into::into))
        .map(|_| ())
        .map_err(|_| Error::Security("invalid update public key"))
}

pub(crate) fn client(timeout: Option<Duration>) -> Result<Client> {
    client_builder(timeout)
        .https_only(true)
        .build()
        .map_err(|_| Error::Security("could not configure update transport"))
}

fn client_builder(timeout: Option<Duration>) -> reqwest::blocking::ClientBuilder {
    Client::builder()
        // A feed must point at its final artifact URL. Redirects can cross
        // origins and leak access headers or downgrade transport security.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(
            timeout
                .unwrap_or(Duration::from_secs(120))
                .min(Duration::from_secs(300)),
        )
}

pub(crate) fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Security("update transfer was interrupted"))?;
    if bytes.len() as u64 > limit {
        return Err(Error::Security("update response exceeds the size limit"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::{io, io::Write, net::TcpListener};

    // Public verification vector from minisign-verify 0.2.5's verify_prehashed
    // test (ISC license). No private signing key is included or generated.
    fn signed_update() -> crate::Update {
        let key = "untrusted comment: minisign public key E7620F1842B4E81F\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let signature = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1556193335\tfile:test\ny/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==";
        crate::Update {
            config: crate::Config {
                pubkey: base64::engine::general_purpose::STANDARD.encode(key),
                ..Default::default()
            },
            body: None,
            current_version: "0.1.0".into(),
            version: "0.2.0".into(),
            date: None,
            target: "macos".into(),
            extract_path: std::env::temp_dir().join("must-not-be-modified.app"),
            download_url: "https://updates.example.org/app".parse().unwrap(),
            signature: base64::engine::general_purpose::STANDARD.encode(signature),
            timeout: None,
            headers: Default::default(),
            format: crate::UpdateFormat::App,
        }
    }

    #[test]
    fn exact_official_signature_required_and_install_stays_gated() {
        let update = signed_update();
        update.verify(b"test").unwrap();
        assert!(update.verify(b"tesu").is_err());
        assert!(update.verify(b"tes").is_err());
        assert!(update.verify(b"").is_err());
        assert!(matches!(
            update.install(b"test".to_vec()),
            Err(Error::InstallationNotValidated)
        ));
        assert!(matches!(
            update.install(b"corrupt".to_vec()),
            Err(Error::Security(_))
        ));
    }

    #[test]
    fn insecure_download_never_connects_even_on_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut update = signed_update();
        update.download_url = format!("http://{}/payload", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        assert!(update.download().is_err());
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn redirect_policy_never_contacts_the_destination() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let destination = format!(
            "http://{}/must-not-receive-headers",
            target.local_addr().unwrap()
        );
        for status in [301, 302, 303, 307, 308] {
            let source = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/feed", source.local_addr().unwrap());
            let destination = destination.clone();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = source.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).unwrap();
                write!(socket, "HTTP/1.1 {status} Redirect\r\nLocation: {destination}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            });
            // Exercise the exact production redirect/timeout builder over a
            // local HTTP fixture. Production adds https_only(true) above.
            let response = client_builder(Some(Duration::from_secs(2)))
                .no_proxy()
                .build()
                .unwrap()
                .get(url)
                .header("Authorization", "Bearer dummy-update-access")
                .send()
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            server.join().unwrap();
            assert!(
                matches!(target.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
            );
        }
    }

    #[test]
    fn bounded_or_interrupted_reads_never_return_partial_updates() {
        assert_eq!(read_bounded(&b"test"[..], 4).unwrap(), b"test");
        assert!(read_bounded(&b"test!"[..], 4).is_err());
        struct Interrupted;
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "reflected-sensitive-text",
                ))
            }
        }
        let error = read_bounded(Interrupted, 32).unwrap_err().to_string();
        assert!(!error.contains("reflected-sensitive-text"));
    }

    #[test]
    fn reject_credential_urls_and_malformed_keys() {
        for raw in [
            "http://example.org",
            "https://user:password@example.org",
            "https://example.org/#fragment",
            "file:///tmp/app",
        ] {
            assert!(validate_url(&raw.parse().unwrap()).is_err());
        }
        assert!(validate_url(&"https://example.org/app?download=1".parse().unwrap()).is_ok());
        assert!(validate_public_key("malformed-sensitive-key")
            .unwrap_err()
            .to_string()
            .contains("invalid update public key"));
    }
}
