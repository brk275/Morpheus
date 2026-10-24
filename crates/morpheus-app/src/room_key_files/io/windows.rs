//! Encrypted files only. Every child open/create is relative to an owned directory
//! handle; no pathname is re-resolved for publication or cleanup. File flush is
//! checked, but is not a directory, removable-media or power-loss durability claim.

use super::{IoError, MAX_FILE_BYTES, Publication};
use std::{
    ffi::{OsStr, c_void},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, Prefix},
    ptr,
};
use zeroize::Zeroizing;

type Handle = *mut c_void;
const INVALID: Handle = -1_isize as Handle;
const SYNCHRONIZE: u32 = 0x0010_0000;
const READ_CONTROL: u32 = 0x0002_0000;
const DELETE: u32 = 0x0001_0000;
const DIRECTORY_ACCESS: u32 = 0x20 | 0x80 | SYNCHRONIZE;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const STATUS_PENDING: i32 = 0x103;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xc000_0035_u32 as i32;
const MAX_COMPONENTS: usize = 256;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: Handle,
    name: *mut UnicodeString,
    attributes: u32,
    descriptor: Handle,
    quality: Handle,
}
#[repr(C)]
union IoStatus {
    status: i32,
    pointer: Handle,
}
#[repr(C)]
struct IoStatusBlock {
    status: IoStatus,
    information: usize,
}
#[repr(C)]
#[derive(Default)]
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
struct RenameInfo {
    replace_or_flags: u32,
    root: Handle,
    bytes: u32,
    name: [u16; 1],
}
#[repr(C)]
struct AclHeader {
    revision: u8,
    reserved: u8,
    size: u16,
    count: u16,
    reserved2: u16,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut Handle,
        access: u32,
        attributes: *const ObjectAttributes,
        status: *mut IoStatusBlock,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_bytes: u32,
    ) -> i32;
    fn NtSetInformationFile(
        handle: Handle,
        status: *mut IoStatusBlock,
        information: *const c_void,
        length: u32,
        class: i32,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: Handle,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn GetFileType(handle: Handle) -> u32;
    fn GetFileInformationByHandle(handle: Handle, info: *mut FileInfo) -> i32;
    fn GetFinalPathNameByHandleW(handle: Handle, name: *mut u16, count: u32, flags: u32) -> u32;
    fn ReadFile(
        handle: Handle,
        data: *mut u8,
        count: u32,
        read: *mut u32,
        overlapped: Handle,
    ) -> i32;
    fn WriteFile(
        handle: Handle,
        data: *const u8,
        count: u32,
        written: *mut u32,
        overlapped: Handle,
    ) -> i32;
    fn FlushFileBuffers(handle: Handle) -> i32;
    fn SetFileInformationByHandle(
        handle: Handle,
        class: u32,
        data: *const c_void,
        bytes: u32,
    ) -> i32;
    fn LocalFree(data: Handle) -> Handle;
}
#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: Handle, data: *mut u8, bytes: u32, flags: u32) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        data: Handle,
        bytes: u32,
        needed: *mut u32,
    ) -> i32;
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
    fn GetSecurityDescriptorControl(
        descriptor: Handle,
        control: *mut u16,
        revision: *mut u32,
    ) -> i32;
    fn IsValidSecurityDescriptor(descriptor: Handle) -> i32;
    fn IsValidAcl(acl: Handle) -> i32;
    fn IsValidSid(sid: Handle) -> i32;
    fn GetLengthSid(sid: Handle) -> u32;
    fn EqualSid(left: Handle, right: Handle) -> i32;
    fn GetAce(acl: Handle, index: u32, ace: *mut Handle) -> i32;
}

/// Unlike ordinary RAII, an unobserved Drop deliberately does not manufacture a
/// successful CloseHandle receipt. Normal paths close explicitly. Panic or an
/// uncertain native completion leaves this exact capability retained until exit;
/// the actual worker/ticket reports failure to the account owner.
struct Owned(Handle);
impl Owned {
    fn close(mut self) -> bool {
        // SAFETY: uniquely owned, never previously closed or exposed as an owner.
        // On failure the raw capability is never retried (it may be invalid).
        let raw = std::mem::replace(&mut self.0, ptr::null_mut());
        if unsafe { CloseHandle(raw) } != 0 {
            true
        } else {
            retain_uncertain(raw);
            false
        }
    }
}
struct RetainedHandle {
    _raw: Handle,
}
fn retain_uncertain(raw: Handle) {
    // Terminal uncertainty only: intentionally no retry, dereference or Drop API.
    let _retained = Box::leak(Box::new(RetainedHandle { _raw: raw }));
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            retain_uncertain(self.0);
        }
    }
}
fn close_all(handles: &mut Vec<Owned>) -> bool {
    let mut clean = true;
    while let Some(handle) = handles.pop() {
        clean &= handle.close();
    }
    clean
}
struct Local(Handle);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn ascii_name_eq(left: &[u16], right: &[u16]) -> bool {
    let fold = |value: u16| {
        if (97..=122).contains(&value) {
            value - 32
        } else {
            value
        }
    };
    left.iter()
        .copied()
        .map(fold)
        .eq(right.iter().copied().map(fold))
}

