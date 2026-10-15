#!/usr/bin/env python3
"""Preseed the exact Cargo Packager 0.11.8 NSIS tools on a fresh Windows runner.

Downloads are inert bytes until all pinned hashes and archive paths pass. Never
reuse, replace or delete an existing NSIS directory. No signing material is used.
"""

import ctypes
import hashlib
import io
import json
import os
import platform
import stat
import sys
import tempfile
import time
import urllib.parse
import urllib.request
import uuid
import zipfile
from pathlib import Path, PurePosixPath

PINS = Path(__file__).with_suffix(".json")
MAX_DOWNLOAD = 16 * 1024 * 1024
MAX_EXPANDED = 32 * 1024 * 1024
MAX_ENTRIES = 512
HOSTS = {"github.com", "release-assets.githubusercontent.com", "objects.githubusercontent.com"}
RESERVED = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)), *(f"LPT{i}" for i in range(1, 10))}


class PreparationError(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise PreparationError(message)


def allowed_url(url):
    try:
        value = urllib.parse.urlsplit(url)
        return (value.scheme == "https" and value.hostname in HOSTS
                and value.port in (None, 443) and value.username is None
                and value.password is None and not value.fragment)
    except ValueError:
        return False


class SafeRedirect(urllib.request.HTTPRedirectHandler):
    max_redirections = 5
    max_repeats = 2

    def redirect_request(self, request, file, code, message, headers, newurl):
        require(allowed_url(newurl), "Tool download redirected outside allowed HTTPS hosts")
        return super().redirect_request(request, file, code, message, headers, newurl)


def validate_asset(data, pin):
    require(type(pin["bytes"]) is int and 0 < pin["bytes"] <= MAX_DOWNLOAD, "Invalid tool size pin")
    require(len(data) == pin["bytes"], "Tool download size differs from pin")
    require(hashlib.sha256(data).hexdigest() == pin["sha256"], "Tool download hash differs from pin")
    return data


def download(pin):
    require(allowed_url(pin["url"]), "Invalid tool source URL")
    require(type(pin["bytes"]) is int and 0 < pin["bytes"] <= MAX_DOWNLOAD, "Invalid tool size pin")
    # Public tool downloads need neither proxy credentials nor authentication.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), SafeRedirect())
    deadline = time.monotonic() + 60
    chunks = []
    size = 0
    with opener.open(pin["url"], timeout=20) as response:
        require(allowed_url(response.geturl()), "Invalid final tool source URL")
        length = response.headers.get("Content-Length")
        require(length is None or (length.isdecimal() and int(length) == pin["bytes"]),
                "Tool response length differs from pin")
        while True:
            require(time.monotonic() < deadline, "Tool download exceeded deadline")
            # read1 does at most one buffered socket read, so a slowly trickling
            # body cannot keep resetting the complete-download deadline.
            chunk = response.read1(min(65536, pin["bytes"] + 1 - size))
            if not chunk:
                break
            size += len(chunk)
            require(size <= pin["bytes"], "Tool response exceeded size pin")
            chunks.append(chunk)
    return validate_asset(b"".join(chunks), pin)


def member_path(info):
    name = info.filename
    require(name and "\\" not in name and not name.startswith("/"), "Unsafe ZIP member path")
    parts = name.rstrip("/").split("/")
    require(info.orig_filename == name and name == "/".join(parts) + ("/" if info.is_dir() else ""),
            "Noncanonical ZIP member path")
    require(all(part.isascii() and part not in ("", ".", "..") and not part.endswith((".", " "))
                and not any(ord(c) < 32 or c in '<>:"|?*' for c in part)
                and part.split(".", 1)[0].upper() not in RESERVED for part in parts),
            "Unsafe ZIP member component")
    mode = stat.S_IFMT(info.external_attr >> 16)
    expected = stat.S_IFDIR if info.is_dir() else stat.S_IFREG
    require(mode in (0, expected) and not (info.external_attr & 0x400),
            "ZIP member is a link or special file")
    require(not (info.flag_bits & 1), "Encrypted ZIP members are unsupported")
    require(info.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED), "Unsupported ZIP compression")
    return PurePosixPath(*parts)


