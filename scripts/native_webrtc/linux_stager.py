#!/usr/bin/env python3
"""Stage reviewed WebRTC tool DATA; never execute a tool or provision a full SDK.

The manifest and per-job admission hashes must come from a separately reviewed
trusted workflow. Neither a candidate tree nor a downloaded archive grants
execution authority. Output is a fresh private directory, never a source tree.
"""
import argparse
import hashlib
import http.client
import json
import lzma
import os
import platform
import re
import ssl
import stat
import struct
import sys
import tarfile
import time
import urllib.parse
import zipfile
from pathlib import Path, PurePosixPath

BASELINE = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
TREE = "fecdf55fdd1e9cafc5531abab7ff14e35a1159f5"
GN = "git_revision:4619125bd337d259c0dc9f958d0102adc99d2543"
NINJA = "version:3@1.12.1.chromium.4"
OUTER_DEPOT = "b52fe87d04af61df3dd07748ff2c2c0b649dca09"
NESTED_DEPOT = "8efa575d754b8703d99b0f827528e45aeaa167aa"
CLANG = "clang-llvmorg-22-init-14273-gea10026b-2.tar.xz"
TARGETS = {
    "Linuxx64": ("linux-amd64", "Linux", ("x86_64",), "Linux_x64", "0e18eb7f358240275c4ef5f92072c9acec37a168287ae69942f6a86462792b89", 56526480, 1763731829626474),
    "MacARM": ("mac-arm64", "Darwin", ("arm64",), "Mac_arm64", "bdd960e7d244b7f05f29481650e7b369739999e245079b58003f98de7d9f5058", 45160412, 1763731858116892),
    "MacIntel": ("mac-amd64", "Darwin", ("x86_64",), "Mac", "0161559fa66e144aff91bd06d573a7c0289aaba15689d3c16130a5bf105286d0", 54125348, 1763731832575158),
    "Windowsx64": ("windows-amd64", "Windows", ("AMD64", "x86_64"), "Win", "fd43dc87de6c86bf9ca414425ccdf4bb244ebf3b77c6e21495ab47bc45fae39c", 48332544, 1763731883374407),
}
CIPD_PINS = {'gn:linux-amd64': ('ebb3df2b1d9dd579013573a91c20ab2a80e1d715a90fa461ba7cb455f57dea5a', 3334361), 'gn:mac-arm64': ('e47b0f81b6438dcbc44ad0a1f02af632d697096bf37fe252513db3908cf324f6', 906173), 'gn:mac-amd64': ('7c68ca8181c3881cf2d82b51ef10f1c0627ad5af84b2fbf73d1234ef1ff43805', 994192), 'gn:windows-amd64': ('653e7b2746471e749be0d7119d7d72d2542ffadc4a7527f6cb0ff1f2a6c6ced8', 1147351), 'ninja:linux-amd64': ('3f1f1cc0f69a1bcfdf67fb6c2bc7419b1dd812bb8d0e79afa9bfa8a3553b5082', 182403), 'ninja:mac-arm64': ('c5e9b4ffab3b2edefbc41849fc81f116c8d047b258906becc061be9ecaf04afd', 145520), 'ninja:mac-amd64': ('9866e69dc0d1efccac019697213b5ab2e0332dcc629682f13733e0792ea95119', 160010), 'ninja:windows-amd64': ('0cf1cb1b9d2b8d2d14a9fd38b984f17dd853115db682880b21ceee45e91deb50', 318551)}
MAC_OMISSION_EVIDENCE = {'record_sha256': 'dde7ad19cb2961a752e17f3bc9b409fef027623a4d34b6c2a2121961e6cff940', 'build_revision': 'be1a8f6dcd7df7e46320192c5e2f364e50d79bbf', 'config_clang_BUILD_gn_sha256': 'eed0f4aabaa0ee03b0b52e5933c6c2c5bdf93fb2a5cca85263f5816bfcf2c565', 'sanitizers_gni_sha256': '07a7bbc2b7e304e909be3570b76c39c75c62f075aaf022288b57750b6102ca2c', 'selected_target_os': 'mac'}
MISSING = ("source_and_DEPS", "SDK_or_MSVC", "Rust", "vpython_and_wheels", "sysroot", "security_patches_and_tests", "licenses_and_corresponding_source")
MAX_JSON = 2 * 1024 * 1024
MAX_ARCHIVE = 128 * 1024 * 1024
MAX_EXPANDED = 2 * 1024 * 1024 * 1024
MAX_FILE = 512 * 1024 * 1024
MAX_MEMBERS = 10000
HEX = re.compile(r"[0-9a-f]{64}\Z")
PHASE = "admission"


class Refusal(Exception):
    pass


def require(value):
    if not value:
        raise Refusal()


def unique(pairs):
    out = {}
    for k, v in pairs:
        require(k not in out)
        out[k] = v
    return out


def decode(raw):
    return json.loads(raw.decode("utf-8"), object_pairs_hook=unique)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def bounded_int(v, low, high):
    require(type(v) is int and low <= v <= high)
    return v


