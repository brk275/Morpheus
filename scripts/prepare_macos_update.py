#!/usr/bin/env python3
"""Produce bounded unsigned Mac update input from an unchanged packaged .app.

This never builds, executes, installs, signs, rewrites a bundle or reads a key.
The exact resulting .app.tar.gz must later use the official Packager signer.
The runtime installation admission gate remains separate and closed.
"""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import struct
import sys
import tarfile
import tempfile


MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_EXPANDED_BYTES = 1024 * 1024 * 1024
MAX_ENTRIES = 32_768
ROOT_NAME = "Morpheus.app"
ARCHIVE_NAME = "Morpheus.app.tar.gz"
EXECUTABLE = "Contents/MacOS/morpheus-app"
PRODUCT = "io.github.brk275.morpheus"
RESOURCES = {
    "aurora.toml": "themes/aurora.toml",
    "LICENSE": "LICENSE",
    "Symphonia-MPL-2.0.txt": "licenses/Symphonia-MPL-2.0.txt",
    "Symphonia-NOTICE.txt": "licenses/Symphonia-NOTICE.txt",
}
INVALID = "macOS update payload input is invalid"


def require(condition):
    if not condition:
        raise ValueError(INVALID)


def _directory(path):
    metadata = path.lstat()
    require(stat.S_ISDIR(metadata.st_mode) and metadata.st_uid == os.geteuid()
            and not metadata.st_mode & 0o7022)
    return metadata


def _regular(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    file = os.fdopen(descriptor, "rb")
    try:
        metadata = os.fstat(file.fileno())
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_uid == os.geteuid()
                and metadata.st_nlink == 1 and not metadata.st_mode & 0o7022
                and metadata.st_size <= MAX_ARCHIVE_BYTES)
        return file, metadata
    except BaseException:
        file.close()
        raise


def _stamp(metadata):
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_uid,
            metadata.st_nlink, metadata.st_size)


def _read(path, limit):
    file, metadata = _regular(path)
    with file:
        require(metadata.st_size <= limit)
        data = file.read(limit + 1)
        require(len(data) == metadata.st_size and _stamp(os.fstat(file.fileno())) == _stamp(metadata))
        return data


def _name(text):
    require(isinstance(text, str) and text.isascii() and len(text) <= 1024
            and all(0x20 <= ord(character) < 0x7f and character not in "\\:" for character in text))


def _path(text):
    _name(text)
    path = PurePosixPath(text)
    require(path.parts and path.parts[0] == ROOT_NAME and len(path.parts) <= 32
            and all(part not in {"", ".", ".."} and len(part) <= 255 for part in path.parts))
    return path


def _link(path, target, bundle):
    _name(target)
    link = PurePosixPath(target)
    require(target and not link.is_absolute())
    depth = len(path.parts) - 1
    for part in link.parts:
        if part == "..":
            require(depth > 1)
            depth -= 1
        elif part != ".":
            depth += 1
    resolved = (bundle / Path(*path.parts[1:])).resolve(strict=True)
    root = bundle.resolve(strict=True)
    require(resolved == root or root in resolved.parents)


def _pairs(items):
    result = {}
    for key, value in items:
        require(key not in result)
        result[key] = value
    return result


def _version(value):
    require(isinstance(value, str) and len(value) <= 128)
    match = re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
                         r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
                         r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?", value)
    require(match is not None)
    require(not match[4] or all(not (part.isdigit() and len(part) > 1 and part[0] == "0")
                                for part in match[4].split(".")))


