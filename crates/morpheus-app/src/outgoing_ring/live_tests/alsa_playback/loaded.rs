//! Admission for the actual loaded Ubuntu24 ALSA object, before any PCM open.
//! No dlopen, plugin/device open, environment mutation or guessed SONAME proof.
use super::Failure;
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    fs::{File, Metadata, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
};

const PATH: &str = "/usr/lib/x86_64-linux-gnu/libasound.so.2.0.0";
const SIZE: u64 = 1_084_144;
const HASH: &str = "ff4c69ef420f2d0ab92c813d2222d097c6a7eafe3b13952aed069e2bb0204f55";
const VERSION: &[u8] = b"1.2.11\0";
const MAX_MAPS: usize = 1024 * 1024;

// Exact pinned ALSA1.2.11 include/global.h and src/pcm/*_open declarations.
// Only the version getter is invoked. Function addresses also bind the actual
// linked PCM entrypoint and all three built-in plugin implementations.
#[link(name = "asound")]
unsafe extern "C" {
    fn snd_asoundlib_version() -> *const c_char;
    fn snd_pcm_open(
        pcm: *mut *mut c_void,
        name: *const c_char,
        stream: c_uint,
        mode: c_int,
    ) -> c_int;
    fn _snd_pcm_file_open(
        pcm: *mut *mut c_void,
        name: *const c_char,
        root: *mut c_void,
        conf: *mut c_void,
        stream: c_uint,
        mode: c_int,
    ) -> c_int;
    fn _snd_pcm_null_open(
        pcm: *mut *mut c_void,
        name: *const c_char,
        root: *mut c_void,
        conf: *mut c_void,
        stream: c_uint,
        mode: c_int,
    ) -> c_int;
    fn _snd_pcm_plug_open(
        pcm: *mut *mut c_void,
        name: *const c_char,
        root: *mut c_void,
        conf: *mut c_void,
        stream: c_uint,
        mode: c_int,
    ) -> c_int;
}