def rel(name):
    require(isinstance(name, str) and 0 < len(name) <= 512)
    require(not name.startswith("/") and "\\" not in name and ":" not in name)
    parts = name.split("/")
    for part in parts:
        require(re.fullmatch(r"[A-Za-z0-9_+.,=@()\-]+", part) is not None)
        require(part not in (".", "..") and not part.endswith((".", " ")))
        stem = part.split(".")[0].upper()
        require(stem not in {"CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"})
        require(re.fullmatch(r"(?:COM|LPT)[1-9]", stem) is None)
    return name


def snapshot(path, limit):
    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_nlink == 1)
    require(not getattr(before, "st_file_attributes", 0) & 0x400)
    fd = os.open(path, flags)
    try:
        st = os.fstat(fd)
        require((st.st_dev, st.st_ino) == (before.st_dev, before.st_ino))
        require(stat.S_ISREG(st.st_mode) and st.st_nlink == 1 and st.st_size <= limit)
        data = bytearray()
        while len(data) <= limit:
            block = os.read(fd, min(1024 * 1024, limit + 1 - len(data)))
            if not block:
                break
            data.extend(block)
        after = os.fstat(fd)
        final = path.lstat()
        require(len(data) == st.st_size <= limit)
        require((st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns) == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns))
        require((final.st_dev, final.st_ino) == (st.st_dev, st.st_ino))
        return bytes(data)
    finally:
        os.close(fd)


class WindowsDirectories:
    """Hold non-reparse directory handles without delete sharing.

    New staging directories have a protected SYSTEM/owner-only DACL. This
    prevents inherited broad write access. Ancestors stay open until staging
    ends; no archive controls a path or a Windows reparse operation.
    """
    def __init__(self):
        import ctypes
        from ctypes import wintypes
        self.c = ctypes
        self.w = wintypes
        self.k = ctypes.WinDLL("kernel32", use_last_error=True)
        self.a = ctypes.WinDLL("advapi32", use_last_error=True)
        self.k.CreateFileW.argtypes = (wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE)
        self.k.CreateFileW.restype = wintypes.HANDLE
        self.k.CloseHandle.argtypes = (wintypes.HANDLE,)
        self.k.CloseHandle.restype = wintypes.BOOL
        self.k.CreateDirectoryW.argtypes = (wintypes.LPCWSTR, wintypes.LPVOID)
        self.k.CreateDirectoryW.restype = wintypes.BOOL
        self.a.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = (wintypes.LPCWSTR, wintypes.DWORD, ctypes.POINTER(wintypes.LPVOID), ctypes.POINTER(wintypes.DWORD))
        self.a.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = wintypes.BOOL
        self.k.LocalFree.argtypes = (wintypes.HLOCAL,)
        self.k.LocalFree.restype = wintypes.HLOCAL
        self.handles = []

    def hold(self, path):
        # OPEN_REPARSE_POINT, BACKUP_SEMANTICS; exclude FILE_SHARE_DELETE.
        handle = self.k.CreateFileW(str(path), 0x20080, 3, None, 3, 0x02200000, None)
        require(handle not in (None, self.c.c_void_p(-1).value))
        self.handles.append(handle)
        st = path.lstat()
        require(stat.S_ISDIR(st.st_mode) and not getattr(st, "st_file_attributes", 0) & 0x400)

    def mkdir(self, path):
        class SA(self.c.Structure):
            _fields_ = [("nLength", self.w.DWORD), ("lpSecurityDescriptor", self.w.LPVOID), ("bInheritHandle", self.w.BOOL)]
        descriptor = self.w.LPVOID()
        require(self.a.ConvertStringSecurityDescriptorToSecurityDescriptorW("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)", 1, self.c.byref(descriptor), None))
        try:
            attributes = SA(self.c.sizeof(SA), descriptor, False)
            require(self.k.CreateDirectoryW(str(path), self.c.byref(attributes)))
        finally:
            self.k.LocalFree(descriptor)

    def close(self):
        for h in reversed(self.handles):
            self.k.CloseHandle(h)
        self.handles.clear()


class Stage:
    def __init__(self, root):
        self.root = root
        self.files = []
        self.dirs = []
        self.windows = WindowsDirectories() if os.name == "nt" else None
        self.held = []
        self.identities = {}
        try:
            self.initialize(root)
        except BaseException:
            self.finish(False)
            raise

    def initialize(self, root):
        for parent in list(reversed(root.parent.parents)) + [root.parent]:
            st = parent.lstat()
            require(stat.S_ISDIR(st.st_mode) and not stat.S_ISLNK(st.st_mode))
            require(not getattr(st, "st_file_attributes", 0) & 0x400)
            if self.windows:
                self.windows.hold(parent)
            else:
                require(st.st_uid in (0, os.getuid()) and st.st_mode & 0o022 == 0)
                fd = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
                self.held.append(fd)
        self.mkdir(root)

    def check(self, path):
        st = path.lstat()
        require(stat.S_ISDIR(st.st_mode) and not getattr(st, "st_file_attributes", 0) & 0x400)
        require((st.st_dev, st.st_ino) == self.identities[path])

    def mkdir(self, path):
        if self.windows:
            self.windows.mkdir(path)
        else:
            path.mkdir(mode=0o700)
        self.dirs.append(path)
        st = path.lstat()
        self.identities[path] = (st.st_dev, st.st_ino)
        if self.windows:
            self.windows.hold(path)
        else:
            self.held.append(os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW))

    def parents(self, path):
        relative = path.relative_to(self.root)
        here = self.root
        self.check(here)
        for part in relative.parts[:-1]:
            here = here / part
            if here not in self.identities:
                self.mkdir(here)
            self.check(here)

    def create(self, path):
        self.parents(path)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOINHERIT", 0), 0o600)
        self.files.append(path)
        try:
            st = os.fstat(fd)
            require(stat.S_ISREG(st.st_mode) and st.st_nlink == 1)
            return os.fdopen(fd, "wb")
        except BaseException:
            os.close(fd)
            raise

    def bytes(self, path, raw):
        with self.create(path) as out:
            out.write(raw)
            out.flush()
            os.fsync(out.fileno())

    def finish(self, success):
        if self.windows:
            self.windows.close()
        for fd in reversed(self.held):
            os.close(fd)
        if not success:
            # Only entries created by this invocation, never recursive deletion
            # of an existing/source tree. Unexpected entries are left private.
            for path in reversed(self.files):
                try:
                    st = path.lstat()
                    if stat.S_ISREG(st.st_mode) and not getattr(st, "st_file_attributes", 0) & 0x400:
                        os.chmod(path, 0o600)
                        path.unlink()
                except OSError:
                    pass
            for path in reversed(self.dirs):
                try:
                    path.rmdir()
                except OSError:
                    pass


