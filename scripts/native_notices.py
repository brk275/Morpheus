"""Candidate-bound portable notice data; no source archives are extracted or executed.

This is evidence about exact source bytes, never a license-route selection or
permission to distribute. Native SDK, relinking and legal acceptance remain open.
"""
from __future__ import annotations

import hashlib
import functools
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import tarfile

ROOT = Path(__file__).resolve().parents[1]
TEXT = "THIRD-PARTY-NOTICES.txt"
INVENTORY = "THIRD-PARTY-NOTICES.json"
NAMES = (TEXT, INVENTORY)
MAX_TEXT = 64 * 1024 * 1024
MAX_INVENTORY = 16 * 1024 * 1024
MAX_BUNDLE = 512 * 1024 * 1024
MAX_FILES = 20000
MAX_FILE = 16 * 1024 * 1024
SOURCE_LIMIT = 128 * 1024 * 1024
HASH = re.compile(r"[0-9a-f]{64}")
OID = re.compile(r"[0-9a-f]{40}")
INVALID = "Candidate notice evidence is invalid or incomplete"
OPEN = ("native-linked-input-and-sdk-source-provenance", "compiler-system-runtime-licenses",
        "license-route-and-attribution-obligations", "generated-preferred-source-forms",
        "relinking-and-installation-information", "legal-distribution-approval")


def require(value):
    if not value:
        raise ValueError(INVALID)


def digest(data):
    return hashlib.sha256(data).hexdigest()


@functools.lru_cache(maxsize=1)
def catalogue():
    # Fixed reviewed helper path, never a module from an input archive or PYTHONPATH.
    path = ROOT / "packaging/linux/corresponding_source.py"
    spec = importlib.util.spec_from_file_location("morpheus_notice_source_catalogue", path)
    require(spec is not None and spec.loader is not None)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def read_regular(path, maximum, *, allow_empty=False):
    path = Path(path)
    require(path.is_absolute())
    for item in (path, *path.parents):
        info = item.lstat()
        require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400)
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_nlink == 1
            and (0 if allow_empty else 1) <= before.st_size <= maximum
            and (os.name == "nt" or before.st_uid == os.getuid() and not before.st_mode & 0o022))
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    descriptor = catalogue().windows_security().open_read(path) if os.name == "nt" else os.open(path, flags)
    with os.fdopen(descriptor, "rb") as stream:
        opened = os.fstat(stream.fileno())
        data = stream.read(maximum + 1)
        after = os.fstat(stream.fileno())
    stamp = lambda x: (x.st_dev, x.st_ino, x.st_mode, x.st_size, x.st_mtime_ns, x.st_ctime_ns)
    require(stamp(before) == stamp(opened) == stamp(after) and len(data) == before.st_size)
    return data



def portable_source_name(name, seen, *, windows=os.name == "nt"):
    """Validate a workspace filesystem name, unlike archive-only data references."""
    catalogue().source_path(name)
    parts=PurePosixPath(name).parts
    require(len(parts)<=32)
    if not windows: return name
    for index,part in enumerate(parts,1):
        require(part.isascii() and len(part)<=255 and not any(c in part for c in '<>:"\\|?*') and not part.endswith((" ","."))
                and not re.fullmatch(r"(?:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?",part,re.I))
        prefix="/".join(parts[:index]);key=prefix.casefold()
        require(key not in seen or seen[key]==prefix)
        seen[key]=prefix;require(len(seen)<=MAX_FILES)
    return name

def archive_files(source, data, maximum, *, links=False, file_limit=MAX_FILE):
    """Bounded archive-as-data. No extraction, executable loading or member import."""
    unpacked = source.bounded_single_gzip(data, maximum)
    files, modes, entries, total = {}, {}, {}, 0
    try:
        with tarfile.open(fileobj=io.BytesIO(unpacked), mode="r:") as archive:
            for member in archive:
                name = source.source_path(member.name)
                require(name not in files and len(files) < MAX_FILES and member.sparse is None
                        and not member.mode & 0o7000 and 0 <= member.size <= file_limit)
                if member.isfile():
                    require(member.mode in (0o644, 0o755))
                    content = archive.extractfile(member).read(member.size + 1)
                    require(len(content) == member.size)
                    mode = "100755" if member.mode == 0o755 else "100644"
                else:
                    require(links and member.issym() and member.size == 0)
                    content, mode = member.linkname.encode("utf-8", "strict"), "120000"
                total += len(content)
                require(total <= maximum)
                files[name], modes[name] = content, mode
                entries[name] = {"mode": mode, "oid": source.object_id("blob", content), "bytes": len(content)}
            tail = unpacked[archive.offset:]
            require(len(tail) >= 1024 and len(tail) % 512 == 0 and not any(tail))
        if links:
            source.check_links(entries, files)
    except (tarfile.TarError, UnicodeError, OSError, ValueError, OverflowError):
        raise ValueError(INVALID) from None
    return files, entries