fn require(value: bool) -> Result<(), Failure> {
    value.then_some(()).ok_or(Failure::Library)
}
fn close(file: File) -> Result<(), Failure> {
    // Never retry or reconstruct a numeric fd after an uncertain close.
    if unsafe { libc::close(file.into_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(Failure::Cleanup)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Mapping {
    start: usize,
    end: usize,
    perms: [u8; 4],
    offset: u64,
    dev: u64,
    ino: u64,
    path: String,
}
impl Mapping {
    fn covers(&self, address: usize, size: usize) -> bool {
        size != 0
            && self.start <= address
            && address.checked_add(size).is_some_and(|end| end <= self.end)
    }
}
fn hex(value: &str) -> Result<u64, Failure> {
    require(
        !value.is_empty() && value.len() <= 16 && value.bytes().all(|c| c.is_ascii_hexdigit()),
    )?;
    u64::from_str_radix(value, 16).map_err(|_| Failure::Library)
}
fn parse_maps(bytes: &[u8]) -> Result<Vec<Mapping>, Failure> {
    require(!bytes.is_empty() && bytes.len() <= MAX_MAPS && !bytes.contains(&0))?;
    let text = std::str::from_utf8(bytes).map_err(|_| Failure::Library)?;
    require(text.ends_with('\n'))?;
    let mut maps = Vec::new();
    for line in text.lines() {
        require(line.len() <= 8192 && maps.len() < 8192)?;
        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
        require(fields.len() >= 5)?;
        let (start, end) = fields[0].split_once('-').ok_or(Failure::Library)?;
        let start = usize::try_from(hex(start)?).map_err(|_| Failure::Library)?;
        let end = usize::try_from(hex(end)?).map_err(|_| Failure::Library)?;
        let perms: [u8; 4] = fields[1]
            .as_bytes()
            .try_into()
            .map_err(|_| Failure::Library)?;
        require(
            start < end
                && matches!(perms[0], b'r' | b'-')
                && matches!(perms[1], b'w' | b'-')
                && matches!(perms[2], b'x' | b'-')
                && matches!(perms[3], b'p' | b's'),
        )?;
        let (major, minor) = fields[3].split_once(':').ok_or(Failure::Library)?;
        let major = u32::try_from(hex(major)?).map_err(|_| Failure::Library)?;
        let minor = u32::try_from(hex(minor)?).map_err(|_| Failure::Library)?;
        require(!fields[4].is_empty() && fields[4].bytes().all(|c| c.is_ascii_digit()))?;
        let ino = fields[4].parse::<u64>().map_err(|_| Failure::Library)?;
        maps.try_reserve(1).map_err(|_| Failure::Library)?;
        maps.push(Mapping {
            start,
            end,
            perms,
            offset: hex(fields[2])?,
            dev: libc::makedev(major, minor),
            ino,
            path: fields[5..].join(" "),
        });
    }
    require(!maps.is_empty())?;
    Ok(maps)
}
fn read_maps() -> Result<Vec<Mapping>, Failure> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/proc/self/maps")
        .map_err(|_| Failure::Library)?;
    let result = (|| {
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        require(
            unsafe { libc::fstatfs(file.as_raw_fd(), &mut info) } == 0
                && info.f_type == libc::PROC_SUPER_MAGIC,
        )?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(MAX_MAPS + 1)
            .map_err(|_| Failure::Library)?;
        (&file)
            .take((MAX_MAPS + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::Library)?;
        parse_maps(&bytes)
    })();
    let closed = close(file);
    closed.and(result)
}
fn containing(maps: &[Mapping], address: usize, size: usize) -> Result<&Mapping, Failure> {
    let mut found = maps.iter().filter(|m| m.covers(address, size));
    let one = found.next().ok_or(Failure::Library)?;
    require(found.next().is_none())?;
    Ok(one)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Object {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl Object {
    fn of(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            links: m.nlink(),
            size: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }
    fn admitted(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
            && self.mode & 0o7777 == 0o644
            && self.uid == 0
            && self.gid == 0
            && self.links == 1
            && self.size == SIZE
            && self.ino != 0
    }
}
fn directory(file: &File) -> Result<(u64, u64, u32, u32, u32), Failure> {
    let m = file.metadata().map_err(|_| Failure::Library)?;
    require(m.is_dir() && m.uid() == 0 && m.mode() & 0o7022 == 0)?;
    Ok((m.dev(), m.ino(), m.mode(), m.uid(), m.gid()))
}
fn open_at(parent: &File, name: &CStr, directory: bool) -> Result<File, Failure> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 {
        Err(Failure::Library)
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn digest(file: &File) -> Result<(), Failure> {
    let mut sha = Sha256::new();
    let mut offset = 0;
    let mut bytes = [0; 8192];
    while offset <= SIZE {
        let remaining = usize::try_from((SIZE + 1 - offset).min(bytes.len() as u64))
            .map_err(|_| Failure::Library)?;
        let n = file
            .read_at(&mut bytes[..remaining], offset)
            .map_err(|_| Failure::Library)?;
        if n == 0 {
            break;
        }
        offset += n as u64;
        require(offset <= SIZE)?;
        sha.update(&bytes[..n]);
    }
    require(offset == SIZE && format!("{:x}", sha.finalize()) == HASH)
}
fn same_object(map: &Mapping, object: Object) -> bool {
    map.path == PATH && map.dev == object.dev && map.ino == object.ino && map.ino != 0
}
fn version_span(maps: &[Mapping], object: Object, address: usize) -> Result<(), Failure> {
    let bytes = containing(maps, address, VERSION.len())?;
    require(same_object(bytes, object) && bytes.perms[0] == b'r' && bytes.perms[1] != b'w')
}
fn version_bytes(bytes: &[u8]) -> Result<(), Failure> {
    require(bytes == VERSION)
}
fn admitted_maps(
    maps: &[Mapping],
    object: Object,
    symbols: &[usize; 5],
    base: usize,
) -> Result<Vec<Mapping>, Failure> {
    require(object.admitted())?;
    for address in symbols {
        let map = containing(maps, *address, 1)?;
        require(same_object(map, object) && map.perms == *b"r-xp")?;
    }
    let first = containing(maps, base, 1)?;
    require(
        same_object(first, object)
            && first.start == base
            && first.offset == 0
            && first.perms == *b"r--p",
    )?;
    let selected = maps
        .iter()
        .filter(|m| m.dev == object.dev && m.ino == object.ino)
        .cloned()
        .collect::<Vec<_>>();
    require(
        !selected.is_empty()
            && selected.len() <= 16
            && selected.iter().filter(|m| m.offset == 0).count() == 1
            && selected
                .iter()
                .all(|m| m.path == PATH && !(m.perms[1] == b'w' && m.perms[2] == b'x')),
    )?;
    Ok(selected)
}

pub(super) struct Identity {
    file: Option<File>,
    parents: Vec<File>,
    parent_ids: Vec<(u64, u64, u32, u32, u32)>,
    object: Object,
    maps: Vec<Mapping>,
    symbols: [usize; 5],
    base: usize,
    version: unsafe extern "C" fn() -> *const c_char,
}
impl Identity {
    pub(super) fn observe() -> Result<Self, Failure> {
        require(cfg!(target_arch = "x86_64"))?;
        let version = snd_asoundlib_version;
        let symbols = [
            version as *const () as usize,
            snd_pcm_open as *const () as usize,
            _snd_pcm_file_open as *const () as usize,
            _snd_pcm_null_open as *const () as usize,
            _snd_pcm_plug_open as *const () as usize,
        ];
        let mut base = None;
        for address in symbols {
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            require(
                unsafe { libc::dladdr(address as *const c_void, &mut info) } != 0
                    && info.dli_saddr as usize == address
                    && !info.dli_fbase.is_null(),
            )?;
            let observed = info.dli_fbase as usize;
            require(base.is_none_or(|base| base == observed))?;
            base = Some(observed);
        }
        let base = base.ok_or(Failure::Library)?;
        let before = read_maps()?;
        // The actual code-address map provides the pathname. Neither the dladdr
        // filename pointer nor a guessed disk SONAME is dereferenced/trusted.
        require(containing(&before, symbols[0], 1)?.path == PATH)?;
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open("/")
            .map_err(|_| Failure::Library)?;
        let mut parents = vec![root];
        for component in [c"usr", c"lib", c"x86_64-linux-gnu"] {
            directory(parents.last().ok_or(Failure::Library)?)?;
            parents.push(open_at(
                parents.last().ok_or(Failure::Library)?,
                component,
                true,
            )?);
        }
        let parent_ids = parents
            .iter()
            .map(directory)
            .collect::<Result<Vec<_>, _>>()?;
        let file = open_at(
            parents.last().ok_or(Failure::Library)?,
            c"libasound.so.2.0.0",
            false,
        )?;
        let object = Object::of(&file.metadata().map_err(|_| Failure::Library)?);
        let maps = admitted_maps(&before, object, &symbols, base)?;
        digest(&file)?;
        let value = Self {
            file: Some(file),
            parents,
            parent_ids,
            object,
            maps,
            symbols,
            base,
            version,
        };
        value.verify()?;
        Ok(value)
    }
    pub(super) fn verify(&self) -> Result<(), Failure> {
        let file = self.file.as_ref().ok_or(Failure::Library)?;
        require(Object::of(&file.metadata().map_err(|_| Failure::Library)?) == self.object)?;
        for (parent, expected) in self.parents.iter().zip(&self.parent_ids) {
            require(directory(parent)? == *expected)?;
        }
        let before = read_maps()?;
        require(admitted_maps(&before, self.object, &self.symbols, self.base)? == self.maps)?;
        digest(file)?;
        // Invoke the SAME function-pointer value whose actual mapped object was
        // admitted above. A PLT/interposed/foreign object never reaches this call.
        let version = unsafe { (self.version)() } as usize;
        let after = read_maps()?;
        version_span(&after, self.object, version)?;
        // Pinned getter returns a static literal. Prove its full bounded readable
        // same-object span before any dereference; never scan a foreign C string.
        let returned = unsafe { std::slice::from_raw_parts(version as *const u8, VERSION.len()) };
        version_bytes(returned)?;
        require(
            admitted_maps(&after, self.object, &self.symbols, self.base)? == self.maps
                && Object::of(&file.metadata().map_err(|_| Failure::Library)?) == self.object,
        )?;
        for (parent, expected) in self.parents.iter().zip(&self.parent_ids) {
            require(directory(parent)? == *expected)?;
        }
        Ok(())
    }
    pub(super) fn record(&self) -> Vec<u8> {
        format!(
            "{{\"schema\":1,\"version\":\"1.2.11\",\"package\":\"libasound2t64_1.2.11-1ubuntu0.3_amd64\",\"path\":\"{PATH}\",\"sha256\":\"{HASH}\",\"dev\":{},\"inode\":{},\"uid\":{},\"gid\":{},\"mode\":{},\"bytes\":{},\"mapped_regions\":{},\"bound_symbols\":5}}\n",
            self.object.dev,
            self.object.ino,
            self.object.uid,
            self.object.gid,
            self.object.mode,
            self.object.size,
            self.maps.len(),
        )
        .into_bytes()
    }
    pub(super) fn close(&mut self) -> Result<(), Failure> {
        let mut result = Ok(());
        if let Some(file) = self.file.take() {
            result = close(file);
        }
        for file in self.parents.drain(..) {
            if close(file).is_err() {
                result = Err(Failure::Cleanup);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object() -> Object {
        Object {
            dev: libc::makedev(8, 1),
            ino: 42,
            mode: libc::S_IFREG | 0o644,
            uid: 0,
            gid: 0,
            links: 1,
            size: SIZE,
            modified: (1, 0),
            changed: (1, 0),
        }
    }
    fn maps() -> Vec<Mapping> {
        parse_maps(
            format!(
                "1000-2000 r--p 00000000 08:01 42 {PATH}\n2000-3000 r-xp 00001000 08:01 42 {PATH}\n"
            )
            .as_bytes(),
        )
        .unwrap()
    }
    #[test]
    fn malformed_ambiguous_and_foreign_loaded_objects_are_refused() {
        let symbols = [0x2100; 5];
        let valid = maps();
        assert!(admitted_maps(&valid, object(), &symbols, 0x1000).is_ok());
        for invalid in [
            b"".as_slice(),
            b"x\n",
            b"1000-2000 rwxp 0 zz:01 42 /lib\n",
            b"1000-1000 r-xp 0 08:01 42 /lib\n",
        ] {
            assert!(parse_maps(invalid).is_err());
        }
        let mut duplicate = valid.clone();
        duplicate.push(valid[1].clone());
        assert!(admitted_maps(&duplicate, object(), &symbols, 0x1000).is_err());
        for change in 0..6 {
            let mut wrong = valid.clone();
            match change {
                0 => wrong[1].ino += 1,
                1 => wrong[1].dev += 1,
                2 => wrong[1].path.push_str(" (deleted)"),
                3 => wrong[1].perms = *b"rwxp",
                4 => wrong[1].path = "/tmp/libasound.so.2.0.0".into(),
                _ => wrong[0].offset = 1,
            }
            assert!(admitted_maps(&wrong, object(), &symbols, 0x1000).is_err());
        }
        let mut wrong = object();
        wrong.uid = 1001;
        assert!(admitted_maps(&valid, wrong, &symbols, 0x1000).is_err());
        wrong = object();
        wrong.mode |= 0o020;
        assert!(admitted_maps(&valid, wrong, &symbols, 0x1000).is_err());
    }
    #[test]
    fn version_bytes_require_exact_supported_value_and_bounded_readable_span() {
        let valid = maps();
        assert!(version_span(&valid, object(), 0x1100).is_ok());
        assert!(version_span(&valid, object(), 0x1ffe).is_err());
        assert!(version_span(&valid, object(), usize::MAX - 1).is_err());
        let mut writable = valid.clone();
        writable[0].perms = *b"rw-p";
        assert!(version_span(&writable, object(), 0x1100).is_err());
        let mut foreign = valid;
        foreign[0].ino += 1;
        assert!(version_span(&foreign, object(), 0x1100).is_err());
        assert!(version_bytes(b"1.2.11\0").is_ok());
        for rejected in [b"1.2.10\0".as_slice(), b"1.2.12\0", b"1.2.11", b"1.2.11\0x"] {
            assert!(version_bytes(rejected).is_err());
        }
    }
    #[test]
    fn wrong_object_bytes_are_refused_at_the_exact_supported_length() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(SIZE).unwrap();
        assert_eq!(digest(&file), Err(Failure::Library));
        assert_eq!(file.metadata().unwrap().len(), SIZE);
    }
}