def https(url, method="GET", body=None, timeout=30, json_response=False):
    u = urllib.parse.urlsplit(url)
    require(u.scheme == "https" and u.port in (None, 443) and not u.username and not u.password and not u.fragment)
    require(u.hostname in ("chrome-infra-packages.appspot.com", "storage.googleapis.com"))
    require(len(url) <= 16384)
    c = http.client.HTTPSConnection(u.hostname, 443, timeout=timeout, context=ssl.create_default_context())
    # No proxy, cookie jar, netrc, token, ambient client certificate or redirect
    # handler. The CIPD signed read URL is never logged or persisted.
    headers = {"Accept-Encoding": "identity", "Content-Type": "application/json"}
    if json_response:
        headers["Accept"] = "application/json"
    c.request(method, urllib.parse.urlunsplit(("", "", u.path, u.query, "")), body=body, headers=headers)
    r = c.getresponse()
    require(r.status == 200 and r.getheader("Location") is None)
    require(r.getheader("Content-Encoding") in (None, "identity"))
    if json_response:
        require(r.getheader("Content-Type") == "application/json")
    return c, r


def download(stage, tool, path):
    source = tool["source"]
    if source["kind"] == "cipd":
        payload = json.dumps({"package": source["package"], "instance": {"hashAlgo": "SHA256", "hexDigest": tool["sha256"]}}, separators=(",", ":")).encode()
        c, r = https("https://chrome-infra-packages.appspot.com/prpc/cipd.Repository/GetInstanceURL", method="POST", body=payload, json_response=True)
        try:
            raw = r.read(65537)
            require(len(raw) <= 65536 and raw.startswith(b")]}'"))
            obj = decode(raw[4:])
            require(set(obj) == {"signedUrl"})
            url = obj["signedUrl"]
            parsed = urllib.parse.urlsplit(url)
            require(parsed.hostname == "storage.googleapis.com" and parsed.path == "/chrome-infra-packages/store/SHA256/" + tool["sha256"])
            require(bool(parsed.query))
        finally:
            c.close()
    else:
        bucket = source["bucket"] if tool["name"] in LINUX_MATERIAL_PINS else "chromium-browser-clang"
        url = "https://storage.googleapis.com/" + bucket + "/" + source["object"] + "?generation=" + str(source["generation"])
    c, r = https(url)
    try:
        require(r.getheader("Content-Length") == str(tool["bytes"]))
        require(r.getheader("Transfer-Encoding") is None)
        if source["kind"] == "gcs":
            require(r.getheader("x-goog-generation") == str(source["generation"]))
        h = hashlib.sha256()
        size = 0
        deadline = time.monotonic() + 300
        with stage.create(path) as out:
            while size < tool["bytes"]:
                require(time.monotonic() < deadline)
                block = r.read(min(1024 * 1024, tool["bytes"] - size))
                require(bool(block))
                size += len(block)
                out.write(block)
                h.update(block)
            require(r.read(1) == b"" and h.hexdigest() == tool["sha256"])
            out.flush()
            os.fsync(out.fileno())
    finally:
        c.close()


