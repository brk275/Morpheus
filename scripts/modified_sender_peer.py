"""Source-only experiment input validation; never an official peer gate."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import os
import re
import stat

ELEMENT_REVISION = "ed6a0e1341a8700980506a883749db16192dc6f7"
SDK_REVISION = "3f05dc4d104a3dfc753c5e3549a18c74fab4b076"
PROPOSAL_SHA256 = "d117ebefcf437665570ab5e1d8e506ddb1c7f81b785e976e41e5f5695b2a54bb"
ELEMENT_LOCK_SHA256 = "a1b59890deead362285d32abe900ac30fa0cacb627af5e8e714bae6d1a6a9e68"
MAX_FILES = 5000
MAX_BYTES = 256 * 1024 * 1024
MAX_FILE = 32 * 1024 * 1024


def refused():
    raise RuntimeError("configuration")


def read_regular(parent, name, limit):
    before = os.stat(name, dir_fd=parent, follow_symlinks=False)
    if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.geteuid()
            or before.st_nlink != 1 or not 0 <= before.st_size <= limit):
        refused()
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=parent)
    try:
        opened = os.fstat(fd)
        identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                                  value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
        if identity(opened) != identity(before):
            refused()
        data = bytearray()
        while block := os.read(fd, min(65536, before.st_size - len(data) + 1)):
            data.extend(block)
            if len(data) > before.st_size:
                refused()
        if (len(data) != before.st_size or identity(os.fstat(fd)) != identity(before)
                or identity(os.stat(name, dir_fd=parent, follow_symlinks=False)) != identity(before)):
            refused()
        return bytes(data)
    finally:
        os.close(fd)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            refused()
        result[key] = value
    return result


def verify_modified_assets(root: Path, expected_manifest_sha256: str, *, variant: str) -> Path:
    """Accept only an independently admitted exact build receipt and full tree.

    Producer/build review is separate: a self-asserted receipt is not provenance.
    This method must only receive the root's previously reviewed receipt digest.
    """
    if variant not in ("baseline44", "renewal44"):
        refused()
    if not re.fullmatch(r"[0-9a-f]{64}", expected_manifest_sha256):
        refused()
    if not root.is_absolute() or any(character in str(root) for character in (",", "\n", "\r", "\\")):
        refused()
    info = root.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
        refused()
    fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        if (os.fstat(fd).st_dev, os.fstat(fd).st_ino) != (info.st_dev, info.st_ino):
            refused()
        raw = read_regular(fd, "build-receipt.json", 2 * 1024 * 1024)
        if hashlib.sha256(raw).hexdigest() != expected_manifest_sha256:
            refused()
        receipt = json.loads(raw, object_pairs_hook=unique_object)
        if (type(receipt) is not dict or set(receipt) != {
                "schema", "classification", "variant", "proposal_applied", "element_revision", "sdk_revision", "proposal_sha256",
                "element_lock_sha256", "builder_source_sha256", "assets"}
                or type(receipt["schema"]) is not int or receipt["schema"] != 1
                or receipt["classification"] != "MODIFIED_PEER"
                or receipt["variant"] != variant or type(receipt["proposal_applied"]) is not bool
                or receipt["proposal_applied"] != (variant == "renewal44")
                or receipt["element_revision"] != ELEMENT_REVISION or receipt["sdk_revision"] != SDK_REVISION
                or receipt["proposal_sha256"] != PROPOSAL_SHA256
                or receipt["element_lock_sha256"] != ELEMENT_LOCK_SHA256
                or not re.fullmatch(r"[0-9a-f]{64}", receipt["builder_source_sha256"])
                or type(receipt["assets"]) is not list or not 1 <= len(receipt["assets"]) <= MAX_FILES):
            refused()
        expected = {}
        for row in receipt["assets"]:
            if type(row) is not dict or set(row) != {"path", "bytes", "sha256"}:
                refused()
            name = row["path"]
            if (not isinstance(name, str) or not name or PurePosixPath(name).is_absolute()
                    or any(part in ("", ".", "..") for part in name.split("/"))
                    or any(character in name for character in ("\\", "\n", "\r", "\x00"))
                    or name in expected or type(row["bytes"]) is not int
                    or not 0 <= row["bytes"] <= MAX_FILE or not re.fullmatch(r"[0-9a-f]{64}", row["sha256"])):
                refused()
            expected[name] = row
        if ("index.html" not in expected or "config.json" not in expected
                or any(name.startswith("config.json.") for name in expected)
                or expected["config.json"]["bytes"] != 0
                or expected["config.json"]["sha256"] != hashlib.sha256(b"").hexdigest()):
            refused()
        assets = os.open("dist", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
        try:
            found = set()
            total = entries = 0

            def visit(directory, prefix, depth):
                nonlocal total, entries
                if depth > 16:
                    refused()
                with os.scandir(directory) as listing:
                    for entry in listing:
                        entries += 1
                        if entries > MAX_FILES * 2:
                            refused()
                        item = entry.stat(follow_symlinks=False)
                        name = prefix + entry.name
                        if item.st_uid != os.geteuid():
                            refused()
                        if stat.S_ISDIR(item.st_mode):
                            child = os.open(entry.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                                            dir_fd=directory)
                            try:
                                if (os.fstat(child).st_dev, os.fstat(child).st_ino) != (item.st_dev, item.st_ino):
                                    refused()
                                visit(child, name + "/", depth + 1)
                            finally:
                                os.close(child)
                        elif stat.S_ISREG(item.st_mode) and name in expected and name not in found:
                            data = read_regular(directory, entry.name, MAX_FILE)
                            row = expected[name]
                            total += len(data)
                            if (len(data) != row["bytes"] or hashlib.sha256(data).hexdigest() != row["sha256"]
                                    or total > MAX_BYTES):
                                refused()
                            found.add(name)
                        else:
                            refused()
            visit(assets, "", 0)
            if found != set(expected):
                refused()
        finally:
            os.close(assets)
    finally:
        os.close(fd)
    return root / "dist"
