"""Exact DATA-to-source projection for the selected Linux WebRTC producer.

This module never extracts archives, follows links, executes tools, invents Git
metadata, or grants build admission. It consumes the original stagers' complete
regular-file outputs and preserves their separate material receipts.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat

CORE_PLAN = "6f4a0b14fe9d64d179d9e381305c961385dbfb53dff8a79ef211b35c71f8f341"
LINUX_PLAN = "f75cf14baaec10fdc9203626737808d3987cc734923b0bb55eae6218234489a5"
LINUX_OVERLAY = "ce7ab3da14ce9dc1338edcac3080093798bc00f2354294f36d7b3a50eee53a7a"
NATIVE = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
TREE = "fecdf55fdd1e9cafc5531abab7ff14e35a1159f5"
CORE_MOUNTS = {"gn": "buildtools/linux64", "ninja": "third_party/ninja",
               "clang": "third_party/llvm-build/Release+Asserts"}
LINUX_MOUNTS = {"rust": "third_party/rust-toolchain",
                "sysroot-amd64": "build/linux/debian_bullseye_amd64-sysroot",
                "sysroot-i386": "build/linux/debian_bullseye_i386-sysroot",
                "llvmobjdump": "third_party/llvm-build/Release+Asserts"}
MAX_JSON = 128 * 1024 * 1024
MAX_FILES = 1_000_000
MAX_BYTES = 32 * 1024 * 1024 * 1024
HEX = re.compile(r"[0-9a-f]{64}\Z")


class Refused(Exception):
    pass


def need(value):
    if not value:
        raise Refused()


def relative(name):
    # Linux archive names are never interpreted using a Windows path parser.
    # The only backslash spellings accepted by callers come from code-pinned
    # complete manifests; this is not a generic archive extraction policy.
    need(type(name) is str and 0 < len(name) <= 4096 and not name.startswith("/")
         and ":" not in name and all(32 <= ord(c) != 127 for c in name))
    parts = name.split("/")
    need(all(p not in ("", ".", "..") and p.casefold() != ".git" for p in parts))
    return name


def identity(st):
    return (st.st_dev, st.st_ino, st.st_size, st.st_mode, st.st_uid,
            st.st_nlink, st.st_mtime_ns, st.st_ctime_ns)


def file_fact(path, expected=None, mode=None, copy_to=None):
    before = path.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and before.st_uid == os.geteuid())
    if expected is not None:
        need(before.st_size == expected["bytes"] and before.st_size <= MAX_BYTES)
    if mode is not None:
        need(stat.S_IMODE(before.st_mode) == mode)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    output = None
    try:
        need(identity(os.fstat(fd)) == identity(before))
        if copy_to is not None:
            output = os.open(copy_to, os.O_WRONLY | os.O_CREAT | os.O_EXCL |
                             os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        h, size = hashlib.sha256(), 0
        while True:
            block = os.read(fd, 1024 * 1024)
            if not block:
                break
            size += len(block)
            need(size <= before.st_size <= MAX_BYTES)
            h.update(block)
            if output is not None:
                view = memoryview(block)
                while view:
                    count = os.write(output, view)
                    need(count > 0)
                    view = view[count:]
        need(size == before.st_size and identity(before) == identity(os.fstat(fd))
             == identity(path.lstat()))
        fact = {"bytes": size, "sha256": h.hexdigest()}
        if expected is not None:
            need(fact == {k: expected[k] for k in ("bytes", "sha256")})
        if output is not None:
            os.fsync(output)
        return fact
    finally:
        if output is not None:
            os.close(output)
        os.close(fd)


def read_json(path, sha):
    need(type(sha) is str and HEX.fullmatch(sha))
    fact = file_fact(path)
    need(fact["sha256"] == sha and fact["bytes"] <= MAX_JSON)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        raw = bytearray()
        while len(raw) <= fact["bytes"]:
            block = os.read(fd, min(1024 * 1024, fact["bytes"] + 1 - len(raw)))
            if not block:
                break
            raw.extend(block)
        need(len(raw) == fact["bytes"] and hashlib.sha256(raw).hexdigest() == sha
             and identity(before) == identity(os.fstat(fd)) == identity(path.lstat()))
        def pairs(rows):
            result = {}
            for key, value in rows:
                need(key not in result)
                result[key] = value
            return result
        return json.loads(raw, object_pairs_hook=pairs)
    finally:
        os.close(fd)


def write_json(path, value):
    raw = (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()
    need(len(raw) <= MAX_JSON)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb", closefd=False) as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(fd)
    finally:
        os.close(fd)
    return {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}


def topology(root, files, directories, *, copy_to=None):
    need(root.is_absolute() and root == root.resolve(strict=True) and "." in directories
         and 0 < len(files) <= MAX_FILES and len(files) + len(directories) <= MAX_FILES)
    need(sum(row["bytes"] for row in files.values()) <= MAX_BYTES)
    need(not (set(files) & directories))
    for name in set(files) | (directories - {"."}):
        relative(name)
        need(all(str(p) in directories for p in PurePosixPath(name).parents))
    found_files, found_dirs = set(), set()
    def failed(_):
        raise Refused() from None
    for directory, children, names in os.walk(root, followlinks=False, onerror=failed):
        here = Path(directory)
        name = here.relative_to(root).as_posix()
        st = here.lstat()
        need(name in directories and stat.S_ISDIR(st.st_mode) and st.st_uid == os.geteuid()
             and stat.S_IMODE(st.st_mode) == 0o700)
        found_dirs.add(name)
        for child in children:
            p = here / child
            st = p.lstat()
            need(p.relative_to(root).as_posix() in directories and stat.S_ISDIR(st.st_mode)
                 and st.st_uid == os.geteuid() and stat.S_IMODE(st.st_mode) == 0o700)
        for child in names:
            name = (here / child).relative_to(root).as_posix()
            need(name in files)
            found_files.add(name)
    need(found_files == set(files) and found_dirs == directories)
    if copy_to is not None:
        need(not os.path.lexists(copy_to))
        copy_to.mkdir(mode=0o700)
        for name in sorted(directories - {"."}, key=lambda x: (x.count("/"), x)):
            (copy_to / name).mkdir(mode=0o700)
    for name, row in sorted(files.items()):
        destination = copy_to / name if copy_to is not None else None
        file_fact(root / name, row, row["mode"], destination)
        if destination is not None:
            os.chmod(destination, row["mode"], follow_symlinks=False)


def profile(kind, root, materials_sha):
    """Return complete stage topology plus its exact source projection."""
    need(kind in ("core", "linux") and os.name == "posix" and os.uname().sysname == "Linux"
         and os.uname().machine == "x86_64")
    need(root.parent == Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
         and re.fullmatch(r"morpheus-webrtc-tools-[0-9a-f]{16}", root.name))
    manifest = read_json(root / "selected-tool-manifest.json", CORE_PLAN)
    need(manifest["schema"] == 1 and manifest["baseline"] == NATIVE and manifest["tree"] == TREE)
    material = read_json(root / "tool-materials.json", materials_sha)
    need(material["manifest_sha256"] == CORE_PLAN and material["target"] == "Linuxx64"
         and material["baseline"] == NATIVE and material["environment_complete"] is False
         and material["executed_tools"] == []
         and material["permission_model"] == "unix_private_directories_0700_files_0600_or_0700")
    if kind == "linux":
        plan = read_json(root / "selected-linux-build-inputs.json", LINUX_PLAN)
        tools = plan["materials"]
        need(plan["schema"] == "morpheus-native-linux-build-inputs-v1"
             and plan["baseline"] == NATIVE and plan["tree"] == TREE
             and [t["name"] for t in tools] == list(LINUX_MOUNTS)
             and material["schema"] == 2 and material["status"] == "linux_build_input_data_staged"
             and material["linux_inputs_sha256"] == LINUX_PLAN and material["materials"] == tools
             and material["required_linux_materials_not_staged"] == [])
        need(all(material[k] is False for k in ("native_sources_complete", "compiler_cipd_runtime_closure",
                 "build_ready", "distribution_ready", "hooks_executed")))
    else:
        tools = manifest["targets"]["Linuxx64"]
        need([t["name"] for t in tools] == list(CORE_MOUNTS)
             and material["schema"] == 1 and material["status"] == "tool_data_staged")
    files, directories, projected = {}, {"."}, {}
    def entry(name, row, mode):
        relative(name)
        need(name not in files and mode in (0o600, 0o700))
        files[name] = {"path": name, "bytes": row["bytes"], "sha256": row["sha256"], "mode": mode}
        directories.update(str(p) for p in PurePosixPath(name).parents)
    def projection(tool, name, row, mode):
        mount = (CORE_MOUNTS if kind == "core" else LINUX_MOUNTS)[tool["name"]]
        destination = relative(mount + "/" + name)
        need(destination not in projected)
        projected[destination] = {"path": destination, "type": "file", "bytes": row["bytes"],
            "sha256": row["sha256"], "mode": "100755" if mode == 0o700 else "100644",
            "origin": ("tools/" if kind == "core" else "linux-inputs/") + tool["name"] + "/" + name}
    archives = []
    for tool in tools:
        prefix = ("tools/" if kind == "core" else "linux-inputs/") + tool["name"]
        suffix = ".zip" if tool["format"] == "zip" else ".tar.xz"
        entry("archives/" + tool["name"] + suffix, tool, 0o600)
        for member in tool["members"]:
            name = member["path"]
            if member["kind"] == "directory":
                full = prefix + ("/" + name if name else "")
                directories.add(full)
                directories.update(str(p) for p in PurePosixPath(full).parents)
            elif member["kind"] == "regular":
                mode = member["output_mode"]
                entry(prefix + "/" + name, member, mode)
                projection(tool, name, member, mode)
            else:
                need(member["kind"] in ("symlink", "hardlink"))
        for alias in tool["alias_copies"]:
            entry(prefix + "/" + alias["destination"], alias, alias["output_mode"])
            projection(tool, alias["destination"], alias, alias["output_mode"])
        for name in tool.get("alias_directories", []):
            full = prefix + "/" + name
            directories.add(full)
            directories.update(str(p) for p in PurePosixPath(full).parents)
        archives.append({k: tool[k] for k in ("name", "sha256", "bytes", "members", "alias_copies",
                        "omitted_aliases", "omission_evidence")})
    need(material["archives"] == archives)
    for name in ["selected-tool-manifest.json", "tool-materials.json"] + (
            ["selected-linux-build-inputs.json"] if kind == "linux" else []):
        entry(name, file_fact(root / name), 0o600)
    topology(root, files, directories)
    return files, directories, projected


def shared_llvm_nm(previous, incoming):
    """Only the identical tool present in both exact selected LLVM archives.

    Both complete source profiles are verified and retained; coalescing this
    one destination never overwrites it or waives either input inventory.
    """
    expected = {"path": "third_party/llvm-build/Release+Asserts/bin/llvm-nm",
                "type": "file", "bytes": 21311248,
                "sha256": "d58075cc6c8e423ae5f07a6dc464563a45c3738db8d272e9a869898f9c28572f",
                "mode": "100755"}
    return all(previous.get(key) == value and incoming.get(key) == value
               for key, value in expected.items())


def project(base, base_files, base_dirs, checkout_markers, profiles):
    """Append verified material copies to a NEW assembler-owned source root.

    The original Git-only receipts/inventories and instrumented receipt remain
    byte-exact. Callers must already have run the pinned original assembler.
    """
    topology(base, base_files, base_dirs)
    source = base / "checkout/src"
    additions, sources = {}, []
    for kind, root, material_sha in profiles:
        files, dirs, rows = profile(kind, root, material_sha)
        sources.append((root, files, dirs))
        for name, row in rows.items():
            full = "checkout/src/" + name
            folded = ("src/" + name).casefold()
            need(not any(folded == marker.casefold() or folded.startswith(marker.casefold() + "/")
                         or marker.casefold().startswith(folded + "/") for marker in checkout_markers))
            need(full not in base_files and full not in base_dirs)
            if full in additions:
                previous = additions[full][1]
                need(kind == "linux" and shared_llvm_nm(previous, row))
                continue
            additions[full] = (root, row)
    # Exact spelling conflicts are already rejected. Case aliases are confined
    # to the complete pinned sysroot inventories, not arbitrary source input.
    base_folded = {name.casefold() for name in base_files}
    need(all(name.casefold() not in base_folded for name in additions))
    new_dirs = set(base_dirs)
    for name in additions:
        new_dirs.update(str(p) for p in PurePosixPath(name).parents)
    need(not (set(base_files) & new_dirs) and not (set(additions) & new_dirs))
    for name in sorted(new_dirs - base_dirs, key=lambda x: (x.count("/"), x)):
        (base / name).mkdir(mode=0o700)
    overlay = []
    for full, (root, row) in sorted(additions.items()):
        mode = 0o755 if row["mode"] == "100755" else 0o644
        file_fact(root / row["origin"], row, 0o700 if mode == 0o755 else 0o600, base / full)
        os.chmod(base / full, mode, follow_symlinks=False)
        base_files[full] = {"path": full, "bytes": row["bytes"], "sha256": row["sha256"], "mode": mode}
        overlay.append({k: row[k] for k in ("path", "type", "bytes", "sha256", "mode")})
    topology(base, base_files, new_dirs)
    for root, files, dirs in sources:
        topology(root, files, dirs)
    return sorted(overlay, key=lambda r: r["path"]), base_files, new_dirs