def check_members(tool):
    members = tool["members"]
    require(isinstance(members, list) and 0 < len(members) <= MAX_MEMBERS)
    table = {}
    names = set()
    expanded = 0
    for row in members:
        name = rel(row["path"])
        require(name.casefold() not in names)
        names.add(name.casefold())
        require(row["kind"] in ("regular", "directory", "symlink"))
        require(type(row["mode"]) is int and row["mode"] & ~0o777 == 0)
        bounded_int(row["bytes"], 0, MAX_FILE)
        if row["kind"] == "regular":
            require(HEX.fullmatch(row["sha256"]) is not None)
            require(row["output_mode"] == (0o700 if row["mode"] & 0o111 else 0o600))
            expanded += row["bytes"]
        else:
            require(row["bytes"] == 0)
        table[name] = row
    require(expanded <= MAX_EXPANDED)
    for name in table:
        for parent in PurePosixPath(name).parents:
            if str(parent) != "." and str(parent) in table:
                require(table[str(parent)]["kind"] == "directory")
    expected_copies = []
    expected_omissions = []
    output_names = {name.casefold() for name, m in table.items() if m["kind"] != "symlink"}
    for name, row in table.items():
        if row["kind"] != "symlink":
            continue
        # Only exact independently reviewed official Clang aliases may be
        # represented. No filesystem link is created or followed.
        require(tool["name"] == "clang")
        raw = rel(row["linkname"])
        target = str(PurePosixPath(name).parent / raw)
        candidates = [target] if target in table and table[target]["kind"] == "regular" else [x for x in table if x.startswith(target + "/") and table[x]["kind"] == "regular"]
        if not candidates:
            # An explicit externally admitted omission is available only for
            # the three already-dangling Linux runtime aliases in Mac clang.
            missing = {
                "lib/clang/22/lib/aarch64-cros-linux-gnu": "aarch64-unknown-linux-gnu",
                "lib/clang/22/lib/armv7a-cros-linux-gnueabihf": "armv7-unknown-linux-gnueabihf",
                "lib/clang/22/lib/x86_64-cros-linux-gnu": "x86_64-unknown-linux-gnu",
            }
            require(tool["source"]["object"].startswith(("Mac/", "Mac_arm64/")))
            require(missing.get(name) == raw)
            require(not any(x == target or x.startswith(target + "/") for x in table))
            expected_omissions.append({"link_path": name, "linkname": raw, "missing_target": target, "reason": "dangling_linux_runtime_alias_not_mac_runtime"})
            continue
        require(not any(x == target or x.startswith(target + "/") for x in table if table[x]["kind"] == "symlink"))
        for src in candidates:
            dst = name + src[len(target):]
            rel(dst)
            require(dst.casefold() not in output_names)
            output_names.add(dst.casefold())
            m = table[src]
            expected_copies.append({"link_path": name, "linkname": raw, "source": src, "destination": dst, "bytes": m["bytes"], "sha256": m["sha256"], "mode": m["mode"], "output_mode": m["output_mode"]})
    require(tool["omission_evidence"] == (MAC_OMISSION_EVIDENCE if expected_omissions else None))
    require(sorted(tool["omitted_aliases"], key=lambda x: x["link_path"]) == sorted(expected_omissions, key=lambda x: x["link_path"]))
    require(expanded + sum(x["bytes"] for x in expected_copies) <= MAX_EXPANDED)
    require(sorted(tool["alias_copies"], key=lambda x: x["destination"]) == sorted(expected_copies, key=lambda x: x["destination"]))
    return table


def manifest_validate(manifest, target):
    require(manifest["schema"] == 1)
    require(manifest["baseline"] == BASELINE and manifest["tree"] == TREE)
    require(manifest["outer_depot"] == OUTER_DEPOT and manifest["nested_depot"] == NESTED_DEPOT)
    require(set(manifest["targets"]) == set(TARGETS))
    tools = manifest["targets"][target]
    require([x["name"] for x in tools] == ["gn", "ninja", "clang"])
    selected = TARGETS[target]
    for tool in tools:
        require(HEX.fullmatch(tool["sha256"]) is not None)
        bounded_int(tool["bytes"], 1, MAX_ARCHIVE)
        src = tool["source"]
        if tool["name"] in ("gn", "ninja"):
            prefix = "gn/gn/" if tool["name"] == "gn" else "infra/3pp/tools/ninja/"
            require(src == {"kind": "cipd", "package": prefix + selected[0], "version": GN if tool["name"] == "gn" else NINJA})
            require((tool["sha256"], tool["bytes"]) == CIPD_PINS[tool["name"] + ":" + selected[0]])
            require(tool["format"] == "zip" and tool["alias_copies"] == [])
        else:
            require(src == {"kind": "gcs", "object": selected[3] + "/" + CLANG, "generation": selected[6]})
            require((tool["sha256"], tool["bytes"], tool["format"]) == (selected[4], selected[5], "tar.xz"))
        check_members(tool)
    return tools


def write_member(stage, destination, stream, row):
    h = hashlib.sha256()
    total = 0
    with stage.create(destination) as out:
        while total < row["bytes"]:
            block = stream.read(min(1024 * 1024, row["bytes"] - total))
            require(bool(block))
            total += len(block)
            h.update(block)
            out.write(block)
        require(stream.read(1) == b"" and h.hexdigest() == row["sha256"])
        out.flush()
        os.fsync(out.fileno())
    os.chmod(destination, row["output_mode"])
    require(digest(snapshot(destination, MAX_FILE)) == row["sha256"])


