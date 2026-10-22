"""An explicit, offline source archive for a reviewed Linux package revision.

This records the complete committed workspace and every Cargo.lock source, not
legal compliance or permission to distribute a binary. Nothing is checked out,
downloaded, imported, built or run from the dependency sources.
"""
from __future__ import annotations

import fnmatch
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
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
            result[key] = {"manifest": name, "declared_license": license_value,
                           "declared_license_file": declared_file,
                           "license_text_candidates": candidates,
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
    require(type(plan) is dict and set(plan) == {"schema", "registry_cache", "git_mirrors"}
            and type(plan["schema"]) is int and plan["schema"] == 1
            and type(plan["registry_cache"]) is str and type(plan["git_mirrors"]) is dict, "input")
    expected = {package["source"] for package in packages if package.get("source", "").startswith("git+")}
    require(set(plan["git_mirrors"]) == expected and all(type(x) is str for x in plan["git_mirrors"].values()), "missing")
    return plan


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
    """Write one complete source bundle or raise; no partial-success manifest."""
    root, repository = Path(root), Path(repository)
    budget = Budget()
    workspace, source, files = git_export(runner, repository, revision, git_tool, environment, budget)
    packages = lock_packages(files)
    local_catalog = package_catalog(files)
    plan = input_plan(plan_path, packages)
    records, git_records = [], {}
    manifest = {"schema": 1, "source": source, "cargo_lock_sha256": digest(files["Cargo.lock"]),
                "source_scope": "complete-committed-workspace-and-all-Cargo.lock-packages",
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
                        "Native/prebuilt SDK inputs, system/compiler sources, generated preferred forms,\n"
                        "relinking/install information and complete license obligations remain unresolved.\n"
                    ).encode())
            stream.flush()
            os.fsync(stream.fileno())
        data = read_owned(target, MAX_BUNDLE, private=True)
        return {"schema": 1, "source_revision": source["commit"], "source_tree": source["tree"],
                "bundle_sha256": digest(data), "manifest_sha256": digest(raw),
                "registry_packages": sum(x.get("source") == REGISTRY for x in packages),
                "git_sources": len(git_records), "locked_packages": len(packages),
                "distribution_ready": False, "unresolved_release_requirements": list(UNRESOLVED)}
    except BaseException:
        if created is not None:
            info = target.lstat()
            require(stat.S_ISREG(info.st_mode) and (info.st_dev, info.st_ino) == created, "identity")
            target.unlink()
        raise


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
