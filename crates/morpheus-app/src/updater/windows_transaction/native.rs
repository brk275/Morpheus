//! Small Win32 boundary. Never launches a shell or resolves tools through PATH.

use super::super::UpdateError;
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf},
    ptr,
};

type Handle = *mut c_void;
const INVALID: Handle = -1_isize as Handle;
const REPARSE: u32 = 0x400;
const OPEN_REPARSE: u32 = 0x0020_0000;
const DIRECTORY: u32 = 0x0200_0000;
const READ_CONTROL: u32 = 0x0002_0000;
const HKCU: Handle = -2_147_483_647_isize as Handle;
const HKLM: Handle = -2_147_483_646_isize as Handle;
const KEY_PATH: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Morpheus";

#[repr(C)]
struct FileInfo {
    attributes: u32,
    creation: [u32; 2],
    access: [u32; 2],
    write: [u32; 2],
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}
#[repr(C)]
struct SecurityAttributes {
    size: u32,
    descriptor: Handle,
    inherit: i32,
}
#[repr(C)]
struct AclHeader {
    revision: u8,
    reserved: u8,
    size: u16,
    count: u16,
    reserved2: u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CloseHandle(handle: Handle) -> i32;
    fn SetLastError(error: u32);
    fn GetCurrentProcess() -> Handle;
    fn GetFileInformationByHandle(handle: Handle, information: *mut FileInfo) -> i32;
    fn GetFinalPathNameByHandleW(handle: Handle, name: *mut u16, size: u32, flags: u32) -> u32;
    fn CreateDirectoryW(name: *const u16, attributes: *const SecurityAttributes) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn CreateMutexW(attributes: *const SecurityAttributes, owner: i32, name: *const u16) -> Handle;
    fn OpenMutexW(access: u32, inherit: i32, name: *const u16) -> Handle;
    fn ReplaceFileW(
        replaced: *const u16,
        replacement: *const u16,
        backup: *const u16,
        flags: u32,
        exclude: Handle,
        reserved: Handle,
    ) -> i32;
    fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, id: u32) -> Handle;
    fn GetProcessTimes(
        process: Handle,
        creation: *mut u64,
        exit: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn LocalFree(memory: Handle) -> Handle;
}
#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: Handle, buffer: *mut u8, bytes: u32, flags: u32) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        information: Handle,
        bytes: u32,
        needed: *mut u32,
    ) -> i32;
    fn EqualSid(left: Handle, right: Handle) -> i32;
    fn ConvertStringSidToSidW(text: *const u16, sid: *mut Handle) -> i32;
    fn ConvertSidToStringSidW(sid: Handle, text: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut Handle,
        bytes: *mut u32,
    ) -> i32;
    fn GetSecurityInfo(
        object: Handle,
        kind: u32,
        information: u32,
        owner: *mut Handle,
        group: *mut Handle,
        dacl: *mut Handle,
        sacl: *mut Handle,
        descriptor: *mut Handle,
    ) -> u32;
    fn GetAce(acl: Handle, index: u32, ace: *mut Handle) -> i32;
    fn RegOpenKeyExW(
        key: Handle,
        name: *const u16,
        options: u32,
        access: u32,
        result: *mut Handle,
    ) -> i32;
    fn RegCloseKey(key: Handle) -> i32;
    fn RegQueryValueExW(
        key: Handle,
        name: *const u16,
        reserved: Handle,
        kind: *mut u32,
        bytes: *mut u8,
        length: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: Handle,
        name: *const u16,
        reserved: u32,
        kind: u32,
        bytes: *const u8,
        length: u32,
    ) -> i32;
    fn RegFlushKey(key: Handle) -> i32;
}