def extract(stage, tool, archive, output):
    table = check_members(tool)
    seen = set()

    def admit(name, kind, size, mode, linkname=None):
        name = rel(name)
        require(name not in seen and name in table)
        seen.add(name)
        row = table[name]
        require((kind, size, mode) == (row["kind"], row["bytes"], row["mode"]))
        if kind == "symlink":
            require(linkname == row["linkname"])
        return row

    if tool["format"] == "zip":
        # Bound the central directory before ZipFile allocates its entry list.
        with archive.open("rb") as f:
            f.seek(max(0, tool["bytes"] - 65557))
            tail = f.read(65557)
        pos = tail.rfind(b"PK\x05\x06")
        require(pos >= 0 and len(tail) - pos >= 22)
        e = struct.unpack("<4s4H2LH", tail[pos:pos + 22])
        require(e[1] == e[2] == 0 and e[3] == e[4] <= MAX_MEMBERS)
        require(e[5] <= 4 * 1024 * 1024 and e[6] + e[5] <= tool["bytes"])
        require(e[7] == len(tail) - pos - 22)
        with zipfile.ZipFile(archive, "r", allowZip64=False) as z:
            for m in z.infolist():
                require(not m.flag_bits & 1 and m.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED))
                mode = (m.external_attr >> 16) & 0o177777
                require(stat.S_IFMT(mode) in (0, stat.S_IFREG, stat.S_IFDIR))
                row = admit(m.filename.rstrip("/") if m.is_dir() else m.filename, "directory" if m.is_dir() else "regular", m.file_size, mode & 0o777)
                if row["kind"] == "regular":
                    with z.open(m) as stream:
                        write_member(stage, output / row["path"], stream, row)
    else:
        # Bounded decompressor memory/output; never tarfile.extract/all.
        tar_path = archive.with_suffix(".tar")
        d = lzma.LZMADecompressor(format=lzma.FORMAT_XZ, memlimit=256 * 1024 * 1024)
        total = 0
        with archive.open("rb") as src, stage.create(tar_path) as out:
            while not d.eof:
                block = src.read(1024 * 1024) if d.needs_input else b""
                require(bool(block) or not d.needs_input)
                raw = d.decompress(block, max_length=1024 * 1024)
                total += len(raw)
                require(total <= MAX_EXPANDED + 32 * 1024 * 1024)
                out.write(raw)
            require(d.unused_data == b"" and src.read(1) == b"")
        with tarfile.open(tar_path, "r:") as a:
            for m in a:
                require(len(seen) < MAX_MEMBERS)
                require(m.isfile() or m.isdir() or m.issym())
                row = admit(m.name.rstrip("/") if m.isdir() else m.name, "regular" if m.isfile() else "directory" if m.isdir() else "symlink", m.size, m.mode, m.linkname if m.issym() else None)
                if row["kind"] == "regular":
                    with a.extractfile(m) as stream:
                        write_member(stage, output / row["path"], stream, row)
        tar_path.unlink()
        stage.files.remove(tar_path)
    require(seen == set(table))
    for copy in tool["alias_copies"]:
        source = output / copy["source"]
        # Read a verified regular member, never follow the archive's linkname.
        data = snapshot(source, MAX_FILE)
        require(len(data) == copy["bytes"] and digest(data) == copy["sha256"])
        stage.bytes(output / copy["destination"], data)
        os.chmod(output / copy["destination"], copy["output_mode"])
        require(digest(snapshot(output / copy["destination"], MAX_FILE)) == copy["sha256"])



# These are original immutable GCS objects, not a compiler execution grant.
LINUX_MATERIAL_PINS = {'rust': {'bucket': 'chromium-browser-clang',
          'bytes': 140380848,
          'generation': 1762971367461755,
          'members_sha256': '4c8a02b6f39fa88117ec63819025e5b1de711ff981dcf37def40b978c846183e',
          'object': 'Linux_x64/rust-toolchain-11339a0ef5ed586bb7ea4f85a9b7287880caac3a-1-llvmorg-22-init-14273-gea10026b.tar.xz',
          'sha256': '15cf3019dd864ea64d63e5cf33de1ded76ace4f70cbb224812fccbf03b342096'},
 'sysroot-amd64': {'bucket': 'chrome-linux-sysroot',
                   'bytes': 20781612,
                   'generation': 1741221486381719,
                   'members_sha256': 'f4d94db2089114ed7679cbdab8deda172438903d53b1968a6547c6b25dcb5f06',
                   'object': '36a164623d03f525e3dfb783a5e9b8a00e98e1ddd2b5cff4e449bd016dd27e50',
                   'sha256': '36a164623d03f525e3dfb783a5e9b8a00e98e1ddd2b5cff4e449bd016dd27e50'},
 'sysroot-i386': {'bucket': 'chrome-linux-sysroot',
                  'bytes': 20786772,
                  'generation': 1741221485445080,
                  'members_sha256': '35b6a5381f60d6fb14ddcab35d22e8188b6323bcf09b83734ee7a05091f72485',
                  'object': '63f0e5128b84f7b0421956a4a40affa472be8da0e58caf27e9acbc84072daee7',
                  'sha256': '63f0e5128b84f7b0421956a4a40affa472be8da0e58caf27e9acbc84072daee7'}}
LINUX_MATERIAL_PINS['llvmobjdump'] = {
    'bucket': 'chromium-browser-clang',
    'object': 'Linux_x64/llvmobjdump-llvmorg-22-init-14273-gea10026b-2.tar.xz',
    'generation': 1763731830357400, 'bytes': 5727588,
    'sha256': 'fc5e696e4aa5b36103089fbe4113f53d19a8f9880bb906c4554d075fc6632b5a',
    'members_sha256': '20ab4b9ee1e93d93ad94d20208db5abdb29334d7dd200098f012328aca52a7e3',
}
MAX_LINUX_JSON = 16 * 1024 * 1024
MAX_LINUX_MEMBERS = 25000