def tree_archive(source, data, identity, budget=None):
    fields = {"git_identity_schema","commit","commit_object_hex","tree","archive_sha256","files","git_attributes_applied"}
    require(type(identity) is dict and (set(identity)==fields or set(identity)==fields|{"archive"})
            and identity.get("git_attributes_applied") is False and type(identity.get("files")) is dict
            and type(identity.get("tree")) is str and OID.fullmatch(identity["tree"])
            and digest(data) == identity.get("archive_sha256"))
    raw_commit = identity.get("commit_object_hex")
    require(type(identity.get("git_identity_schema")) is int and identity["git_identity_schema"] == 2 and type(raw_commit) is str
            and 0 < len(raw_commit) <= 2 * 1024 * 1024 and len(raw_commit) % 2 == 0
            and all(c in "0123456789abcdef" for c in raw_commit)
            and type(identity.get("commit")) is str and OID.fullmatch(identity["commit"]))
    commit = bytes.fromhex(raw_commit)
    require(source.object_id("commit", commit) == identity["commit"]
            and commit.split(b"\n", 1)[0] == ("tree " + identity["tree"]).encode("ascii"))
    files, entries = archive_files(source, data, SOURCE_LIMIT, links=True)
    expected = identity["files"]
    require(set(files) == set(expected) and source.tree_identity(entries) == identity["tree"])
    for name, content in files.items():
        require(expected[name] == {**entries[name], "sha256": digest(content)})
    if budget is not None:
        budget.add(entries=len(files), expanded=sum(map(len, files.values())), inputs=len(data),
                   paths=sum(len(name.encode("utf-8")) for name in files))
    return files


def plain(data, maximum=MAX_FILE):
    require(type(data) is bytes and 0 < len(data) <= maximum)
    try:
        text = data.decode("utf-8", "strict")
    except UnicodeError:
        raise ValueError(INVALID) from None
    require(all(ord(c) >= 32 or c in "\r\n\t" for c in text) and "\x7f" not in text)
    return data


