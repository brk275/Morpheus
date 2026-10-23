"""An explicit, offline source archive for a reviewed Linux package revision.

This records the complete committed workspace and every Cargo.lock source, not
legal compliance or permission to distribute a binary. Nothing is checked out,
downloaded, imported, built or run from the dependency sources.

Schema-1 input plans retain their Rust/workspace-only scope. Schema 2 additionally
requires native_inputs with coverage=known-components, package_artifact,
package_receipt, runtime_notices and pipewire_notices. These are existing owned
outputs from the same reviewed AppImage build, not URLs or replacement build
commands. Assembly copies only the already-shipped, receipt/pin-bound material.
Complete-native requests remain blocked by the SDK/compiler/system/relink gaps.
"""
from __future__ import annotations

import ast
import fnmatch
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import struct
import tarfile
import tomllib
import zlib

MAX_ARCHIVE = 64 * 1024 * 1024
MAX_FILE = 16 * 1024 * 1024
MAX_GIT_CONTENT = 128 * 1024 * 1024
MAX_GIT_ENTRIES = 20000
MAX_GIT_COMMANDS = 2048
MAX_BUNDLE = 512 * 1024 * 1024
MAX_MANIFEST = 32 * 1024 * 1024
MAX_NATIVE_RECEIPT = 4 * 1024 * 1024
MAX_RUNTIME = 4 * 1024 * 1024
RUNTIME_DOC = "usr/share/doc/morpheus-app/appimage-runtime"
PIPEWIRE_DOC = "usr/share/doc/morpheus-app/pipewire-client"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
HASH = re.compile(r"[0-9a-f]{64}")
OID = re.compile(r"[0-9a-f]{40}")
ATOM = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.+-]{0,127}")
BUNDLE_NAME = "morpheus-corresponding-source.tar.gz"
UNRESOLVED = (
    "native-and-prebuilt-build-input-corresponding-sources",
    "system-package-corresponding-sources",
    "compiler-toolchain-runtime-sources-and-exceptions",
    "generated-code-preferred-source-forms",
    "relinking-and-installation-information",
    "complete-license-obligations-and-distribution-approval",
    "source-bundle-content-review-and-secret-scan",
)


class SourceFailure(RuntimeError):
    def __init__(self, reason):
        allowed = {"input", "path", "bounds", "identity", "lock", "missing",
                   "archive", "link", "submodule", "git", "manifest", "output"}
        self.reason = reason if reason in allowed else "input"
        super().__init__("Corresponding source preparation failed: " + self.reason)


def require(value, reason="input"):
    if not value:
        raise SourceFailure(reason)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def object_id(kind, data):
    return hashlib.sha1(kind.encode() + b" " + str(len(data)).encode() + b"\0" + data).hexdigest()


def unique_json(data):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result)
            result[key] = value
        return result
    try:
        return json.loads(data, object_pairs_hook=pairs)
    except (ValueError, UnicodeError):
        raise SourceFailure("input") from None


def read_owned(path, bound, *, private=False):
    """No followed final link, external writer, or changed open-file identity."""
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path, "path")
    for ancestor in path.parents:
        info = ancestor.stat()
        require(info.st_uid in {0, os.getuid()} and not info.st_mode & 0o022, "path")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(descriptor, "rb") as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid()
                and before.st_nlink == 1 and not before.st_mode & (0o077 if private else 0o022)
                and 0 < before.st_size <= bound, "input")
        data = stream.read(bound + 1)
        after = os.fstat(stream.fileno())
        fields = lambda x: (x.st_dev, x.st_ino, x.st_mode, x.st_uid, x.st_size, x.st_mtime_ns, x.st_ctime_ns)
        require(len(data) == before.st_size and fields(before) == fields(after), "identity")
        return data


def clean_git_environment(environment):
    # Repository configuration cannot select a credential helper or external
    # process: these commands read raw objects only, never transports or hooks.
    result = {key: value for key, value in environment.items()
              if not key.startswith(("GIT_", "SSH_"))}
    result.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1",
                  GIT_NO_REPLACE_OBJECTS="1", GIT_ATTR_NOSYSTEM="1",
                  GIT_TERMINAL_PROMPT="0", GIT_OPTIONAL_LOCKS="0",
                  GIT_NO_LAZY_FETCH="1", GIT_ALLOW_PROTOCOL="")
    return result


def safe_git(tool, repository):
    require(Path(tool).is_absolute(), "git")
    path = Path(repository)
    require(path.is_absolute() and path.resolve(strict=True) == path, "path")
    for item in (path, *path.parents):
        info = item.stat()
        require(stat.S_ISDIR(info.st_mode) and info.st_uid in {0, os.getuid()}
                and not info.st_mode & 0o022, "path")
    return [tool, "--no-pager", "-c", "core.hooksPath=/dev/null",
            "-c", "core.fsmonitor=false", "-c", "core.attributesFile=/dev/null",
            "-c", "gc.auto=0", "-c", "maintenance.auto=false",
            "-c", "diff.external=", "-c", "core.pager=cat",
            "-C", str(path)]


def source_path(value):
    require(type(value) is str and 0 < len(value.encode("utf-8")) <= 1024
            and not any(ord(c) < 32 or ord(c) == 127 for c in value)
            and "\\" not in value, "path")
    path = PurePosixPath(value)
    require(path.parts and not path.is_absolute() and str(path) == value
            and not any(part in {"", ".", "..", ".git"} for part in path.parts), "path")
    return value


def check_links(entries, contents):
    """Retain authenticated Git symlinks only when their whole graph stays inside."""
    directories = {str(parent) for name in entries for parent in PurePosixPath(name).parents
                   if str(parent) != "."}
    require(not any(str(parent) in entries for name in entries
                    for parent in PurePosixPath(name).parents), "path")
    for name, item in entries.items():
        if item["mode"] != "120000":
            continue
        try:
            target = contents[name].decode("utf-8", "strict")
        except UnicodeError:
            raise SourceFailure("link") from None
        require(0 < len(target.encode()) <= 1024 and not target.startswith("/")
                and "\\" not in target and not any(ord(c) < 32 or ord(c) == 127 for c in target), "link")
        pending = [*PurePosixPath(name).parent.parts, *target.split("/")]
        resolved = []
        followed = set()
        while pending:
            part = pending.pop(0)
            if part in {"", "."}:
                continue
            if part == "..":
                require(resolved, "link")
                resolved.pop()
                continue
            require(part != ".git", "link")
            candidate = "/".join([*resolved, part])
            if candidate in entries and entries[candidate]["mode"] == "120000":
                require(candidate not in followed and len(followed) < 32, "link")
                followed.add(candidate)
                try:
                    next_target = contents[candidate].decode("utf-8", "strict")
                except UnicodeError:
                    raise SourceFailure("link") from None
                require(0 < len(next_target.encode()) <= 1024
                        and not next_target.startswith("/") and "\\" not in next_target
                        and not any(ord(c) < 32 or ord(c) == 127 for c in next_target), "link")
                pending = next_target.split("/") + pending
            else:
                require(not pending or candidate in directories, "link")
                resolved.append(part)
        require("/".join(resolved) in entries.keys() | directories, "link")


def tree_identity(entries):
    root = {}
    for name, item in entries.items():
        node = root
        parts = name.split("/")
        for part in parts[:-1]:
            node = node.setdefault(part, {})
            require(type(node) is dict, "identity")
        require(parts[-1] not in node, "identity")
        node[parts[-1]] = (item["mode"], item["oid"])
    def calculate(node):
        data = bytearray()
        for name, value in sorted(node.items(), key=lambda pair:
                                 (pair[0] + ("/" if type(pair[1]) is dict else "")).encode()):
            mode, oid = ("40000", calculate(value)) if type(value) is dict else value
            data.extend(mode.encode() + b" " + name.encode() + b"\0" + bytes.fromhex(oid))
        return object_id("tree", bytes(data))
    return calculate(root)


class Budget:
    def __init__(self):
        self.entries = self.expanded = self.inputs = 0
        self.commands = 0
        self.path_bytes = 0

    def add(self, *, entries=0, expanded=0, inputs=0, paths=0):
        self.entries += entries
        self.expanded += expanded
        self.inputs += inputs
        self.path_bytes += paths
        require(self.entries <= 500000 and self.expanded <= 2 * 1024**3
                and self.inputs <= MAX_BUNDLE and self.path_bytes <= 8 * 1024**2, "bounds")