def linux_rel(name, *, empty=False):
    # Linux-only material paths. The complete, code-pinned inventory admits the
    # real case-distinct headers and one literal backslash systemd basename.
    # It never changes rel() or Windows/Mac/core-tool policy.
    require(type(name) is str and (bool(name) or empty) and len(name) <= 1024)
    require(not name.startswith("/") and ":" not in name and "\x00" not in name)
    if not name:
        return name
    require(all(part not in ("", ".", "..") and part.casefold() != ".git"
                and all(32 <= ord(char) != 127 for char in part)
                for part in name.split("/")))
    return name


def linux_resolve(table, name):
    # Resolve only logical archive records, never a host filesystem path.
    seen = set()
    for _ in range(65):
        parts = name.split("/")
        changed = False
        for count in range(1, len(parts) + 1):
            prefix = "/".join(parts[:count])
            require(prefix in table)
            row = table[prefix]
            if row["kind"] in ("symlink", "hardlink"):
                require((prefix, name) not in seen)
                seen.add((prefix, name))
                raw = row["linkname"]
                require(type(raw) is str and 0 < len(raw) <= 1024
                        and "\x00" not in raw and ":" not in raw)
                base = [] if raw.startswith("/") or row["kind"] == "hardlink" else parts[:count - 1]
                for part in raw.split("/"):
                    if part in ("", "."):
                        continue
                    if part == "..":
                        require(bool(base))
                        base.pop()
                    else:
                        base.append(part)
                require(bool(base))
                name = linux_rel("/".join(base + parts[count:]))
                changed = True
                break
            if count < len(parts):
                require(row["kind"] == "directory")
        if not changed:
            require(table[name]["kind"] in ("regular", "directory"))
            return name, table[name]
    raise Refusal()


def check_linux_members(tool):
    require(tool["name"] in LINUX_MATERIAL_PINS)
    pin = LINUX_MATERIAL_PINS[tool["name"]]
    members = tool["members"]
    require(type(members) is list and 0 < len(members) <= MAX_LINUX_MEMBERS)
    raw_inventory = (json.dumps(members, sort_keys=True, separators=(",", ":")) + "\n").encode()
    require(digest(raw_inventory) == pin["members_sha256"]
            and tool["member_inventory_sha256"] == pin["members_sha256"])
    table = {}
    expanded = 0
    for row in members:
        name = linux_rel(row["path"], empty=True)
        require(name not in table and row["kind"] in ("regular", "directory", "symlink", "hardlink"))
        raw = row["raw_path"]
        require(raw == name or raw == "./" + name or (not name and raw in (".", "./")))
        require(type(row["mode"]) is int and row["mode"] & ~0o777 == 0)
        bounded_int(row["bytes"], 0, MAX_FILE)
        if row["kind"] == "regular":
            require(HEX.fullmatch(row["sha256"])
                    and row["output_mode"] == (0o700 if row["mode"] & 0o111 else 0o600))
            expanded += row["bytes"]
        else:
            require(row["bytes"] == 0)
        if not name:
            require(row["kind"] == "directory")
        table[name] = row
    for name in table:
        parts = name.split("/")
        for count in range(1, len(parts)):
            require("/".join(parts[:count]) in table
                    and table["/".join(parts[:count])]["kind"] == "directory")
    copies, directories = [], set()
    ordinary = {name for name, row in table.items() if row["kind"] not in ("symlink", "hardlink")}
    for name, row in table.items():
        if row["kind"] not in ("symlink", "hardlink"):
            continue
        resolved, target = linux_resolve(table, name)
        if target["kind"] == "regular":
            candidates = [(name, resolved)]
        else:
            directories.add(name)
            candidates = []
            for child, member in table.items():
                if child.startswith(resolved + "/"):
                    destination = linux_rel(name + child[len(resolved):])
                    if member["kind"] == "directory":
                        directories.add(destination)
                    else:
                        source, final = linux_resolve(table, child)
                        require(final["kind"] == "regular")
                        candidates.append((destination, source))
        for destination, source in candidates:
            member = table[source]
            copies.append({"link_path": name, "kind": row["kind"], "linkname": row["linkname"],
                           "source": source, "destination": destination, "bytes": member["bytes"],
                           "sha256": member["sha256"], "output_mode": member["output_mode"]})
    copies.sort(key=lambda row: row["destination"])
    destinations = {row["destination"] for row in copies}
    require(len(destinations) == len(copies) and not ordinary.intersection(destinations)
            and not destinations.intersection(directories))
    outputs = ordinary | destinations | directories
    for name in outputs:
        for parent in PurePosixPath(name).parents:
            if str(parent) != ".":
                require(str(parent) not in destinations
                        and (str(parent) not in table or table[str(parent)]["kind"] != "regular"))
    folded = {}
    for name in outputs:
        folded.setdefault(name.casefold(), []).append(name)
    groups = sorted(sorted(names) for names in folded.values() if len(names) > 1)
    require(tool["case_distinct_groups"] == groups
            and tool["alias_copies"] == copies
            and tool["alias_directories"] == sorted(directories)
            and tool["omitted_aliases"] == [] and tool["omission_evidence"] is None)
    require(expanded + sum(row["bytes"] for row in copies) <= MAX_EXPANDED)
    return table