fn wide(value: &std::ffi::OsStr) -> Result<Vec<u16>, UpdateError> {
    let mut units: Vec<_> = value.encode_wide().collect();
    if units.is_empty() || units.len() > 30_000 || units.contains(&0) {
        return Err(UpdateError::UnsupportedPackage);
    }
    units.push(0);
    Ok(units)
}
fn text(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct Owned(Handle);
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Local(Handle);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

struct User {
    _token: Owned,
    _storage: Vec<usize>,
    _owner_storage: Vec<usize>,
    sid: Handle,
    default_owner: Handle,
    system: Local,
    admin: Local,
    installer: Local,
}
impl User {
    fn current() -> Result<Self, UpdateError> {
        let mut token = ptr::null_mut();
        // SAFETY: valid output pointer; the returned token is closed by Owned.
        if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
            return Err(UpdateError::NotWritable);
        }
        let token = Owned(token);
        let mut needed = 0;
        unsafe {
            GetTokenInformation(token.0, 1, ptr::null_mut(), 0, &mut needed);
        }
        if !(std::mem::size_of::<Handle>() as u32..=4096).contains(&needed) {
            return Err(UpdateError::NotWritable);
        }
        let mut storage = vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(token.0, 1, storage.as_mut_ptr().cast(), needed, &mut needed)
        } == 0
        {
            return Err(UpdateError::NotWritable);
        }
        let sid = unsafe { *(storage.as_ptr().cast::<Handle>()) };
        let mut owner_needed = 0;
        unsafe {
            GetTokenInformation(token.0, 4, ptr::null_mut(), 0, &mut owner_needed);
        }
        if !(std::mem::size_of::<Handle>() as u32..=4096).contains(&owner_needed) {
            return Err(UpdateError::NotWritable);
        }
        let mut owner_storage =
            vec![0_usize; (owner_needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                4,
                owner_storage.as_mut_ptr().cast(),
                owner_needed,
                &mut owner_needed,
            )
        } == 0
        {
            return Err(UpdateError::NotWritable);
        }
        let default_owner = unsafe { *(owner_storage.as_ptr().cast::<Handle>()) };
        let known = |value: &str| -> Result<Local, UpdateError> {
            let mut sid = ptr::null_mut();
            if unsafe { ConvertStringSidToSidW(text(value).as_ptr(), &mut sid) } == 0 {
                return Err(UpdateError::NotWritable);
            }
            Ok(Local(sid))
        };
        Ok(Self {
            _token: token,
            _storage: storage,
            _owner_storage: owner_storage,
            sid,
            default_owner,
            system: known("S-1-5-18")?,
            admin: known("S-1-5-32-544")?,
            installer: known("S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464")?,
        })
    }
    fn trusted(&self, sid: Handle) -> bool {
        unsafe {
            EqualSid(sid, self.sid) != 0
                || EqualSid(sid, self.system.0) != 0
                || EqualSid(sid, self.admin.0) != 0
                || EqualSid(sid, self.installer.0) != 0
        }
    }
    fn descriptor(&self) -> Result<Local, UpdateError> {
        let mut string = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid, &mut string) } == 0 {
            return Err(UpdateError::NotWritable);
        }
        let memory = Local(string.cast());
        let mut length = 0;
        while length < 192 && unsafe { *string.add(length) } != 0 {
            length += 1;
        }
        if length == 192 {
            return Err(UpdateError::NotWritable);
        }
        let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(string, length) })
            .map_err(|_| UpdateError::NotWritable)?;
        let mut descriptor = ptr::null_mut();
        let value = text(&format!("D:P(A;;FA;;;SY)(A;;FA;;;{sid})"));
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                value.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(UpdateError::NotWritable);
        }
        drop(memory);
        Ok(Local(descriptor))
    }
}