class CappedBytes(io.BytesIO):
    def write(self, data):
        require(self.tell() + len(data) <= MAX_ARCHIVE, "bounds")
        return super().write(data)


def tar_add(archive, name, data, mode=0o644, link=None):
    source_path(name)
    info = tarfile.TarInfo(name)
    info.mode, info.uid, info.gid, info.mtime = mode, 0, 0, 0
    if link is not None:
        info.type, info.linkname = tarfile.SYMTYPE, link
    else:
        info.size = len(data)
    archive.addfile(info, None if link is not None else io.BytesIO(data))


def git_export(runner, repository, revision, tool, environment, budget):
    """Authenticate every raw blob and the complete tree, with no Git archive attributes."""
    require(type(revision) is str and OID.fullmatch(revision), "identity")
    command = safe_git(tool, repository)
    env = clean_git_environment(environment)
    def run(stage, args, bound, **kwargs):
        budget.commands += 1
        require(budget.commands <= MAX_GIT_COMMANDS, "bounds")
        return runner.run(stage, [*command, *args], cwd=repository, env=env,
                          timeout=60, output_limit=bound, binary_output=True,
                          stderr_limit=65536, **kwargs)
    commit = run("source-commit", ["cat-file", "commit", revision], 1024 * 1024)
    require(object_id("commit", commit) == revision, "identity")
    first = commit.split(b"\n", 1)[0]
    require(re.fullmatch(rb"tree [0-9a-f]{40}", first), "identity")
    tree = first[5:].decode("ascii")
    listing = run("source-tree", ["ls-tree", "--full-tree", "-r", "-l", "-z", tree], 8 * 1024 * 1024)
    require(listing.endswith(b"\0"), "git")
    entries = {}
    for raw in listing[:-1].split(b"\0"):
        header, name = raw.split(b"\t", 1)
        fields = header.split()
        require(len(fields) == 4, "git")
        mode, kind, oid, size = fields
        if mode == b"160000" or kind == b"commit":
            raise SourceFailure("submodule")
        require(mode in (b"100644", b"100755", b"120000") and kind == b"blob"
                and re.fullmatch(rb"[0-9a-f]{40}", oid)
                and re.fullmatch(rb"[0-9]{1,12}", size), "git")
        path = source_path(name.decode("utf-8", "strict"))
        require(path not in entries and len(entries) < MAX_GIT_ENTRIES
                and int(size) <= MAX_FILE, "bounds")
        entries[path] = {"mode": mode.decode(), "oid": oid.decode(), "bytes": int(size)}
    require(entries and tree_identity(entries) == tree
            and sum(item["bytes"] for item in entries.values()) <= MAX_GIT_CONTENT, "identity")
    budget.add(entries=len(entries), expanded=sum(item["bytes"] for item in entries.values()),
               paths=sum(len(name.encode()) for name in entries))
    contents = {}
    group = []
    expected_size = 0
    def take(group):
        query = "".join(item["oid"] + "\n" for _, item in group).encode("ascii")
        maximum = sum(item["bytes"] + 128 for _, item in group)
        output = run("source-objects", ["cat-file", "--batch"], maximum, stdin_bytes=query)
        cursor = 0
        for path, item in group:
            end = output.find(b"\n", cursor)
            require(end >= cursor, "git")
            expected = (item["oid"] + " blob " + str(item["bytes"])).encode("ascii")
            require(output[cursor:end] == expected, "identity")
            cursor = end + 1
            data = output[cursor:cursor + item["bytes"]]
            cursor += len(data)
            require(len(data) == item["bytes"] and output[cursor:cursor + 1] == b"\n"
                    and object_id("blob", data) == item["oid"], "identity")
            cursor += 1
            contents[path] = data
        require(cursor == len(output), "git")
    for path, item in entries.items():
        if group and (len(group) >= 64 or expected_size + item["bytes"] > 32 * 1024 * 1024):
            take(group)
            group, expected_size = [], 0
        group.append((path, item))
        expected_size += item["bytes"]
    if group:
        take(group)
    check_links(entries, contents)
    sink = CappedBytes()
    with gzip.GzipFile(fileobj=sink, mode="wb", filename="", mtime=0) as zipped:
        with tarfile.open(fileobj=zipped, mode="w|", format=tarfile.PAX_FORMAT) as archive:
            for path, item in sorted(entries.items()):
                link = contents[path].decode() if item["mode"] == "120000" else None
                tar_add(archive, path, contents[path], 0o755 if item["mode"] == "100755" else 0o644, link)
    data = sink.getvalue()
    budget.add(inputs=len(data))
    inventory = {name: {**item, "sha256": digest(contents[name])} for name, item in entries.items()}
    return data, {"commit": revision, "tree": tree, "archive_sha256": digest(data),
                  "files": inventory, "git_attributes_applied": False}, contents


def registry_contents(data, package, budget):
    require(len(data) <= MAX_ARCHIVE and digest(data) == package["checksum"], "identity")
    prefix = package["name"] + "-" + package["version"]
    files, names, expanded = {}, set(), 0
    try:
        # Bound the entire decompressed stream before tarfile parses PAX or GNU
        # metadata. Bounding only yielded members would miss oversized headers.
        with gzip.GzipFile(fileobj=io.BytesIO(data), mode="rb") as zipped:
            unpacked = zipped.read(MAX_GIT_CONTENT + 1)
        require(len(unpacked) <= MAX_GIT_CONTENT, "bounds")
        with tarfile.open(fileobj=io.BytesIO(unpacked), mode="r:") as archive:
            for member in archive:
                raw = member.name.rstrip("/") if member.isdir() else member.name
                name = source_path(raw)
                require(name not in names and name.split("/")[0] == prefix
                        and len(names) < MAX_GIT_ENTRIES, "archive")
                names.add(name)
                require(member.isdir() or member.isfile(), "link")
                require(not member.mode & 0o7000 and member.sparse is None, "archive")
                require(0 <= member.size <= MAX_FILE, "bounds")
                expanded += member.size
                require(expanded <= MAX_GIT_CONTENT, "bounds")
                if member.isfile():
                    relative = name[len(prefix) + 1:]
                    source_path(relative)
                    content = archive.extractfile(member).read(member.size + 1)
                    require(len(content) == member.size, "archive")
                    files[relative] = content
            # Do not silently ignore a second archive or a malformed header
            # after the first end marker. Published source bytes stay exact.
            trailer = unpacked[archive.offset:]
            require(len(unpacked) % 512 == 0 and len(trailer) >= 1024
                    and not any(trailer), "archive")
        require(files and not any(str(parent) in files for name in files
                                  for parent in PurePosixPath(name).parents), "archive")
    except (tarfile.TarError, OSError, EOFError, UnicodeError, ValueError, zlib.error):
        raise SourceFailure("archive") from None
    budget.add(entries=len(names), expanded=len(unpacked), inputs=len(data),
               paths=sum(len(name.encode()) for name in names))
    return files


def toml_file(files, name, *, maximum=256 * 1024):
    require(name in files and len(files[name]) <= maximum, "manifest")
    try:
        return tomllib.loads(files[name].decode("utf-8"))
    except (ValueError, UnicodeError):
        raise SourceFailure("manifest") from None