struct User {
    _token: OwnedHandle,
    storage: Vec<usize>,
    system: Local,
}
impl User {
    fn current() -> Result<Self, IoError> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
            return Err(IoError::Unsupported);
        }
        // SAFETY: successful OpenProcessToken transfers one valid token handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        use std::os::windows::io::AsRawHandle;
        let mut needed = 0;
        unsafe {
            GetTokenInformation(token.as_raw_handle(), 1, ptr::null_mut(), 0, &mut needed);
        }
        if !(size_of::<Handle>() as u32..=4096).contains(&needed) {
            return Err(IoError::Unsupported);
        }
        let mut storage = vec![0_usize; (needed as usize).div_ceil(size_of::<usize>())];
        let supplied = needed;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                1,
                storage.as_mut_ptr().cast(),
                supplied,
                &mut needed,
            )
        } == 0
            || needed > supplied
        {
            return Err(IoError::Unsupported);
        }
        let mut system = ptr::null_mut();
        if unsafe { ConvertStringSidToSidW(wide("S-1-5-18").as_ptr(), &mut system) } == 0 {
            return Err(IoError::Unsupported);
        }
        let user = Self {
            _token: token,
            storage,
            system: Local(system),
        };
        let sid = user.sid();
        let begin = sid as usize;
        let low = user.storage.as_ptr() as usize;
        let high = low + user.storage.len() * size_of::<usize>();
        if begin < low || begin.checked_add(8).is_none_or(|end| end > high) {
            return Err(IoError::Unsupported);
        }
        let header = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), 8) };
        let bytes = 8 + 4 * usize::from(header[1]);
        if header[0] != 1
            || begin.checked_add(bytes).is_none_or(|end| end > high)
            || unsafe { IsValidSid(sid) } == 0
        {
            return Err(IoError::Unsupported);
        }
        Ok(user)
    }
    fn sid(&self) -> Handle {
        unsafe { *self.storage.as_ptr().cast::<Handle>() }
    }
    fn sid_string(&self) -> Result<String, IoError> {
        let mut pointer = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut pointer) } == 0 {
            return Err(IoError::Unsupported);
        }
        let allocation = Local(pointer.cast());
        let mut length = 0;
        while length < 192 && unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        if length == 192 {
            return Err(IoError::Unsupported);
        }
        let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(pointer, length) })
            .map_err(|_| IoError::Unsupported)?;
        drop(allocation);
        Ok(sid)
    }
    fn descriptor(&self) -> Result<Local, IoError> {
        let sid = self.sid_string()?;
        let text = wide(&format!("O:{sid}D:P(A;;FA;;;SY)(A;;FA;;;{sid})"));
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(IoError::Unsupported);
        }
        Ok(Local(descriptor))
    }
}

/// A FILE_CREATE stage must have exactly the descriptor we requested. Admin,
/// default token-owner, foreign/inherited/callback/object ACE exceptions do not
/// establish this private-file contract.
fn private_file(handle: Handle, user: &User) -> Result<(), IoError> {
    let (mut owner, mut acl, mut descriptor) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
    if unsafe {
        GetSecurityInfo(
            handle,
            1,
            5,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        )
    } != 0
    {
        return Err(IoError::Write);
    }
    let _allocation = Local(descriptor);
    let (mut control, mut revision) = (0, 0);
    if descriptor.is_null()
        || owner.is_null()
        || acl.is_null()
        || unsafe { IsValidSecurityDescriptor(descriptor) } == 0
        || unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
        || control & 0x1004 != 0x1004
        || revision != 1
        || unsafe { IsValidSid(owner) } == 0
        || unsafe { EqualSid(owner, user.sid()) } == 0
        || unsafe { IsValidAcl(acl) } == 0
    {
        return Err(IoError::Write);
    }
    let header = unsafe { &*acl.cast::<AclHeader>() };
    if header.size < 8 || header.count != 2 {
        return Err(IoError::Write);
    }
    let (mut saw_user, mut saw_system) = (false, false);
    for index in 0..2 {
        let mut ace = ptr::null_mut();
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(IoError::Write);
        }
        let start = ace as usize;
        let end = acl as usize + usize::from(header.size);
        if start < acl as usize + 8 || start.checked_add(16).is_none_or(|p| p > end) {
            return Err(IoError::Write);
        }
        let prefix = unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), 16) };
        let bytes = usize::from(u16::from_le_bytes([prefix[2], prefix[3]]));
        let sid_bytes = 8 + 4 * usize::from(prefix[9]);
        if prefix[0] != 0
            || prefix[1] != 0
            || bytes != 8 + sid_bytes
            || start.checked_add(bytes).is_none_or(|p| p > end)
            || u32::from_le_bytes(prefix[4..8].try_into().map_err(|_| IoError::Write)?)
                != 0x001f_01ff
        {
            return Err(IoError::Write);
        }
        let sid = unsafe { ace.cast::<u8>().add(8).cast() };
        if unsafe { IsValidSid(sid) } == 0 || unsafe { GetLengthSid(sid) } as usize != sid_bytes {
            return Err(IoError::Write);
        }
        if unsafe { EqualSid(sid, user.sid()) } != 0 && !saw_user {
            saw_user = true;
        } else if unsafe { EqualSid(sid, user.system.0) } != 0 && !saw_system {
            saw_system = true;
        } else {
            return Err(IoError::Write);
        }
    }
    if !saw_user || !saw_system {
        return Err(IoError::Write);
    }
    Ok(())
}

