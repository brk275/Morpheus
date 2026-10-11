"""Authenticate the ordinary installed AppImage's process and mapped client.

No preload/library-path override, execution of inspected ELF, or production
reporting endpoint is used. /proc evidence and the loader trace stay private.
The caller owns an unreaped launcher for the entire lifetime of this handle.
"""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import re
import select
import stat
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import package_smoke as smoke
import appimage_client as client
from appimage_bindings import imported_pipewire_symbols, provider_spellings, verify_binding_trace


class IdentityFailure(RuntimeError):
    def __init__(self):
        super().__init__("Installed application runtime identity could not be proved")


@dataclass(frozen=True)
class FileIdentity:
    path: str
    device_major: int
    device_minor: int
    inode: int

    @classmethod
    def opened(cls, path, info):
        return cls(str(path), os.major(info.st_dev), os.minor(info.st_dev), info.st_ino)


def validate_map_inventory(contents, executable, library):
    if not isinstance(contents, bytes) or not 0 < len(contents) <= 1024 * 1024:
        raise IdentityFailure()
    try:
        lines = contents.decode("utf-8", "strict").splitlines()
    except UnicodeError:
        raise IdentityFailure() from None
    executable_seen = library_seen = False
    grammar = re.compile(r"([0-9a-f]+)-([0-9a-f]+) ([r-][w-][x-][ps]) ([0-9a-f]+) ([0-9a-f]+):([0-9a-f]+) ([0-9]+)(?: +(.*))?")
    if len(lines) > 16384:
        raise IdentityFailure()
    for line in lines:
        match = grammar.fullmatch(line)
        if not match or int(match[1], 16) >= int(match[2], 16):
            raise IdentityFailure()
        path = match[8] or ""
        if "libpipewire-0.3.so" in Path(path).name and path != library.path:
            raise IdentityFailure()
        for expected, role in ((executable, "executable"), (library, "library")):
            if path != expected.path:
                continue
            actual = (int(match[5], 16), int(match[6], 16), int(match[7]))
            if actual != (expected.device_major, expected.device_minor, expected.inode):
                raise IdentityFailure()
            if "x" in match[3]:
                if "w" in match[3] or "r" not in match[3]:
                    raise IdentityFailure()
                if role == "executable":
                    executable_seen = True
                else:
                    library_seen = True
    if not executable_seen or not library_seen:
        raise IdentityFailure()
    return {"executable_mapped": True, "app_local_pipewire_mapped": True, "unexpected_pipewire_provider": False}


def file_facts(info):
    return info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_size, info.st_mtime_ns, info.st_ctime_ns


def collision_symbols():
    contents = client.read_regular(ROOT / "vendor/webrtc-sys/pipewire-symbols.txt", 32768)
    names = set()
    for row in contents.decode("ascii").splitlines():
        match = re.fullmatch(r"[a-z0-9_]+\.o (pw_[a-z0-9_]+) ([UW])", row)
        if not match:
            raise IdentityFailure()
        if match[2] == "W":
            names.add(match[1])
    if len(names) != 33:
        raise IdentityFailure()
    return names


def read_opened(path, maximum):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        info = os.fstat(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o022 or not 0 < info.st_size <= maximum):
            raise IdentityFailure()
        chunks = []
        remaining = info.st_size
        while remaining:
            chunk = os.read(descriptor, min(remaining, 1024 * 1024))
            if not chunk:
                raise IdentityFailure()
            chunks.append(chunk)
            remaining -= len(chunk)
        if os.read(descriptor, 1) or file_facts(os.fstat(descriptor)) != file_facts(info):
            raise IdentityFailure()
        return descriptor, info, b"".join(chunks)
    except BaseException:
        os.close(descriptor)
        raise