/// No foreign writer, junction, device or hardlink is admitted. ACL checks use
/// the opened object, not a racy security descriptor fetched by path.
fn secure(handle: Handle, kind: u32, owner_required: bool) -> Result<(), UpdateError> {
    let user = User::current()?;
    let mut owner = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    if unsafe {
        GetSecurityInfo(
            handle,
            kind,
            5,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    } != 0
    {
        return Err(UpdateError::NotWritable);
    }
    let _descriptor = Local(descriptor);
    if owner.is_null()
        || dacl.is_null()
        || (owner_required
            && unsafe {
                EqualSid(owner, user.sid) == 0 && EqualSid(owner, user.default_owner) == 0
            })
        || (!owner_required && !user.trusted(owner))
    {
        return Err(UpdateError::NotWritable);
    }
    let header = unsafe { &*dacl.cast::<AclHeader>() };
    if header.size < 8 || header.count > 512 {
        return Err(UpdateError::NotWritable);
    }
    // A drive/ancestor may grant creation of unrelated children (normal C:\\
    // ACLs do). Its opened next child cannot be removed/renamed while retained.
    // The owned install/state directories themselves allow no foreign writer.
    let mask = if kind == 6 {
        0x500d_0001 // Kernel mutex: modify state, delete, owner or DACL mutation.
    } else if kind == 4 {
        0x500d_0026
    } else if owner_required {
        0x500d_0156
    } else {
        0x500d_0040
    };
    for index in 0..u32::from(header.count) {
        let mut pointer = ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut pointer) } == 0 {
            return Err(UpdateError::NotWritable);
        }
        let begin = pointer as usize;
        let acl_begin = dacl as usize;
        if begin < acl_begin + 8
            || begin
                .checked_add(4)
                .is_none_or(|end| end > acl_begin + header.size as usize)
        {
            return Err(UpdateError::NotWritable);
        }
        let prefix = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), 4) };
        let size = u16::from_le_bytes([prefix[2], prefix[3]]) as usize;
        if size < 4
            || begin
                .checked_add(size)
                .is_none_or(|end| end > acl_begin + header.size as usize)
        {
            return Err(UpdateError::NotWritable);
        }
        // Inherit-only ACEs do not grant access to this opened object.
        if prefix[1] & 8 != 0 {
            continue;
        }
        match prefix[0] {
            1 => continue, // An access-denied ACE cannot grant write access.
            0 => {
                if size < 16 {
                    return Err(UpdateError::NotWritable);
                }
                let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) };
                let access = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                let sid_len = 8 + 4 * bytes[9] as usize;
                if bytes[8] != 1 || sid_len > size - 8 {
                    return Err(UpdateError::NotWritable);
                }
                if access & mask != 0
                    && !user.trusted(unsafe { pointer.cast::<u8>().add(8).cast() })
                {
                    return Err(UpdateError::NotWritable);
                }
            }
            _ => return Err(UpdateError::NotWritable), // No opaque/object/callback grants.
        }
    }
    Ok(())
}

pub(super) fn open(path: &Path) -> Result<File, UpdateError> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(OPEN_REPARSE)
        .open(path)
        .map_err(|_| UpdateError::RecoveryRequired)?;
    let metadata = file.metadata().map_err(|_| UpdateError::RecoveryRequired)?;
    let mut information = std::mem::MaybeUninit::<FileInfo>::uninit();
    if !metadata.is_file()
        || metadata.file_attributes() & REPARSE != 0
        || unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) }
            == 0
    {
        return Err(UpdateError::RecoveryRequired);
    }
    if unsafe { information.assume_init() }.links != 1 {
        return Err(UpdateError::RecoveryRequired);
    }
    secure(file.as_raw_handle(), 1, true)?;
    Ok(file)
}