def binary_identity(data, version, arch):
    """Read exactly the same thin native Mach-O/immutable identity contract."""
    require(len(data) >= 32 and len(data) <= MAX_ARCHIVE_BYTES)
    word = lambda offset: struct.unpack_from("<I", data, offset)[0]
    wide = lambda offset: struct.unpack_from("<Q", data, offset)[0]
    require(arch in {"aarch64", "x86_64"} and word(0) == 0xfeedfacf
            and word(4) == {"aarch64": 0x0100000c, "x86_64": 0x01000007}[arch]
            and word(12) == 2)
    count, size = word(16), word(20)
    require(0 < count <= 4096 and size <= 1024 * 1024 and size <= len(data) - 32)
    position, end, identity = 32, 32 + size, None
    for _ in range(count):
        require(position + 8 <= end)
        command, length = word(position), word(position + 4)
        require(length >= 8 and length % 8 == 0 and length <= end - position)
        if command == 0x19:
            require(length >= 72)
            sections = word(position + 64)
            require(sections <= 4096 and 72 + sections * 80 == length)
            for index in range(sections):
                section = position + 72 + index * 80
                if data[section:section + 16] == b"__morpheus_upd\0\0":
                    require(identity is None and data[section + 16:section + 32] == b"__TEXT" + b"\0" * 10
                            and data[position + 8:position + 24] == b"__TEXT" + b"\0" * 10
                            and not word(position + 60) & 2 and not word(section + 64) & 0xff)
                    length, offset = wide(section + 40), word(section + 48)
                    require(0 < length <= 4096 and offset >= end and offset + length <= len(data)
                            and offset >= wide(position + 40)
                            and wide(position + 40) + wide(position + 48) < (1 << 64)
                            and offset + length <= wide(position + 40) + wide(position + 48))
                    identity = json.loads(data[offset:offset + length], object_pairs_hook=_pairs)
        position += word(position + 4)
    require(position == end and isinstance(identity, dict)
            and set(identity) == {"protocol", "product", "version", "arch", "persistence"}
            and type(identity["protocol"]) is int and identity["protocol"] == 2
            and identity["product"] == PRODUCT and identity["arch"] == arch
            and identity["version"] == version)
    _version(identity["version"])
    require(isinstance(identity["persistence"], str)
            and re.fullmatch("[0-9a-fA-F]{64}", identity["persistence"]) is not None)
    return identity


def plist_values(data):
    """Closed root XML string/bool dictionary accepted by the native reader."""
    require(len(data) <= 64 * 1024)
    rest = data.decode("utf-8")

    def consume(token, required=False):
        nonlocal rest
        rest = rest.lstrip(" \t\r\n")
        found = rest.startswith(token)
        require(found or not required)
        if found:
            rest = rest[len(token):]
        return found

    def text(tag):
        nonlocal rest
        consume(f"<{tag}>", True)
        end = rest.find("<")
        require(0 <= end and len(rest[:end].encode("utf-8")) <= 16 * 1024)
        value, rest = rest[:end], rest[end:]
        consume(f"</{tag}>", True)
        entities = {"&amp;": "&", "&lt;": "<", "&gt;": ">", "&quot;": '"', "&apos;": "'"}
        require("&" not in re.sub(r"&(amp|lt|gt|quot|apos);", "", value))
        value = re.sub(r"&(amp|lt|gt|quot|apos);", lambda found: entities[found[0]], value)
        require(all(character in "\t\r\n" or (ord(character) >= 0x20
                    and not 0x7f <= ord(character) <= 0x9f) for character in value))
        return value

    consume('<?xml version="1.0" encoding="UTF-8"?>')
    consume('<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">')
    consume('<plist version="1.0">', True)
    consume("<dict>", True)
    values = {}
    while not consume("</dict>"):
        require(len(values) < 128)
        key = text("key")
        require(key and len(key.encode("utf-8")) <= 128 and key not in values)
        if consume("<true/>"):
            value = True
        elif consume("<false/>"):
            value = False
        else:
            value = text("string")
        values[key] = value
    consume("</plist>", True)
    require(not rest.strip(" \t\r\n"))
    return values


