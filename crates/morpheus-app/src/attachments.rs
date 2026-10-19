//! Local attachment file helpers. Decrypted bytes are only written to a path
//! selected by the user; nothing is opened or run. The composer's upload
//! queue lives in `attachment_workbench`.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

pub fn mime_type_for(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

/// The name suggested to the native Save dialog for a name chosen by the
/// sender. Only the last path component is kept; control, direction and
/// zero-width characters and those Windows forbids become `_`; leading and
/// trailing dots and spaces are trimmed; reserved device names are prefixed;
/// and the result is bounded with its extension kept. The user still picks
/// the location, and nothing is opened or run.
pub fn safe_file_name(name: &str) -> String {
    const MAX_CHARS: usize = 120;
    const MAX_EXTENSION_CHARS: usize = 16;
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>'
                        | ':'
                        | '"'
                        | '|'
                        | '?'
                        | '*'
                        | '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    let trimmed =
        cleaned.trim_matches(|character: char| character == '.' || character.is_whitespace());
    let mut name = if trimmed.is_empty() {
        "attachment".to_owned()
    } else {
        trimmed.to_owned()
    };
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let numbered = stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit();
    if numbered || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        name.insert(0, '_');
    }
    if name.chars().count() > MAX_CHARS {
        let extension = name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_owned())
            .filter(|extension| {
                !extension.is_empty() && extension.chars().count() <= MAX_EXTENSION_CHARS
            });
        let reserved = extension
            .as_ref()
            .map_or(0, |extension| extension.chars().count() + 1);
        let keep = MAX_CHARS - reserved;
        let stem: String = name.chars().take(keep).collect();
        name = match extension {
            Some(extension) => format!("{stem}.{extension}"),
            None => stem,
        };
    }
    name
}

/// Called on the blocking pool after the account and room are checked again.
/// A same-directory temporary file prevents a failed write from truncating a
/// destination that the user chose in the native Save dialog.
pub fn save_download(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    save_download_as(path, bytes, true)
}

/// As `save_download`, replacing an existing destination only with `replace`.
/// Without it a file that already exists is left untouched and the write
/// fails with `AlreadyExists`; the temporary file is removed either way.
pub fn save_download_as(path: &Path, bytes: &[u8], replace: bool) -> io::Result<PathBuf> {
    let directory = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Choose a full save path"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    if replace {
        temporary.persist(path).map_err(|error| error.error)?;
    } else {
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
    }
    Ok(path.to_path_buf())
}

/// Submit a guarded plaintext save. A cancelled or abandoned operation never
/// starts a write; an admitted write must finish and report its real result.
/// Call only after account/room/selection guards have all been acquired.
pub(crate) fn begin_download_save(
    path: PathBuf,
    bytes: Vec<u8>,
    cancelled: &tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<io::Result<PathBuf>>> {
    let stopped = cancelled.borrow();
    if *stopped || cancelled.has_changed().is_err() {
        return None;
    }
    // The read guard orders cancellation with submitting the real write.
    Some(tokio::task::spawn_blocking(move || {
        save_download(&path, &bytes)
    }))
}

/// As `begin_download_save`, with `save_download_as`'s replace rule.
pub(crate) fn begin_download_save_as(
    path: PathBuf,
    bytes: Vec<u8>,
    replace: bool,
    cancelled: &tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<io::Result<PathBuf>>> {
    let stopped = cancelled.borrow();
    if *stopped || cancelled.has_changed().is_err() {
        return None;
    }
    Some(tokio::task::spawn_blocking(move || {
        save_download_as(&path, &bytes, replace)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_save_replaces_only_after_complete_write() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("attachment.bin");
        std::fs::write(&target, b"previous").unwrap();
        save_download(&target, b"decrypted media").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"decrypted media");
        // Without the Save dialog's confirmation an existing file is never
        // replaced, and no temporary file is left behind.
        let kept = save_download_as(&target, b"unconfirmed", false).unwrap_err();
        assert_eq!(kept.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target).unwrap(), b"decrypted media");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let fresh = dir.path().join("fresh.bin");
        assert_eq!(save_download_as(&fresh, b"new file", false).unwrap(), fresh);
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new file");
        assert_eq!(mime_type_for(Path::new("photo.png")), "image/png");
        assert_eq!(
            mime_type_for(Path::new("unknown.oddextension")),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_a_save_guard_never_writes() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("chosen.bin");
        std::fs::write(&target, b"original").unwrap();
        let gate = tokio::sync::Mutex::new(());
        let (cancel, signal) = tokio::sync::watch::channel(false);
        {
            let held = gate.lock().await;
            let delayed = async {
                let _guard = gate.lock().await;
                begin_download_save(target.clone(), b"cancelled plaintext".to_vec(), &signal)
            };
            tokio::pin!(delayed);
            assert!(futures_util::poll!(delayed.as_mut()).is_pending());
            cancel.send(true).unwrap();
            drop(held);
            assert!(delayed.await.is_none());
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        let (abandoned, signal) = tokio::sync::watch::channel(false);
        drop(abandoned);
        assert!(
            begin_download_save(target.clone(), b"abandoned plaintext".to_vec(), &signal).is_none()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        // After admission cancellation cannot turn a real write into a claim
        // that nothing was written. Its caller must await this exact result.
        let (cancel, signal) = tokio::sync::watch::channel(false);
        let admitted =
            begin_download_save(target.clone(), b"admitted plaintext".to_vec(), &signal).unwrap();
        cancel.send(true).unwrap();
        assert_eq!(admitted.await.unwrap().unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"admitted plaintext");
    }

    #[test]
    fn suggested_save_names_cannot_escape_or_disguise_the_chosen_folder() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("C:\\Users\\me\\evil.exe"), "evil.exe");
        assert_eq!(safe_file_name("a<b>:c|d?.png"), "a_b__c_d_.png");
        assert_eq!(safe_file_name("report\u{202e}fdp.exe"), "report_fdp.exe");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name(" . "), "attachment");
        assert_eq!(safe_file_name(""), "attachment");
        assert_eq!(safe_file_name("con.txt"), "_con.txt");
        assert_eq!(safe_file_name("COM3"), "_COM3");
        let long = format!("{}.jpeg", "x".repeat(300));
        let bounded = safe_file_name(&long);
        assert_eq!(bounded.chars().count(), 120);
        assert!(bounded.ends_with(".jpeg"));
    }
}
