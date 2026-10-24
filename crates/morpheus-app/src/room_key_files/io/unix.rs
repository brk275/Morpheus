//! Only handle-relative operations under retained no-follow directory FDs.
//! Existing paths/permissions are never modified. Only a new private stage may
//! be removed. Same-user hostile mutation and media durability are not promised.

use super::{IoError, MAX_FILE_BYTES, Publication};
use std::{
    ffi::{CStr, CString},
    fs::{File, Metadata, Permissions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use zeroize::Zeroizing;

static NEXT_STAGE: AtomicU64 = AtomicU64::new(1);
// close() may have consumed the descriptor even when it reports failure. The
// numeric value is never retried, reconstructed, recorded as a new owner or
// used again. A failed receipt is sticky at the enclosing actual task owner.
fn close_file(file: File) -> bool {
    let descriptor = file.into_raw_fd();
    unsafe { libc::close(descriptor) == 0 }
}
fn close_files(files: &mut Vec<File>) -> bool {
    let mut clean = true;
    while let Some(file) = files.pop() {
        clean &= close_file(file);
    }
    clean
}
fn close_result<T>(outcome: Result<T, IoError>, files: &mut Vec<File>) -> Result<T, IoError> {
    if close_files(files) {
        outcome
    } else {
        Err(IoError::CleanupUncertain)
    }
}
struct Parent {
    chain: Vec<File>,
    leaf: CString,
}
impl Parent {
    fn directory(&self) -> &File {
        self.chain.last().expect("Retained absolute root")
    }
}
fn path_parts(path: &Path) -> Result<Vec<CString>, IoError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > 4096 || !bytes.starts_with(b"/") || bytes.contains(&0) {
        return Err(IoError::InvalidPath);
    }
    let parts: Vec<_> = bytes[1..].split(|byte| *byte == b'/').collect();
    if parts.is_empty()
        || parts.len() > 128
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == b"." || *part == b".." || part.len() > 255)
    {
        return Err(IoError::InvalidPath);
    }
    if matches!(parts[0], b"dev" | b"proc" | b"sys") {
        return Err(IoError::InvalidPath);
    }
    parts
        .into_iter()
        .map(|part| CString::new(part).map_err(|_| IoError::InvalidPath))
        .collect()
}
fn open_at(parent: i32, name: &CStr, flags: i32, mode: libc::mode_t) -> Result<File, IoError> {
    // SAFETY: one validated NUL-terminated component and retained parent FD.
    // All results transfer once into File; no descriptor is inherited.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            // Darwin mode_t is u16: C varargs require integer promotion.
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(IoError::Read);
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}
fn parent(path: &Path) -> Result<Parent, IoError> {
    let mut parts = path_parts(path)?;
    let leaf = parts.pop().ok_or(IoError::InvalidPath)?;
    let root = open_at(libc::AT_FDCWD, c"/", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let mut chain = vec![root];
    let result = (|| {
        for name in parts {
            let directory = open_at(
                chain.last().unwrap().as_raw_fd(),
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )?;
            chain.push(directory);
            if !chain
                .last()
                .unwrap()
                .metadata()
                .map_err(|_| IoError::Read)?
                .is_dir()
            {
                return Err(IoError::InvalidPath);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        return close_result(Err(error), &mut chain);
    }
    Ok(Parent { chain, leaf })
}
fn identity(metadata: &Metadata) -> (u64, u64, u64, u32, u32, u64, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mode(),
        metadata.uid(),
        metadata.nlink(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}
fn regular(file: &File) -> Result<Metadata, IoError> {
    let metadata = file.metadata().map_err(|_| IoError::Read)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(IoError::NotRegular);
    }
    Ok(metadata)
}
pub(super) fn read_encrypted(path: &Path) -> Result<Zeroizing<Vec<u8>>, IoError> {
    let mut parent = parent(path)?;
    // O_NONBLOCK is set BEFORE fstat; FIFO/device admission cannot block first.
    let mut file = match open_at(
        parent.directory().as_raw_fd(),
        &parent.leaf,
        libc::O_RDONLY,
        0,
    ) {
        Ok(file) => file,
        Err(error) => return close_result(Err(error), &mut parent.chain),
    };
    let outcome = (|| {
        let before = regular(&file)?;
        if before.len() > MAX_FILE_BYTES as u64 {
            return Err(IoError::Limit);
        }
        let mut bytes = Zeroizing::new(Vec::new());
        let mut buffer = Zeroizing::new([0u8; 8192]);
        loop {
            let count = file.read(&mut buffer[..]).map_err(|_| IoError::Read)?;
            if count == 0 {
                break;
            }
            let needed = bytes.len().checked_add(count).ok_or(IoError::Limit)?;
            if needed > MAX_FILE_BYTES {
                return Err(IoError::Limit);
            }
            if needed > bytes.capacity() {
                let capacity = needed
                    .max(bytes.capacity().saturating_mul(2))
                    .min(MAX_FILE_BYTES);
                bytes
                    .try_reserve_exact(capacity - bytes.len())
                    .map_err(|_| IoError::Limit)?;
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        // A metadata consistency check, not hostile-same-user snapshot proof.
        // Authentication/parser admission still uses these exact held bytes.
        let after = regular(&file)?;
        if identity(&before) != identity(&after) || after.len() != bytes.len() as u64 {
            return Err(IoError::Read);
        }
        Ok(bytes)
    })();
    let file_closed = close_file(file);
    let parents_closed = close_files(&mut parent.chain);
    if file_closed && parents_closed {
        outcome
    } else {
        Err(IoError::CleanupUncertain)
    }
}

#[cfg(target_os = "macos")]
mod darwin_acl {
    use super::*;
    use std::ffi::c_void;
    unsafe extern "C" {
        fn acl_init(count: i32) -> *mut c_void;
        fn acl_get_fd(fd: i32) -> *mut c_void;
        fn acl_set_fd(fd: i32, acl: *mut c_void) -> i32;
        fn acl_valid(acl: *mut c_void) -> i32;
        fn acl_get_entry(acl: *mut c_void, index: i32, entry: *mut *mut c_void) -> i32;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut i32) -> i32;
        fn acl_free(object: *mut c_void) -> i32;
    }
    struct Acl(*mut c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }
    fn get(file: &File) -> Result<Option<Acl>, IoError> {
        unsafe {
            *libc::__error() = 0;
        }
        let acl = unsafe { acl_get_fd(file.as_raw_fd()) };
        if acl.is_null() {
            return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                Ok(None)
            } else {
                Err(IoError::Unsupported)
            };
        }
        // FILESEC_REMOVE_ACL is a setter sentinel, never a dereferenceable ACL.
        if acl as usize == 1 {
            return Err(IoError::Unsupported);
        }
        let acl = Acl(acl);
        if unsafe { acl_valid(acl.0) } != 0 {
            return Err(IoError::Unsupported);
        }
        Ok(Some(acl))
    }
    fn entries(file: &File, allow_deny_only: bool) -> Result<(), IoError> {
        let Some(acl) = get(file)? else { return Ok(()) };
        for index in 0..=128 {
            let mut entry = std::ptr::null_mut();
            let result = unsafe { acl_get_entry(acl.0, index, &mut entry) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
            {
                // Authenticated Apple Libc: valid ACL + sequential index >=
                // entry count returns EINVAL; no entry has been skipped.
                return Ok(());
            }
            if result != 0 || entry.is_null() || index == 128 || !allow_deny_only {
                return Err(IoError::Unsupported);
            }
            let mut tag = 0;
            if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 || tag != 2 {
                return Err(IoError::Unsupported);
            }
        }
        Err(IoError::Unsupported)
    }
    pub(super) fn parent(file: &File) -> Result<(), IoError> {
        // Existing deny-only ACLs (common Documents/Desktop protection) stay
        // untouched. Any allow ACE is conservatively unsupported before mkdir,
        // preventing inherited grants during the new empty-stage creation.
        entries(file, true)
    }
    pub(super) fn verify_empty(file: &File) -> Result<(), IoError> {
        entries(file, false)
    }
    pub(super) fn clear_created(file: &File) -> Result<(), IoError> {
        let acl = Acl(unsafe { acl_init(0) });
        if acl.0.is_null() {
            return Err(IoError::Unsupported);
        }
        if unsafe { acl_valid(acl.0) } != 0 || unsafe { acl_set_fd(file.as_raw_fd(), acl.0) } != 0 {
            return Err(IoError::Unsupported);
        }
        entries(file, false)
    }
}
fn private(file: &File, mode: u32, directory: bool) -> Result<(), IoError> {
    #[cfg(target_os = "macos")]
    darwin_acl::clear_created(file)?;
    file.set_permissions(Permissions::from_mode(mode))
        .map_err(|_| IoError::Write)?;
    #[cfg(target_os = "macos")]
    darwin_acl::verify_empty(file)?;
    let metadata = file.metadata().map_err(|_| IoError::Write)?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != mode
        || metadata.is_dir() != directory
        || !directory && (!metadata.is_file() || metadata.nlink() != 1)
    {
        return Err(IoError::Unsupported);
    }
    Ok(())
}
fn entry_stat(parent: &File, name: &CStr) -> Result<libc::stat, IoError> {
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(IoError::Read);
    }
    Ok(unsafe { info.assume_init() })
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Preparing,
    CommitAttempted,
    Published,
}
pub(super) struct ExportStage {
    parent: Parent,
    stage_name: CString,
    directory: Option<File>,
    file: Option<File>,
    stage_identity: (u64, u64),
    file_identity: (u64, u64),
    length: u64,
    state: State,
}
impl ExportStage {
    fn directory(&self) -> Result<&File, IoError> {
        self.directory.as_ref().ok_or(IoError::CleanupUncertain)
    }
    fn close_owned(&mut self) -> bool {
        let mut clean = true;
        if let Some(file) = self.file.take() {
            clean &= close_file(file);
        }
        if let Some(directory) = self.directory.take() {
            clean &= close_file(directory);
        }
        clean &= close_files(&mut self.parent.chain);
        clean
    }
    pub(super) fn prepare(path: &Path, encrypted: &[u8]) -> Result<Self, IoError> {
        if encrypted.len() > MAX_FILE_BYTES {
            return Err(IoError::Limit);
        }
        let mut parent = parent(path)?;
        let preflight = (|| {
            let metadata = parent.directory().metadata().map_err(|_| IoError::Write)?;
            if !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o022 != 0
            {
                return Err(IoError::Unsupported);
            }
            #[cfg(target_os = "macos")]
            darwin_acl::parent(parent.directory())?;
            let mut exists = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    parent.directory().as_raw_fd(),
                    parent.leaf.as_ptr(),
                    exists.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                return Err(IoError::Exists);
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
                return Err(IoError::Write);
            }
            for _ in 0..128 {
                let nonce = NEXT_STAGE
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        value.checked_add(1)
                    })
                    .map_err(|_| IoError::Unsupported)?;
                let name = CString::new(format!(
                    ".morpheus-room-keys-{}-{nonce}",
                    std::process::id()
                ))
                .map_err(|_| IoError::InvalidPath)?;
                if unsafe { libc::mkdirat(parent.directory().as_raw_fd(), name.as_ptr(), 0o700) }
                    == 0
                {
                    return Ok(name);
                }
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(IoError::Write);
                }
            }
            Err(IoError::Unsupported)
        })();
        let stage_name = match preflight {
            Ok(name) => name,
            Err(error) => return close_result(Err(error), &mut parent.chain),
        };
        let directory = match open_at(
            parent.directory().as_raw_fd(),
            &stage_name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        ) {
            Ok(directory) => directory,
            Err(_) => {
                // No bytes have been created in this new empty namespace.
                // Existing parents are not modified; hostile same-user changes
                // are outside the stated private-stage threat model.
                let removed = unsafe {
                    libc::unlinkat(
                        parent.directory().as_raw_fd(),
                        stage_name.as_ptr(),
                        libc::AT_REMOVEDIR,
                    )
                } == 0;
                let closed = close_files(&mut parent.chain);
                return Err(if removed && closed {
                    IoError::Unsupported
                } else {
                    IoError::CleanupUncertain
                });
            }
        };
        if private(&directory, 0o700, true).is_err() {
            let removed = unsafe {
                libc::unlinkat(
                    parent.directory().as_raw_fd(),
                    stage_name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } == 0;
            let directory_closed = close_file(directory);
            let parents_closed = close_files(&mut parent.chain);
            return Err(if removed && directory_closed && parents_closed {
                IoError::Unsupported
            } else {
                IoError::CleanupUncertain
            });
        }
        let dm = match directory.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                let _directory_closed = close_file(directory);
                let _parents_closed = close_files(&mut parent.chain);
                return Err(IoError::CleanupUncertain);
            }
        };
        let mut stage = Self {
            parent,
            stage_name,
            directory: Some(directory),
            file: None,
            stage_identity: (dm.dev(), dm.ino()),
            file_identity: (0, 0),
            length: encrypted.len() as u64,
            state: State::Preparing,
        };
        let file = match open_at(
            stage.directory()?.as_raw_fd(),
            c"encrypted",
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => file,
            Err(_) => {
                let removed = stage.remove_private_directory(false).is_ok();
                let closed = stage.close_owned();
                return Err(if removed && closed {
                    IoError::Write
                } else {
                    IoError::CleanupUncertain
                });
            }
        };
        stage.file = Some(file);
        let fm = match stage.file.as_ref().unwrap().metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                let _closed = stage.close_owned();
                return Err(IoError::CleanupUncertain);
            }
        };
        stage.file_identity = (fm.dev(), fm.ino());
        let written = (|| {
            let file = stage.file.as_mut().ok_or(IoError::Write)?;
            private(file, 0o600, false)?;
            file.write_all(encrypted).map_err(|_| IoError::Write)?;
            file.sync_all().map_err(|_| IoError::Write)?;
            Ok(())
        })();
        if let Err(error) = written {
            return match stage.discard() {
                Ok(()) => Err(error),
                Err(error) => Err(error),
            };
        }
        // No publication can follow a failed data-file close. Keep only the
        // private namespace and its bound inode; final admission lstats it.
        if !close_file(stage.file.take().unwrap()) {
            let _cleanup = stage.discard();
            return Err(IoError::CleanupUncertain);
        }
        Ok(stage)
    }
    fn remove_private_directory(&self, file_present: bool) -> Result<(), IoError> {
        let stat = entry_stat(self.parent.directory(), &self.stage_name)
            .map_err(|_| IoError::CleanupUncertain)?;
        if (stat.st_dev as u64, stat.st_ino as u64) != self.stage_identity
            || stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_mode & 0o7777 != 0o700
        {
            return Err(IoError::CleanupUncertain);
        }
        if file_present {
            let file = entry_stat(self.directory()?, c"encrypted")
                .map_err(|_| IoError::CleanupUncertain)?;
            if (file.st_dev as u64, file.st_ino as u64) != self.file_identity
                || file.st_mode & libc::S_IFMT != libc::S_IFREG
                || file.st_nlink != 1
            {
                return Err(IoError::CleanupUncertain);
            }
            if unsafe { libc::unlinkat(self.directory()?.as_raw_fd(), c"encrypted".as_ptr(), 0) }
                != 0
            {
                return Err(IoError::CleanupUncertain);
            }
        }
        if unsafe {
            libc::unlinkat(
                self.parent.directory().as_raw_fd(),
                self.stage_name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(IoError::CleanupUncertain);
        }
        Ok(())
    }
    pub(super) fn discard(mut self) -> Result<(), IoError> {
        if self.state != State::Preparing {
            return Err(IoError::CommitUncertain);
        }
        let removed = self.remove_private_directory(true).is_ok();
        let closed = self.close_owned();
        if removed && closed {
            Ok(())
        } else {
            Err(IoError::CleanupUncertain)
        }
    }
    pub(super) fn publish(&mut self) -> Result<Publication, IoError> {
        if self.state != State::Preparing || self.file.is_some() {
            return Err(IoError::CommitUncertain);
        }
        let metadata =
            entry_stat(self.directory()?, c"encrypted").map_err(|_| IoError::CleanupUncertain)?;
        if (metadata.st_dev as u64, metadata.st_ino as u64) != self.file_identity
            || metadata.st_size < 0
            || metadata.st_size as u64 != self.length
            || metadata.st_mode & libc::S_IFMT != libc::S_IFREG
            || metadata.st_nlink != 1
            || metadata.st_uid != unsafe { libc::geteuid() }
            || metadata.st_mode & 0o7777 != 0o600
        {
            return Err(IoError::CleanupUncertain);
        }
        let source_directory = self.directory()?.as_raw_fd();
        let target_directory = self.parent.directory().as_raw_fd();
        self.state = State::CommitAttempted;
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                source_directory,
                c"encrypted".as_ptr(),
                target_directory,
                self.parent.leaf.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                source_directory,
                c"encrypted".as_ptr(),
                target_directory,
                self.parent.leaf.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            // Capture errno before any cleanup/native call can replace it.
            let error = std::io::Error::last_os_error().raw_os_error();
            if error == Some(libc::EEXIST) {
                // Exactly documented no-replace refusal: no rename happened.
                // The caller still owns Preparing and must explicitly discard.
                self.state = State::Preparing;
                return Err(IoError::Exists);
            }
            let _closed = self.close_owned();
            return Err(IoError::CommitUncertain);
        }
        // Mark publication before any cleanup; never delete the destination.
        self.state = State::Published;
        let removed = self.remove_private_directory(false).is_ok();
        let closed = self.close_owned();
        Ok(if removed && closed {
            Publication::Published
        } else {
            Publication::PublishedCleanupUncertain
        })
    }
}
// No Drop deletion: uncertainty/panic cannot guess whether a commit succeeded.
// Normal paths explicitly close each File once. Panic is separately a sticky
// native-worker failure, never a fabricated clean descriptor receipt.

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let temporary = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        // Darwin's ordinary temporary path may start with the /var alias.
        // Only this new synthetic fixture is canonicalized; production paths
        // retain the no-follow traversal contract.
        let root = temporary.path().canonicalize().unwrap();
        (temporary, root)
    }

    #[test]
    fn input_uses_one_regular_file_and_refuses_aliases_fifo_and_oversize() {
        let (_temporary, root) = fixture();
        let file = root.join("encrypted.txt");
        fs::write(&file, b"synthetic ciphertext").unwrap();
        let accepted = read_encrypted(&file).unwrap();
        assert!(accepted.as_slice() == b"synthetic ciphertext");
        let link = root.join("alias");
        symlink(&file, &link).unwrap();
        assert!(read_encrypted(&link).is_err());
        let hard = root.join("hard");
        fs::hard_link(&file, &hard).unwrap();
        assert!(matches!(read_encrypted(&file), Err(IoError::NotRegular)));
        let fifo = root.join("fifo");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        // Opening is nonblocking before type admission: no writer is needed.
        assert!(matches!(read_encrypted(&fifo), Err(IoError::NotRegular)));
        let large = File::create(root.join("large")).unwrap();
        large.set_len(MAX_FILE_BYTES as u64 + 1).unwrap();
        assert!(matches!(
            read_encrypted(&root.join("large")),
            Err(IoError::Limit)
        ));
        assert!(fs::read(&file).unwrap() == b"synthetic ciphertext");
    }

    #[test]
    fn private_stage_discards_only_its_owned_namespace_and_publishes_no_replace() {
        let (_temporary, root) = fixture();
        let destination = root.join("new.txt");
        let stage = ExportStage::prepare(&destination, b"synthetic ciphertext").unwrap();
        let stage_path = root.join(stage.stage_name.to_str().unwrap());
        assert_eq!(
            stage.directory.as_ref().unwrap().metadata().unwrap().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(stage_path.join("encrypted")).unwrap().mode() & 0o7777,
            0o600
        );
        assert!(
            stage.file.is_none(),
            "data file has a checked close before publication"
        );
        assert!(!destination.exists());
        stage.discard().unwrap();
        assert!(!stage_path.exists() && !destination.exists());

        let mut stage = ExportStage::prepare(&destination, b"complete ciphertext").unwrap();
        assert_eq!(stage.publish(), Ok(Publication::Published));
        assert!(fs::read(&destination).unwrap() == b"complete ciphertext");
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o7777, 0o600);
        assert!(matches!(
            ExportStage::prepare(&destination, b"other"),
            Err(IoError::Exists)
        ));
    }

    #[test]
    fn competing_destination_preserves_target_and_discards_only_private_stage() {
        let (_temporary, root) = fixture();
        let destination = root.join("race.txt");
        let mut stage = ExportStage::prepare(&destination, b"private ciphertext").unwrap();
        let stage_path = root.join(stage.stage_name.to_str().unwrap());
        fs::write(&destination, b"existing sentinel").unwrap();
        let result = stage.publish();
        assert_eq!(result, Err(IoError::Exists));
        assert_eq!(stage.discard(), Ok(()));
        assert!(fs::read(&destination).unwrap() == b"existing sentinel");
        assert!(!stage_path.exists());
    }
    #[test]
    fn unknown_stage_refuses_deletion_and_shared_write_parent_refuses_creation() {
        let (_temporary, root) = fixture();
        let destination = root.join("unknown.txt");
        let mut stage = ExportStage::prepare(&destination, b"encrypted uncertain fixture").unwrap();
        let stage_path = root.join(stage.stage_name.to_str().unwrap());
        // Model only the post-call unknown-state rule, not a native rename
        // failure or storage fault. Actual owned closes are still checked.
        stage.state = State::CommitAttempted;
        let closed = stage.close_owned();
        let discard = stage.discard();
        let preserved =
            fs::read(stage_path.join("encrypted")).unwrap() == b"encrypted uncertain fixture";
        fs::set_permissions(&root, Permissions::from_mode(0o770)).unwrap();
        let refused = matches!(
            ExportStage::prepare(&root.join("shared.txt"), b"fixture"),
            Err(IoError::Unsupported)
        );
        fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        assert!(closed && preserved && refused);
        assert_eq!(discard, Err(IoError::CommitUncertain));
        assert!(!destination.exists() && !root.join("shared.txt").exists());
    }
}