def prepare_bytes(bundle, receipt_bytes, revision, *, platform, version, working=None):
    """Return exactly two plain-data files, derived from authenticated source bytes."""
    require(platform in {"darwin", "win32"} and type(revision) is str and OID.fullmatch(revision)
            and type(version) is str and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.+-]{0,127}", version)
            and len(bundle) <= MAX_BUNDLE and len(receipt_bytes) <= MAX_INVENTORY)
    source = catalogue()
    receipt = source.unique_json(receipt_bytes)
    source.validate_receipt(receipt, revision)
    require(digest(bundle) == receipt["bundle_sha256"])
    outer, _ = archive_files(source, bundle, MAX_BUNDLE + 64 * 1024 * 1024, file_limit=64 * 1024 * 1024)
    require("MANIFEST.json" in outer and digest(outer["MANIFEST.json"]) == receipt["manifest_sha256"])
    manifest = source.unique_json(outer["MANIFEST.json"])
    require(type(manifest) is dict and type(manifest.get("schema")) is int and manifest["schema"] == 1
            and manifest.get("distribution_ready") is False
            and manifest.get("license_compliance_verified") is False
            and manifest.get("unresolved_release_requirements") == list(source.UNRESOLVED))
    identity = manifest.get("source")
    require(type(identity) is dict and identity.get("commit") == revision
            and identity.get("tree") == receipt["source_tree"])
    budget = source.Budget()
    files = tree_archive(source, outer.get("workspace.tar.gz", b""), identity, budget)
    require(digest(files["Cargo.lock"]) == manifest.get("cargo_lock_sha256"))
    if working is not None:
        if __package__:
            from .package_revision import versioned_source_bytes, UPDATE_VERSION
        else:
            from package_revision import versioned_source_bytes, UPDATE_VERSION
        root = Path(working)
        names={}
        for path, expected in files.items():
            portable_source_name(path,names)
            require(identity["files"][path]["mode"] != "120000")
            if UPDATE_VERSION.fullmatch(version):
                expected = versioned_source_bytes(path, expected, version)
            require(read_regular(root / path, MAX_FILE, allow_empty=True) == expected)
            require(os.name == "nt" or bool((root / path).lstat().st_mode & 0o111)
                    == (identity["files"][path]["mode"] == "100755"))
    packages = source.lock_packages(files)
    require(receipt["registry_packages"] == sum(p.get("source") == source.REGISTRY for p in packages)
            and receipt["git_sources"] == len({p["source"] for p in packages if p.get("source", "").startswith("git+")})
            and receipt["locked_packages"] == len(packages)
            and type(manifest.get("cargo_packages")) is list
            and len(manifest["cargo_packages"]) == len(packages))
    original_records = {}
    for record in manifest["cargo_packages"]:
        require(type(record) is dict)
        key = (record.get("name"), record.get("version"), record.get("source"))
        require(key not in original_records)
        original_records[key] = record
    local = source.package_catalog(files)
    records, texts, provenance = [], {}, []
    git_files = {}

    text_bytes = 0
    def keep_text(checksum, value):
        nonlocal text_bytes
        if checksum not in texts:
            text_bytes += len(value)
            require(text_bytes <= MAX_TEXT)
            texts[checksum] = value

    def collect(package, contents, catalog, origin):
        key = (package["name"], package["version"])
        require(key in catalog)
        metadata = catalog[key]
        original_key = (*key, "committed-workspace" if package.get("source") is None else package["source"])
        original = original_records.get(original_key)
        require(original is not None and all(original.get(k) == v for k, v in metadata.items()))
        candidates = dict(metadata["license_text_candidates"])
        parent = PurePosixPath(metadata["manifest"]).parent
        for name, data in contents.items():
            path = PurePosixPath(name)
            if not path.is_relative_to(parent):
                continue
            relative = path.relative_to(parent)
            if (re.fullmatch(r"(?:LICENSE|COPYING|COPYRIGHT|NOTICE|UNLICENSE)(?:[._-].*)?", path.name, re.I)
                    or any(part.lower() in {"licenses", "license"} for part in relative.parts[:-1])):
                candidates[name] = digest(data)
        require(len(candidates) <= 256)
        license_refs = []
        for name, checksum in sorted(candidates.items()):
            require(name in contents and digest(contents[name]) == checksum)
            value = plain(contents[name]); keep_text(checksum, value)
            license_refs.append({"source_path": name, "sha256": checksum})
        records.append({"name": key[0], "version": key[1], "source": original_key[2],
                        "source_identity": origin, "declared_license": metadata["declared_license"],
                        "license_texts": license_refs,
                        "missing_license_text": not bool(license_refs),
                        "license_obligations_verified": False,
                        "native_linked_inputs_verified": False})

    for package in packages:
        origin = package.get("source")
        if origin is None:
            collect(package, files, local, {"tree": identity["tree"]})
        elif origin == source.REGISTRY:
            name = "registry/" + package["name"] + "-" + package["version"] + ".crate"
            require(name in outer)
            data = outer[name]
            contents = source.registry_contents(data, package, budget)
            collect(package, contents, source.package_catalog(contents, published=True),
                    {"archive_sha256": package["checksum"]})
        else:
            require(type(manifest.get("git_sources")) is dict and origin in manifest["git_sources"])
            git = manifest["git_sources"][origin]
            require(git.get("commit") == origin.rsplit("#", 1)[1])
            name = git.get("archive"); require(type(name) is str and name in outer)
            if origin not in git_files:
                git_files[origin] = tree_archive(source, outer[name], git, budget)
            contents = git_files[origin]
            collect(package, contents, source.package_catalog(contents), {"tree": git["tree"], "commit": git["commit"]})
    # Include exact recorded local patch explanations; these are source evidence,
    # not invented license text or an assertion of complete legal obligations.
    for name, value in sorted(files.items()):
        path = PurePosixPath(name)
        if (name == "vendor/README.md" or name.startswith("vendor/") and path.name in
                {"MORPHEUS_PATCH.md", "OWNED_COREAUDIO.md", "README.md"}):
            checksum = digest(plain(value)); keep_text(checksum, value)
            provenance.append({"source_path": name, "sha256": checksum})
    require(any(r["name"] == "slint" and r["license_texts"] for r in records)
            and any(x["source_path"] == "vendor/cpal/OWNED_COREAUDIO.md" for x in provenance))
    # Outer native material remains separately identified, never misattributed to
    # a Mac/Windows binary merely because a Linux source bundle contains it.
    native = manifest.get("native_sources")
    require(type(native) is dict and native.get("coverage") in {"rust-only", "receipt-bound-known-native-components-only"}
            and native.get("native_sources_complete") is False and native.get("actual_native_build_inputs_verified") is False)
    header = ("Morpheus third-party source notices\n"
              "This application uses Slint (https://slint.dev).\n"
              "Exact source evidence follows; no license route or distribution permission is selected.\n"
              "Native SDK/compiler/system/relinking obligations remain unverified.\n\n")
    output = bytearray(header.encode("utf-8")); index = []
    for record in sorted(records, key=lambda x: (x["name"], x["version"], x["source"])):
        line = {key: record[key] for key in ("name", "version", "source", "declared_license", "license_texts", "missing_license_text")}
        output.extend((json.dumps(line, sort_keys=True, ensure_ascii=True) + "\n").encode("ascii"))
        require(len(output) <= MAX_TEXT)
    output.extend(b"\nLocal patch provenance:\n")
    for record in provenance:
        output.extend((json.dumps(record, sort_keys=True, ensure_ascii=True) + "\n").encode("ascii"))
        require(len(output) <= MAX_TEXT)
    for checksum, text in sorted(texts.items()):
        output.extend(("\n===== SOURCE TEXT SHA256 " + checksum + " =====\n").encode("ascii"))
        offset = len(output); output.extend(text); output.extend(b"\n")
        require(len(output) <= MAX_TEXT)
        index.append({"sha256": checksum, "offset": offset, "bytes": len(text)})
    built_lock = read_regular(Path(working) / "Cargo.lock", 4 * 1024 * 1024) if working is not None else files["Cargo.lock"]
    inventory = {"schema": 1, "source_revision": revision, "source_tree": identity["tree"],
                 "cargo_lock_sha256": digest(files["Cargo.lock"]), "built_cargo_lock_sha256": digest(built_lock), "package_version": version,
                 "platform": platform, "source_bundle_sha256": digest(bundle),
                 "source_manifest_sha256": digest(outer["MANIFEST.json"]),
                 "text_sha256": digest(output), "text_bytes": len(output), "texts": index,
                 "locked_packages": sorted(records, key=lambda x: (x["name"], x["version"], x["source"])),
                 "local_patch_provenance": provenance, "native_source_evidence": {
                     "coverage": native.get("coverage"), "bound_to_this_platform_binary": False},
                 "distribution_ready": False, "license_compliance_verified": False,
                 "unresolved_release_requirements": list(OPEN)}
    raw = json.dumps(inventory, sort_keys=True, indent=2, ensure_ascii=True).encode("ascii") + b"\n"
    require(len(raw) <= MAX_INVENTORY)
    return {TEXT: bytes(output), INVENTORY: raw}


