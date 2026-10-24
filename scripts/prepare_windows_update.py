"""Produce the fixed per-user NSIS install marker and signed-data update input.

No installer or executable is run here. The official signer signs the FINAL
envelope separately; no private key, URL, account, registry value or local path
is serialized into the distributable payload. Installation remains disabled.
"""

import hashlib
import json
import os
from pathlib import Path
import stat
import struct

MAGIC = b"MORPHEUS-WINDOWS-PAYLOAD-V2\n"
MAX_BYTES = 512 * 1024 * 1024
MARKER_LIMIT = 16 * 1024
FILES = ("LICENSE", "licenses/Symphonia-MPL-2.0.txt", "licenses/Symphonia-NOTICE.txt",
         "morpheus-app.exe", "themes/aurora.toml",
         "licenses/THIRD-PARTY-NOTICES.txt", "licenses/THIRD-PARTY-NOTICES.json")
CONTRACT_SHA256 = "f76b4492cc3281c91c5ec703c39f69d732aa892427dbff5aa8d3f4b335bd02b6"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")


def read_regular(path, limit, *, cargo_binary=False):
    path = Path(path)
    for parent in [path, *path.parents]:
        info = parent.lstat()
        require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400,
                "Reparse or symlink producer input")
    before = path.lstat()
    # Cargo may hardlink the final executable from target/release/deps. This
    # read-only producer captures stable bytes; installed/staged native files
    # still require one link and are always freshly written, never hardlinked.
    require(stat.S_ISREG(before.st_mode) and before.st_nlink >= 1
            and (cargo_binary or before.st_nlink == 1) and 0 < before.st_size <= limit,
            "Unsupported producer input")
    with path.open("rb") as source:
        opened = os.fstat(source.fileno())
        data = source.read(limit + 1)
        after = os.fstat(source.fileno())
    require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
            (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mtime_ns) ==
            (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns)
            and len(data) == before.st_size, "Producer input changed")
    return data


def pe_identity(data):
    """Bound the native section; never load a PE or execute its code."""
    require(256 <= len(data) <= MAX_BYTES and data[:2] == b"MZ", "Expected Windows PE")
    pe, = struct.unpack_from("<I", data, 60)
    require(64 <= pe <= 1024 * 1024 and pe + 24 <= len(data) and data[pe:pe + 4] == b"PE\0\0",
            "Invalid PE header")
    machine, count = struct.unpack_from("<HH", data, pe + 4)
    optional, flags = struct.unpack_from("<HH", data, pe + 20)
    require(machine == 0x8664 and 1 <= count <= 96 and optional >= 112 and flags & 0x2002 == 2
            and pe + 24 + optional + count * 40 <= len(data), "Unsupported Windows image")
    require(struct.unpack_from("<H", data, pe + 24)[0] == 0x20B
            and struct.unpack_from("<H", data, pe + 24 + 68)[0] == 2, "Expected x64 GUI PE")
    table = pe + 24 + optional
    table_end = table + count * 40
    ranges, identity = [], None
    for index in range(count):
        at = table + index * 40
        used, = struct.unpack_from("<I", data, at + 8)
        size, offset = struct.unpack_from("<II", data, at + 16)
        characteristics, = struct.unpack_from("<I", data, at + 36)
        if size:
            require(table_end <= offset <= offset + size <= len(data)
                    and all(not (offset < end and start < offset + size) for start, end in ranges),
                    "Overlapping or unbounded PE sections")
            ranges.append((offset, offset + size))
        if data[at:at + 8] == b".mrphupd":
            require(identity is None and 1 <= used <= 4096 and used <= size
                    and characteristics & 0xE2000040 == 0x40000040
                    and not any(data[offset + used:offset + size]), "Unsupported identity section")
            identity = json.loads(data[offset:offset + used])
    require(isinstance(identity, dict) and set(identity) == {"protocol", "product", "version", "arch", "persistence"}
            and type(identity["protocol"]) is int and identity["protocol"] == 2
            and identity["product"] == "io.github.brk275.morpheus"
            and identity["arch"] == "x86_64" and isinstance(identity["version"], str)
            and 0 < len(identity["version"]) <= 128 and isinstance(identity["persistence"], str)
            and len(identity["persistence"]) == 64
            and all(c in "0123456789abcdef" for c in identity["persistence"]), "Invalid package identity")
    return identity