def package_catalog(files, *, published=False):
    """Only resolve declared workspace/path manifests; do not parse arbitrary test fixtures."""
    pending, found, result = ["Cargo.toml"], set(), {}
    while pending:
        name = pending.pop()
        if name in found:
            continue
        require(len(found) < 512, "bounds")
        found.add(name)
        document = toml_file(files, name)
        parent = PurePosixPath(name).parent
        workspace = document.get("workspace", {})
        require(type(workspace) is dict, "manifest")
        # Registry archives are Cargo's published source form. Their normalized
        # root manifest identifies the package; retained development metadata
        # may refer to unpublished test/workspace members. Every *locked* source
        # is still mandatory below, and the full checked archive is preserved.
        members = [] if published else workspace.get("members", [])
        require(type(members) is list and all(type(x) is str for x in members), "manifest")
        for pattern in members:
            require(type(pattern) is str and not pattern.startswith("/") and ".." not in PurePosixPath(pattern).parts, "manifest")
            match = str(parent / pattern / "Cargo.toml")
            pieces = match.split("/")
            require("**" not in pieces, "manifest")
            selected = [candidate for candidate in files
                        if len(candidate.split("/")) == len(pieces)
                        and all(fnmatch.fnmatchcase(part, pattern)
                                for part, pattern in zip(candidate.split("/"), pieces))]
            require(selected, "missing")
            pending.extend(selected)
        tables = [] if published else [document]
        if not published and type(workspace.get("dependencies")) is dict:
            tables.append({"dependencies": workspace["dependencies"]})
        if not published:
            targets = document.get("target", {})
            require(type(targets) is dict, "manifest")
            tables.extend(targets.values())
        for table in tables:
            require(type(table) is dict, "manifest")
            for group in ("dependencies", "dev-dependencies", "build-dependencies"):
                dependencies = table.get(group, {})
                require(type(dependencies) is dict, "manifest")
                for value in dependencies.values():
                    if type(value) is dict and "path" in value:
                        pending.append(relative_manifest(parent, value["path"], files))
        patches = {} if published else document.get("patch", {})
        require(type(patches) is dict, "manifest")
        for table in patches.values():
            require(type(table) is dict, "manifest")
            for value in table.values():
                if type(value) is dict and "path" in value:
                    pending.append(relative_manifest(parent, value["path"], files))
        package = document.get("package")
        if package is not None:
            require(type(package) is dict, "manifest")
            inherited = {}
            inherited_root = parent
            for ancestor in (parent, *parent.parents):
                path = str(ancestor / "Cargo.toml")
                if path in files:
                    candidate = toml_file(files, path).get("workspace", {}).get("package", {})
                    if candidate:
                        inherited = candidate
                        inherited_root = ancestor
                        break
            def value(field):
                declared = package.get(field)
                if type(declared) is dict:
                    require(declared == {"workspace": True} and field in inherited, "manifest")
                    return inherited[field]
                return declared
            key = (package.get("name"), value("version"))
            require(all(type(x) is str and ATOM.fullmatch(x) for x in key), "manifest")
            require(key not in result, "manifest")
            license_value = value("license")
            require(license_value is None or type(license_value) is str and len(license_value) <= 4096, "manifest")
            candidates = {}
            for ancestor in (parent, *parent.parents):
                for path, data in files.items():
                    if PurePosixPath(path).parent != ancestor:
                        continue
                    if re.fullmatch(r"(?:LICENSE|COPYING|COPYRIGHT|NOTICE|UNLICENSE)(?:[._-].*)?", PurePosixPath(path).name, re.I):
                        require(len(data) <= MAX_FILE, "bounds")
                        candidates[path] = digest(data)
                        require(len(candidates) <= 128, "bounds")
            declared_file = value("license-file")
            if declared_file is not None:
                origin = inherited_root if package.get("license-file") == {"workspace": True} else parent
                declared_path = relative_path(origin, declared_file)
                require(declared_path in files, "missing")
                candidates[declared_path] = digest(files[declared_path])
            links = package.get("links")
            require(links is None or type(links) is str and ATOM.fullmatch(links), "manifest")
            declared_build = package.get("build")
            require(declared_build is None or type(declared_build) in (bool, str), "manifest")
            script = None
            if declared_build is not False:
                candidate = relative_path(parent, declared_build if type(declared_build) is str else "build.rs")
                if candidate in files:
                    script = {"path": candidate, "sha256": digest(files[candidate])}
                else:
                    require(declared_build is None, "missing")
            result[key] = {"manifest": name, "declared_license": license_value,
                           "declared_license_file": declared_file,
                           "license_text_candidates": candidates,
                           "declared_build_interface": {"links": links, "build_script": script,
                               "executed_here": False, "actual_linked_inputs_verified": False,
                               "separately_fetched_inputs_covered": False},
                           "license_obligations_verified": False}
    return result


def relative_path(parent, value):
    require(type(value) is str and value and not value.startswith("/") and "\\" not in value, "path")
    parts = list(parent.parts)
    for part in value.split("/"):
        if part in {"", "."}:
            continue
        if part == "..":
            require(parts, "path")
            parts.pop()
        else:
            parts.append(part)
    return source_path("/".join(parts))


def relative_manifest(parent, value, files):
    path = relative_path(parent, value + "/Cargo.toml" if type(value) is str else value)
    require(path in files, "missing")
    return path


def lock_packages(files):
    lock = toml_file(files, "Cargo.lock", maximum=4 * 1024 * 1024)
    require(lock.get("version") == 4 and type(lock.get("package")) is list
            and 0 < len(lock["package"]) <= 4096, "lock")
    seen = set()
    for package in lock["package"]:
        require(type(package) is dict and all(type(package.get(k)) is str and ATOM.fullmatch(package[k])
                                            for k in ("name", "version")), "lock")
        source = package.get("source")
        require(source is None or type(source) is str and len(source) <= 2048, "lock")
        key = (package["name"], package["version"], source)
        require(key not in seen, "lock")
        seen.add(key)
        if source == REGISTRY:
            require(type(package.get("checksum")) is str and HASH.fullmatch(package["checksum"]), "lock")
        elif source is not None:
            require(re.fullmatch(r"git\+https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\?(?:rev|tag)=[A-Za-z0-9_.-]+#[0-9a-f]{40}", source), "lock")
    return lock["package"]


def input_plan(path, packages):
    plan = unique_json(read_owned(path, 64 * 1024, private=True))
    require(type(plan) is dict and type(plan.get("schema")) is int and plan["schema"] in (1, 2), "input")
    fields = {"schema", "registry_cache", "git_mirrors"} | ({"native_inputs"} if plan["schema"] == 2 else set())
    require(set(plan) == fields and type(plan["registry_cache"]) is str
            and type(plan["git_mirrors"]) is dict, "input")
    if plan["schema"] == 2:
        native = plan["native_inputs"]
        require(type(native) is dict and set(native) == {"coverage", "package_artifact", "package_receipt",
                    "runtime_notices", "pipewire_notices"}
                and native["coverage"] in ("known-components", "complete-native")
                and all(type(native[key]) is str and 0 < len(native[key]) <= 4096
                        for key in native if key != "coverage"), "input")
        # A binary URL/tag/digest cannot prove matching SDK source or waive the
        # compiler/system-source and full relinking/installation requirements.
        require(native["coverage"] == "known-components", "missing")
    expected = {package["source"] for package in packages if package.get("source", "").startswith("git+")}
    require(set(plan["git_mirrors"]) == expected and all(type(x) is str for x in plan["git_mirrors"].values()), "missing")
    return plan



def hash_owned_file(path, bound, *, prefix_bytes=0, private=False):
    """Stream package bytes, retaining only a bounded runtime prefix as data."""
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path, "path")
    require(type(prefix_bytes) is int and 0 <= prefix_bytes <= MAX_RUNTIME, "bounds")
    for ancestor in path.parents:
        info = ancestor.stat()
        require(info.st_uid in {0, os.getuid()} and not info.st_mode & 0o022, "path")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(descriptor, "rb") as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid() and before.st_nlink == 1
                and not before.st_mode & (0o077 if private else 0o022)
                and 0 < before.st_size <= bound, "input")
        hashed, prefix, count = hashlib.sha256(), bytearray(), 0
        while True:
            data = stream.read(65536)
            if not data:
                break
            count += len(data)
            require(count <= bound, "bounds")
            hashed.update(data)
            if len(prefix) < prefix_bytes:
                prefix.extend(data[:prefix_bytes - len(prefix)])
        after = os.fstat(stream.fileno())
        fields = lambda x: (x.st_dev, x.st_ino, x.st_mode, x.st_uid, x.st_size, x.st_mtime_ns, x.st_ctime_ns)
        require(count == before.st_size and fields(before) == fields(after), "identity")
        return count, hashed.hexdigest(), prefix


def source_assignment(files, path, name, *, node=False):
    """Read a top-level literal assignment as AST data; never import recipes."""
    require(path in files and len(files[path]) <= 256 * 1024, "manifest")
    try:
        tree = ast.parse(files[path], filename=path)
        values = [item.value for item in tree.body if isinstance(item, ast.Assign)
                  and len(item.targets) == 1 and isinstance(item.targets[0], ast.Name)
                  and item.targets[0].id == name]
        require(len(values) == 1, "manifest")
        return values[0] if node else ast.literal_eval(values[0])
    except (SyntaxError, ValueError, TypeError, RecursionError):
        raise SourceFailure("manifest") from None