fn information(handle: Handle, directory: bool) -> Result<FileInfo, IoError> {
    let mut info = FileInfo::default();
    if unsafe { GetFileType(handle) } != 1
        || unsafe { GetFileInformationByHandle(handle, &mut info) } == 0
        || info.attributes & 0x400 != 0
        || (info.attributes & 0x10 != 0) != directory
        || (!directory && info.links != 1)
    {
        return Err(IoError::NotRegular);
    }
    Ok(info)
}
fn size(info: &FileInfo) -> u64 {
    (u64::from(info.size_high) << 32) | u64::from(info.size_low)
}

fn component(name: &OsStr) -> Result<Vec<u16>, IoError> {
    let count = name.encode_wide().count();
    if count == 0 || count > 255 {
        return Err(IoError::InvalidPath);
    }
    let mut units = Vec::new();
    units.try_reserve_exact(count).map_err(|_| IoError::Limit)?;
    units.extend(name.encode_wide());
    if units.is_empty()
        || units.len() > 255
        || units.iter().any(|c| {
            *c < 32
                || [
                    b'/' as u16,
                    b'\\' as u16,
                    b':' as u16,
                    b'<' as u16,
                    b'>' as u16,
                    b'"' as u16,
                    b'|' as u16,
                    b'?' as u16,
                    b'*' as u16,
                ]
                .contains(c)
        })
        || matches!(units.last(), Some(32 | 46))
    {
        return Err(IoError::InvalidPath);
    }
    let stem: Vec<u16> = units
        .iter()
        .take_while(|c| **c != 46)
        .copied()
        .map(|c| if (97..=122).contains(&c) { c - 32 } else { c })
        .collect();
    let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$"];
    if reserved
        .iter()
        .any(|name| stem == name.encode_utf16().collect::<Vec<_>>())
        || (stem.len() == 4
            && (stem[..3] == [67, 79, 77] || stem[..3] == [76, 80, 84])
            && ((49..=57).contains(&stem[3]) || [0xb9, 0xb2, 0xb3].contains(&stem[3])))
    {
        return Err(IoError::InvalidPath);
    }
    Ok(units)
}
struct ChosenPath {
    root: Vec<u16>,
    components: Vec<Vec<u16>>,
}
fn chosen(path: &Path) -> Result<ChosenPath, IoError> {
    let count = path.as_os_str().encode_wide().count();
    if count > 30_000 {
        return Err(IoError::InvalidPath);
    }
    let mut units = Vec::new();
    units.try_reserve_exact(count).map_err(|_| IoError::Limit)?;
    units.extend(path.as_os_str().encode_wide());
    if units.len() > 30_000
        || units.contains(&0)
        || units.contains(&(b'/' as u16))
        || units
            .split(|c| *c == b'\\' as u16)
            .any(|p| p == [46] || p == [46, 46])
    {
        return Err(IoError::InvalidPath);
    }
    let mut parts = path.components();
    let drive = match parts.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) if d.is_ascii_alphabetic() => {
                d.to_ascii_uppercase()
            }
            _ => return Err(IoError::InvalidPath),
        },
        _ => return Err(IoError::InvalidPath),
    };
    if !matches!(parts.next(), Some(Component::RootDir)) {
        return Err(IoError::InvalidPath);
    }
    let mut components = Vec::new();
    for part in parts {
        if components.len() == MAX_COMPONENTS {
            return Err(IoError::InvalidPath);
        }
        match part {
            Component::Normal(name) => components.push(component(name)?),
            _ => return Err(IoError::InvalidPath),
        }
    }
    if components.is_empty() || components.len() > MAX_COMPONENTS {
        return Err(IoError::InvalidPath);
    }
    Ok(ChosenPath {
        root: wide(&format!("\\\\?\\{}:\\", char::from(drive))),
        components,
    })
}

