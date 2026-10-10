//! Native attachment dialogs and local transfer state. Decrypted bytes are
//! only written to a path selected by the user; nothing is opened or run.

use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadCommand {
    Cancel,
    Retry,
}

#[derive(Default)]
pub struct UploadController {
    current: Mutex<HashMap<i32, mpsc::UnboundedSender<UploadCommand>>>,
}

impl UploadController {
    pub fn begin(&self, operation: i32) -> mpsc::UnboundedReceiver<UploadCommand> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut current = self.current.lock().expect("Upload controller poisoned");
        if let Some(old) = current.insert(operation, sender) {
            let _ = old.send(UploadCommand::Cancel);
        }
        receiver
    }

    pub fn cancel(&self, operation: i32) -> bool {
        self.send(operation, UploadCommand::Cancel)
    }

    pub fn retry(&self, operation: i32) -> bool {
        self.send(operation, UploadCommand::Retry)
    }

    fn send(&self, operation: i32, command: UploadCommand) -> bool {
        self.current
            .lock()
            .expect("Upload controller poisoned")
            .get(&operation)
            .is_some_and(|sender| sender.send(command).is_ok())
    }

    pub fn finish(&self, operation: i32) {
        let mut current = self.current.lock().expect("Upload controller poisoned");
        current.remove(&operation);
    }
}

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
    fn upload_commands_target_matching_operation() {
        let controller = UploadController::default();
        let mut old = controller.begin(1);
        let mut new = controller.begin(2);
        assert!(old.try_recv().is_err());
        assert!(controller.cancel(2));
        assert_eq!(new.try_recv(), Ok(UploadCommand::Cancel));
        assert!(controller.cancel(1));
        assert_eq!(old.try_recv(), Ok(UploadCommand::Cancel));
        assert!(controller.retry(1));
        assert_eq!(old.try_recv(), Ok(UploadCommand::Retry));
    }

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