def write_files(directory, contents):
    directory = Path(directory)
    require(set(contents) == set(NAMES) and directory.is_dir() and not any(directory.iterdir()))
    for name in NAMES:
        with (directory / name).open("xb") as stream:
            if os.name != "nt": os.fchmod(stream.fileno(), 0o644)
            stream.write(contents[name]); stream.flush(); os.fsync(stream.fileno())
    return {name: {"bytes": len(contents[name]), "sha256": digest(contents[name])} for name in NAMES}


def verify_bytes(contents, *, revision=None, platform=None, version=None, lock=None):
    """Verify plain output/ledger only; never infer its legal or linked-input status."""
    require(type(contents) is dict and set(contents) == set(NAMES)
            and 0 < len(contents[TEXT]) <= MAX_TEXT and 0 < len(contents[INVENTORY]) <= MAX_INVENTORY)
    source = catalogue()
    value = source.unique_json(contents[INVENTORY])
    expected_keys = {"schema", "source_revision", "source_tree", "cargo_lock_sha256", "built_cargo_lock_sha256",
                     "package_version", "platform", "source_bundle_sha256", "source_manifest_sha256",
                     "text_sha256", "text_bytes", "texts", "locked_packages", "local_patch_provenance",
                     "native_source_evidence", "distribution_ready", "license_compliance_verified",
                     "unresolved_release_requirements"}
    require(type(value) is dict and set(value) == expected_keys and type(value["schema"]) is int and value["schema"] == 1
            and value["distribution_ready"] is False and value["license_compliance_verified"] is False
            and value["unresolved_release_requirements"] == list(OPEN)
            and type(value["source_revision"]) is str and OID.fullmatch(value["source_revision"])
            and type(value["source_tree"]) is str and OID.fullmatch(value["source_tree"])
            and value["platform"] in {"darwin", "win32"})
    for key in ("cargo_lock_sha256", "built_cargo_lock_sha256", "source_bundle_sha256", "source_manifest_sha256", "text_sha256"):
        require(type(value[key]) is str and HASH.fullmatch(value[key]))
    require(value["text_sha256"] == digest(contents[TEXT]) and type(value["text_bytes"]) is int
            and value["text_bytes"] == len(contents[TEXT])
            and (revision is None or value["source_revision"] == revision)
            and (platform is None or value["platform"] == platform)
            and (version is None or value["package_version"] == version)
            and (lock is None or value["built_cargo_lock_sha256"] == digest(lock)))
    plain(contents[TEXT], MAX_TEXT)
    require(type(value["texts"]) is list and len(value["texts"]) <= 20000)
    hashes, previous = set(), 0
    for item in value["texts"]:
        require(type(item) is dict and set(item) == {"sha256", "offset", "bytes"}
                and type(item["sha256"]) is str and HASH.fullmatch(item["sha256"])
                and item["sha256"] not in hashes and type(item["offset"]) is int
                and type(item["bytes"]) is int and 0 < item["bytes"] <= MAX_FILE
                and previous <= item["offset"] <= item["offset"] + item["bytes"] <= len(contents[TEXT]))
        require(digest(contents[TEXT][item["offset"]:item["offset"] + item["bytes"]]) == item["sha256"])
        hashes.add(item["sha256"]); previous = item["offset"] + item["bytes"]
    records = value["locked_packages"]
    require(type(records) is list and 0 < len(records) <= 4096)
    seen = set()
    for record in records:
        require(type(record) is dict and set(record) == {"name", "version", "source", "source_identity", "declared_license",
                "license_texts", "missing_license_text", "license_obligations_verified", "native_linked_inputs_verified"}
                and record["license_obligations_verified"] is False and record["native_linked_inputs_verified"] is False
                and all(type(record[key]) is str and 0 < len(record[key]) <= 2048 for key in ("name", "version", "source"))
                and type(record["license_texts"]) is list and len(record["license_texts"]) <= 256
                and record["missing_license_text"] is (not bool(record["license_texts"])))
        origin = record["source_identity"]
        require(type(origin) is dict and (record["declared_license"] is None or type(record["declared_license"]) is str
                and len(record["declared_license"]) <= 4096))
        if record["source"] == "committed-workspace":
            require(set(origin) == {"tree"} and type(origin["tree"]) is str and OID.fullmatch(origin["tree"]))
        elif record["source"] == source.REGISTRY:
            require(set(origin) == {"archive_sha256"} and type(origin["archive_sha256"]) is str
                    and HASH.fullmatch(origin["archive_sha256"]))
        else:
            require(set(origin) == {"tree", "commit"} and all(type(origin[key]) is str and OID.fullmatch(origin[key])
                    for key in ("tree", "commit")) and record["source"].endswith("#" + origin["commit"]))
        key = (record["name"], record["version"], record["source"]); require(key not in seen); seen.add(key)
        for item in record["license_texts"]:
            require(type(item) is dict and set(item) == {"source_path", "sha256"}
                    and source.source_path(item["source_path"]) == item["source_path"] and item["sha256"] in hashes)
    require(any(record["name"] == "slint" and record["license_texts"] for record in records))
    require(type(value["local_patch_provenance"]) is list and len(value["local_patch_provenance"]) <= MAX_FILES)
    for item in value["local_patch_provenance"]:
        require(type(item) is dict and set(item) == {"source_path", "sha256"}
                and source.source_path(item["source_path"]) == item["source_path"] and item["sha256"] in hashes)
    require(any(item["source_path"] == "vendor/cpal/OWNED_COREAUDIO.md" for item in value["local_patch_provenance"]))
    require(type(value["native_source_evidence"]) is dict and set(value["native_source_evidence"]) ==
            {"coverage", "bound_to_this_platform_binary"} and value["native_source_evidence"]["bound_to_this_platform_binary"] is False
            and value["native_source_evidence"]["coverage"] in {"rust-only", "receipt-bound-known-native-components-only"})
    return value


def prepare(bundle_path, receipt_path, directory, *, revision, platform, version, working):
    contents = prepare_bytes(read_regular(Path(bundle_path), MAX_BUNDLE),
                             read_regular(Path(receipt_path), MAX_INVENTORY), revision,
                             platform=platform, version=version, working=working)
    verify_bytes(contents, revision=revision, platform=platform, version=version,
                 lock=read_regular(Path(working) / "Cargo.lock", 4 * 1024 * 1024))
    write_files(directory, contents)
    return contents


def read_files(directory):
    directory = Path(directory)
    return {TEXT: read_regular(directory / TEXT, MAX_TEXT),
            INVENTORY: read_regular(directory / INVENTORY, MAX_INVENTORY)}