/// All storage that the NT call can reference is heap-stable before entry. An
/// unexpected pending/ambiguous completion retains the whole request, descriptor
/// and every ancestor capability; no stack out-param or hidden waiter survives.
struct Request {
    output: Handle,
    status: IoStatusBlock,
    name: Vec<u16>,
    unicode: UnicodeString,
    attributes: ObjectAttributes,
    _descriptor: Option<Local>,
    parents: Vec<Owned>,
}
fn relative(
    parents: &mut Vec<Owned>,
    name: &[u16],
    access: u32,
    share: u32,
    create: bool,
    directory: bool,
    descriptor: Option<Local>,
) -> Result<Owned, IoError> {
    let length = u16::try_from(name.len().checked_mul(2).ok_or(IoError::InvalidPath)?)
        .map_err(|_| IoError::InvalidPath)?;
    let root = parents.last().ok_or(IoError::InvalidPath)?.0;
    let mut request = Box::new(Request {
        output: ptr::null_mut(),
        status: IoStatusBlock {
            status: IoStatus {
                status: STATUS_PENDING,
            },
            information: usize::MAX,
        },
        name: name.to_vec(),
        unicode: UnicodeString {
            length,
            maximum_length: length,
            buffer: ptr::null_mut(),
        },
        attributes: ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root,
            name: ptr::null_mut(),
            attributes: 0x1040,
            descriptor: descriptor.as_ref().map_or(ptr::null_mut(), |d| d.0),
            quality: ptr::null_mut(),
        },
        _descriptor: descriptor,
        parents: std::mem::take(parents),
    });
    request.unicode.buffer = request.name.as_mut_ptr();
    request.attributes.name = &mut request.unicode;
    // Relinquish Rust's boxed/reference access before the native operation can
    // retain the outslots. Pending/unknown paths never touch this allocation
    // again. All ancestor capabilities already live inside it.
    let request = Box::into_raw(request);
    let status = unsafe {
        NtCreateFile(
            ptr::addr_of_mut!((*request).output),
            access,
            ptr::addr_of!((*request).attributes),
            ptr::addr_of_mut!((*request).status),
            ptr::null(),
            0x80,
            share,
            if create { 2 } else { 1 },
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT | if directory { 1 } else { 64 },
            ptr::null(),
            0,
        )
    };
    if status == STATUS_PENDING || status > 0 {
        // Intentionally retain the raw allocation without a field read, new
        // reference, Box reconstruction, close, drop or backing-buffer move.
        return Err(IoError::NativeUncertain);
    }
    // NtCreateFile's nonpending negative return is a completed native error;
    // its return status, not GetLastError or IOSB, is authoritative. A returned
    // capability on that error is still ambiguous and remains retained.
    let output = unsafe { ptr::read(ptr::addr_of!((*request).output)) };
    if status < 0 {
        if !output.is_null() {
            return Err(IoError::NativeUncertain);
        }
        // SAFETY: a completed error with no handle cannot retain native writes.
        let mut completed = unsafe { Box::from_raw(request) };
        *parents = std::mem::take(&mut completed.parents);
        return Err(if status == STATUS_OBJECT_NAME_COLLISION {
            IoError::Exists
        } else if create {
            IoError::Write
        } else {
            IoError::Read
        });
    }
    if output.is_null() || output == INVALID {
        return Err(IoError::NativeUncertain);
    }
    if unsafe { ptr::read(ptr::addr_of!((*request).status.status.status)) } != 0 {
        return Err(IoError::NativeUncertain);
    }
    if unsafe { ptr::read(ptr::addr_of!((*request).status.information)) }
        != if create { 2 } else { 1 }
    {
        return Err(IoError::NativeUncertain);
    }
    // SAFETY: exact native+IOSB completion and disposition prove the request no
    // longer owns asynchronous outslots. Only this branch regains the Box.
    let mut completed = unsafe { Box::from_raw(request) };
    *parents = std::mem::take(&mut completed.parents);
    Ok(Owned(output))
}