def native_recipe(files):
    base = "packaging/linux/"
    spec = unique_json(files.get(base + "runtime-build.json", b""))
    require(digest(files[base + "runtime-build.json"]) ==
            source_assignment(files, base + "appimage_runtime_build.py", "MANIFEST_SHA256")
            and type(spec) is dict and type(spec.get("schema")) is int and spec["schema"] == 1, "identity")
    require(set(spec.get("archives", {})) == {"runtime", "squashfuse", "musl", "fuse", "zlib", "zstd", "mimalloc"}, "manifest")
    host = unique_json(files.get(base + "runtime-build-host.json", b""))
    require(digest(files[base + "runtime-build-host.json"]) == spec.get("host_manifest_sha256")
            and type(host) is dict and type(host.get("files")) is dict, "identity")
    recipe = "scripts/pipewire_startup_probe.py"
    pipewire = {key: source_assignment(files, recipe, key) for key in
                ("VERSION", "COMMIT", "UPSTREAM_PATCH_SHA256", "PATCHED_SOURCE_SHA256", "CONFIGURE")}
    require(type(pipewire["VERSION"]) is str and type(pipewire["COMMIT"]) is str and OID.fullmatch(pipewire["COMMIT"])
            and all(type(pipewire[key]) is str and HASH.fullmatch(pipewire[key]) for key in
                    ("UPSTREAM_PATCH_SHA256", "PATCHED_SOURCE_SHA256"))
            and type(pipewire["CONFIGURE"]) is tuple
            and all(type(x) is str and len(x) <= 512 for x in pipewire["CONFIGURE"]), "manifest")
    archive_node = source_assignment(files, recipe, "ARCHIVES", node=True)
    require(isinstance(archive_node, ast.Dict) and len(archive_node.keys) == 3, "manifest")
    def maximum(node, depth=0):
        require(depth <= 3, "manifest")
        if isinstance(node, ast.Constant) and type(node.value) is int:
            result = node.value
        elif isinstance(node, ast.BinOp) and isinstance(node.op, ast.Mult):
            result = maximum(node.left, depth + 1) * maximum(node.right, depth + 1)
        else:
            raise SourceFailure("manifest")
        require(0 < result <= MAX_ARCHIVE, "bounds")
        return result
    archives = {}
    for key, value in zip(archive_node.keys, archive_node.values):
        require(isinstance(key, ast.Constant) and type(key.value) is str and key.value not in archives
                and isinstance(value, ast.Tuple) and len(value.elts) == 3, "manifest")
        filename_node, sha_node, maximum_node = value.elts
        if isinstance(filename_node, ast.JoinedStr):
            parts = []
            for part in filename_node.values:
                if isinstance(part, ast.Constant) and type(part.value) is str:
                    parts.append(part.value)
                else:
                    require(isinstance(part, ast.FormattedValue) and isinstance(part.value, ast.Name)
                            and part.value.id == "VERSION" and part.conversion == -1 and part.format_spec is None, "manifest")
                    parts.append(pipewire["VERSION"])
            filename = "".join(parts)
        else:
            require(isinstance(filename_node, ast.Constant) and type(filename_node.value) is str, "manifest")
            filename = filename_node.value
        require(ATOM.fullmatch(filename) and isinstance(sha_node, ast.Constant)
                and type(sha_node.value) is str and HASH.fullmatch(sha_node.value), "manifest")
        archives[key.value] = {"filename": filename, "sha256": sha_node.value,
                               "maximum_bytes": maximum(maximum_node)}
    require(set(archives) == {"dsc", "original", "debian"}, "manifest")
    pipewire["archives"] = archives
    pipewire["licenses"] = source_assignment(files, base + "appimage_client.py", "LICENSES")
    require(type(pipewire["licenses"]) is dict and set(pipewire["licenses"]) == {"LICENSE", "COPYING"}
            and all(type(x) is str and HASH.fullmatch(x) for x in pipewire["licenses"].values()), "manifest")
    return spec, host, pipewire


def literal_recipe_text(files, path, destination):
    """Only the existing write_new(path/name, literal.encode()) source form."""
    require(path in files and len(files[path]) <= 256 * 1024, "manifest")
    try:
        tree = ast.parse(files[path], filename=path)
        matches = []
        for node in ast.walk(tree):
            if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                    and node.func.attr == "write_new" and len(node.args) == 2):
                continue
            target, value = node.args
            if not (isinstance(target, ast.BinOp) and isinstance(target.op, ast.Div)
                    and isinstance(target.right, ast.Constant) and target.right.value == destination):
                continue
            require(isinstance(value, ast.Call) and isinstance(value.func, ast.Attribute)
                    and value.func.attr == "encode" and not value.args and not value.keywords, "manifest")
            text = ast.literal_eval(value.func.value)
            require(type(text) is str and 0 < len(text.encode()) <= 65536, "manifest")
            matches.append(text.encode())
        require(len(matches) == 1, "manifest")
        return matches[0]
    except (SyntaxError, ValueError, TypeError, RecursionError):
        raise SourceFailure("manifest") from None


def fixed_stage(path, expected):
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path, "path")
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and not info.st_mode & 0o022, "path")
    found = set()
    for entry in path.iterdir():
        require(len(found) < 128 and entry.name in expected and entry.name not in found, "missing")
        found.add(entry.name)
    require(found == set(expected), "missing")
    return path


def bounded_single_gzip(data, maximum):
    try:
        decoder = zlib.decompressobj(31)
        unpacked = decoder.decompress(data, maximum + 1)
        require(len(unpacked) <= maximum and decoder.eof and not decoder.unused_data
                and not decoder.unconsumed_tail, "bounds")
        return unpacked
    except zlib.error:
        raise SourceFailure("archive") from None


def runtime_prefix_identity(prefix, runtime, embedded):
    """Normalize only the exact non-code .digest_md5:16 section of this ELF."""
    require(type(runtime) is dict and type(embedded) is dict and len(prefix) >= 64
            and prefix[:7] == b"\x7fELF\x02\x01\x01" and prefix[8:11] == b"AI\x02"
            and runtime.get("static_pie") is True and runtime.get("needed") == []
            and runtime.get("interpreter") is None and runtime.get("size") == len(prefix), "identity")
    require(struct.unpack_from("<HHI", prefix, 16) == (3, 62, 1), "identity")
    shoff = struct.unpack_from("<Q", prefix, 40)[0]
    shsize, count, names = struct.unpack_from("<HHH", prefix, 58)
    require(shsize == 64 and 1 <= count <= 512 and 0 < names < count and shoff + count * 64 <= len(prefix), "identity")
    headers = [struct.unpack_from("<IIQQQQIIQQ", prefix, shoff + i * 64) for i in range(count)]
    string_header = headers[names]
    require(string_header[1] == 3 and string_header[4] + string_header[5] <= len(prefix), "identity")
    strings = prefix[string_header[4]:string_header[4] + string_header[5]]
    slots = []
    for header in headers:
        name, kind, flags, _, offset, size, _, _, _, _ = header
        require(name < len(strings) and (kind == 8 or offset + size <= len(prefix)), "identity")
        end = strings.find(b"\0", name)
        require(end >= 0, "identity")
        if strings[name:end] == b".digest_md5":
            require(kind == 1 and size == 16 and not flags & 4, "identity")
            slots.append([offset, size])
    require(len(slots) == 1 and runtime.get("md5_slot") == slots[0]
            and embedded.get("only_mutable_region") == ".digest_md5:16"
            and embedded.get("code_and_all_other_runtime_bytes_exact") is True
            and embedded.get("runtime_bytes") == len(prefix)
            and embedded.get("embedded_prefix_sha256") == digest(prefix), "identity")
    offset, size = slots[0]
    phoff = struct.unpack_from("<Q", prefix, 32)[0]
    phsize, phcount = struct.unpack_from("<HH", prefix, 54)
    require(offset >= 64 and phsize == 56 and 1 <= phcount <= 128
            and phoff + phcount * 56 <= len(prefix)
            and not (offset < shoff + count * 64 and shoff < offset + size)
            and not (offset < phoff + phcount * 56 and phoff < offset + size), "identity")
    for header in headers:
        section_name = strings[header[0]:strings.find(b"\0", header[0])]
        if header[1] not in (0, 8) and header[5] and section_name != b".digest_md5":
            require(not (offset < header[4] + header[5] and header[4] < offset + size), "identity")
    for index in range(phcount):
        kind, flags, start, _, _, length, _, _ = struct.unpack_from("<IIQQQQQQ", prefix, phoff + index * 56)
        require(start + length <= len(prefix), "identity")
        if kind == 1 and offset < start + length and start < offset + size:
            require(not flags & 1, "identity")
    prefix[offset:offset + size] = bytes(size)
    require(digest(prefix) == runtime.get("sha256") == embedded.get("runtime_sha256"), "identity")