class RuntimeIdentity:
    def __init__(self, pid, launcher, nonce, temporary_root, expected_appdir):
        self.descriptors = []
        self.pid = pid
        self.launcher = launcher
        self.nonce = nonce
        self.start = None
        self.verified_library_aliases = set()
        try:
            if not smoke.owned_linux_pid(pid, launcher.pid, nonce):
                raise IdentityFailure()
            self.start = smoke.linux_process_identity(pid)
            self.pidfd = os.pidfd_open(pid)
            self.descriptors.append(self.pidfd)
            self.executable = Path(os.readlink(f"/proc/{pid}/exe"))
            # The actual installed runtime extracted the AppImage under this
            # invocation's TMPDIR. Match whole shipped bytes, not a basename.
            if (not self.executable.is_relative_to(temporary_root)
                    or self.executable.parts[-3:] != ("usr", "bin", "morpheus-app")
                    or self.executable.resolve() != self.executable):
                raise IdentityFailure()
            actual_appdir = self.executable.parents[2]
            self.library = actual_appdir / "usr/lib/libpipewire-0.3.so.0"
            if self.library.resolve() != self.library:
                raise IdentityFailure()
            for relative, maximum in (("usr/bin/morpheus-app", 512 * 1024 * 1024),
                                      ("usr/lib/libpipewire-0.3.so.0", 64 * 1024 * 1024),
                                      ("AppRun", 4096)):
                descriptor, info, contents = read_opened(actual_appdir / relative, maximum)
                self.descriptors.append(descriptor)
                expected = client.read_regular(expected_appdir / relative, maximum)
                if contents != expected:
                    raise IdentityFailure()
                if relative == "usr/bin/morpheus-app":
                    self.app_identity = FileIdentity.opened(self.executable, info)
                    self.executable_sha256 = hashlib.sha256(contents).hexdigest()
                    if not info.st_mode & 0o111:
                        raise IdentityFailure()
                elif relative == "usr/lib/libpipewire-0.3.so.0":
                    self.library_identity = FileIdentity.opened(self.library, info)
                    self.library_sha256 = hashlib.sha256(contents).hexdigest()
            # /proc/exe must still refer to the exact opened executable inode.
            linked = os.stat(f"/proc/{pid}/exe")
            if FileIdentity.opened(self.executable, linked) != self.app_identity:
                raise IdentityFailure()
            self.check()
        except BaseException:
            self.close()
            raise IdentityFailure() from None

    def check(self):
        if (select.select([self.pidfd], [], [], 0)[0]
                or smoke.linux_process_identity(self.pid)[1:] != self.start[1:]
                or not smoke.owned_linux_pid(self.pid, self.launcher.pid, self.nonce)
                or Path(os.readlink(f"/proc/{self.pid}/exe")) != self.executable):
            raise IdentityFailure()
        with open(f"/proc/{self.pid}/maps", "rb") as source:
            contents = source.read(1024 * 1024 + 1)
        result = validate_map_inventory(contents, self.app_identity, self.library_identity)
        if select.select([self.pidfd], [], [], 0)[0]:
            raise IdentityFailure()
        return result

    def attest_aliases(self, trace):
        """Authenticate loader spellings while this exact child is alive.

        Normal linuxdeploy RUNPATH may print usr/bin/../lib. Accept only exact
        observed spellings resolving to the already held shipped library inode
        and whole-file hash; the final trace cannot invent additional aliases.
        """
        self.check()
        for spelling in provider_spellings(trace, pid=self.pid, executable=str(self.executable)):
            path = Path(spelling)
            if (not path.is_absolute() or len(spelling) > 4096
                    or any(c in spelling for c in "\r\n\x00") or path.resolve(strict=True) != self.library):
                raise IdentityFailure()
            descriptor, info, contents = read_opened(path, 64 * 1024 * 1024)
            try:
                actual = FileIdentity.opened(self.library, info)
                if actual != self.library_identity or hashlib.sha256(contents).hexdigest() != self.library_sha256:
                    raise IdentityFailure()
            finally:
                os.close(descriptor)
            self.verified_library_aliases.add(spelling)
        self.check()

    def binding_facts(self, readelf_output, trace, *, complete):
        imports = imported_pipewire_symbols(readelf_output, collision_symbols())
        evidence = verify_binding_trace(trace, pid=self.pid, executable=str(self.executable),
                                        library=str(self.library), imports=imports["imports"], complete=complete,
                                        verified_library_aliases=self.verified_library_aliases)
        return {**evidence, "executable_sha256": self.executable_sha256,
                "pipewire_sha256": self.library_sha256,
                "actual_imports": sorted(imports["imports"]),
                "collision_symbols_not_imported": sorted(imports["not_imported_collisions"])}

    def close(self):
        for descriptor in reversed(self.descriptors):
            os.close(descriptor)
        self.descriptors.clear()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