pub(super) struct Directory {
    pub path: PathBuf,
    _ancestors: Vec<File>,
}
impl Directory {
    pub fn open(path: &Path) -> Result<Self, UpdateError> {
        if !path.is_absolute()
            || path
                .components()
                .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
        {
            return Err(UpdateError::UnsupportedPackage);
        }
        let mut ancestors = Vec::new();
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            let file = OpenOptions::new()
                .read(true)
                .access_mode(READ_CONTROL | 0x80)
                .share_mode(3)
                .custom_flags(OPEN_REPARSE | DIRECTORY)
                .open(&current)
                .map_err(|_| UpdateError::NotWritable)?;
            let metadata = file.metadata().map_err(|_| UpdateError::NotWritable)?;
            if !metadata.is_dir() || metadata.file_attributes() & REPARSE != 0 {
                return Err(UpdateError::NotWritable);
            }
            secure(file.as_raw_handle(), 1, current == path)?;
            ancestors.push(file);
        }
        if ancestors.is_empty() {
            return Err(UpdateError::UnsupportedPackage);
        }
        let mut units = vec![0_u16; 30_001];
        let count = unsafe {
            GetFinalPathNameByHandleW(
                ancestors.last().unwrap().as_raw_handle(),
                units.as_mut_ptr(),
                units.len() as u32,
                0,
            )
        } as usize;
        if count == 0 || count >= units.len() {
            return Err(UpdateError::UnsupportedPackage);
        }
        use std::os::windows::ffi::OsStringExt;
        let canonical = PathBuf::from(std::ffi::OsString::from_wide(&units[..count]));
        // Only a local drive installation; no UNC, network share or device path.
        let display = canonical.as_os_str().encode_wide().collect::<Vec<_>>();
        if display.len() < 7
            || display[..4] != [92, 92, 63, 92]
            || !u8::try_from(display[4]).is_ok_and(|b| b.is_ascii_alphabetic())
            || display[5..7] != [58, 92]
        {
            return Err(UpdateError::UnsupportedPackage);
        }
        let drive_root = display[4..7]
            .iter()
            .copied()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // GetDriveType requires the trailing separator. The drive letter comes
        // from the retained opened object's canonical path, never caller input.
        require_fixed_drive(unsafe { GetDriveTypeW(drive_root.as_ptr()) })?;
        Ok(Self {
            path: canonical,
            _ancestors: ancestors,
        })
    }
}

