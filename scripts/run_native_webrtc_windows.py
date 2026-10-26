#!/usr/bin/env python3
"""Windows x64 native source producer. Exact DATA first; native work needs admission.

No upstream .cmd, DEPS, hook, installer or version resolver is executed by staging.
Windows host SDK/MSVC are observed/validated on the hosted runner, never packaged.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import http.client
import json
import lzma
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import ssl
import stat
import sys
import struct
import tarfile
import time
import urllib.parse
import types
import zipfile

REPOSITORY = "brk275/Morpheus"
NATIVE = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
TREE = "fecdf55fdd1e9cafc5531abab7ff14e35a1159f5"
SECURITY = "a9f2220dfb0e527690ace518911e3e92c12124ab8c37e6cb391a47c89f19c080"
CORE = "6f4a0b14fe9d64d179d9e381305c961385dbfb53dff8a79ef211b35c71f8f341"
HELPERS = {
    "python_stager.py": "4de9a016a988535b7576f2e0d7561afa290529dae7b1eb267bb8382956d49d02",
    "git_source_stager.py": "5debe4d363c339de898b965dee15660867b9a0bc4a96380f434bf19deab20c7b",
}
RECIPE = "08830749810e79b9977ac4bdb39b48059e0b837305873e486747812240a54c8b"
HEX = re.compile(r"[0-9a-f]{64}\Z")
MAX_JSON = 128 * 1024 * 1024
MAX_FILE = 64 * 1024 ** 3
MAX_FILES = 1_000_000
MAX_TOTAL = 256 * 1024 ** 3


# Original Windows Python ZIPs contain7literal internal-space paths. Only
# these full immutable archive/source/member identities use the ZIP path mode.
# Three original universal CIPD instances use SHA1 identity. Their full
# independent archive SHA256 remains mandatory; no equivalent lookup is guessed.
WINDOWS_CIPD_SHA1 = {
    "wheels-a93ae92281633f3c": {
        "name": "wheels-a93ae92281633f3c",
        "format": "zip",
        "bytes": 18131,
        "sha256": "cb6fb4cb7adcefe636c6568059d428cd5c5c23181060594313e1085b35173ba8",
        "source": {
            "kind": "cipd",
            "package": "infra/python/wheels/funcsigs-py2_py3",
            "version": "version:1.0.2"
        },
        "instance_sha1": "647d7235398bf27d91f882879db00e029aee9a08"
    },
    "wheels-5b08a8c609e85acc": {
        "name": "wheels-5b08a8c609e85acc",
        "format": "zip",
        "bytes": 23504,
        "sha256": "77027fb33b5933b7801613c8ea6999552969fe1882de702d434927c59fee550e",
        "source": {
            "kind": "cipd",
            "package": "infra/python/wheels/packaging-py2_py3",
            "version": "version:16.8"
        },
        "instance_sha1": "fe6a85cf07fb8b9fdfd97ed5ec171983dc4ada97"
    },
    "wheels-321750c49ce19b7c": {
        "name": "wheels-321750c49ce19b7c",
        "format": "zip",
        "bytes": 10762,
        "sha256": "4a23290e689d8df1695cc54758e9c3c7e969a820c25396043ce6b893816ebb33",
        "source": {
            "kind": "cipd",
            "package": "infra/python/wheels/six-py2_py3",
            "version": "version:1.10.0"
        },
        "instance_sha1": "e00dc04ad5029579cffc146324c365377f8bcd1d"
    }
}

WINDOWS_PYTHON_ZIPS = {
    "vpython": {
        "name": "vpython",
        "format": "zip",
        "bytes": 49390243,
        "sha256": "4de8953e0721bd13e614f1f6ee576ef3e0f019170b01b2f6bb680af906c6cefe",
        "source": {
            "kind": "cipd",
            "package": "infra/tools/luci/vpython3/windows-amd64",
            "version": "git_revision:ad0c4b622c7d4cee870f57eed8170c059ca42ed3"
        },
        "member_count": 3772,
        "members_sha256": "37b9118afea858976fb67a5a6116ddd61d69d2f900fcb8e63ad51a225e402712"
    },
    "cpython": {
        "name": "cpython",
        "format": "zip",
        "bytes": 19711639,
        "sha256": "11fc068d726c84eae9bf43ec1bb56a435febdadbac24e6a80a0d8b8daac19d50",
        "source": {
            "kind": "cipd",
            "package": "infra/3pp/tools/cpython3/windows-amd64",
            "version": "version:2@3.11.8.chromium.35"
        },
        "member_count": 1907,
        "members_sha256": "d088daa8c3dc94d5605ac32e0b3d832e7fc69d0c6107081759a845a7e130258d"
    }
}

# Exact Windows Rust DATA from the pinned tools/rust/update_rust.py caller.
# This is not the Linux rust material, a version resolver or execution authority.
WINDOWS_RUST = {
    "name": "rust", "format": "tar.xz", "bytes": 197809928,
    "sha256": "8cd2f0c4d312c0a7c127a8e6adf0a73ceecad316997e925fbebc1d855e95f6ba",
    "source": {"kind": "gcs",
        "object": "Win/rust-toolchain-11339a0ef5ed586bb7ea4f85a9b7287880caac3a-1-llvmorg-22-init-14273-gea10026b.tar.xz",
        "generation": 1762971372158285},
}
WINDOWS_RUST_INVENTORY = "6a3d37c5ac6be6a5492ee4a3869039e0c51d4c8527559a22ef747de77e39bdbd"
WINDOWS_RUST_PDB = {
    "path": "bin/rustc_driver-3a0942c7db1abfe9.pdb",
    "raw_path": "bin/rustc_driver-3a0942c7db1abfe9.pdb", "kind": "regular",
    "bytes": 556744704, "mode": 0o666, "output_mode": 0o600,
    "sha256": "e46612a1d55a7f1b36a74a33ff338a578d7ebc1fa921c24d8a149ec39a1d4a10",
}

class Refused(Exception):
    pass


def need(value):
    if not value:
        raise Refused()


def unique(pairs):
    result = {}
    for key, value in pairs:
        need(key not in result)
        result[key] = value
    return result


def relative(value):
    need(type(value) is str and 0 < len(value) <= 2048 and "\\" not in value and ":" not in value)
    p = PurePosixPath(value)
    need(not p.is_absolute() and p.as_posix() == value and len(p.parts) <= 64)
    for part in p.parts:
        need(part not in (".", "..") and part.casefold() != ".git" and not part.endswith((".", " "))
             and not any(ord(c) < 32 or ord(c) == 127 or c in '<>"|?*' for c in part)
             and re.fullmatch(r"(?i)(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?", part) is None)
    return value


def path(value):
    need(type(value) is str and 0 < len(value) <= 4096)
    p = Path(value)
    need(p.is_absolute() and re.fullmatch(r"[A-Za-z]:", p.drive) and not str(p).startswith("\\\\"))
    here = Path(p.anchor)
    for part in p.parts[1:]:
        relative(part)
        here /= part
        info = here.lstat()
        need(not info.st_file_attributes & 0x400)
    return p


def fact(p, limit=MAX_FILE):
    p = path(str(p))
    before = p.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and 0 <= before.st_size <= limit)
    total, digest = 0, hashlib.sha256()
    with p.open("rb") as f:
        while block := f.read(1024 * 1024):
            total += len(block)
            need(total <= limit)
            digest.update(block)
        after = os.fstat(f.fileno())
    named = p.lstat()
    identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns, s.st_nlink)
    need(total == before.st_size and identity(before) == identity(after) == identity(named))
    return {"bytes": total, "sha256": digest.hexdigest()}


def read(p, limit=MAX_JSON):
    expected = fact(p, limit)
    with p.open("rb") as f:
        raw = f.read(limit + 1)
    need(len(raw) == expected["bytes"] and hashlib.sha256(raw).hexdigest() == expected["sha256"])
    return raw


def load(p, expected, limit=MAX_JSON):
    need(type(expected) is str and HEX.fullmatch(expected))
    raw = read(p, limit)
    need(hashlib.sha256(raw).hexdigest() == expected)
    return json.loads(raw, object_pairs_hook=unique)


def fixed_module(name, expected):
    # Only literal in-repository helpers, never a path/code chosen by an input.
    need(name in HELPERS or name == "build_native_webrtc_windows.py")
    script = Path(__file__).resolve().parent
    p = script / (name if name == "build_native_webrtc_windows.py" else "native_webrtc/" + name)
    raw = read(p, 256 * 1024)
    need(hashlib.sha256(raw).hexdigest() == expected)
    module = types.ModuleType("morpheus_windows_" + name.replace(".", "_"))
    module.__file__ = str(p)
    sys.modules[module.__name__] = module
    exec(compile(raw, str(p), "exec"), module.__dict__)
    return module


def invoke(module, args):
    original = sys.argv
    sys.argv = [module.__file__, *args]
    try:
        need(module.main() in (0, None))
    finally:
        sys.argv = original


def repository_public():
    token = os.environ.get("NATIVE_READ_TOKEN", "")
    need(bool(token))
    c = http.client.HTTPSConnection("api.github.com", 443, timeout=30, context=ssl.create_default_context())
    try:
        c.request("GET", "/repos/" + REPOSITORY, headers={"Authorization": "Bearer " + token,
            "Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "Morpheus-native-windows-producer", "Accept-Encoding": "identity"})
        r = c.getresponse()
        need(r.status == 200 and r.getheader("Location") is None
             and r.getheader("Content-Encoding") in (None, "identity"))
        raw = r.read(1024 * 1024 + 1)
        need(len(raw) <= 1024 * 1024)
        value = json.loads(raw, object_pairs_hook=unique)
        need(value["full_name"] == REPOSITORY and value["private"] is False and value["default_branch"] == "main")
    finally:
        c.close()


def environment_admitted(os_name, env):
    return (os_name == "nt" and env.get("GITHUB_ACTIONS") == "true"
        and env.get("RUNNER_ENVIRONMENT") == "github-hosted"
        and env.get("RUNNER_OS") == "Windows" and env.get("RUNNER_ARCH") == "X64"
        and env.get("GITHUB_REPOSITORY") == REPOSITORY
        and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
        and env.get("GITHUB_REF") == "refs/heads/main"
        and env.get("GITHUB_ACTOR") == "brk275" and env.get("GITHUB_TRIGGERING_ACTOR") == "brk275")


def runner():
    need(environment_admitted(os.name, os.environ))
    need(re.fullmatch(r"[1-9][0-9]{0,19}", os.environ.get("GITHUB_RUN_ID", ""))
         and re.fullmatch(r"[1-9][0-9]{0,5}", os.environ.get("GITHUB_RUN_ATTEMPT", "")))
    repository_public()
    return path(os.environ["RUNNER_TEMP"]), path(os.environ["GITHUB_WORKSPACE"])


def tree_files(root):
    # Enumeration failures propagate; no os.walk default error swallowing.
    todo, files, folded, total, entries = [(root, 0)], [], set(), 0, 0
    while todo:
        parent, depth = todo.pop()
        need(depth <= 64)
        with os.scandir(parent) as listing:
            for entry in listing:
                entries += 1
                need(entries <= MAX_FILES)
                info = entry.stat(follow_symlinks=False)
                need(not info.st_file_attributes & 0x400)
                name = relative(Path(entry.path).relative_to(root).as_posix())
                need(name.casefold() not in folded)
                folded.add(name.casefold())
                if stat.S_ISDIR(info.st_mode):
                    todo.append((Path(entry.path), depth + 1))
                else:
                    value = fact(Path(entry.path))
                    total += value["bytes"]
                    need(total <= MAX_TOTAL)
                    files.append({"path": name, **value})
    return sorted(files, key=lambda row: row["path"])


def bound_asset(workspace, record):
    need(type(record) is dict and set(record) == {"path", "bytes", "sha256"})
    name = relative(record["path"])
    need(name.startswith("native/webrtc/") and type(record["bytes"]) is int and 0 < record["bytes"] <= MAX_JSON
         and type(record["sha256"]) is str and HEX.fullmatch(record["sha256"]))
    p = path(str(workspace / name))
    need(fact(p, MAX_JSON) == {k: record[k] for k in ("bytes", "sha256")})
    return p, load(p, record["sha256"])


def write_json(stage, p, value):
    raw = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
    need(len(raw) <= MAX_JSON)
    stage.bytes(p, raw)
    return {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}


def copy_file(stage, source, destination, expected):
    need(fact(source) == expected)
    digest, total = hashlib.sha256(), 0
    with source.open("rb") as src, stage.create(destination) as dst:
        while block := src.read(1024 * 1024):
            total += len(block)
            need(total <= expected["bytes"])
            digest.update(block)
            dst.write(block)
        need(total == expected["bytes"] and digest.hexdigest() == expected["sha256"])
        dst.flush()
        os.fsync(dst.fileno())
    need(fact(destination) == expected)


def core_stage(stager, temp, workspace, plan, control):
    core, value = bound_asset(workspace, plan["core_manifest"])
    need(plan["core_manifest"]["sha256"] == CORE and value["baseline"] == NATIVE and value["tree"] == TREE
         and set(row["name"] for row in value["targets"]["Windowsx64"]) == {"gn", "ninja", "clang"})
    key = hashlib.sha256((os.environ["GITHUB_RUN_ID"] + ":" + os.environ["GITHUB_RUN_ATTEMPT"] + ":windows-core").encode()).hexdigest()[:16]
    output = temp / ("morpheus-webrtc-tools-" + key)
    admission = {"schema": 1, "purpose": "stage_verified_tool_data_only", "manifest_sha256": CORE,
        "target": "Windowsx64", "root": str(output), "public_repository": True,
        "standard_hosted_runner": True, "repository": REPOSITORY,
        "accept_exact_mac_dangling_alias_omissions": False,
        **{k: os.environ[k] for k in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB")}}
    ref = write_json(control, control.root / "core-stage-admission.json", admission)
    invoke(stager, ["--target", "Windowsx64", "--manifest", str(core), "--manifest-sha256", CORE,
        "--admission", str(control.root / "core-stage-admission.json"), "--admission-sha256", ref["sha256"], "--root", str(output)])
    return output


def source_stage(stager, temp, workspace, plan, control):
    # A separately reviewed Windows selection is mandatory. Never mutate the
    # Linux plan's platform flags and pretend that is a reviewed Windows closure.
    _, profile = bound_asset(workspace, plan["source_profile"])
    need(set(profile) == {"schema", "target", "source_plan", "patch_manifest", "security_plan", "patch_files", "host_git"}
         and profile["schema"] == "morpheus-windows-source-stage-v1" and profile["target"] == "win-x64")
    _, original = bound_asset(workspace, profile["source_plan"])
    _, patch = bound_asset(workspace, profile["patch_manifest"])
    security, security_value = bound_asset(workspace, profile["security_plan"])
    need(profile["security_plan"]["sha256"] == SECURITY
         and original["schema"] == "morpheus-native-webrtc-sources-v2" and original["target"] == "win-x64"
         and security_value["baseline_commit"] == NATIVE and security_value["baseline_tree"] == TREE)
    need(type(profile["patch_files"]) is list and 13 <= len(profile["patch_files"]) <= 32)
    # Keep the original protected-temp input boundary of the pinned exporter.
    # Copy exact authenticated DATA; only path fields relocate, never the bytes.
    assets = control.root / "source-inputs"
    control.mkdir(assets)
    patch_root = assets / "patches"
    control.mkdir(patch_root)
    security_copy = assets / "root-security-source-plan.json"
    copy_file(control, security, security_copy,
              {k: profile["security_plan"][k] for k in ("bytes", "sha256")})
    patch_assets = {}
    for item in profile["patch_files"]:
        file, _ = bound_asset_raw(workspace, item)
        need(file.name not in patch_assets)
        destination = patch_root / file.name
        copy_file(control, file, destination, {k: item[k] for k in ("bytes", "sha256")})
        patch_assets[file.name] = (destination, item)
    relocated = copy.deepcopy(patch)
    for row in relocated["patches"]:
        file, item = patch_assets[PurePosixPath(row["file"]).name]
        need({k: row[k] for k in ("bytes", "sha256")} == {k: item[k] for k in ("bytes", "sha256")})
        row["file"] = str(file)
    patch_path = control.root / "patch-manifest.json"
    patch_fact = write_json(control, patch_path, relocated)
    git = profile["host_git"]
    need(set(git) == {"path", "bytes", "sha256", "version"} and Path(git["path"]).name.lower() == "git.exe"
         and fact(path(git["path"])) == {k: git[k] for k in ("bytes", "sha256")})
    need(type(git["version"]) is str and 0 < len(git["version"]) <= 128)
    selected = copy.deepcopy(original)
    need(all("UNBOUND" in str(v) for v in selected["git"].values() if v is not None))
    selected["git"] = git
    selected["patch_manifest"] = {"path": str(patch_path), "sha256": patch_fact["sha256"]}
    selected["security_plan"] = {"path": str(security_copy), "sha256": SECURITY}
    selected_path = control.root / "source-plan.json"
    selected_fact = write_json(control, selected_path, selected)
    output = control.root / "source-provision"
    helper = fixed_module("git_source_stager.py", HELPERS["git_source_stager.py"])
    invoke(helper, ["--plan", str(selected_path), "--plan-sha256", selected_fact["sha256"], "--output-root", str(output)])
    return output


def bound_asset_raw(workspace, record):
    need(type(record) is dict and set(record) == {"path", "bytes", "sha256"})
    name = relative(record["path"])
    need(name.startswith("native/webrtc/") and type(record["bytes"]) is int and 0 < record["bytes"] <= MAX_JSON
         and type(record["sha256"]) is str and HEX.fullmatch(record["sha256"]))
    p = path(str(workspace / name))
    need(fact(p, MAX_JSON) == {k: record[k] for k in ("bytes", "sha256")})
    return p, record


def register_parent(owner, parent):
    # Only descendants of the original protected private control root. Reuse
    # the source export's genuine directories without deleting/recreating them.
    need(parent.is_relative_to(owner.root))
    here = owner.root
    owner.check(here)
    for part in parent.relative_to(owner.root).parts:
        here /= part
        if here not in owner.identities:
            if os.path.lexists(here):
                need(stat.S_ISDIR(path(str(here)).lstat().st_mode))
                owner.windows.hold(here)
                info = here.lstat()
                owner.identities[here] = (info.st_dev, info.st_ino)
            else:
                owner.mkdir(here)
        owner.check(here)


def project(owner, source, core, core_manifest, runtime, runtime_profile):
    # Exact additive source overlay only. No original file, gitlink scope or
    # other tool material can be silently overwritten or coalesced.
    src = source / "checkout/src"
    original = {row["path"].casefold() for row in tree_files(src)}
    markers = load_unbound(source / "materials/gitlinks.json")
    roots = {row["exported_path"].removeprefix("src/").casefold() for row in markers
             if row["exported_path"].startswith("src/")}
    out = []
    def add(origin, name, value, output_mode):
        name = relative(name)
        folded = name.casefold()
        need(folded not in original and all(not (folded == root or folded.startswith(root + "/")
            or root.startswith(folded + "/")) for root in roots))
        original.add(folded)
        destination = src / name
        register_parent(owner, destination.parent)
        copy_file(owner, origin, destination, value)
        out.append({"path": name, "type": "file",
                    "mode": "100755" if output_mode & 0o111 else "100644", **value})
    for tool in core_manifest["targets"]["Windowsx64"]:
        mount = {"gn": "buildtools/win", "ninja": "third_party/ninja",
                 "clang": "third_party/llvm-build/Release+Asserts"}[tool["name"]]
        members = [row for row in tool["members"] if row["kind"] == "regular"]
        need(not tool["alias_copies"] and not tool["omitted_aliases"])
        # GN's CIPD manifest is retained in the tool DATA root, not a source
        # header. The exact executables and complete Clang/Ninja license/data
        # cohorts are projected, with every archive remaining separately held.
        if tool["name"] == "gn":
            members = [row for row in members if row["path"] == "gn.exe"]
            need(len(members) == 1)
        for row in members:
            add(core / "tools" / tool["name"] / row["path"], mount + "/" + row["path"],
                {k: row[k] for k in ("bytes", "sha256")}, row["output_mode"])
    if runtime_profile is not None:
        for item in runtime_profile["materials"]:
            if item["source_mount"] is None:
                continue
            need(item["source_mount"] == "third_party/rust-toolchain")
            for row in item["tool"]["members"]:
                if row["kind"] == "regular":
                    add(runtime / "runtime" / item["role"] / row["path"],
                        item["source_mount"] + "/" + row["path"], {k: row[k] for k in ("bytes", "sha256")}, row["output_mode"])
    overlay = source / "materials/windows-source-overlay.json"
    register_parent(owner, overlay.parent)
    write_json(owner, overlay, sorted(out, key=lambda row: row["path"]))
    return overlay


def load_unbound(p):
    # This metadata was emitted by the exact retained source exporter during
    # this invocation. The final complete inventory binds its actual bytes.
    return json.loads(read(p), object_pairs_hook=unique)


def windows_rust_identity(tool):
    need(all(tool.get(key) == value for key, value in WINDOWS_RUST.items())
         and tool.get("alias_copies") == [] and tool.get("omitted_aliases") == []
         and tool.get("omission_evidence") is None)


def windows_rust_member_limit(stager, tool, row):
    windows_rust_identity(tool)
    if type(row.get("bytes")) is int and row["bytes"] > stager.MAX_FILE:
        # No manifest-selected cap: precisely this original PDB in this archive.
        need(row == WINDOWS_RUST_PDB)
        return WINDOWS_RUST_PDB["bytes"]
    return stager.MAX_FILE


def windows_rust_members(stager, tool):
    windows_rust_identity(tool)
    members = tool["members"]
    need(type(members) is list and len(members) == 4866)
    ordinary, table, expanded = [], {}, 0
    for row in members:
        need(type(row) is dict and row.get("kind") in ("regular", "directory"))
        keys = {"path", "raw_path", "kind", "bytes", "mode"}
        if row["kind"] == "regular":
            keys |= {"sha256", "output_mode"}
        need(set(row) == keys)
        name = stager.rel(row["path"])
        need(name not in table and row["raw_path"] == name
             and row["kind"] in ("regular", "directory"))
        stager.bounded_int(row["bytes"], 0, windows_rust_member_limit(stager, tool, row))
        table[name] = row
        expanded += row["bytes"]
        if row != WINDOWS_RUST_PDB:
            ordinary.append(row)
    # Preserve the generic spelling/case/parent/mode/hash/alias checks and its
    # default 512MiB limit for every other original archive member.
    stager.check_members(dict(tool, members=ordinary))
    # Serialize only this bounded, typed inventory, not arbitrary profile data.
    raw = (json.dumps(members, sort_keys=True, separators=(",", ":")) + "\n").encode()
    need(hashlib.sha256(raw).hexdigest() == WINDOWS_RUST_INVENTORY)
    need(table.get(WINDOWS_RUST_PDB["path"]) == WINDOWS_RUST_PDB
         and expanded <= stager.MAX_EXPANDED)
    return table


def windows_rust_download(stager, stage, tool, archive):
    windows_rust_identity(tool)
    # Generic download selects bucket for name=rust; bind the actual original
    # fixed bucket without rewriting the profile or borrowing Linux's pin.
    request = dict(tool, source=dict(tool["source"], bucket="chromium-browser-clang"))
    stager.download(stage, request, archive)


def windows_rust_write_member(stager, stage, destination, stream, tool, row):
    stager.bounded_int(row["bytes"], 0, windows_rust_member_limit(stager, tool, row))
    total, digest = 0, hashlib.sha256()
    with stage.create(destination) as out:
        while total < row["bytes"]:
            block = stream.read(min(1024 * 1024, row["bytes"] - total))
            need(bool(block))
            total += len(block)
            need(total <= row["bytes"])
            digest.update(block)
            out.write(block)
        need(stream.read(1) == b"" and digest.hexdigest() == row["sha256"])
        out.flush()
        os.fsync(out.fileno())
    os.chmod(destination, row["output_mode"])
    # fact streams a stable non-reparse, single-link original file. Unlike the
    # generic snapshot this never allocates the entire large PDB in memory.
    need(fact(destination, row["bytes"]) == {key: row[key] for key in ("bytes", "sha256")})


def windows_rust_extract(stager, stage, tool, archive, output):
    table = windows_rust_members(stager, tool)
    tar_path = archive.with_suffix(".tar")
    decoder = lzma.LZMADecompressor(format=lzma.FORMAT_XZ, memlimit=256 * 1024 * 1024)
    total = 0
    with archive.open("rb") as source, stage.create(tar_path) as out:
        while not decoder.eof:
            block = source.read(1024 * 1024) if decoder.needs_input else b""
            need(bool(block) or not decoder.needs_input)
            data = decoder.decompress(block, max_length=1024 * 1024)
            total += len(data)
            need(total <= stager.MAX_EXPANDED + 32 * 1024 * 1024)
            out.write(data)
        need(decoder.unused_data == b"" and source.read(1) == b"")
    seen = set()
    with tarfile.open(tar_path, "r:") as source:
        for member in source:
            need(len(seen) < stager.MAX_MEMBERS and (member.isfile() or member.isdir()))
            name = stager.rel(member.name.rstrip("/") if member.isdir() else member.name)
            need(name not in seen and name in table)
            seen.add(name)
            row = table[name]
            kind = "regular" if member.isfile() else "directory"
            need((member.name, kind, member.size, member.mode) ==
                 (row["raw_path"], row["kind"], row["bytes"], row["mode"]))
            if kind == "directory":
                stage.parents(output / name / "_directory_only")
            else:
                with source.extractfile(member) as stream:
                    windows_rust_write_member(stager, stage, output / name, stream, tool, row)
    need(seen == set(table))
    tar_path.unlink()
    stage.files.remove(tar_path)


def windows_python_zip_members(stager, tool):
    pin = WINDOWS_PYTHON_ZIPS.get(tool.get("name"))
    need(pin is not None and all(tool.get(key) == pin[key]
         for key in ("name", "format", "bytes", "sha256", "source"))
         and tool["alias_copies"] == [] and tool["omitted_aliases"] == []
         and tool["omission_evidence"] is None)
    members = tool["members"]
    need(type(members) is list and len(members) == pin["member_count"] <= stager.MAX_MEMBERS)
    table, ordinary, prefixes = {}, [], {}
    folded, expanded = set(), 0
    for row in members:
        need(type(row) is dict and set(row) == {"path", "kind", "bytes", "mode", "sha256", "output_mode"}
             and row["kind"] == "regular")
        name = stager.rel(row["path"], internal_spaces=True)
        need(name not in table and name.casefold() not in folded)
        folded.add(name.casefold())
        stager.bounded_int(row["bytes"], 0, stager.MAX_FILE)
        need(type(row["mode"]) is int and row["mode"] & ~0o777 == 0
             and type(row["sha256"]) is str and HEX.fullmatch(row["sha256"])
             and row["output_mode"] == (0o700 if row["mode"] & 0o111 else 0o600))
        table[name] = row
        expanded += row["bytes"]
        if " " not in name:
            ordinary.append(row)
        parts = name.split("/")
        for count in range(1, len(parts)):
            parent = "/".join(parts[:count])
            need(prefixes.setdefault(parent.casefold(), parent) == parent)
    need(expanded <= stager.MAX_EXPANDED and not folded.intersection(prefixes))
    # The unchanged checker still covers every ordinary spelling. The full
    # pinned inventory and the same explicit path/type/mode/hash checks above
    # cover the7original spaced files; no generic parser policy is loosened.
    stager.check_members(dict(tool, members=ordinary))
    raw = (json.dumps(members, sort_keys=True, separators=(",", ":")) + "\n").encode()
    need(hashlib.sha256(raw).hexdigest() == pin["members_sha256"])
    return table


def windows_python_zip_extract(stager, stage, tool, archive, output):
    table = windows_python_zip_members(stager, tool)
    # Retain the original generic ZIP central-directory and entry limits.
    with archive.open("rb") as source:
        source.seek(max(0, tool["bytes"] - 65557))
        tail = source.read(65557)
    position = tail.rfind(b"PK\x05\x06")
    need(position >= 0 and len(tail) - position >= 22)
    header = struct.unpack("<4s4H2LH", tail[position:position + 22])
    need(header[1] == header[2] == 0 and header[3] == header[4] <= stager.MAX_MEMBERS
         and header[5] <= 4 * 1024 * 1024 and header[6] + header[5] <= tool["bytes"]
         and header[7] == len(tail) - position - 22)
    seen = set()
    with zipfile.ZipFile(archive, "r", allowZip64=False) as source:
        for member in source.infolist():
            need(not member.flag_bits & 1 and not member.is_dir()
                 and member.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED))
            name = stager.rel(member.filename, internal_spaces=True)
            need(name in table and name not in seen)
            seen.add(name)
            row = table[name]
            mode = (member.external_attr >> 16) & 0o177777
            need(stat.S_IFMT(mode) in (0, stat.S_IFREG)
                 and (member.file_size, mode & 0o777) == (row["bytes"], row["mode"]))
            with source.open(member) as stream:
                stager.write_member(stage, output / name, stream, row)
    need(seen == set(table))


def windows_cipd_instance(tool):
    pin = WINDOWS_CIPD_SHA1.get(tool.get("name"))
    need(pin is not None and all(tool.get(key) == pin[key]
         for key in ("name", "format", "bytes", "sha256", "source")))
    return {"hashAlgo": "SHA1", "hexDigest": pin["instance_sha1"]}


def windows_cipd_download(stager, stage, tool, archive):
    instance = windows_cipd_instance(tool)
    payload = json.dumps({"package": tool["source"]["package"], "instance": instance},
                         separators=(",", ":")).encode()
    connection, response = stager.https(
        "https://chrome-infra-packages.appspot.com/prpc/cipd.Repository/GetInstanceURL",
        method="POST", body=payload, json_response=True)
    try:
        raw = response.read(65537)
        need(len(raw) <= 65536 and raw.startswith(b")]}'"))
        record = stager.decode(raw[4:])
        need(type(record) is dict and set(record) == {"signedUrl"})
        signed = urllib.parse.urlsplit(record["signedUrl"])
        need(signed.hostname == "storage.googleapis.com"
             and signed.path == "/chrome-infra-packages/store/SHA1/" + instance["hexDigest"]
             and bool(signed.query))
    finally:
        connection.close()
    # The original reviewed transport independently enforces HTTPS/host/port,
    # no redirects/credentials/ambient auth/encoding before this bounded read.
    connection, response = stager.https(record["signedUrl"])
    try:
        need(response.getheader("Content-Length") == str(tool["bytes"])
             and response.getheader("Transfer-Encoding") is None)
        actual, original, count = hashlib.sha256(), hashlib.sha1(), 0
        deadline = time.monotonic() + 300
        with stage.create(archive) as output:
            while count < tool["bytes"]:
                need(time.monotonic() < deadline)
                block = response.read(min(1024 * 1024, tool["bytes"] - count))
                need(bool(block))
                count += len(block)
                need(count <= tool["bytes"])
                output.write(block)
                actual.update(block)
                original.update(block)
            need(response.read(1) == b"" and actual.hexdigest() == tool["sha256"]
                 and original.hexdigest() == instance["hexDigest"])
            output.flush()
            os.fsync(output.fileno())
    finally:
        connection.close()


def runtime_stage(stager, workspace, plan, control):
    _, profile = bound_asset(workspace, plan["runtime_profile"])
    need(type(profile) is dict and set(profile) == {"schema", "target", "materials"}
         and profile["schema"] == "morpheus-windows-runtime-data-v1" and profile["target"] == "win-x64"
         and type(profile["materials"]) is list and 1 <= len(profile["materials"]) <= 40)
    roles, material_keys, origins = set(), set(), set()
    for item in profile["materials"]:
        need(set(item) == {"role", "tool", "source_mount"}
             and item["role"] in ("cpython", "vpython", "cipd", "rust", "wheels")
             and (item["role"] == "wheels" or item["role"] not in roles))
        roles.add(item["role"])
        tool = item["tool"]
        need(set(tool) == {"name", "format", "sha256", "bytes", "source", "members", "alias_copies", "omitted_aliases", "omission_evidence"}
             and tool["format"] in ("zip", "tar.xz")
             and type(tool["bytes"]) is int and 0 < tool["bytes"] <= 2 * 1024 ** 3
             and type(tool["sha256"]) is str and HEX.fullmatch(tool["sha256"])
             and not tool["alias_copies"] and not tool["omitted_aliases"] and tool["omission_evidence"] is None
             and item["source_mount"] == ("third_party/rust-toolchain" if item["role"] == "rust" else None))
        origin = tool["source"]
        if origin.get("kind") == "cipd":
            need(set(origin) == {"kind", "package", "version"}
                 and type(origin["package"]) is str and len(origin["package"]) <= 256
                 and (origin["package"] == {"cpython": "infra/3pp/tools/cpython3/windows-amd64",
                     "vpython": "infra/tools/luci/vpython3/windows-amd64",
                     "cipd": "infra/tools/cipd/windows-amd64"}.get(item["role"])
                     or item["role"] == "wheels" and (re.fullmatch(
                         r"infra/python/wheels/(?:[A-Za-z0-9_-]+-py(?:2_py3|3)|"
                         r"[A-Za-z0-9_-]+/windows-amd64_cp311_cp311)", origin["package"])
                         or origin["package"] == "infra/3pp/tools/virtualenv"))
                 and type(origin["version"]) is str and 0 < len(origin["version"]) <= 256
                 and "latest" not in origin["version"].lower())
        else:
            need(item["role"] == "rust")
            windows_rust_identity(tool)
        # Multiple genuine wheels/versions keep their original package archives and resource layouts.
        # A fixed package+version-derived key prevents shared wheel output/entrypoint collisions.
        key = item["role"]
        if key == "wheels":
            key += "-" + hashlib.sha256((origin["package"] + "\0" + origin["version"]).encode()).hexdigest()[:16]
        identity = json.dumps(origin, sort_keys=True, separators=(",", ":"))
        need(tool["name"] == key and key not in material_keys and identity not in origins)
        material_keys.add(key)
        origins.add(identity)
        if key == "rust":
            windows_rust_members(stager, tool)
        elif key in WINDOWS_PYTHON_ZIPS:
            windows_python_zip_members(stager, tool)
        else:
            stager.check_members(tool)
        need(all(row["kind"] in ("regular", "directory") for row in tool["members"]))
        archive = control.root / "runtime-archives" / (key + (".zip" if tool["format"] == "zip" else ".tar.xz"))
        if key == "rust":
            windows_rust_download(stager, control, tool, archive)
            windows_rust_extract(stager, control, tool, archive, control.root / "runtime" / key)
        else:
            if key in WINDOWS_CIPD_SHA1:
                windows_cipd_download(stager, control, tool, archive)
            else:
                stager.download(control, tool, archive)
            if key in WINDOWS_PYTHON_ZIPS:
                windows_python_zip_extract(stager, control, tool, archive, control.root / "runtime" / key)
            else:
                stager.extract(control, tool, archive, control.root / "runtime" / key)
    # These are genuine archive bytes, not a fabricated CIPD cache/tag lookup,
    # vpython environment realization, Python/Rust execution or SDK substitute.
    write_json(control, control.root / "runtime-data-receipt.json", {"schema": 1,
        "profile_sha256": plan["runtime_profile"]["sha256"], "roles": sorted(roles),
        "genuine_vpython_realized": False, "tools_executed": [], "build_ready": False,
        "host_sdk_redistributed": False, "distribution_ready": False})
    return control.root, profile


def stage(args, temp, workspace, plan):
    stager = fixed_module("python_stager.py", HELPERS["python_stager.py"])
    key = hashlib.sha256((os.environ["GITHUB_RUN_ID"] + ":" + os.environ["GITHUB_RUN_ATTEMPT"] + ":windows").encode()).hexdigest()[:16]
    root = temp / ("morpheus-webrtc-windows-" + key)
    need(not os.path.lexists(root))
    owner = stager.Stage(root)
    try:
        core = core_stage(stager, temp, workspace, plan, owner)
        source = source_stage(stager, temp, workspace, plan, owner) if plan["source_profile"] is not None else None
        runtime, runtime_profile = runtime_stage(stager, workspace, plan, owner) if plan["runtime_profile"] else (None, None)
        _, core_manifest = bound_asset(workspace, plan["core_manifest"])
        overlay = project(owner, source, core, core_manifest, runtime, runtime_profile) if source is not None else None
        receipt = {"schema": "morpheus-windows-source-stage-receipt-v1", "target": "win-x64",
            "pipeline_plan_sha256": args.plan_sha256, "core_manifest_sha256": CORE,
            "core_root": str(core), "core_files": tree_files(core),
            "source_root": str(source) if source is not None else None,
            "source_files": tree_files(source) if source is not None else None,
            "source_profile_sha256": plan["source_profile"]["sha256"] if plan["source_profile"] else None,
            "windows_runtime_profile_bound": plan["runtime_profile"] is not None,
            "runtime_files": tree_files(runtime / "runtime") if runtime is not None else None,
            "source_overlay": {"path": str(overlay), **fact(overlay)} if overlay is not None else None,
            "compiler_cipd_runtime_closure": False, "host_sdk_redistributed": False,
            "native_sources_complete": False, "build_ready": False, "distribution_ready": False,
            "build_admission_granted": False, "hooks_executed": False, "executed_downloaded_tools": []}
        info = write_json(owner, root / "windows-stage-receipt.json", receipt)
        print("MORPHEUS_NATIVE_WINDOWS_STAGE " + json.dumps({"source_data_staged": source is not None,
            "core_data_staged": True, "receipt_sha256": info["sha256"], "build_ready": False,
            "distribution_ready": False}, sort_keys=True), flush=True)
        if args.phase == "build":
            need(source is not None and plan["runtime_profile"] is not None)
            build(args, temp, workspace, plan, owner, core, source, runtime)
    finally:
        # Preserve private staging DATA on failure; never delete an unexpected
        # source/host tree or treat release of directory anchors as tool cleanup.
        owner.finish(True)


def build(args, temp, workspace, plan, owner, core, source, runtime):
    # No runtime/SDK readiness can be synthesized from archive presence. A real
    # complete Windows admission (12 independent gates) is a required source asset.
    need(runtime is not None)
    template_file, template = bound_asset(workspace, plan["build_inputs"])
    need(template["schema"] == "morpheus-native-webrtc-windows-inputs-v4")
    replacements = {"$RUNNER_TEMP/": temp, "$GITHUB_WORKSPACE/": workspace,
        "$WINDOWS_SOURCE/": source, "$WINDOWS_CORE/": core, "$WINDOWS_RUNTIME/": runtime}
    def relocate(value):
        if type(value) is dict:
            return {k: relocate(v) for k, v in value.items()}
        if type(value) is list:
            return [relocate(v) for v in value]
        if type(value) is str:
            for prefix, root in replacements.items():
                if value == prefix[:-1]:
                    return str(root)
                if value.startswith(prefix):
                    return str(root / relative(value[len(prefix):]))
        return value
    actual = relocate(template)
    # All gates are copied byte-for-byte from admitted assets; only defined path
    # prefixes relocate. No approved flag, file hash or inventory is constructed.
    manifest = owner.root / "windows-build-inputs.json"
    info = write_json(owner, manifest, actual)
    package_parent = temp / ("morpheus-windows-package-" + args.plan_sha256[:16])
    owner.mkdir(package_parent)
    recipe = fixed_module("build_native_webrtc_windows.py", RECIPE)
    invoke(recipe, ["--inputs", str(manifest), "--inputs-sha256", info["sha256"],
        "--source-root", str(source / "checkout/src"), "--depot-tools-root", str(source / "checkout/outer-depot"),
        "--package-root", str(package_parent / "win-x64-release"), "--jobs", str(args.jobs)])
    # Nothing is uploaded, cached, installed, signed or made distribution-ready.
    write_json(owner, owner.root / "windows-producer-consumption.json", {
        "runtime_admission_sha256": plan["runtime_profile"]["sha256"],
        "build_template_sha256": plan["build_inputs"]["sha256"],
        "actual_build_inputs_sha256": info["sha256"], "recipe_sha256": RECIPE,
        "host_sdk_redistributed": False, "distribution_ready": False})


class FixedParser(argparse.ArgumentParser):
    def error(self, message):
        raise Refused()


def main():
    parser = FixedParser(allow_abbrev=False)
    parser.add_argument("--phase", choices=("stage", "build"), required=True)
    parser.add_argument("--plan", required=True)
    parser.add_argument("--plan-sha256", required=True)
    parser.add_argument("--jobs", type=int, choices=(1, 2, 3, 4), default=2)
    args = parser.parse_args()
    temp, workspace = runner()
    plan = load(path(args.plan), args.plan_sha256)
    need(set(plan) == {"schema", "target", "dispatcher_sha256", "recipe_sha256", "core_manifest",
        "source_profile", "runtime_profile", "build_inputs", "security_plan_sha256", "native_test_count",
        "minimum_free_bytes", "cache_handoff_authorized"}
        and plan["schema"] == "morpheus-native-windows-pipeline-v1" and plan["target"] == "win-x64"
        and plan["dispatcher_sha256"] == hashlib.sha256(read(Path(__file__).resolve(), 256 * 1024)).hexdigest()
        and plan["recipe_sha256"] == RECIPE and plan["security_plan_sha256"] == SECURITY
        and plan["native_test_count"] == 12 and plan["cache_handoff_authorized"] is False
        and type(plan["minimum_free_bytes"]) is int and 12 * 1024 ** 3 <= plan["minimum_free_bytes"] <= MAX_TOTAL)
    need(shutil.disk_usage(temp).free >= plan["minimum_free_bytes"])
    if args.phase == "build":
        need(plan["source_profile"] is not None and plan["runtime_profile"] is not None and plan["build_inputs"] is not None)
    stage(args, temp, workspace, plan)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        print("MORPHEUS_NATIVE_WINDOWS_PRODUCER failed", file=sys.stderr, flush=True)
        raise SystemExit(1)