def uapi_headers(data, spec, host):
    unpacked = bounded_single_gzip(data, 2 * 1024 * 1024)
    expected = spec.get("kernel_headers")
    require(type(expected) is dict and 0 < len(expected) <= 32, "manifest")
    seen = set()
    try:
        with tarfile.open(fileobj=io.BytesIO(unpacked), mode="r:") as archive:
            for member in archive:
                name = source_path(member.name)
                require(name in expected and name not in seen and member.isfile() and not member.mode & 0o7000
                        and member.sparse is None and 0 < member.size <= 65536, "archive")
                seen.add(name)
                pin = host["files"].get(expected[name])
                require(type(pin) is dict and member.size == pin.get("bytes"), "identity")
                content = archive.extractfile(member).read(member.size + 1)
                require(len(content) == member.size and digest(content) == pin.get("sha256"), "identity")
            trailer = unpacked[archive.offset:]
            require(len(unpacked) % 512 == 0 and len(trailer) >= 1024 and not any(trailer), "archive")
        require(seen == set(expected), "missing")
    except (tarfile.TarError, OSError, EOFError, UnicodeError, ValueError):
        raise SourceFailure("archive") from None


def runtime_linked_sources(runtime, spec, host):
    expected = {"schema", "runtime", "runtime_revision", "version_label", "sources", "patches", "security_backports",
                "host_inputs", "tool_version_output_sha256", "runtime_object_sha256", "link_map_sha256",
                "malformedtest_returned_zero", "runtime_path_regression_returned_zero", "runtime_path_regression",
                "malformedtest_sha256", "commands", "linked_inputs", "fresh_private_prefix",
                "upstream_runtime_binary_downloaded", "reproducible_build_proof", "distribution_ready"}
    require(set(runtime) == expected and runtime["version_label"] == spec["version_label"]
            and runtime["security_backports"] == spec["security_backports"]
            and runtime["malformedtest_returned_zero"] is True
            and runtime["runtime_path_regression_returned_zero"] is True
            and runtime["runtime_path_regression"] == spec["regressions"]["type2-runtime-path-regression.c"]
            and runtime["reproducible_build_proof"] is False, "identity")
    for field in ("runtime_object_sha256", "link_map_sha256", "malformedtest_sha256"):
        require(type(runtime[field]) is str and HASH.fullmatch(runtime[field]), "identity")
    require(type(runtime["commands"]) is list and 0 < len(runtime["commands"]) <= 256
            and type(runtime["linked_inputs"]) is list and 0 < len(runtime["linked_inputs"]) <= 128, "bounds")
    for command in runtime["commands"]:
        require(type(command) is dict and set(command) == {"stage", "argv", "cwd", "maximum_seconds", "maximum_output"}
                and type(command["stage"]) is str and re.fullmatch(r"[a-z][a-z0-9-]{0,47}", command["stage"])
                and type(command["argv"]) is list and 0 < len(command["argv"]) <= 128
                and all(type(x) is str and 0 < len(x) <= 4096 and not any(c in x for c in "\r\n\0") for x in command["argv"])
                and type(command["cwd"]) is str and (command["cwd"] == "$BUILD" or command["cwd"].startswith("$BUILD/"))
                and type(command["maximum_seconds"]) in (int, float) and 0 < command["maximum_seconds"] <= 1200
                and type(command["maximum_output"]) is int and 0 < command["maximum_output"] <= 8 * 1024 * 1024, "identity")
    libraries = {"libsquashfuse.a": "squashfuse", "libsquashfuse_ll.a": "squashfuse", "libzstd.a": "zstd",
                 "libz.a": "zlib", "libfuse3.a": "fuse", "libmimalloc.a": "mimalloc", "libc.a": "musl"}
    musl_outputs = {"Scrt1.o", "rcrt1.o", "crti.o", "crtn.o", "libm.a", "libpthread.a", "libdl.a",
                    "librt.a", "libresolv.a", "libutil.a", "libxnet.a", "libcrypt.a"}
    tools = runtime["tool_version_output_sha256"]
    require(type(tools) is dict and set(tools) == {"gcc-13", "make", "ar", "ranlib", "strip", "readelf",
                "patch", "meson", "ninja", "autoreconf", "pkg-config"}
            and all(type(value) is str and HASH.fullmatch(value) for value in tools.values()), "identity")
    host_inputs = runtime["host_inputs"]
    require(type(host_inputs) is dict and set(host_inputs) == {"manifest_sha256", "packages", "hashed_inputs",
                "actual_bytes_match_pinned_packages", "base_interpreter_and_coreutils"}
            and host_inputs["manifest_sha256"] == spec["host_manifest_sha256"]
            and host_inputs["packages"] == host["packages"] and host_inputs["hashed_inputs"] == len(host["files"])
            and host_inputs["actual_bytes_match_pinned_packages"] is True
            and host_inputs["base_interpreter_and_coreutils"] == "standard_fresh_ubuntu_host_not_a_bit_reproducibility_claim", "identity")
    bound, seen, present = [], {}, set()
    for entry in runtime["linked_inputs"]:
        require(type(entry) is dict and set(entry) == {"path", "sha256", "bytes"}
                and type(entry["path"]) is str
                and type(entry["sha256"]) is str and HASH.fullmatch(entry["sha256"])
                and type(entry["bytes"]) is int and 0 < entry["bytes"] <= MAX_ARCHIVE, "identity")
        name = entry["path"]
        # The real producer records every linker-map LOAD line. A library may
        # occur more than once; deduplicate only the identical bound identity,
        # retain its count, and reject any conflicting duplicate.
        if name in seen:
            previous = bound[seen[name]]
            require(previous["sha256"] == entry["sha256"] and previous["bytes"] == entry["bytes"], "identity")
            previous["load_occurrences"] += 1
            continue
        seen[name] = len(bound)
        if name == "$BUILD/runtime.o":
            require(entry["sha256"] == runtime["runtime_object_sha256"], "identity")
            source = {"component": "runtime", "source_archive_sha256": spec["archives"]["runtime"]["sha256"]}
        elif name.startswith("$BUILD/prefix/lib/") and name[len("$BUILD/prefix/lib/"):] in libraries:
            library = name[len("$BUILD/prefix/lib/"):]
            present.add(library)
            component = libraries[library]
            source = {"component": component, "source_archive_sha256": spec["archives"][component]["sha256"]}
        elif name.startswith("$BUILD/prefix/lib/") and name[len("$BUILD/prefix/lib/"):] in musl_outputs:
            source = {"component": "musl", "source_archive_sha256": spec["archives"]["musl"]["sha256"]}
        else:
            pin = host["files"].get(name)
            require(type(pin) is dict and pin.get("sha256") == entry["sha256"] and pin.get("bytes") == entry["bytes"], "missing")
            source = {"component": "pinned-host-input", "corresponding_source_verified": False}
        logical = (name[len("$BUILD/prefix/lib/"):] if name.startswith("$BUILD/prefix/lib/") else
                   "runtime.o" if name == "$BUILD/runtime.o" else "pinned-host-" + digest(name.encode()))
        bound.append({"logical_input": logical, "sha256": entry["sha256"], "bytes": entry["bytes"],
                      **source, "load_occurrences": 1, "reproducible_link_proof": False})
    require(present == set(libraries) and "$BUILD/runtime.o" in seen, "missing")
    return bound