def archive_files(data, prefix=None, selected=None):
    """Validate the whole pinned ZIP; return only requested ordinary files."""
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        entries = archive.infolist()
        require(0 < len(entries) <= MAX_ENTRIES, "Unexpected ZIP entry count")
        require(sum(entry.file_size for entry in entries) <= MAX_EXPANDED,
                "ZIP expanded size exceeds bound")
        seen = set()
        planned = []
        for entry in entries:
            path = member_path(entry)
            identity = str(path).casefold()
            require(identity not in seen, "Ambiguous duplicate ZIP path")
            seen.add(identity)
            require(0 <= entry.file_size <= MAX_DOWNLOAD, "ZIP member size exceeds bound")
            if prefix is not None:
                require(path.parts[0] == prefix, "Unexpected ZIP root")
                if len(path.parts) == 1:
                    require(entry.is_dir(), "ZIP root must be a directory")
                    continue
                path = PurePosixPath(*path.parts[1:])
            planned.append((path, entry))
        files = {str(path).casefold() for path, entry in planned if not entry.is_dir()}
        for path, _ in planned:
            require(all(str(parent).casefold() not in files for parent in path.parents),
                    "ZIP file conflicts with a parent directory")
        result = {}
        for path, entry in planned:
            if entry.is_dir() or (selected is not None and str(path) not in selected):
                continue
            with archive.open(entry) as source:
                content = source.read(entry.file_size + 1)
            require(len(content) == entry.file_size, "ZIP member size differs from metadata")
            result[str(path)] = content
        if selected is not None:
            require(set(result) == set(selected), "Required ZIP member missing")
        return result


def tool_files(pins, fetch=download):
    require(pins.get("schema") == 1 and pins.get("packager_version") == "0.11.8", "Unsupported tool manifest")
    require(set(pins["assets"]) == {"nsis", "application_id", "tauri_utils"}, "Unexpected tool inputs")
    # Complete all size/hash checks before opening either archive, even with a
    # supplied fetch function in tests or a future caller.
    blobs = {name: validate_asset(fetch(pin), pin) for name, pin in pins["assets"].items()}
    files = archive_files(blobs["nsis"], prefix="nsis-3.09")
    plugin = archive_files(blobs["application_id"], selected={"ReleaseUnicode/ApplicationID.dll"})
    additions = {
        "Plugins/x86-unicode/ApplicationID.dll": plugin["ReleaseUnicode/ApplicationID.dll"],
        "Plugins/x86-unicode/nsis_tauri_utils.dll": blobs["tauri_utils"],
    }
    require(not {name.casefold() for name in files} & {name.casefold() for name in additions},
            "NSIS archive unexpectedly includes separately pinned plugins")
    files.update(additions)
    for name, digest in pins["required_files"].items():
        require(name in files and hashlib.sha256(files[name]).hexdigest() == digest,
                "Required Packager tool is missing or changed")
    # This complete list is the branch condition used by the pinned packager.
    expected = {"makensis.exe", "Bin/makensis.exe", "Stubs/lzma-x86-unicode",
                "Stubs/lzma_solid-x86-unicode", "Plugins/x86-unicode/ApplicationID.dll",
                "Plugins/x86-unicode/nsis_tauri_utils.dll", "Include/MUI2.nsh",
                "Include/FileFunc.nsh", "Include/x64.nsh", "Include/nsDialogs.nsh",
                "Include/WinMessages.nsh"}
    require(set(pins["required_files"]) == expected, "Incomplete Packager cached-path contract")
    return files


def require_plain_directory(path):
    details = path.lstat()
    require(stat.S_ISDIR(details.st_mode) and not stat.S_ISLNK(details.st_mode)
            and not (getattr(details, "st_file_attributes", 0) & 0x400),
            "Tool cache parent is a link or not a directory")


