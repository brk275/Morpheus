//! Bounded reads of the user-editable configuration and theme files, shared
//! by live reload, Settings saves and the offline validator.

use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::Path;

/// Largest accepted `config.toml`.
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;
/// Largest accepted theme file.
pub const MAX_THEME_BYTES: u64 = 64 * 1024;

/// Why a file could not be read. Fixed text only: no path, size or content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputError {
    Missing,
    /// A directory, FIFO, socket, device or other non-regular file.
    NotRegularFile,
    TooLarge,
    Unreadable,
    NotUtf8,
}

impl InputError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Missing => "missing-file",
            Self::NotRegularFile => "not-a-regular-file",
            Self::TooLarge => "file-too-large",
            Self::Unreadable => "unreadable-file",
            Self::NotUtf8 => "not-utf8",
        }
    }
}

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "the file does not exist",
            Self::NotRegularFile => "the path is not a regular file",
            Self::TooLarge => "the file is larger than the configuration limit",
            Self::Unreadable => "the file could not be read",
            Self::NotUtf8 => "the file is not UTF-8 text",
        })
    }
}

impl std::error::Error for InputError {}

/// Read a user-editable text file of at most `limit` bytes.
///
/// Symbolic links are followed for reading; nothing here writes. Only a
/// regular file is opened: a directory, FIFO, socket or device is refused
/// before the open, so an ordinary special file cannot block a reader or
/// stream into it. The type check and the open are separate system calls, so
/// a file replaced in between is not covered by that check; the size limit
/// is enforced on the bytes actually read either way.
pub fn read_bounded(path: &Path, limit: u64) -> Result<String, InputError> {
    let metadata = fs::metadata(path).map_err(open_error)?;
    if !metadata.is_file() {
        return Err(InputError::NotRegularFile);
    }
    if metadata.len() > limit {
        return Err(InputError::TooLarge);
    }
    let file = fs::File::open(path).map_err(open_error)?;
    match file.metadata() {
        Ok(opened) if opened.is_file() => {}
        Ok(_) => return Err(InputError::NotRegularFile),
        Err(_) => return Err(InputError::Unreadable),
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| InputError::Unreadable)?;
    if bytes.len() as u64 > limit {
        return Err(InputError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| InputError::NotUtf8)
}

fn open_error(error: std::io::Error) -> InputError {
    match error.kind() {
        ErrorKind::NotFound => InputError::Missing,
        _ => InputError::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_bounded_regular_utf8_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        assert_eq!(read_bounded(&path, 16), Err(InputError::Missing));
        fs::write(&path, "x = 1\n").unwrap();
        assert_eq!(read_bounded(&path, 16).as_deref(), Ok("x = 1\n"));
        assert_eq!(read_bounded(&path, 6).as_deref(), Ok("x = 1\n"));
        assert_eq!(read_bounded(&path, 5), Err(InputError::TooLarge));
        fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
        assert_eq!(read_bounded(&path, 16), Err(InputError::NotUtf8));
        assert_eq!(
            read_bounded(directory.path(), 16),
            Err(InputError::NotRegularFile)
        );
        // Diagnostics carry fixed text, never the path or content.
        for error in [
            InputError::Missing,
            InputError::NotRegularFile,
            InputError::TooLarge,
            InputError::Unreadable,
            InputError::NotUtf8,
        ] {
            assert!(!error.to_string().contains("config.toml"));
            assert!(!error.code().is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_followed_only_to_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.toml");
        fs::write(&target, "x = 1\n").unwrap();
        let link = directory.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_bounded(&link, 16).as_deref(), Ok("x = 1\n"));
        let to_directory = directory.path().join("directory.toml");
        std::os::unix::fs::symlink(directory.path(), &to_directory).unwrap();
        assert_eq!(
            read_bounded(&to_directory, 16),
            Err(InputError::NotRegularFile)
        );
        let dangling = directory.path().join("dangling.toml");
        std::os::unix::fs::symlink(directory.path().join("absent"), &dangling).unwrap();
        assert_eq!(read_bounded(&dangling, 16), Err(InputError::Missing));
    }

    /// A FIFO with no writer would block an open for reading. It is refused
    /// before the open. The standard library cannot create one, so this test
    /// uses the system `mkfifo` utility and is skipped where it is missing.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("config.toml");
        let created = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|status| status.success());
        if !created {
            return;
        }
        let link = directory.path().join("linked.toml");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send((read_bounded(&fifo, 16), read_bounded(&link, 16)));
        });
        let results = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a FIFO must not block");
        assert_eq!(
            results,
            (
                Err(InputError::NotRegularFile),
                Err(InputError::NotRegularFile)
            )
        );
    }
}