def native_material(plan, files, revision, budget):
    try:
        return _native_material(plan, files, revision, budget)
    except SourceFailure:
        raise
    except FileNotFoundError:
        raise SourceFailure("missing") from None
    except (OSError, KeyError, TypeError, AttributeError, ValueError, UnicodeError,
            OverflowError, RecursionError, struct.error):
        raise SourceFailure("identity") from None


def _native_material(plan, files, revision, budget):
    """Assemble the already-shipped native material, bound to one real package.

    Receipts are owned build evidence, not a cryptographic/reproducibility proof.
    No source archives are fetched, extracted on disk, imported, patched or run.
    """
    if "native_inputs" not in plan:
        return {}, {"coverage": "rust-only", "native_sources_complete": False,
                    "actual_native_build_inputs_verified": False}
    selected = plan["native_inputs"]
    require(type(selected) is dict and set(selected) == {"coverage", "package_artifact", "package_receipt",
                "runtime_notices", "pipewire_notices"} and selected["coverage"] == "known-components"
            and all(type(selected[key]) is str and 0 < len(selected[key]) <= 4096
                    for key in selected if key != "coverage"), "missing")
    spec, host, pipewire = native_recipe(files)
    receipt_bytes = read_owned(selected["package_receipt"], MAX_NATIVE_RECEIPT, private=True)
    receipt = unique_json(receipt_bytes)
    require(type(receipt) is dict and type(receipt.get("schema")) is int and receipt["schema"] == 1 and receipt.get("format") == "appimage"
            and receipt.get("source_revision") == revision and receipt.get("unsigned") is True
            and receipt.get("distribution_ready") is False, "identity")
    inventory = receipt.get("inventory")
    require(type(inventory) is dict and type(inventory.get("schema")) is int and inventory["schema"] == 1
            and inventory.get("distribution_ready") is False
            and type(inventory.get("notice_sha256")) is dict
            and type(inventory.get("deployed")) is list and len(inventory["deployed"]) <= 256, "identity")
    runtime = receipt.get("source_built_runtime")
    require(type(runtime) is dict and type(runtime.get("schema")) is int and runtime["schema"] == 1
            and runtime.get("sources") == spec["archives"] and runtime.get("patches") == spec["patches"]
            and runtime.get("runtime_revision") == spec["runtime_revision"]
            and runtime.get("fresh_private_prefix") is True
            and runtime.get("upstream_runtime_binary_downloaded") is False
            and runtime.get("distribution_ready") is False
            and runtime.get("host_inputs", {}).get("manifest_sha256") == spec["host_manifest_sha256"]
            and inventory.get("runtime_sources") == spec["archives"]
            and inventory.get("runtime_source_patches") == spec["patches"], "identity")
    linked_sources = runtime_linked_sources(runtime, spec, host)
    runtime_identity = runtime.get("runtime")
    require(type(runtime_identity) is dict and type(runtime_identity.get("size")) is int
            and 1024 <= runtime_identity["size"] <= MAX_RUNTIME, "identity")
    package_size, package_hash, prefix = hash_owned_file(selected["package_artifact"], MAX_BUNDLE,
                                                        prefix_bytes=runtime_identity["size"])
    require(package_hash == receipt.get("appimage_sha256") and package_size > len(prefix)
            and inventory.get("runtime_input_sha256") == runtime_identity.get("sha256"), "identity")
    runtime_prefix_identity(prefix, runtime_identity, receipt.get("embedded_runtime"))
    del prefix
    expected_runtime = {"SOURCE.json", "RELINKING.txt", "linux-uapi-headers.tar.gz", "runtime-relink.o.gz",
                        "runtime-build.json", "runtime-build-host.json", "appimage_runtime_build.py"}
    for component, pin in spec["archives"].items():
        require(type(pin) is dict and type(pin.get("filename")) is str and ATOM.fullmatch(pin["filename"])
                and type(pin.get("size")) is int and 0 < pin["size"] <= MAX_FILE
                and type(pin.get("sha256")) is str and HASH.fullmatch(pin["sha256"])
                and type(pin.get("licenses")) is dict, "manifest")
        expected_runtime.add(pin["filename"])
        expected_runtime.update(component + "-" + name for name in pin["licenses"])
    expected_runtime.update(spec["patches"])
    expected_runtime.update(spec["regressions"])
    expected_pipewire = {"SOURCE.json", "exported-node-startup.patch", *pipewire["licenses"],
                         *(pin["filename"] for pin in pipewire["archives"].values())}
    material, recorded = {}, {}
    for component, names, doc, directory in (("runtime", expected_runtime, RUNTIME_DOC, selected["runtime_notices"]),
                                            ("pipewire", expected_pipewire, PIPEWIRE_DOC, selected["pipewire_notices"])):
        directory = fixed_stage(directory, names)
        inventory_names = {path[len(doc) + 1:] for path in inventory["notice_sha256"] if path.startswith(doc + "/")}
        require(inventory_names == names, "missing")
        for name in sorted(names):
            source_path(name)
            data = read_owned(directory / name, MAX_FILE)
            require(digest(data) == inventory["notice_sha256"][doc + "/" + name], "identity")
            target = "native/" + component + "/" + name
            require(target not in material, "identity")
            budget.add(entries=1, expanded=len(data), inputs=len(data), paths=len(target.encode()))
            material[target] = data
            recorded[target] = {"sha256": digest(data), "bytes": len(data), "deployed_notice": doc + "/" + name,
                                "included_in_bundle": True}
    def content(component, name):
        return material["native/" + component + "/" + name]
    require(unique_json(content("runtime", "SOURCE.json")) == runtime, "identity")
    for component, pin in spec["archives"].items():
        data = content("runtime", pin["filename"])
        require(len(data) == pin["size"] and digest(data) == pin["sha256"], "identity")
        for name, license_pin in pin["licenses"].items():
            data = content("runtime", component + "-" + name)
            require(len(data) == license_pin["bytes"] and digest(data) == license_pin["sha256"], "identity")
    for group in ("patches", "regressions"):
        for name, pin in spec[group].items():
            data = content("runtime", name)
            require(data == files.get("packaging/linux/runtime-patches/" + name)
                    and len(data) == pin["size"] and digest(data) == pin["sha256"], "identity")
    for name in ("runtime-build.json", "runtime-build-host.json", "appimage_runtime_build.py"):
        require(content("runtime", name) == files.get("packaging/linux/" + name), "identity")
    require(content("runtime", "RELINKING.txt") == literal_recipe_text(files,
                "packaging/linux/appimage_runtime_build.py", "RELINKING.txt"), "identity")
    uapi_headers(content("runtime", "linux-uapi-headers.tar.gz"), spec, host)
    relink = bounded_single_gzip(content("runtime", "runtime-relink.o.gz"), MAX_RUNTIME)
    require(relink[:7] == b"\x7fELF\x02\x01\x01" and len(relink) >= 64
            and struct.unpack_from("<HH", relink, 16) == (1, 62)
            and digest(relink) == runtime.get("runtime_object_sha256"), "identity")
    del relink
    client = unique_json(content("pipewire", "SOURCE.json"))
    require(type(client) is dict and set(client) == {"schema", "source_version", "upstream_commit", "upstream_patch_sha256",
                "patched_source_sha256", "source_archives", "source_client_sha256", "build_id", "needed", "objects",
                "tool_version_output_sha256", "configure", "config_sha256", "recipe_sha256"}
            and type(client.get("schema")) is int and client["schema"] == 1
            and client.get("source_version") == pipewire["VERSION"]
            and client.get("upstream_commit") == pipewire["COMMIT"]
            and client.get("upstream_patch_sha256") == pipewire["UPSTREAM_PATCH_SHA256"]
            and client.get("patched_source_sha256") == pipewire["PATCHED_SOURCE_SHA256"]
            and client.get("source_archives") == {key: pin["sha256"] for key, pin in pipewire["archives"].items()}
            and client.get("configure") == list(pipewire["CONFIGURE"])
            and "variant" not in client, "identity")
    sources = source_assignment(files, "scripts/pipewire_startup_probe.py", "SOURCES")
    require(type(sources) is tuple and type(client["objects"]) is dict
            and set(client["objects"]) == {name + ".c.o" for name in sources}
            and all(type(x) is str and HASH.fullmatch(x) for x in client["objects"].values()), "identity")
    for field in ("source_client_sha256", "config_sha256", "recipe_sha256"):
        require(type(client[field]) is str and HASH.fullmatch(client[field]), "identity")
    require(type(client["build_id"]) is str and OID.fullmatch(client["build_id"])
            and type(client["needed"]) is list and 0 < len(client["needed"]) <= 128
            and all(type(value) is str and ATOM.fullmatch(value) for value in client["needed"])
            and type(client["tool_version_output_sha256"]) is dict
            and set(client["tool_version_output_sha256"]) == {"patch", "meson", "ninja", "cc", "pkg-config"}
            and all(type(value) is str and HASH.fullmatch(value)
                    for value in client["tool_version_output_sha256"].values()), "identity")
    require(digest(content("pipewire", "exported-node-startup.patch")) == pipewire["UPSTREAM_PATCH_SHA256"], "identity")
    for pin in pipewire["archives"].values():
        data = content("pipewire", pin["filename"])
        require(len(data) <= pin["maximum_bytes"] and digest(data) == pin["sha256"], "identity")
    for name, sha in pipewire["licenses"].items():
        require(digest(content("pipewire", name)) == sha, "identity")
    shipped = receipt.get("pipewire")
    require(type(shipped) is dict and shipped.get("source_client_sha256") == client.get("source_client_sha256")
            and shipped.get("build_id") == client.get("build_id") and shipped.get("needed") == client.get("needed"), "identity")
    for kind, relative, source_hash, deployed_hash in (
            ("project", "usr/bin/morpheus-app", receipt.get("native_executable_sha256"), None),
            ("pipewire", "usr/lib/libpipewire-0.3.so.0", client.get("source_client_sha256"), shipped.get("sha256"))):
        matches = [entry for entry in inventory["deployed"] if type(entry) is dict and entry.get("relative_path") == relative]
        require(len(matches) == 1 and matches[0].get("kind") == kind
                and type(source_hash) is str and HASH.fullmatch(source_hash)
                and matches[0].get("source_sha256") == source_hash
                and (deployed_hash is None or matches[0].get("deployed_sha256") == deployed_hash), "identity")
    # The exact authenticated SDK pin and wrapper source are included by the
    # committed workspace. They do not identify full matching WebRTC source or
    # attest which prebuilt archive member the final native link consumed.
    sdk_path = "vendor/webrtc-sys-build/src/pins.rs"
    require(sdk_path in files and len(files[sdk_path]) <= 65536, "missing")
    sdk = re.findall(rb'"linux-x64-release"\s*=>\s*\(\s*([0-9]+),\s*"([0-9a-f]{64})",\s*\)', files[sdk_path])
    require(len(sdk) == 1, "manifest")
    # Raw receipt JSON may contain command text and local build paths. Validate
    # it above but export only its digest/closed bindings, never its raw text.
    # Recipes/patches already in the complete workspace are referenced once.
    for component in ("runtime", "pipewire"):
        target = "native/" + component + "/SOURCE.json"
        del material[target]
        recorded[target]["included_in_bundle"] = False
        recorded[target]["disposition"] = "verified-receipt-hash-only"
    workspace_runtime = {name: "packaging/linux/" + name for name in
                         ("runtime-build.json", "runtime-build-host.json", "appimage_runtime_build.py")}
    workspace_runtime.update({name: "packaging/linux/runtime-patches/" + name
                              for name in (*spec["patches"], *spec["regressions"])})
    for name, workspace_path in workspace_runtime.items():
        target = "native/runtime/" + name
        del material[target]
        recorded[target]["included_in_bundle"] = False
        recorded[target]["workspace_reference"] = {"archive": "workspace.tar.gz", "path": workspace_path}
    return material, {"coverage": "receipt-bound-known-native-components-only", "native_sources_complete": False,
        "actual_native_build_inputs_verified": False, "native_lifecycle_acceptance": False,
        "raw_receipts_included": False, "package_artifact_sha256": package_hash,
        "package_artifact_bytes": package_size, "build_receipt_sha256": digest(receipt_bytes),
        "receipt_is_reproducibility_or_source_completeness_proof": False, "files": recorded,
        "runtime_prefix_bound": True, "runtime_relink_object_bound": True, "uapi_headers_bound": True,
        "runtime_linked_source_inventory": linked_sources,
        "pipewire_source_and_deployed_inventory_bound": True,
        "sdk": {"pin_file": sdk_path, "pin_file_sha256": digest(files[sdk_path]),
                "selected_archive_bytes": int(sdk[0][0]), "selected_archive_sha256": sdk[0][1].decode(),
                "selection_basis": "committed-linux-x64-release-recipe-only",
                "complete_matching_source_verified": False, "actual_linked_input_binding": False},
        "remaining_native_requirements": ["prebuilt-sdk-complete-matching-source-and-linked-object-provenance",
            "compiler-runtime-and-system-package-sources", "complete-relinking-and-installation-information",
            "generated-preferred-source-forms", "license-obligations-and-distribution-approval"]}