def _snapshot(bundle):
    root = _directory(bundle)
    records, aliases, total = [], set(), 0
    tree = hashlib.sha256(b"morpheus-macos-bundle-tree-v1\0")
    tree.update(struct.pack("<I", root.st_mode & 0o777))

    def walk(directory, relative):
        nonlocal total
        children = []
        with os.scandir(directory) as scan:
            for child in scan:
                children.append(child)
                require(len(children) + len(records) + 1 <= MAX_ENTRIES)
        children.sort(key=lambda child: os.fsencode(child.name))
        for child in children:
            name = relative / child.name
            logical = _path(f"{ROOT_NAME}/{name.as_posix()}")
            key = logical.as_posix().lower()
            require(key not in aliases)
            aliases.add(key)
            metadata = child.stat(follow_symlinks=False)
            require(metadata.st_uid == os.geteuid())
            mode = metadata.st_mode & 0o777
            record = {"path": name.as_posix(), "mode": mode, "size": metadata.st_size,
                      "stamp": _stamp(metadata)}
            records.append(record)
            require(len(records) + 1 <= MAX_ENTRIES)
            if stat.S_ISDIR(metadata.st_mode):
                require(not metadata.st_mode & 0o7022)
                record.update(kind="directory", size=0)
                walk(Path(child.path), name)
            elif stat.S_ISREG(metadata.st_mode):
                require(len(logical.parts) >= 3 and metadata.st_nlink == 1
                        and not metadata.st_mode & 0o7022 and metadata.st_size <= MAX_ARCHIVE_BYTES)
                total += metadata.st_size
                require(total <= MAX_EXPANDED_BYTES)
                record["kind"] = "file"
            else:
                require(stat.S_ISLNK(metadata.st_mode) and len(logical.parts) >= 3
                        and metadata.st_nlink == 1 and not metadata.st_mode & 0o7000
                        and metadata.st_mode & 0o400)
                target = os.readlink(child.path)
                _link(logical, target, bundle)
                record.update(kind="link", size=0, target=target)

    walk(bundle, PurePosixPath())
    records.sort(key=lambda record: record["path"])
    for record in records:
        name = record["path"].encode("ascii")
        tree.update(struct.pack("<Q", len(name)) + name + struct.pack("<I", record["mode"]))
        if record["kind"] == "directory":
            tree.update(b"d")
        elif record["kind"] == "link":
            target = record["target"].encode("ascii")
            tree.update(b"l" + struct.pack("<Q", len(target)) + target)
        else:
            tree.update(b"f" + struct.pack("<Q", record["size"]))
            file, metadata = _regular(bundle / record["path"])
            with file:
                require(_stamp(metadata) == record["stamp"])
                digest, size = hashlib.sha256(), 0
                while chunk := file.read(64 * 1024):
                    size += len(chunk)
                    require(size <= record["size"])
                    digest.update(chunk)
                    tree.update(chunk)
                require(size == record["size"] and _stamp(os.fstat(file.fileno())) == record["stamp"])
                record["sha256"] = digest.hexdigest()
    return {"mode": root.st_mode & 0o777, "records": records, "tree_sha256": tree.hexdigest()}


def _header(record):
    if record["kind"] == "link":
        require(not record["mode"] & ~0o777 and record["mode"] & 0o400)
    info = tarfile.TarInfo(ROOT_NAME + ("/" + record["path"] if record["path"] else ""))
    info.mode, info.uid, info.gid, info.mtime = record["mode"], 0, 0, 0
    info.uname = info.gname = ""
    info.size = record["size"] if record["kind"] == "file" else 0
    info.type = {"directory": tarfile.DIRTYPE, "file": tarfile.REGTYPE, "link": tarfile.SYMTYPE}[record["kind"]]
    info.linkname = record.get("target", "")
    # Preflight rejects paths/links needing GNU/PAX extensions before output.
    info.tobuf(format=tarfile.USTAR_FORMAT, encoding="ascii", errors="strict")
    return info


class _BoundedWriter:
    def __init__(self, inner, limit):
        self.inner, self.limit, self.size = inner, limit, 0

    def write(self, data):
        require(len(data) <= self.limit - self.size)
        written = self.inner.write(data)
        require(written == len(data))
        self.size += written
        return written

    def tell(self):
        return self.size

    def flush(self):
        self.inner.flush()


class _PayloadReader:
    def __init__(self, file, record):
        self.file, self.record, self.digest, self.size = file, record, hashlib.sha256(), 0

    def read(self, size):
        require(0 <= size <= MAX_ARCHIVE_BYTES)
        data = self.file.read(size)
        self.size += len(data)
        require(self.size <= self.record["size"])
        self.digest.update(data)
        return data

    def finish(self):
        require(self.size == self.record["size"] and not self.file.read(1)
                and self.digest.hexdigest() == self.record["sha256"]
                and _stamp(os.fstat(self.file.fileno())) == self.record["stamp"])