def linux_inputs_validate(manifest, target):
    require(target == "Linuxx64" and os.name == "posix"
            and platform.system() == "Linux" and platform.machine() == "x86_64")
    require(manifest["schema"] == "morpheus-native-linux-build-inputs-v1"
            and manifest["baseline"] == BASELINE and manifest["tree"] == TREE
            and manifest["target"] == target)
    require(all(manifest[name] is False for name in ("environment_complete", "build_ready", "distribution_ready", "hooks_executed")))
    materials = manifest["materials"]
    require(type(materials) is list and [row["name"] for row in materials] in
            (["rust"], ["sysroot-amd64", "sysroot-i386"],
             ["rust", "sysroot-amd64", "sysroot-i386"],
             ["rust", "sysroot-amd64", "sysroot-i386", "llvmobjdump"]))
    for tool in materials:
        pin = LINUX_MATERIAL_PINS[tool["name"]]
        require(tool["sha256"] == pin["sha256"] and tool["bytes"] == pin["bytes"]
                and tool["format"] == "tar.xz"
                and tool["source"] == {"kind": "gcs", "bucket": pin["bucket"],
                                       "object": pin["object"], "generation": pin["generation"]})
        require(tool["intended_source_mount"] == {
            "rust": "src/third_party/rust-toolchain",
            "sysroot-amd64": "src/build/linux/debian_bullseye_amd64-sysroot",
            "sysroot-i386": "src/build/linux/debian_bullseye_i386-sysroot",
            "llvmobjdump": "src/third_party/llvm-build/Release+Asserts",
        }[tool["name"]])
        check_linux_members(tool)
    return materials


def linux_xz_decoder(tool):
    # The two original pinned sysroots use a 256 MiB XZ dictionary. That is
    # smaller than liblzma's total decoder memory: allow fixed overhead only
    # for these exact archive identities, keeping all other tools at 256 MiB.
    name = tool["name"]
    require(name in LINUX_MATERIAL_PINS)
    pin = LINUX_MATERIAL_PINS[name]
    require(tool["format"] == "tar.xz" and tool["sha256"] == pin["sha256"]
            and tool["bytes"] == pin["bytes"])
    limit = (320 if name in ("sysroot-amd64", "sysroot-i386") else 256) * 1024 * 1024
    return lzma.LZMADecompressor(format=lzma.FORMAT_XZ, memlimit=limit)


def extract_linux(stage, tool, archive, output):
    table = check_linux_members(tool)
    # Same bounded decompression policy as core Clang, larger member bound only
    # for these exact code-pinned Linux inventories. Nothing is extractall'd.
    tar_path = archive.with_suffix(".tar")
    decompressor = linux_xz_decoder(tool)
    total = 0
    with archive.open("rb") as src, stage.create(tar_path) as out:
        while not decompressor.eof:
            block = src.read(1024 * 1024) if decompressor.needs_input else b""
            require(bool(block) or not decompressor.needs_input)
            raw = decompressor.decompress(block, max_length=1024 * 1024)
            total += len(raw)
            require(total <= MAX_EXPANDED + 32 * 1024 * 1024)
            out.write(raw)
        require(decompressor.unused_data == b"" and src.read(1) == b"")
    seen = set()
    stage.parents(output / "_directory_only")
    with tarfile.open(tar_path, "r:") as source:
        for member in source:
            require(len(seen) < MAX_LINUX_MEMBERS)
            kind = ("regular" if member.isfile() else "directory" if member.isdir() else
                    "symlink" if member.issym() else "hardlink" if member.islnk() else "unsupported")
            logical = member.name.removeprefix("./").rstrip("/")
            if member.name == ".":
                logical = ""
            linux_rel(logical, empty=True)
            require(logical not in seen and logical in table)
            seen.add(logical)
            row = table[logical]
            require((member.name, kind, member.size, member.mode) ==
                    (row["raw_path"], row["kind"], row["bytes"], row["mode"]))
            if kind in ("symlink", "hardlink"):
                require(member.linkname == row["linkname"])
            elif kind == "directory":
                if logical:
                    stage.parents(output / logical / "_directory_only")
            elif kind == "regular":
                with source.extractfile(member) as stream:
                    write_member(stage, output / logical, stream, row)
            else:
                raise Refusal()
    require(seen == set(table))
    for name in tool["alias_directories"]:
        stage.parents(output / name / "_directory_only")
    for row in tool["alias_copies"]:
        # Source is a rehashed, original regular member. Never resolve a link on
        # disk, preserve a hardlink, or read a host root's apparent target.
        data = snapshot(output / row["source"], MAX_FILE)
        require(len(data) == row["bytes"] and digest(data) == row["sha256"])
        stage.bytes(output / row["destination"], data)
        os.chmod(output / row["destination"], row["output_mode"])
        require(digest(snapshot(output / row["destination"], MAX_FILE)) == row["sha256"])
    tar_path.unlink()
    stage.files.remove(tar_path)