class BoundedOutput:
    def __init__(self, stream):
        self.stream, self.count = stream, 0

    def write(self, data):
        require(self.count + len(data) <= MAX_BUNDLE, "bounds")
        count = self.stream.write(data)
        require(count == len(data), "output")
        self.count += count
        return count

    def flush(self):
        self.stream.flush()


def build(runner, root, repository, revision, git_tool, environment, plan_path):
    """Write the selected workspace/lock scope; keep partial native coverage explicit."""
    root, repository = Path(root), Path(repository)
    budget = Budget()
    workspace, source, files = git_export(runner, repository, revision, git_tool, environment, budget)
    packages = lock_packages(files)
    local_catalog = package_catalog(files)
    plan = input_plan(plan_path, packages)
    native_files, native_coverage = native_material(plan, files, source["commit"], budget)
    records, git_records = [], {}
    manifest = {"schema": 1, "source": source, "cargo_lock_sha256": digest(files["Cargo.lock"]),
                "source_scope": ("complete-committed-workspace-all-Cargo.lock-and-receipt-bound-known-native-material"
                                 if native_files else "complete-committed-workspace-and-all-Cargo.lock-packages"),
                "native_sources": native_coverage,
                "cargo_packages": records, "git_sources": git_records,
                "distribution_ready": False, "license_compliance_verified": False,
                "unresolved_release_requirements": list(UNRESOLVED)}
    target = root / BUNDLE_NAME
    created = None
    try:
        with target.open("xb") as stream:
            info = os.fstat(stream.fileno())
            created = (info.st_dev, info.st_ino)
            os.fchmod(stream.fileno(), 0o600)
            with gzip.GzipFile(fileobj=BoundedOutput(stream), mode="wb", filename="", mtime=0) as zipped:
                with tarfile.open(fileobj=zipped, mode="w|", format=tarfile.PAX_FORMAT) as archive:
                    tar_add(archive, "workspace.tar.gz", workspace)
                    # Native material was authenticated against the complete
                    # committed recipes and this exact package's notice/build
                    # inventory before any output was created. Retain admitted
                    # source/archive bytes once; raw receipt text stays private. Never download
                    # or execute a provider program to reconstruct missing data.
                    for name in sorted(native_files):
                        tar_add(archive, name, native_files[name])
                    del native_files
                    for package in packages:
                        if package.get("source") is None:
                            key = (package["name"], package["version"])
                            require(key in local_catalog, "missing")
                            records.append({"name": key[0], "version": key[1], "source": "committed-workspace",
                                            "workspace_archive": "workspace.tar.gz", **local_catalog[key]})
                    del files, workspace, local_catalog
                    for git_source, mirror in sorted(plan["git_mirrors"].items()):
                        revision_pin = git_source.rsplit("#", 1)[1]
                        payload, identity, git_files = git_export(runner, mirror, revision_pin, git_tool, environment, budget)
                        name = "git/" + digest(git_source.encode()) + "-" + revision_pin + ".tar.gz"
                        tar_add(archive, name, payload)
                        catalog = package_catalog(git_files)
                        git_records[git_source] = {"archive": name, **identity}
                        for package in packages:
                            if package.get("source") == git_source:
                                key = (package["name"], package["version"])
                                require(key in catalog, "missing")
                                records.append({"name": key[0], "version": key[1], "source": git_source,
                                                "source_archive": name, **catalog[key]})
                        del payload, git_files, catalog
                    cache = Path(plan["registry_cache"])
                    require(cache.is_absolute() and cache.resolve(strict=True) == cache, "path")
                    for package in packages:
                        if package.get("source") != REGISTRY:
                            continue
                        filename = package["name"] + "-" + package["version"] + ".crate"
                        payload = read_owned(cache / filename, MAX_ARCHIVE)
                        crate_files = registry_contents(payload, package, budget)
                        catalog = package_catalog(crate_files, published=True)
                        key = (package["name"], package["version"])
                        require(key in catalog and catalog[key]["manifest"] == "Cargo.toml", "identity")
                        name = "registry/" + filename
                        tar_add(archive, name, payload)
                        records.append({"name": key[0], "version": key[1], "source": REGISTRY,
                                        "source_archive": name, "archive_sha256": package["checksum"],
                                        "archive_bytes": len(payload), **catalog[key]})
                        del payload, crate_files, catalog
                    require(len(records) == len(packages), "missing")
                    manifest["cargo_packages"] = sorted(records, key=lambda x: (x["name"], x["version"], x["source"]))
                    manifest["bounds_observed"] = vars(budget)
                    raw = json.dumps(manifest, sort_keys=True, indent=2).encode() + b"\n"
                    require(len(raw) <= MAX_MANIFEST, "bounds")
                    tar_add(archive, "MANIFEST.json", raw)
                    tar_add(archive, "BUILDING.txt", (
                        "This bundle preserves the complete committed Morpheus workspace and local patches,\n"
                        "plus every locked published crate and authenticated Git source tree.\n"
                        "Extract workspace.tar.gz into a new directory; Git archives preserve bounded internal\n"
                        "symlinks. Registry archives are byte exact to Cargo.lock checksums. No hooks ran here.\n"
                        "Use the included locked toolchain/manifests and technical build instructions.\n"
                        "This is source evidence, not an offline rebuild guarantee or distribution approval.\n"
                        "MANIFEST.json states the exact scope: legacy plans are Rust/workspace-only.\n"
                        "An explicit known-components native plan additionally binds preserved source material\n"
                        "to the actual package hash, embedded runtime, relink object, UAPI headers and deployed\n"
                        "notice/build inventory. This is a partial assembly, not a claim of complete native source.\n"
                        "Build-script/links declarations are data only; their execution/linking was not proved.\n"
                        "Prebuilt SDK matching source/link inputs, system/compiler sources, generated preferred\n"
                        "forms, full relinking/install information and license obligations remain unresolved.\n"
                    ).encode())
            stream.flush()
            os.fsync(stream.fileno())
        _, bundle_hash, _ = hash_owned_file(target, MAX_BUNDLE, private=True)
        receipt = {"schema": 1, "source_revision": source["commit"], "source_tree": source["tree"],
                   "bundle_sha256": bundle_hash, "manifest_sha256": digest(raw),
                   "registry_packages": sum(x.get("source") == REGISTRY for x in packages),
                   "git_sources": len(git_records), "locked_packages": len(packages),
                   "distribution_ready": False, "unresolved_release_requirements": list(UNRESOLVED)}
        if "native_inputs" in plan:
            # Closed identities only. Raw build receipts and paths stay private;
            # this is partial source assembly, never a completeness assertion.
            receipt["native_sources"] = {key: native_coverage[key] for key in (
                "coverage", "package_artifact_sha256", "build_receipt_sha256",
                "native_sources_complete", "actual_native_build_inputs_verified",
                "raw_receipts_included", "native_lifecycle_acceptance")}
        return validate_receipt(receipt, source["commit"])
    except BaseException:
        if created is not None:
            info = target.lstat()
            require(stat.S_ISREG(info.st_mode) and (info.st_dev, info.st_ino) == created, "identity")
            target.unlink()
        raise