def prepare(config, root, directory, *, notice_contents):
    """After the ordinary marked cargo build, before the one NSIS invocation."""
    root, directory = Path(root), Path(directory)
    require(directory.is_dir() and not any(directory.iterdir()), "Expected empty owned producer directory")
    require(config.get("name") == "morpheus-app" and config.get("product-name") == "Morpheus"
            and config.get("identifier") == "io.github.brk275.morpheus"
            and config.get("binaries") == [{"path": "morpheus-app", "main": True}], "Unsupported product layout")
    require(config.get("resources") == ["themes/aurora.toml", "LICENSE", "licenses/Symphonia-MPL-2.0.txt",
                                        "licenses/Symphonia-NOTICE.txt"], "Uninstaller resource ledger changed")
    require(not any(key in config for key in ("nsis", "windows", "publisher", "external-binaries", "externalBinaries", "file-associations", "fileAssociations",
                                             "deep-link-protocols", "deepLinkProtocols", "appdata-paths", "appdataPaths")),
            "Unsupported Windows installer customization")
    contract = json.loads(read_regular(root / "packaging/windows/update-contract.json", 16384))
    require(hashlib.sha256(canonical(contract)).hexdigest() == CONTRACT_SHA256, "Windows update contract changed")
    template = root / "packaging/windows/morpheus-installer.nsi"
    require(hashlib.sha256(read_regular(template, 128 * 1024)).hexdigest() == contract["template_sha256"],
            "Windows installer template changed")
    binary_dir = config.get("binaries-dir", "target/release")
    require(binary_dir == "target/release", "Unsupported binary directory")
    if __package__:
        from . import native_notices as notices
    else:
        import native_notices as notices
    notices.verify_bytes(notice_contents, platform="win32", version=config["version"],
                         lock=read_regular(root / "Cargo.lock", 4 * 1024 * 1024))
    contents = {}
    remaining = MAX_BYTES - len(MAGIC) - 4 - MARKER_LIMIT
    for name in FILES:
        source = root / ("target/release/morpheus-app.exe" if name == "morpheus-app.exe" else name)
        contents[name] = (notice_contents[Path(name).name] if name.startswith("licenses/THIRD-PARTY-NOTICES.")
                          else read_regular(source, remaining, cargo_binary=name == "morpheus-app.exe"))
        require(0 < len(contents[name]) <= remaining, "Windows resource exceeds payload bound")
        remaining -= len(contents[name])
    identity = pe_identity(contents["morpheus-app.exe"])
    require(identity["version"] == config.get("version"), "Package and native versions differ")
    marker = canonical({"schema": 2, "identity": identity, "uninstaller_contract": CONTRACT_SHA256,
                        "files": [{"path": name, "bytes": len(contents[name]),
                                   "sha256": hashlib.sha256(contents[name]).hexdigest()} for name in FILES]})
    require(0 < len(marker) <= MARKER_LIMIT and sum(map(len, contents.values())) + len(marker) + len(MAGIC) + 4 <= MAX_BYTES,
            "Windows payload exceeds its bound")
    marker_path = directory / "morpheus-install.json"
    with marker_path.open("xb") as stream:
        stream.write(marker)
        stream.flush()
        os.fsync(stream.fileno())
    payload = directory / "Morpheus.windows-payload-v2.bin"
    with payload.open("xb") as stream:
        stream.write(MAGIC)
        stream.write(struct.pack("<I", len(marker)))
        stream.write(marker)
        for name in FILES:
            stream.write(contents[name])
        stream.flush()
        os.fsync(stream.fileno())
    result = dict(config)
    result.pop("before-packaging-command", None)  # The exact ordinary build just finished.
    # A target path is explicit; no absolute producer path enters the installed marker.
    result["resources"] = list(config["resources"])
    for name in notices.NAMES:
        notice_path = directory / name
        with notice_path.open("xb") as stream:
            if os.name != "nt": os.fchmod(stream.fileno(), 0o644)
            stream.write(notice_contents[name]); stream.flush(); os.fsync(stream.fileno())
        result["resources"].append({"src": str(notice_path), "target": "licenses/" + name})
    result["resources"].append({"src": str(marker_path), "target": "morpheus-install.json"})
    result["nsis"] = {"template": str(template), "installMode": "currentUser"}
    result["windows"] = {"allowDowngrades": False}
    result["publisher"] = "Morpheus"
    return result, payload
