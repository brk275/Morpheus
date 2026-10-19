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

/// Called on the blocking pool after the account and room are checked again.
/// A same-directory temporary file prevents a failed write from truncating a
/// destination that the user chose in the native Save dialog.
pub fn save_download(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let directory = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Choose a full save path"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(path.to_path_buf())
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
        assert_eq!(mime_type_for(Path::new("photo.png")), "image/png");
        assert_eq!(
            mime_type_for(Path::new("unknown.oddextension")),
            "application/octet-stream"
        );
    }
}