def validate_receipt(receipt, revision, *, native_artifact=None, native_receipt=None):
    """Accept only the producer's closed result, with optional exact native binding."""
    fields = {"schema", "source_revision", "source_tree", "bundle_sha256", "manifest_sha256",
              "registry_packages", "git_sources", "locked_packages", "distribution_ready",
              "unresolved_release_requirements"}
    require(type(receipt) is dict and (set(receipt) == fields
            or set(receipt) == fields | {"native_sources"}), "identity")
    require(type(receipt["schema"]) is int and receipt["schema"] == 1
            and type(revision) is str and OID.fullmatch(revision)
            and receipt["source_revision"] == revision
            and type(receipt["source_tree"]) is str and OID.fullmatch(receipt["source_tree"])
            and receipt["distribution_ready"] is False
            and receipt["unresolved_release_requirements"] == list(UNRESOLVED), "identity")
    for key in ("bundle_sha256", "manifest_sha256"):
        require(type(receipt[key]) is str and HASH.fullmatch(receipt[key]), "identity")
    for key in ("registry_packages", "git_sources", "locked_packages"):
        require(type(receipt[key]) is int and 0 <= receipt[key] <= 4096, "identity")
    if "native_sources" in receipt:
        native = receipt["native_sources"]
        require(type(native) is dict and set(native) == {
            "coverage", "package_artifact_sha256", "build_receipt_sha256",
            "native_sources_complete", "actual_native_build_inputs_verified",
            "raw_receipts_included", "native_lifecycle_acceptance"}
            and native["coverage"] == "receipt-bound-known-native-components-only", "identity")
        for key in ("package_artifact_sha256", "build_receipt_sha256"):
            require(type(native[key]) is str and HASH.fullmatch(native[key]), "identity")
        for key in ("native_sources_complete", "actual_native_build_inputs_verified",
                    "raw_receipts_included", "native_lifecycle_acceptance"):
            require(native[key] is False, "identity")
        if native_artifact is not None:
            require(native["package_artifact_sha256"] == native_artifact, "identity")
        if native_receipt is not None:
            require(native["build_receipt_sha256"] == native_receipt, "identity")
    else:
        require(native_artifact is None and native_receipt is None, "missing")
    return receipt


def output_directory(path, repository):
    path, repository = Path(path), Path(repository)
    require(path.is_absolute() and path.resolve(strict=True) == path
            and path != repository and repository not in path.parents
            and path not in repository.parents, "path")
    for ancestor in path.parents:
        info = ancestor.stat()
        require(info.st_uid in {0, os.getuid()} and not info.st_mode & 0o022, "path")
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
            and not info.st_mode & 0o077 and not any(path.iterdir()), "output")
    return path


def publish(directory, data, receipt):
    """One atomic non-replacing handoff, only after owner cleanup and scanning."""
    require(0 < len(data) <= MAX_BUNDLE and digest(data) == receipt.get("bundle_sha256")
            and receipt.get("distribution_ready") is False, "identity")
    descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    temporary = ".source-bundle-" + os.urandom(16).hex()
    created = published = False
    try:
        info = os.fstat(descriptor)
        require(info.st_uid == os.getuid() and not info.st_mode & 0o077
                and not os.listdir(descriptor), "output")
        file = os.open(temporary, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=descriptor)
        created = True
        with os.fdopen(file, "w+b") as stream:
            require(stream.write(data) == len(data), "output")
            stream.flush()
            os.fsync(stream.fileno())
            stream.seek(0)
            require(digest(stream.read(MAX_BUNDLE + 1)) == receipt["bundle_sha256"], "identity")
            os.link(temporary, BUNDLE_NAME, src_dir_fd=descriptor, dst_dir_fd=descriptor, follow_symlinks=False)
            published = True
            identity = os.fstat(stream.fileno())
        return identity.st_dev, identity.st_ino, identity.st_size
    except BaseException:
        if published:
            os.unlink(BUNDLE_NAME, dir_fd=descriptor)
        raise
    finally:
        try:
            if created:
                try:
                    os.unlink(temporary, dir_fd=descriptor)
                except BaseException:
                    if published:
                        os.unlink(BUNDLE_NAME, dir_fd=descriptor)
                    raise
        finally:
            os.close(descriptor)


def remove_published(directory, identity):
    path = Path(directory) / BUNDLE_NAME
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and (info.st_dev, info.st_ino, info.st_size) == identity, "identity")
    path.unlink()