def prepare_update(bundle, output, *, version, arch, root):
    """Return a closed receipt for an unsigned archive; never mutate the app."""
    require(not any(key in os.environ for key in
                    ("CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD")))
    bundle, output, root = Path(bundle), Path(output), Path(root)
    require(bundle.is_absolute() and output.is_absolute() and bundle.name == ROOT_NAME
            and output.name == ARCHIVE_NAME and bundle.parent.resolve(strict=True) == output.parent.resolve(strict=True))
    _directory(bundle.parent)
    require(not output.exists() and not output.is_symlink())
    snapshot = _snapshot(bundle)
    records = {record["path"]: record for record in snapshot["records"]}
    require(EXECUTABLE in records and records[EXECUTABLE]["kind"] == "file"
            and records[EXECUTABLE]["mode"] & 0o100)
    binary = _read(bundle / EXECUTABLE, MAX_ARCHIVE_BYTES)
    require(hashlib.sha256(binary).hexdigest() == records[EXECUTABLE]["sha256"])
    identity = binary_identity(binary, version, arch)
    del binary
    plist_record = records.get("Contents/Info.plist")
    require(plist_record is not None and plist_record["kind"] == "file")
    plist_bytes = _read(bundle / "Contents/Info.plist", 64 * 1024)
    require(len(plist_bytes) == plist_record["size"]
            and hashlib.sha256(plist_bytes).hexdigest() == plist_record["sha256"])
    plist = plist_values(plist_bytes)
    require(all(plist.get(key) == value for key, value in {
        "CFBundleIdentifier": PRODUCT, "CFBundleExecutable": "morpheus-app",
        "CFBundleShortVersionString": version, "CFBundlePackageType": "APPL"}.items()))
    purposes = plist_values(_read(root / "packaging/macos/Info.plist", 64 * 1024))
    require(all(plist.get(key) == value for key, value in purposes.items()))
    for name, source in RESOURCES.items():
        resource = records.get(f"Contents/Resources/{name}")
        require(resource is not None and resource["kind"] == "file"
                and resource["sha256"] == hashlib.sha256(_read(root / source, 16 * 1024 * 1024)).hexdigest())
    entries = [{"path": "", "mode": snapshot["mode"], "size": 0, "kind": "directory"}] + snapshot["records"]
    headers = [_header(record) for record in entries]
    created = False
    with tempfile.NamedTemporaryFile(prefix=".morpheus-mac-update-", suffix=".tmp", dir=output.parent) as temp:
        try:
            compressed = _BoundedWriter(temp, MAX_ARCHIVE_BYTES)
            with gzip.GzipFile(filename="", fileobj=compressed, mode="wb", mtime=0, compresslevel=6) as gzip_file:
                expanded = _BoundedWriter(gzip_file, MAX_EXPANDED_BYTES)
                with tarfile.open(fileobj=expanded, mode="w|", format=tarfile.USTAR_FORMAT,
                                  encoding="ascii", errors="strict") as archive:
                    for record, info in zip(entries, headers):
                        if record["kind"] == "file":
                            file, metadata = _regular(bundle / record["path"])
                            with file:
                                require(_stamp(metadata) == record["stamp"])
                                reader = _PayloadReader(file, record)
                                archive.addfile(info, reader)
                                reader.finish()
                        else:
                            archive.addfile(info)
            require(_snapshot(bundle) == snapshot)
            temp.flush()
            os.fsync(temp.fileno())
            temp.seek(0)
            digest = hashlib.file_digest(temp, "sha256").hexdigest()
            before = os.fstat(temp.fileno())
            os.link(temp.name, output, follow_symlinks=False)
            created = True
            published = output.lstat()
            require(stat.S_ISREG(published.st_mode) and (published.st_dev, published.st_ino) == (before.st_dev, before.st_ino))
            directory = os.open(output.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
            return {"schema": 1, "format": "app", "artifact": ARCHIVE_NAME,
                    "version": version, "arch": arch, "persistence": identity["persistence"],
                    "bytes": before.st_size, "sha256": digest,
                    "bundle_sha256": snapshot["tree_sha256"], "entries": len(entries), "signed": False}
        except BaseException:
            if created:
                actual, owned = output.lstat(), os.fstat(temp.fileno())
                if (actual.st_dev, actual.st_ino) == (owned.st_dev, owned.st_ino):
                    output.unlink()
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--arch", required=True, choices=["aarch64", "x86_64"])
    args = parser.parse_args()
    require(sys.platform == "darwin")
    receipt = prepare_update(args.app, args.output, version=args.version, arch=args.arch,
                             root=Path(__file__).resolve().parents[1])
    print("MORPHEUS_MAC_UPDATE " + json.dumps(receipt, sort_keys=True))


if __name__ == "__main__":
    main()