fn parents(path: &ChosenPath) -> Result<Vec<Owned>, IoError> {
    // This is the only absolute open: a syntactically fixed drive root, not a
    // caller-controlled child pathname. Its actual normalized handle name must
    // match. Every remaining component uses RootDirectory, including the file.
    let root = unsafe {
        CreateFileW(
            path.root.as_ptr(),
            DIRECTORY_ACCESS,
            3,
            ptr::null_mut(),
            3,
            0x0200_0000 | FILE_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if root.is_null() || root == INVALID {
        return Err(IoError::Unsupported);
    }
    let mut result = vec![Owned(root)];
    let opened = (|| {
        information(root, true)?;
        let mut name = [0_u16; 64];
        let count =
            unsafe { GetFinalPathNameByHandleW(root, name.as_mut_ptr(), name.len() as u32, 0) }
                as usize;
        if count == 0
            || count >= name.len()
            || !ascii_name_eq(&name[..count], &path.root[..path.root.len() - 1])
        {
            return Err(IoError::Unsupported);
        }
        for name in &path.components[..path.components.len() - 1] {
            let next = relative(&mut result, name, DIRECTORY_ACCESS, 3, false, true, None)?;
            result.push(next);
            information(result.last().ok_or(IoError::Unsupported)?.0, true)?;
        }
        Ok(())
    })();
    match opened {
        Ok(()) => Ok(result),
        Err(error) => {
            if close_all(&mut result) {
                Err(error)
            } else {
                Err(IoError::CleanupUncertain)
            }
        }
    }
}

pub(super) fn read_encrypted(path: &Path) -> Result<Zeroizing<Vec<u8>>, IoError> {
    let path = chosen(path)?;
    let mut handles = parents(&path)?;
    let outcome = (|| {
        let input = relative(
            &mut handles,
            path.components.last().ok_or(IoError::InvalidPath)?,
            READ_CONTROL | SYNCHRONIZE | 0x89,
            1,
            false,
            false,
            None,
        )?;
        handles.push(input);
        let input = handles.last().ok_or(IoError::Read)?.0;
        let before = information(input, false)?;
        if size(&before) > MAX_FILE_BYTES as u64 {
            return Err(IoError::Limit);
        }
        let limit = MAX_FILE_BYTES.checked_add(1).ok_or(IoError::Limit)?;
        let mut bytes = Zeroizing::new(Vec::new());
        bytes.try_reserve_exact(limit).map_err(|_| IoError::Limit)?;
        let mut chunk = Zeroizing::new([0_u8; 65_536]);
        loop {
            let remaining = limit - bytes.len();
            if remaining == 0 {
                return Err(IoError::Limit);
            }
            let mut read = 0;
            let wanted = remaining.min(chunk.len()) as u32;
            if unsafe {
                ReadFile(
                    input,
                    chunk.as_mut_ptr(),
                    wanted,
                    &mut read,
                    ptr::null_mut(),
                )
            } == 0
                || read > wanted
            {
                return Err(IoError::Read);
            }
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read as usize]);
            if bytes.len() > MAX_FILE_BYTES {
                return Err(IoError::Limit);
            }
        }
        let after = information(input, false)?;
        if size(&before) != bytes.len() as u64
            || size(&after) != size(&before)
            || before.volume != after.volume
            || before.index_high != after.index_high
            || before.index_low != after.index_low
        {
            return Err(IoError::Read);
        }
        Ok(bytes)
    })();
    if close_all(&mut handles) {
        outcome
    } else {
        Err(IoError::CleanupUncertain)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Preparing,
    CommitAttempted,
    Published,
    Unknown,
    Discarded,
}
pub(super) struct ExportStage {
    handles: Vec<Owned>,
    name: Vec<u16>,
    bytes: usize,
    parent_count: usize,
    state: State,
}
struct RenameRequest {
    status: IoStatusBlock,
    _buffer: Vec<usize>,
    handles: Vec<Owned>,
}
impl ExportStage {
    pub(super) fn prepare(path: &Path, encrypted: &[u8]) -> Result<Self, IoError> {
        if encrypted.len() > MAX_FILE_BYTES {
            return Err(IoError::Limit);
        }
        let path = chosen(path)?;
        let mut handles = parents(&path)?;
        let parent_count = handles.len();
        let prepared = (|| {
            let user = User::current()?;
            let mut created = None;
            for _ in 0..8 {
                let mut random = [0_u8; 16];
                if unsafe {
                    BCryptGenRandom(ptr::null_mut(), random.as_mut_ptr(), random.len() as u32, 2)
                } != 0
                {
                    return Err(IoError::Write);
                }
                let mut text = String::from(".morpheus-encrypted-");
                use std::fmt::Write;
                for byte in random {
                    write!(&mut text, "{byte:02x}").map_err(|_| IoError::Write)?;
                }
                text.push_str(".tmp");
                let name = component(OsStr::new(&text))?;
                if ascii_name_eq(&name, path.components.last().ok_or(IoError::InvalidPath)?) {
                    continue;
                }
                match relative(
                    &mut handles,
                    &name,
                    DIRECTORY_ACCESS | READ_CONTROL | DELETE,
                    3,
                    true,
                    true,
                    Some(user.descriptor()?),
                ) {
                    Ok(handle) => {
                        created = Some(handle);
                        break;
                    }
                    Err(IoError::Exists) => continue,
                    Err(error) => return Err(error),
                }
            }
            handles.push(created.ok_or(IoError::Write)?);
            let directory = handles.last().ok_or(IoError::Write)?.0;
            information(directory, true)?;
            private_file(directory, &user)?;
            // Short names of this child cannot resolve in the chosen original
            // parent. A staging-directory alias exposes no encrypted file at
            // the selected destination before final caller admission.
            let child = component(OsStr::new("encrypted.keys"))?;
            let file = relative(
                &mut handles,
                &child,
                READ_CONTROL | SYNCHRONIZE | 0x196 | DELETE,
                0,
                true,
                false,
                Some(user.descriptor()?),
            )?;
            handles.push(file);
            let stage = handles.last().ok_or(IoError::Write)?.0;
            information(stage, false)?;
            private_file(stage, &user)?;
            let mut position = 0;
            while position < encrypted.len() {
                let wanted = (encrypted.len() - position).min(65_536) as u32;
                let mut written = 0;
                if unsafe {
                    WriteFile(
                        stage,
                        encrypted[position..].as_ptr(),
                        wanted,
                        &mut written,
                        ptr::null_mut(),
                    )
                } == 0
                    || written == 0
                    || written > wanted
                {
                    return Err(IoError::Write);
                }
                position += written as usize;
            }
            if unsafe { FlushFileBuffers(stage) } == 0
                || size(&information(stage, false)?) != encrypted.len() as u64
            {
                return Err(IoError::Write);
            }
            private_file(stage, &user)?;
            Ok(())
        })();
        if let Err(error) = prepared {
            let cleanup = cleanup_preparing(&mut handles, parent_count);
            return if cleanup {
                Err(error)
            } else {
                Err(IoError::CleanupUncertain)
            };
        }
        Ok(Self {
            handles,
            name: path.components.last().ok_or(IoError::InvalidPath)?.clone(),
            bytes: encrypted.len(),
            parent_count,
            state: State::Preparing,
        })
    }
    pub(super) fn publish(&mut self) -> Result<Publication, IoError> {
        if self.state != State::Preparing
            || self.handles.len() != self.parent_count + 2
            || self.parent_count == 0
        {
            return Err(IoError::CommitUncertain);
        }
        let parent = self.handles[self.parent_count - 1].0;
        let file = self.handles.last().ok_or(IoError::CommitUncertain)?.0;
        let user = User::current()?;
        let directory = self.handles[self.parent_count].0;
        information(directory, true)?;
        private_file(directory, &user)?;
        if size(&information(file, false)?) != self.bytes as u64 {
            return Err(IoError::Write);
        }
        private_file(file, &user)?;
        let name_bytes = self.name.len().checked_mul(2).ok_or(IoError::Limit)?;
        // Microsoft requires full sizeof(FILE_RENAME_INFORMATION) + filename
        // bytes. Keep its padding AND an extra UTF16 NUL in the supplied size.
        let length = size_of::<RenameInfo>()
            .checked_add(name_bytes)
            .and_then(|n| n.checked_add(2))
            .ok_or(IoError::Limit)?;
        let supplied = u32::try_from(length).map_err(|_| IoError::Limit)?;
        let mut storage = vec![0_usize; length.div_ceil(size_of::<usize>())];
        let info = storage.as_mut_ptr().cast::<RenameInfo>();
        // SAFETY: usize-aligned backing spans the fixed header and complete name
        // including NUL. FileNameLength excludes NUL. Zero union means no replace.
        unsafe {
            (*info).replace_or_flags = 0;
            (*info).root = parent;
            (*info).bytes = u32::try_from(name_bytes).map_err(|_| IoError::Limit)?;
            ptr::copy_nonoverlapping(
                self.name.as_ptr(),
                ptr::addr_of_mut!((*info).name).cast(),
                self.name.len(),
            );
        }
        let request = Box::new(RenameRequest {
            status: IoStatusBlock {
                status: IoStatus {
                    status: STATUS_PENDING,
                },
                information: usize::MAX,
            },
            _buffer: storage,
            handles: std::mem::take(&mut self.handles),
        });
        self.state = State::CommitAttempted;
        let request = Box::into_raw(request);
        let status = unsafe {
            NtSetInformationFile(
                file,
                ptr::addr_of_mut!((*request).status),
                info.cast(),
                supplied,
                10,
            )
        };
        if status > 0 {
            self.state = State::Unknown;
            // Native-pending/unknown memory and all actual owners remain raw,
            // untouched and retained. No cleanup, field access or retry.
            return Err(IoError::NativeUncertain);
        }
        if status == STATUS_OBJECT_NAME_COLLISION {
            // MS-FSA 2.1.5.15.12 fails here BEFORE any link mutation when a
            // different target exists and ReplaceIfExists=false. This one
            // completed negative status proves our stage is unpublished.
            let mut completed = unsafe { Box::from_raw(request) };
            self.handles = std::mem::take(&mut completed.handles);
            self.state = State::Preparing;
            return Err(IoError::Exists);
        }
        if status < 0 {
            self.state = State::Unknown;
            return Err(IoError::CommitUncertain);
        }
        if unsafe { ptr::read(ptr::addr_of!((*request).status.status.status)) } != 0 {
            self.state = State::Unknown;
            return Err(IoError::NativeUncertain);
        }
        let mut completed = unsafe { Box::from_raw(request) };
        self.handles = std::mem::take(&mut completed.handles);
        self.state = State::Published;
        if cleanup_published(&mut self.handles, self.parent_count) {
            Ok(Publication::Published)
        } else {
            Ok(Publication::PublishedCleanupUncertain)
        }
    }
    pub(super) fn discard(mut self) -> Result<(), IoError> {
        if self.state != State::Preparing {
            return Err(IoError::CommitUncertain);
        }
        let clean = cleanup_preparing(&mut self.handles, self.parent_count);
        self.state = if clean {
            State::Discarded
        } else {
            State::Unknown
        };
        if clean {
            Ok(())
        } else {
            Err(IoError::CleanupUncertain)
        }
    }
}
// Deliberately no Drop filesystem operations: an unknown commit may already name
// the chosen destination. Unknown/panic owners stay retained, never path-deleted.
fn remove_created_last(handles: &mut Vec<Owned>) -> bool {
    let Some(file) = handles.last() else {
        return false;
    };
    let delete = 1_u8; // FILE_DISPOSITION_INFO.DeleteFile BOOLEAN, not BOOL.
    if unsafe { SetFileInformationByHandle(file.0, 4, (&delete as *const u8).cast(), 1) } == 0 {
        return false;
    }
    let Some(file) = handles.pop() else {
        return false;
    };
    file.close()
}
fn cleanup_preparing(handles: &mut Vec<Owned>, parent_count: usize) -> bool {
    if handles.is_empty() {
        return true;
    } // Pending already retained owners; original error remains uncertain.
    if handles.len() < parent_count || handles.len() > parent_count + 2 {
        return false;
    }
    // Delete the known-created child first, then the now-empty known-created
    // private directory. Never recurse or remove an unrelated added entry.
    while handles.len() > parent_count {
        if !remove_created_last(handles) {
            return false;
        }
    }
    close_all(handles)
}
fn cleanup_published(handles: &mut Vec<Owned>, parent_count: usize) -> bool {
    if handles.len() != parent_count + 2 {
        return false;
    }
    let Some(published) = handles.pop() else {
        return false;
    };
    // The renamed destination is ONLY closed, never marked for deletion.
    if !published.close() {
        return false;
    }
    if !remove_created_last(handles) {
        return false;
    }
    close_all(handles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;
    use std::os::windows::io::AsRawHandle;

    // All observations are gathered before fixture cleanup and assertions. An
    // unexpected uncertain capability is intentionally not converted into a
    // successful cleanup result merely to let a regression pass.
    fn fixture(run: impl FnOnce(&Path) -> Result<(), &'static str>) {
        let directory = tempfile::tempdir().expect("synthetic encrypted-file root");
        let outcome = run(directory.path());
        let cleanup = directory.close();
        assert!(cleanup.is_ok(), "synthetic encrypted-file cleanup failed");
        assert!(
            outcome.is_ok(),
            "synthetic encrypted-file observation failed"
        );
    }

    #[test]
    fn encrypted_stage_publishes_private_bytes_and_discard_has_no_destination() {
        fixture(|root| {
            let target = root.join("encrypted.keys");
            let mut stage =
                ExportStage::prepare(&target, b"encrypted fixture only").map_err(|_| "prepare")?;
            if target.exists() {
                let _cleanup = stage.discard();
                return Err("early publication");
            }
            let layout = std::fs::read_dir(root).ok().and_then(|mut entries| {
                let entry = entries.next()?.ok()?;
                Some(entries.next().is_none() && entry.file_type().ok()?.is_dir())
            });
            if layout != Some(true) {
                let _cleanup = stage.discard();
                return Err("private directory staging");
            }
            if !matches!(stage.publish(), Ok(Publication::Published)) {
                // Preparing failures are explicitly discarded. Attempted or
                // published states refuse this without deletion or retry.
                let _cleanup = stage.discard();
                return Err("publish");
            }
            let read = read_encrypted(&target).map_err(|_| "read")?;
            if &**read != b"encrypted fixture only" {
                return Err("bytes");
            }
            let file = std::fs::File::open(&target).map_err(|_| "open receipt")?;
            let user = User::current().map_err(|_| "token")?;
            let private = private_file(file.as_raw_handle(), &user).is_ok();
            drop(file);
            if !private {
                return Err("private descriptor");
            }
            let other = root.join("discarded.keys");
            ExportStage::prepare(&other, b"encrypted discarded fixture")
                .map_err(|_| "discard prepare")?
                .discard()
                .map_err(|_| "discard")?;
            if other.exists() || std::fs::read_dir(root).map_err(|_| "inventory")?.count() != 1 {
                return Err("discard inventory");
            }
            Ok(())
        });
    }

    #[test]
    fn encrypted_read_refuses_hardlinks_and_over_limit_before_reading() {
        fixture(|root| {
            let source = root.join("encrypted.keys");
            let alias = root.join("alias.keys");
            std::fs::write(&source, b"encrypted shared fixture").map_err(|_| "fixture write")?;
            std::fs::hard_link(&source, &alias).map_err(|_| "fixture hardlink")?;
            let hardlink_refused = matches!(read_encrypted(&source), Err(IoError::NotRegular));
            std::fs::remove_file(&alias).map_err(|_| "fixture unlink")?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&source)
                .map_err(|_| "fixture open")?;
            file.set_len(MAX_FILE_BYTES as u64 + 1)
                .map_err(|_| "fixture length")?;
            drop(file);
            let limit_refused = matches!(read_encrypted(&source), Err(IoError::Limit));
            if !hardlink_refused || !limit_refused {
                return Err("bounded single-link admission");
            }
            Ok(())
        });
    }

    #[test]
    fn private_stage_rejects_an_additional_foreign_read_ace() {
        fixture(|root| {
            let path = chosen(&root.join("foreign-ace.keys")).map_err(|_| "path")?;
            let mut handles = parents(&path).map_err(|_| "parents")?;
            let observed = (|| {
                let user = User::current().map_err(|_| "token")?;
                let sid = user.sid_string().map_err(|_| "sid")?;
                let text = wide(&format!(
                    "O:{sid}D:P(A;;FA;;;SY)(A;;FA;;;{sid})(A;;FR;;;WD)"
                ));
                let mut descriptor = ptr::null_mut();
                if unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        text.as_ptr(),
                        1,
                        &mut descriptor,
                        ptr::null_mut(),
                    )
                } == 0
                {
                    return Err("fixture descriptor");
                }
                let file = relative(
                    &mut handles,
                    path.components.last().ok_or("name")?,
                    READ_CONTROL | SYNCHRONIZE | 0x196 | DELETE,
                    0,
                    true,
                    false,
                    Some(Local(descriptor)),
                )
                .map_err(|_| "fixture create")?;
                handles.push(file);
                Ok(private_file(handles.last().ok_or("file")?.0, &user).is_err())
            })();
            let created = handles.len() == path.components.len() + 1;
            let clean = if created {
                remove_created_last(&mut handles) && close_all(&mut handles)
            } else {
                close_all(&mut handles)
            };
            if !clean || !matches!(observed, Ok(true)) {
                return Err("foreign ACL rejection/cleanup");
            }
            Ok(())
        });
    }

    fn collision_case(created_after_prepare: bool) {
        fixture(|root| {
            let target = root.join("existing-backup.keys");
            let original = b"existing encrypted backup fixture";
            if !created_after_prepare {
                std::fs::write(&target, original).map_err(|_| "existing target fixture")?;
            }
            let mut stage = ExportStage::prepare(&target, b"new encrypted backup fixture")
                .map_err(|_| "prepare collision fixture")?;
            if created_after_prepare && std::fs::write(&target, original).is_err() {
                let _cleanup = stage.discard();
                return Err("racing target fixture");
            }
            let publication = stage.publish();
            let remained_preparing = stage.state == State::Preparing;
            let observed = std::fs::read(&target);
            // Always request actual exact-handle cleanup before assertions,
            // including unexpected results. Unknown/Published refuses safely.
            let discarded = stage.discard();
            let entries = std::fs::read_dir(root).map(|entries| entries.count());
            if !matches!(publication, Err(IoError::Exists))
                || !remained_preparing
                || !discarded.is_ok()
                || !matches!(observed, Ok(ref bytes) if bytes.as_slice() == original)
                || !matches!(entries, Ok(1))
            {
                return Err("no-clobber collision or checked cleanup");
            }
            Ok(())
        });
    }

    #[test]
    fn encrypted_publish_preserves_an_existing_destination_and_discards_its_stage() {
        collision_case(false);
    }

    #[test]
    fn encrypted_publish_preserves_a_destination_created_after_prepare_and_discards_its_stage() {
        collision_case(true);
    }

    #[test]
    fn selected_paths_reject_ambient_names_streams_and_normalized_parent_aliases() {
        assert_eq!(size_of::<UnicodeString>(), 2 * size_of::<usize>());
        assert_eq!(size_of::<ObjectAttributes>(), 6 * size_of::<usize>());
        assert_eq!(size_of::<IoStatusBlock>(), 2 * size_of::<usize>());
        assert_eq!(offset_of!(RenameInfo, name), 2 * size_of::<usize>() + 4);
        for path in [
            "relative.keys",
            "C:relative.keys",
            "\\\\server\\share\\file",
            "\\\\.\\NUL",
            "C:\\x\\..\\file",
            "C:\\x\\.\\file",
            "C:\\x\\file:stream",
            "C:\\x\\NUL.txt",
            "C:\\x\\COM1.keys",
            "C:\\x\\LPT¹.keys",
            "C:\\x\\file.",
            "C:\\x\\file ",
            "C:/x/file",
        ] {
            assert!(
                chosen(Path::new(path)).is_err(),
                "unsupported selected-path form admitted"
            );
        }
        assert!(chosen(Path::new("C:\\selected\\encrypted.keys")).is_ok());
        assert!(chosen(Path::new("\\\\?\\C:\\selected\\encrypted.keys")).is_ok());
    }
}