pub(super) fn recovery_nonce() -> Result<String, UpdateError> {
    let mut bytes = [0_u8; 32];
    // Windows' system-preferred cryptographic RNG, no seed or weak fallback.
    if unsafe { BCryptGenRandom(ptr::null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, 2) } != 0 {
        return Err(UpdateError::RestartFailed);
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) fn require_fixed_drive(kind: u32) -> Result<(), UpdateError> {
    if kind == 3 {
        Ok(())
    } else {
        Err(UpdateError::UnsupportedPackage)
    }
}

/// A randomly named, current-user/System-only lifetime object across Windows
/// sessions. It is never thread-owned or waited/released as a mutex. The last
/// real handle closes on process exit; a late helper may OPEN but never CREATE.
/// Global mutex creation does not require SeCreateGlobalPrivilege (that rule is
/// specific to file mappings/symbolic links). There is no per-session fallback.
pub(super) struct RecoveryLease {
    _handle: Owned,
}
impl RecoveryLease {
    fn name(id: &str, nonce: &str) -> Result<Vec<u16>, UpdateError> {
        if !super::valid_id(id) || !super::valid_id(nonce) {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(text(&format!(
            "Global\\Morpheus.Update.Recovery.{id}.{nonce}"
        )))
    }
    pub fn create(id: &str, nonce: &str) -> Result<Self, UpdateError> {
        let name = Self::name(id, nonce)?;
        let user = User::current()?;
        let descriptor = user.descriptor()?;
        let attributes = SecurityAttributes {
            size: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: descriptor.0,
            inherit: 0,
        };
        unsafe {
            SetLastError(0);
        }
        let raw = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
        let error = std::io::Error::last_os_error().raw_os_error();
        if raw.is_null() || raw == INVALID {
            return Err(UpdateError::RestartFailed);
        }
        let handle = Owned(raw);
        // Never accept an existing named object or its ignored security attrs.
        if error == Some(183) {
            return Err(UpdateError::RecoveryRequired);
        }
        secure(handle.0, 6, true)?;
        Ok(Self { _handle: handle })
    }
    pub fn open(id: &str, nonce: &str) -> Result<Option<Self>, UpdateError> {
        let name = Self::name(id, nonce)?;
        let raw = unsafe { OpenMutexW(READ_CONTROL | 0x0010_0000, 0, name.as_ptr()) };
        if raw.is_null() || raw == INVALID {
            return if std::io::Error::last_os_error().raw_os_error() == Some(2) {
                Ok(None)
            } else {
                Err(UpdateError::RecoveryRequired)
            };
        }
        let handle = Owned(raw);
        secure(handle.0, 6, true)?;
        Ok(Some(Self { _handle: handle }))
    }
}

pub(super) fn create_directory(path: &Path) -> Result<Directory, UpdateError> {
    let user = User::current()?;
    let descriptor = user.descriptor()?;
    let attributes = SecurityAttributes {
        size: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: descriptor.0,
        inherit: 0,
    };
    if unsafe { CreateDirectoryW(wide(path.as_os_str())?.as_ptr(), &attributes) } == 0
        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(UpdateError::NotWritable);
    }
    Directory::open(path)
}

pub(super) fn read(path: &Path, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let file = open(path)?;
    if file
        .metadata()
        .map_err(|_| UpdateError::RecoveryRequired)?
        .len()
        > limit
    {
        return Err(UpdateError::RecoveryRequired);
    }
    let mut value = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut value)
        .map_err(|_| UpdateError::RecoveryRequired)?;
    if value.len() as u64 > limit {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(value)
}
pub(super) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .custom_flags(OPEN_REPARSE)
        .open(path)
        .map_err(|_| UpdateError::StageFailed)?;
    secure(file.as_raw_handle(), 1, true)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| UpdateError::StageFailed)
}
pub(super) fn journal_temp(parent: &Path, bytes: &[u8]) -> Result<tempfile::TempPath, UpdateError> {
    let mut file = tempfile::Builder::new()
        .prefix(".journal-")
        .tempfile_in(parent)
        .map_err(|_| UpdateError::StageFailed)?;
    secure(file.as_file().as_raw_handle(), 1, true)?;
    file.write_all(bytes)
        .and_then(|_| file.as_file().sync_all())
        .map_err(|_| UpdateError::StageFailed)?;
    Ok(file.into_temp_path())
}
pub(super) fn replace(destination: &Path, replacement: &Path) -> Result<(), UpdateError> {
    if unsafe {
        ReplaceFileW(
            wide(destination.as_os_str())?.as_ptr(),
            wide(replacement.as_os_str())?.as_ptr(),
            ptr::null(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}
pub(super) fn publish_new(destination: &Path, source: &Path) -> Result<(), UpdateError> {
    // No replacement, same-volume owned stage; MOVEFILE_WRITE_THROUGH.
    if unsafe {
        MoveFileExW(
            wide(source.as_os_str())?.as_ptr(),
            wide(destination.as_os_str())?.as_ptr(),
            8,
        )
    } == 0
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(())
}
pub(super) fn flush(path: &Path) -> Result<(), UpdateError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(1)
        .custom_flags(OPEN_REPARSE)
        .open(path)
        .map_err(|_| UpdateError::RecoveryRequired)?;
    secure(file.as_raw_handle(), 1, true)?;
    file.sync_all().map_err(|_| UpdateError::RecoveryRequired)
}
pub(super) fn lock(path: &Path) -> Result<File, UpdateError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .custom_flags(OPEN_REPARSE)
        .open(path)
        .map_err(|_| UpdateError::UpdateActive)?;
    secure(file.as_raw_handle(), 1, true)?;
    let metadata = file.metadata().map_err(|_| UpdateError::RecoveryRequired)?;
    let mut info = std::mem::MaybeUninit::<FileInfo>::uninit();
    if !metadata.is_file()
        || metadata.file_attributes() & REPARSE != 0
        || metadata.len() != 0
        || unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0
        || unsafe { info.assume_init() }.links != 1
    {
        return Err(UpdateError::RecoveryRequired);
    }
    Ok(file)
}

pub(super) struct ParentProcess(Owned);
impl ParentProcess {
    pub fn current_stamp() -> Result<(u32, u64), UpdateError> {
        Ok((
            std::process::id(),
            process_stamp(unsafe { GetCurrentProcess() })?,
        ))
    }
    pub fn child_stamp(child: &std::process::Child) -> Result<u64, UpdateError> {
        process_stamp(child.as_raw_handle())
    }
    pub fn running(id: u32, stamp: u64) -> Result<bool, UpdateError> {
        if id == 0 || stamp == 0 {
            return Err(UpdateError::RecoveryRequired);
        }
        let raw = unsafe { OpenProcess(0x0010_1000, 0, id) };
        if raw.is_null() || raw == INVALID {
            return if std::io::Error::last_os_error().raw_os_error() == Some(87) {
                Ok(false)
            } else {
                Err(UpdateError::RecoveryRequired)
            };
        }
        let owned = Owned(raw);
        if process_stamp(owned.0)? != stamp {
            return Ok(false);
        }
        Ok(!Self(owned).wait(0)?)
    }
    pub fn open(id: u32, stamp: u64) -> Result<Self, UpdateError> {
        if id == 0 || id == std::process::id() {
            return Err(UpdateError::RestartFailed);
        }
        let handle = unsafe { OpenProcess(0x0010_1000, 0, id) };
        if handle.is_null() || handle == INVALID {
            return Err(UpdateError::RestartFailed);
        }
        let owned = Owned(handle);
        if process_stamp(handle)? != stamp {
            return Err(UpdateError::RestartFailed);
        }
        Ok(Self(owned))
    }
    pub fn wait(&self, milliseconds: u32) -> Result<bool, UpdateError> {
        match unsafe { WaitForSingleObject(self.0.0, milliseconds) } {
            0 => Ok(true),
            258 => Ok(false),
            _ => Err(UpdateError::RestartFailed),
        }
    }
}
fn process_stamp(handle: Handle) -> Result<u64, UpdateError> {
    let (mut creation, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0
        || creation == 0
    {
        return Err(UpdateError::RestartFailed);
    }
    Ok(creation)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    pub version: String,
    pub estimated_size: u32,
}
struct RegistryKey(Handle);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
pub(super) struct Registry {
    key: RegistryKey,
    _parents: Vec<RegistryKey>,
}
// SAFETY: this is one owned registry handle, closed exactly once. Win32 registry
// handles may move between threads; all transaction access is session-serialized.
unsafe impl Send for Registry {}
impl Registry {
    pub fn open() -> Result<Self, UpdateError> {
        let mut machine = ptr::null_mut();
        let found = unsafe { RegOpenKeyExW(HKLM, text(KEY_PATH).as_ptr(), 0, 0x101, &mut machine) };
        if found == 0 {
            unsafe {
                RegCloseKey(machine);
            }
            return Err(UpdateError::UnsupportedPackage);
        }
        if found != 2 {
            return Err(UpdateError::UnsupportedPackage);
        }
        let parts = KEY_PATH.split('\\').collect::<Vec<_>>();
        let mut parents = Vec::<RegistryKey>::new();
        let mut base = HKCU;
        for (index, part) in parts.iter().enumerate() {
            let mut handle = ptr::null_mut();
            let final_key = index + 1 == parts.len();
            let access = READ_CONTROL | 0x109 | if final_key { 2 } else { 0 };
            // Open the actual key, not a registry symbolic-link target. Keep
            // opened ancestors and resolve children relative to those handles.
            if unsafe { RegOpenKeyExW(base, text(part).as_ptr(), 8, access, &mut handle) } != 0 {
                return Err(UpdateError::UnsupportedPackage);
            }
            let key = RegistryKey(handle);
            let mut kind = 0;
            let mut length = 0;
            if unsafe {
                RegQueryValueExW(
                    handle,
                    text("SymbolicLinkValue").as_ptr(),
                    ptr::null_mut(),
                    &mut kind,
                    ptr::null_mut(),
                    &mut length,
                )
            } != 2
            {
                return Err(UpdateError::UnsupportedPackage);
            }
            secure(handle, 4, final_key)?;
            if final_key {
                return Ok(Self {
                    key,
                    _parents: parents,
                });
            }
            base = handle;
            parents.push(key);
        }
        Err(UpdateError::UnsupportedPackage)
    }

    fn value(&self, name: &str, kind: u32) -> Result<Vec<u8>, UpdateError> {
        let mut value = vec![0_u8; 65_536];
        let mut size = value.len() as u32;
        let mut actual = 0;
        if unsafe {
            RegQueryValueExW(
                self.key.0,
                text(name).as_ptr(),
                ptr::null_mut(),
                &mut actual,
                value.as_mut_ptr(),
                &mut size,
            )
        } != 0
            || actual != kind
            || size as usize > value.len()
        {
            return Err(UpdateError::RecoveryRequired);
        }
        value.truncate(size as usize);
        Ok(value)
    }
    fn string(&self, name: &str) -> Result<String, UpdateError> {
        let bytes = self.value(name, 1)?;
        if bytes.len() < 2 || bytes.len() % 2 != 0 {
            return Err(UpdateError::RecoveryRequired);
        }
        let mut units = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>();
        if units.pop() != Some(0) || units.contains(&0) {
            return Err(UpdateError::RecoveryRequired);
        }
        String::from_utf16(&units).map_err(|_| UpdateError::RecoveryRequired)
    }
    fn number(&self, name: &str) -> Result<u32, UpdateError> {
        let bytes = self.value(name, 4)?;
        Ok(u32::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| UpdateError::RecoveryRequired)?,
        ))
    }
    pub fn snapshot(&self, root: &Path) -> Result<Registration, UpdateError> {
        if self.string("DisplayName")? != "Morpheus"
            || self.string("Publisher")? != "Morpheus"
            || self.number("NoModify")? != 1
            || self.number("NoRepair")? != 1
        {
            return Err(UpdateError::UnsupportedPackage);
        }
        let install = self.string("InstallLocation")?;
        let location = install
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .ok_or(UpdateError::UnsupportedPackage)?;
        if Directory::open(Path::new(location))?.path != root {
            return Err(UpdateError::UnsupportedPackage);
        }
        let uninstall = self.string("UninstallString")?;
        let uninstall_path = uninstall
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .ok_or(UpdateError::UnsupportedPackage)?;
        let expected = root.join("uninstall.exe");
        let handle = open(Path::new(uninstall_path))?;
        let other = open(&expected)?;
        let file_id = |file: &File| -> Result<(u32, u32, u32), UpdateError> {
            let mut info = std::mem::MaybeUninit::<FileInfo>::uninit();
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
                return Err(UpdateError::RecoveryRequired);
            }
            let info = unsafe { info.assume_init() };
            Ok((info.volume, info.index_high, info.index_low))
        };
        if file_id(&handle)? != file_id(&other)? {
            return Err(UpdateError::UnsupportedPackage);
        }
        Ok(Registration {
            version: self.string("DisplayVersion")?,
            estimated_size: self.number("EstimatedSize")?,
        })
    }
    pub fn set_conditional(
        &self,
        root: &Path,
        from: &Registration,
        to: &Registration,
    ) -> Result<(), UpdateError> {
        let current = self.snapshot(root)?;
        // Recover either write in a two-value commit, but never unrelated edits.
        if (current.version != from.version && current.version != to.version)
            || (current.estimated_size != from.estimated_size
                && current.estimated_size != to.estimated_size)
        {
            return Err(UpdateError::RecoveryRequired);
        }
        let version = text(&to.version);
        let bytes =
            unsafe { std::slice::from_raw_parts(version.as_ptr().cast::<u8>(), version.len() * 2) };
        if unsafe {
            RegSetValueExW(
                self.key.0,
                text("DisplayVersion").as_ptr(),
                0,
                1,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        } != 0
            || unsafe { RegFlushKey(self.key.0) } != 0
        {
            return Err(UpdateError::RecoveryRequired);
        }
        let size = to.estimated_size.to_le_bytes();
        if unsafe {
            RegSetValueExW(
                self.key.0,
                text("EstimatedSize").as_ptr(),
                0,
                4,
                size.as_ptr(),
                4,
            )
        } != 0
            || unsafe { RegFlushKey(self.key.0) } != 0
            || self.snapshot(root)? != *to
        {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(())
    }
}