def verify_tree(root, files):
    actual = set()
    for folder, directories, names in os.walk(root, followlinks=False):
        require_plain_directory(Path(folder))
        for name in directories:
            require_plain_directory(Path(folder) / name)
        for name in names:
            path = Path(folder) / name
            details = path.lstat()
            require(stat.S_ISREG(details.st_mode) and not (getattr(details, "st_file_attributes", 0) & 0x400),
                    "Tool cache contains a link or special file")
            relative = path.relative_to(root).as_posix()
            require(relative in files and details.st_size == len(files[relative]), "Unexpected tool cache file")
            require(hashlib.sha256(path.read_bytes()).digest() == hashlib.sha256(files[relative]).digest(),
                    "Tool cache content changed")
            actual.add(relative)
    require(actual == set(files), "Tool cache file missing")


def prepare(local_app_data, pins, fetch=download):
    local_app_data = Path(local_app_data)
    require(local_app_data.is_absolute(), "Local AppData path must be absolute")
    for parent in [*reversed(local_app_data.parents), local_app_data]:
        require_plain_directory(parent)
    cache = local_app_data / ".cargo-packager"
    if not os.path.lexists(cache):
        cache.mkdir()
    require_plain_directory(cache)
    destination = cache / "NSIS"
    require(not os.path.lexists(destination), "Refusing an existing NSIS tool directory")
    files = tool_files(pins, fetch)
    with tempfile.TemporaryDirectory(prefix=".morpheus-nsis-", dir=cache) as temporary:
        staging = Path(temporary) / "NSIS"
        staging.mkdir()
        for name, content in files.items():
            target = staging.joinpath(*PurePosixPath(name).parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("xb") as output:
                output.write(content)
        verify_tree(staging, files)
        require(not os.path.lexists(destination), "NSIS tool directory appeared during preparation")
        # Windows rename refuses an existing destination. Production main is
        # Windows-only; this publishes only our fully verified fresh tree.
        os.rename(staging, destination)
        verify_tree(destination, files)
    return len(files)


def local_app_data():
    """Use the same Windows known folder as dirs::cache_dir(), not an env override."""
    class Guid(ctypes.Structure):
        _fields_ = [("data1", ctypes.c_uint32), ("data2", ctypes.c_uint16),
                    ("data3", ctypes.c_uint16), ("data4", ctypes.c_ubyte * 8)]

    identifier = Guid.from_buffer_copy(uuid.UUID("f1b32785-6fba-4fcf-9d55-7b8e7f157091").bytes_le)
    shell = ctypes.WinDLL("shell32", use_last_error=True)
    ole = ctypes.WinDLL("ole32", use_last_error=True)
    shell.SHGetKnownFolderPath.argtypes = [ctypes.POINTER(Guid), ctypes.c_uint32, ctypes.c_void_p,
                                          ctypes.POINTER(ctypes.c_void_p)]
    shell.SHGetKnownFolderPath.restype = ctypes.c_long
    ole.CoTaskMemFree.argtypes = [ctypes.c_void_p]
    ole.CoTaskMemFree.restype = None
    pointer = ctypes.c_void_p()
    require(shell.SHGetKnownFolderPath(ctypes.byref(identifier), 0, None, ctypes.byref(pointer)) == 0,
            "Could not resolve Local AppData known folder")
    try:
        value = Path(ctypes.wstring_at(pointer))
        require(value.is_absolute() and not str(value).startswith("\\\\"), "Unexpected Local AppData location")
        return value
    finally:
        ole.CoTaskMemFree(pointer)


def main():
    try:
        require(sys.platform == "win32" and platform.machine() == "AMD64"
                and os.environ.get("GITHUB_ACTIONS") == "true"
                and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
                and os.environ.get("RUNNER_OS") == "Windows"
                and os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus"
                and os.environ.get("GITHUB_REF") == "refs/heads/main",
                "Tool preparation requires the fresh standard Windows package job")
        pins = json.loads(PINS.read_text())
        count = prepare(local_app_data(), pins)
        print(json.dumps({"check": "verified_windows_nsis_tools", "status": "passed",
                          "packager": pins["packager_version"], "files": count,
                          "sha256": {name: pin["sha256"] for name, pin in pins["assets"].items()}}, sort_keys=True))
    except Exception:
        # Network exceptions can carry signed redirect URLs or proxy details.
        print('{"check":"verified_windows_nsis_tools","status":"failed"}', flush=True)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