def main():
    global PHASE
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=tuple(TARGETS), required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--manifest-sha256", required=True)
    parser.add_argument("--admission", type=Path, required=True)
    parser.add_argument("--admission-sha256", required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--linux-inputs", type=Path)
    parser.add_argument("--linux-inputs-sha256")
    args = parser.parse_args()
    require(HEX.fullmatch(args.manifest_sha256) and HEX.fullmatch(args.admission_sha256))
    manifest_raw = snapshot(args.manifest, MAX_JSON)
    admission_raw = snapshot(args.admission, 65536)
    require(digest(manifest_raw) == args.manifest_sha256 and digest(admission_raw) == args.admission_sha256)
    manifest, admission = decode(manifest_raw), decode(admission_raw)
    tools = manifest_validate(manifest, args.target)
    linux_raw = None
    if args.linux_inputs is not None:
        require(type(args.linux_inputs_sha256) is str and HEX.fullmatch(args.linux_inputs_sha256))
        linux_raw = snapshot(args.linux_inputs, MAX_LINUX_JSON)
        require(digest(linux_raw) == args.linux_inputs_sha256)
        tools = linux_inputs_validate(decode(linux_raw), args.target)
        require(admission["schema"] == 2 and admission["purpose"] == "stage_verified_linux_build_input_data_only"
                and admission["linux_inputs_sha256"] == args.linux_inputs_sha256)
    else:
        require(args.linux_inputs_sha256 is None)
        require(admission["schema"] == 1 and admission["purpose"] == "stage_verified_tool_data_only")
    require(admission["manifest_sha256"] == args.manifest_sha256 and admission["target"] == args.target)
    require(admission["accept_exact_mac_dangling_alias_omissions"] is (args.target in ("MacARM", "MacIntel")))
    require(admission["public_repository"] is True and admission["standard_hosted_runner"] is True)
    require(os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted")
    require(admission["repository"] == os.environ.get("GITHUB_REPOSITORY"))
    for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB"):
        require(admission[key] == os.environ.get(key) and bool(admission[key]))
    root = args.root
    require(root.is_absolute() and str(root) == admission["root"])
    require(root.parent == Path(os.environ["RUNNER_TEMP"]))
    require(re.fullmatch(r"morpheus-webrtc-tools-[0-9a-f]{16}", root.name))
    require(not root.exists() and not root.is_symlink())
    if os.name == "nt":
        require(re.fullmatch(r"[A-Za-z]:", root.drive) is not None)
    chosen = TARGETS[args.target]
    require(platform.system() == chosen[1] and platform.machine() in chosen[2])
    stage = Stage(root)
    success = False
    try:
        for tool in tools:
            PHASE = "download_" + tool["name"]
            archive = root / "archives" / (tool["name"] + (".zip" if tool["format"] == "zip" else ".tar.xz"))
            download(stage, tool, archive)
            PHASE = "stage_" + tool["name"]
            if linux_raw is not None:
                extract_linux(stage, tool, archive, root / "linux-inputs" / tool["name"])
            else:
                extract(stage, tool, archive, root / "tools" / tool["name"])
        PHASE = "receipt"
        receipt = {"schema": 1, "status": "tool_data_staged", "environment_complete": False, "target": args.target, "baseline": BASELINE, "manifest_sha256": args.manifest_sha256, "admission_sha256": args.admission_sha256, "missing_required_closure": list(MISSING), "archives": [{"name": t["name"], "sha256": t["sha256"], "bytes": t["bytes"], "members": t["members"], "alias_copies": t["alias_copies"], "omitted_aliases": t["omitted_aliases"], "omission_evidence": t["omission_evidence"]} for t in tools], "normalization": "manifest_bound_regular_copies_and_explicit_dangling_mac_alias_omissions_no_filesystem_links", "executed_tools": [], "permission_model": "windows_inherited_protected_SYSTEM_owner_DACL" if os.name == "nt" else "unix_private_directories_0700_files_0600_or_0700"}
        if linux_raw is not None:
            receipt.update({"schema": 2, "status": "linux_build_input_data_staged",
                            "linux_inputs_sha256": args.linux_inputs_sha256,
                            "materials": tools,
                            "required_linux_materials_not_staged": [name for name in LINUX_MATERIAL_PINS
                                if name not in {tool["name"] for tool in tools}],
                            "native_sources_complete": False,
                            "compiler_cipd_runtime_closure": False, "build_ready": False,
                            "distribution_ready": False, "hooks_executed": False,
                            "normalization": "exact_Linux_archive_alias_regular_copies_no_filesystem_links",
                            "missing_required_closure": ["source_and_DEPS", "all_other_selected_non_git_materials",
                                "Python3.11_vpython_and_wheels", "generated_metadata",
                                "security_patches_and_tests", "licenses_and_corresponding_source",
                                "actual_tool_execution_and_build_admission"]})
            stage.bytes(root / "selected-linux-build-inputs.json", linux_raw)
        stage.bytes(root / "tool-materials.json", (json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n").encode())
        stage.bytes(root / "selected-tool-manifest.json", manifest_raw)
        success = True
    finally:
        stage.finish(success)
    print('MORPHEUS_WEBRTC_TOOLS_STAGED {"environment_complete":false}')


if __name__ == "__main__":
    try:
        main()
    except (Refusal, OSError, ValueError, KeyError, TypeError, EOFError, lzma.LZMAError, tarfile.TarError, zipfile.BadZipFile, http.client.HTTPException):
        # Closed categories only; never emit URL queries, paths or remote text.
        print("MORPHEUS_WEBRTC_TOOLS_FAILED " + PHASE, file=sys.stderr)
        raise SystemExit(1)
