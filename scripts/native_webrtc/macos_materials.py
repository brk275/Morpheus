"""Exact Mac DATA staging/projection for the shared WebRTC Unix producer.

No Apple SDK bytes are downloaded, copied or redistributed here. An externally
SHA-bound reviewed Mac profile is required; absent profiles remain unresolved.
Archive links become only explicitly declared verified regular COPY outputs.
"""
from __future__ import annotations
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import tarfile
import time
import lzma
import zipfile

BASELINE = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
TREE = "fecdf55fdd1e9cafc5531abab7ff14e35a1159f5"
CORE_PLAN = "6f4a0b14fe9d64d179d9e381305c961385dbfb53dff8a79ef211b35c71f8f341"
RUST = {
 "mac-arm64": ("Mac_arm64", "72b4421aa531822ba4b32533e6efae4e7c06eb15dcc32d1fb361023dd937c63c", 121923464, 1762971370566007),
 "mac-x64": ("Mac", "bb8be97e327b72f03f9caadae5557c7ce0ddac66e6dcc37cd3b8e65fca074e4b", 134345832, 1762971369029231),
}
RUST_ARCHIVE = "rust-toolchain-11339a0ef5ed586bb7ea4f85a9b7287880caac3a-1-llvmorg-22-init-14273-gea10026b.tar.xz"
LLVM_OBJDUMP_ARCHIVE = "llvmobjdump-llvmorg-22-init-14273-gea10026b-2.tar.xz"
LLVM_OBJDUMP = {
 "mac-arm64": ("Mac_arm64", "cace63bdf68ff4be9680c497a9d15482f52fe01ebeb0de65dce1828e5628df12", 5350540, 1763731858865476),
 "mac-x64": ("Mac", "65c229f8ac773e258bcf4ba67038d8db9f9100fea063c60263d7d37a8cee2374", 5611140, 1763731833330327),
}
TARGETS = {"mac-arm64": ("MacARM", "mac-arm64", "arm64", "mac"),
           "mac-x64": ("MacIntel", "mac-amd64", "x86_64", "mac")}
SPEC_HASHES = {"native": "101dc83d0e719b51d84d9c42719a78ec4dd873e9ee82a5e9c83485b8c6ccf600",
               "depot": "f6ffcf5917cb4c3ae0e541fc888706045f7eb506c06f260d85dbe295c5a4008c"}
MAX_ARCHIVE = 256 * 1024 * 1024
MAX_EXPANDED = 4 * 1024 ** 3
MAX_MEMBERS = 100000
HEX = re.compile(r"[0-9a-f]{64}\Z")

class Refused(Exception):
    pass

def need(ok):
    if not ok:
        raise Refused()

def pax_metadata(member):
    # Only authenticated Rust/LLVM archive metadata. A directory's original PAX
    # path may retain one terminal slash; the canonical inventory does not.
    # Preserve the exact metadata bytes without redirecting a path or type.
    fields = member.get("pax_headers", {})
    need(type(fields) is dict and set(fields) <= {"path", "mtime"})
    for key, value in fields.items():
        need(type(value) is str and 0 < len(value.encode("utf-8")) <= 2048)
        if key == "path":
            need(value == member["path"] or (member["kind"] == "directory"
                 and value == member["path"] + "/"))
        else:
            need(re.fullmatch(r"[0-9]{1,12}(?:\.[0-9]{1,9})?", value) is not None)
    return fields

def paths(row, material):
    """Validate a complete original member table and explicit normalization."""
    table, folded, expanded = {}, set(), 0
    need(type(row["members"]) is list and 0 < len(row["members"]) <= MAX_MEMBERS)
    for member in row["members"]:
        name = material.relative(member["path"])
        need("\\" not in name and name.casefold() not in folded)
        folded.add(name.casefold()); table[name] = member
        need(member["kind"] in ("regular", "directory", "symlink", "hardlink")
             and type(member["bytes"]) is int and 0 <= member["bytes"] <= MAX_ARCHIVE)
        if member["kind"] == "regular":
            need(HEX.fullmatch(member["sha256"]) and member["output_mode"] in (0o600, 0o700)
                 and member["output_mode"] == (0o700 if member["mode"] & 0o111 else 0o600))
            expanded += member["bytes"]
        elif member["kind"] == "directory" or row["format"] == "tar.xz":
            need(member["bytes"] == 0)
        else:
            need(member["kind"] == "symlink" and member["bytes"] <= 4096)
    # CIPD ZIPs omit directory records. Derive only their canonical ancestors,
    # retain the original member table, and reject implicit/explicit case aliases.
    topology = {name.casefold(): name for name in table}
    for name in table:
        if row["format"] == "tar.xz": pax_metadata(table[name])
        for parent in PurePosixPath(name).parents:
            path = str(parent)
            if path == ".": continue
            need((path not in table and row["format"] == "zip")
                 or (path in table and table[path]["kind"] == "directory"))
            need(topology.setdefault(path.casefold(), path) == path)
    copies, destinations = row["alias_copies"], set(table)
    links = {name for name, member in table.items() if member["kind"] in ("symlink", "hardlink")}
    need(type(copies) is list and len(copies) == len(links))
    seen = set()
    for copy in copies:
        need(set(copy) == {"path", "kind", "linkname", "source", "bytes", "sha256", "output_mode"})
        name, source = material.relative(copy["path"]), material.relative(copy["source"])
        need(name in links and name not in seen and source in table and table[source]["kind"] == "regular")
        seen.add(name); link = table[name]
        need(copy["kind"] == link["kind"] and copy["linkname"] == link["linkname"])
        raw = copy["linkname"]
        need(type(raw) is str and not raw.startswith("/") and "\\" not in raw and ":" not in raw)
        base = [] if copy["kind"] == "hardlink" else name.split("/")[:-1]
        for part in raw.split("/"):
            if part in ("", "."): continue
            if part == "..":
                need(bool(base)); base.pop()
            else: base.append(part)
        need("/".join(base) == source)
        need(all(copy[key] == table[source][key] for key in ("bytes", "sha256", "output_mode")))
        expanded += copy["bytes"]
    need(seen == links and expanded <= MAX_EXPANDED)
    return table

def validate(plan, target, material, python):
    need(set(plan) == {"schema", "target", "baseline", "tree", "core_manifest_sha256", "materials", "specs", "roles",
        "environment_complete", "build_ready", "distribution_ready", "genuine_tag_cache_created", "vpython_derivations_created"})
    need(target in TARGETS and plan["schema"] == "morpheus-native-macos-materials-v2"
         and plan["target"] == target and plan["baseline"] == BASELINE and plan["tree"] == TREE
         and plan["core_manifest_sha256"] == CORE_PLAN)
    need(all(plan[key] is False for key in ("environment_complete", "build_ready", "distribution_ready",
                                          "genuine_tag_cache_created", "vpython_derivations_created")))
    rows = plan["materials"]
    need(type(rows) is list and 4 <= len(rows) <= 128)
    names = {row["name"] for row in rows}
    required = {"rust", "llvmobjdump", "vpython", "cipd-client", "cpython-bootstrap", "virtualenv"}
    need(len(names) == len(rows) and required <= names)
    need(all(name in required or re.fullmatch(r"wheel-[0-9a-f]{16}", name) for name in names))
    expanded_total = 0
    seen_cache = set()
    for row in rows:
        fields = {"name", "format", "source", "sha256", "bytes", "seed_cache", "extraction", "members", "alias_copies"}
        need(set(row) == fields | (set() if row["name"] in ("rust", "llvmobjdump") else {"package", "version", "instance", "instance_id"}))
        need(type(row["name"]) is str and re.fullmatch(r"[a-z0-9-]{1,64}", row["name"])
             and row["format"] in ("zip", "tar.xz") and HEX.fullmatch(row["sha256"])
             and type(row["bytes"]) is int and 0 < row["bytes"] <= MAX_ARCHIVE
             and type(row["seed_cache"]) is bool)
        if row["name"] in ("rust", "llvmobjdump"):
            pins = RUST if row["name"] == "rust" else LLVM_OBJDUMP
            archive = RUST_ARCHIVE if row["name"] == "rust" else LLVM_OBJDUMP_ARCHIVE
            prefix, sha, size, generation = pins[target]
            need(row["source"] == {"kind": "gcs", "bucket": "chromium-browser-clang", "object": prefix + "/" + archive,
                                   "generation": generation}
                 and (row["sha256"], row["bytes"], row["format"]) == (sha, size, "tar.xz")
                 and row["seed_cache"] is False and row["extraction"] == "tree")
        else:
            need(row["source"] == {"kind": "cipd", "package": row["package"], "version": row["version"]}
                 and row["package"].startswith(("infra/tools/luci/", "infra/tools/cipd/", "infra/3pp/tools/", "infra/python/wheels/"))
                 and "linux" not in row["package"] and "windows" not in row["package"]
                 and row["instance_id"] == python.cipd_instance_id(row["instance"])
                 and row["format"] == "zip" and row["extraction"] in ("tree", "archive-only"))
            if row["seed_cache"]:
                need(row["extraction"] == "archive-only" and row["instance_id"] not in seen_cache)
                seen_cache.add(row["instance_id"])
        if row["name"].startswith("wheel-"):
            need(row["package"].startswith("infra/python/wheels/")
                 and row["name"] == "wheel-" + hashlib.sha256((row["package"] + "@" + row["version"]).encode()).hexdigest()[:16]
                 and row["seed_cache"] is True and row["extraction"] == "archive-only")
        if row["extraction"] == "tree": paths(row, material)
        else: need(row["members"] == [] and row["alias_copies"] == [])
        expanded_total += row["bytes"] * (2 if row["seed_cache"] else 1)
        expanded_total += sum(m["bytes"] for m in row["members"] if m["kind"] == "regular")
        expanded_total += sum(m["bytes"] for m in row["alias_copies"])
        need(expanded_total <= 8*1024**3)
    vpython = next(r for r in rows if r["name"] == "vpython")
    need(vpython["package"] == "infra/tools/luci/vpython3/" + TARGETS[target][1]
         and vpython["version"] == "git_revision:ad0c4b622c7d4cee870f57eed8170c059ca42ed3"
         and vpython["extraction"] == "tree")
    client = next(r for r in rows if r["name"] == "cipd-client")
    bootstrap = next(r for r in rows if r["name"] == "cpython-bootstrap")
    need(client["package"] == "infra/tools/cipd/" + TARGETS[target][1]
         and client["version"] == "git_revision:b1f414539ac10cc67a0250890a38712cc06cf102"
         and client["extraction"] == "tree" and client["seed_cache"] is False
         and bootstrap["package"] == "infra/3pp/tools/cpython3/" + TARGETS[target][1]
         and bootstrap["version"] == "version:2@3.11.8.chromium.35"
         and bootstrap["extraction"] == "archive-only" and bootstrap["seed_cache"] is False)
    virtualenv = next(r for r in rows if r["name"] == "virtualenv")
    need(virtualenv["package"] == "infra/3pp/tools/virtualenv"
         and virtualenv["version"] == "version:2@20.25.1.chromium.8"
         and virtualenv["seed_cache"] is True and virtualenv["extraction"] == "archive-only")
    need(0 < len(seen_cache) <= 125)
    need([spec["name"] for spec in plan["specs"]] == ["native", "depot"])
    for spec in plan["specs"]:
        need(set(spec) == {"name", "runtime_python", "source", "bytes", "sha256"})
        data = spec["source"].encode()
        need(spec["runtime_python"] == "3.11" and spec["sha256"] == SPEC_HASHES[spec["name"]]
             and hashlib.sha256(data).hexdigest() == spec["sha256"] and len(data) == spec["bytes"] <= 65536)
    roles = plan["roles"]
    role_paths = {
        "cipd": ("cipd-client", "cipd"),
        # Use the genuine regular app executable. Executing the normalized root
        # alias COPY would move os.Executable() and break ../Resources lookup.
        "vpython3": ("vpython", "vpython3.app/Contents/MacOS/vpython3"),
        "python": ("vpython", "vpython3.app/Contents/Resources/3.11/bin/python3.11"),
        "rustc": ("rust", "bin/rustc"), "cargo": ("rust", "bin/cargo"),
    }
    need(set(roles) == set(role_paths))
    for role, ref in roles.items():
        need(set(ref) == {"path", "sha256"} and HEX.fullmatch(ref["sha256"]))
        cohort, name = role_paths[role]
        need(ref["path"] == "runtime/" + cohort + "/" + name)
        member = next((m for r in rows if r["name"] == cohort for m in r["members"] if m["path"] == name), None)
        need(member is not None and member["kind"] == "regular" and member["output_mode"] == 0o700
             and member["sha256"] == ref["sha256"])
    return plan

class HeaderBudgetFile(io.FileIO):
    # A complete admitted table bounds cumulative hidden tar metadata before
    # tarfile can allocate it. Only the separately bounded regular body reads
    # bypass this budget; no archive-controlled extraction or links.
    def __init__(self, fd, limit):
        super().__init__(fd, "rb", closefd=True)
        self.remaining, self.payload = limit, False

    def read(self, size=-1):
        need(type(size) is int and 0 <= size <= 1024**2)
        if not self.payload:
            need(size <= self.remaining); self.remaining -= size
        return super().read(size)

def extract(stage, row, archive, destination, material, transport):
    table = paths(row, material); seen = set()
    def admit(name, kind, size, mode, link=None):
        need(name in table and name not in seen); seen.add(name)
        member = table[name]
        need((member["kind"], member["bytes"], member["mode"]) == (kind, size, mode))
        if link is not None: need(member["linkname"] == link)
        return member
    if row["format"] == "zip":
        # Preserve the existing central-directory resource bound before allocation.
        with archive.open("rb") as f:
            f.seek(max(0, row["bytes"] - 65557)); tail = f.read(65557)
        import struct
        pos = tail.rfind(b"PK\x05\x06"); need(pos >= 0 and len(tail) - pos >= 22)
        h = struct.unpack("<4s4H2LH", tail[pos:pos+22])
        need(h[1] == h[2] == 0 and h[3] == h[4] <= MAX_MEMBERS and h[5] <= 16*1024**2
             and h[6] + h[5] <= row["bytes"] and h[7] == len(tail) - pos - 22)
        with zipfile.ZipFile(archive, allowZip64=False) as source:
            for entry in source.infolist():
                need(not entry.flag_bits & 1 and entry.compress_type in (0, 8))
                name = entry.filename.rstrip("/") if entry.is_dir() else entry.filename
                mode = entry.external_attr >> 16
                kind = "symlink" if stat.S_ISLNK(mode) else "directory" if entry.is_dir() else "regular"
                member = admit(name, kind, entry.file_size, mode)
                if kind == "regular":
                    need(stat.S_IFMT(mode) in (0, stat.S_IFREG))
                    with source.open(entry) as stream: transport.write_member(stage, destination / name, stream, member)
                elif kind == "directory":
                    need(stat.S_IFMT(mode) in (0, stat.S_IFDIR))
                    stage.parents(destination / name / "_directory")
                else:
                    need(entry.file_size <= 4096)
                    raw = source.read(entry)
                    need(raw.decode() == member["linkname"] and hashlib.sha256(raw).hexdigest() == member["sha256"])
    else:
        temporary = archive.with_suffix(".tar")
        decoder = lzma.LZMADecompressor(format=lzma.FORMAT_XZ, memlimit=256*1024**2); total = 0
        with archive.open("rb") as source, stage.create(temporary) as output:
            while not decoder.eof:
                block = source.read(1024**2) if decoder.needs_input else b""
                need(bool(block) or not decoder.needs_input)
                data = decoder.decompress(block, max_length=1024**2); total += len(data)
                need(total <= MAX_EXPANDED + 32*1024**2); output.write(data)
            need(not decoder.unused_data and source.read(1) == b"")
        # Account for the original single PAX extension per member using its
        # bounded, exact inventory fields. No unknown metadata can consume an
        # unbounded sequence before the first real member is yielded.
        metadata_budget = len(table)*1024 + 65536
        for member in table.values():
            fields = pax_metadata(member)
            if fields:
                size = sum(len(k.encode()) + len(v.encode()) + 32 for k, v in fields.items())
                metadata_budget += 512 + ((size + 511)//512)*512
        fd = os.open(temporary, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        with HeaderBudgetFile(fd, metadata_budget) as bounded:
            with tarfile.open(fileobj=bounded, mode="r:") as source:
                for entry in source:
                    need(not entry.issparse() and len(seen) < MAX_MEMBERS
                         and (entry.isfile() or entry.isdir() or entry.issym() or entry.islnk()))
                    name = entry.name.rstrip("/") if entry.isdir() else entry.name
                    kind = "regular" if entry.isfile() else "directory" if entry.isdir() else "symlink" if entry.issym() else "hardlink"
                    member = admit(name, kind, entry.size, entry.mode, entry.linkname if entry.issym() or entry.islnk() else None)
                    need(entry.pax_headers == pax_metadata(member))
                    if entry.isfile():
                        bounded.payload = True
                        try:
                            with source.extractfile(entry) as stream: transport.write_member(stage, destination / name, stream, member)
                        finally: bounded.payload = False
                    elif entry.isdir(): stage.parents(destination / name / "_directory")
                while True:
                    padding = bounded.read(512)
                    if not padding: break
                    need(not any(padding))
        temporary.unlink(); stage.files.remove(temporary)
    need(seen == set(table))
    for copy in row["alias_copies"]:
        data = transport.snapshot(destination / copy["source"], MAX_ARCHIVE)
        need(len(data) == copy["bytes"] and hashlib.sha256(data).hexdigest() == copy["sha256"])
        stage.bytes(destination / copy["path"], data); os.chmod(destination / copy["path"], copy["output_mode"])

def stage_data(plan, raw_plan, root, material, transport):
    stage = transport.Stage(root); good = False
    try:
        for row in plan["materials"]:
            suffix = ".zip" if row["format"] == "zip" else ".tar.xz"
            archive = root / "archives" / (row["name"] + suffix)
            if row["name"] in ("rust", "llvmobjdump"):
                transport.download(stage, row, archive)
            else:
                payload = json.dumps({"package": row["package"], "instance": row["instance"]}, separators=(",", ":")).encode()
                c, r = transport.https("https://chrome-infra-packages.appspot.com/prpc/cipd.Repository/GetInstanceURL", method="POST", body=payload, json_response=True)
                try:
                    raw = r.read(65537); need(len(raw) <= 65536 and raw.startswith(b")]}'"))
                    value = transport.decode(raw[4:]); need(set(value) == {"signedUrl"})
                    import urllib.parse
                    u = urllib.parse.urlsplit(value["signedUrl"])
                    need(u.hostname == "storage.googleapis.com" and u.path == "/chrome-infra-packages/store/" + row["instance"]["hashAlgo"] + "/" + row["instance"]["hexDigest"] and u.query)
                finally: c.close()
                c, r = transport.https(value["signedUrl"])
                try:
                    need(r.getheader("Content-Length") == str(row["bytes"]) and r.getheader("Transfer-Encoding") is None)
                    h, instance, count, deadline = hashlib.sha256(), hashlib.new(row["instance"]["hashAlgo"].lower()), 0, time.monotonic() + 300
                    with stage.create(archive) as out:
                        while count < row["bytes"]:
                            need(time.monotonic() < deadline)
                            block = r.read(min(1024**2, row["bytes"] - count)); need(bool(block))
                            count += len(block); h.update(block); instance.update(block); out.write(block)
                        need(r.read(1) == b"" and h.hexdigest() == row["sha256"] and instance.hexdigest() == row["instance"]["hexDigest"])
                        out.flush(); os.fsync(out.fileno())
                finally: c.close()
            if row["seed_cache"]:
                data = transport.snapshot(archive, MAX_ARCHIVE)
                stage.bytes(root / "cipd-cache/instances" / row["instance_id"], data)
            if row["extraction"] == "tree": extract(stage, row, archive, root / "runtime" / row["name"], material, transport)
        for spec in plan["specs"]: stage.bytes(root / "specs" / spec["name"] / ".vpython3", spec["source"].encode())
        for role in plan["roles"].values():
            path = root / material.relative(role["path"])
            need(material.file_fact(path)["sha256"] == role["sha256"] and os.access(path, os.X_OK))
        stage.bytes(root / "material-plan.json", raw_plan)
        material.write_json(root / "tool-materials.json", {"schema": "morpheus-native-macos-data-staged-v2",
            "target": plan["target"], "plan_sha256": hashlib.sha256(raw_plan).hexdigest(),
            "original_archives_retained": True, "executed_tools": [], "environment_complete": False,
            "native_sources_complete": False, "build_ready": False, "distribution_ready": False})
        good = True
    finally: stage.finish(good)

def profile(root, plan_sha, target, material, python):
    plan = material.read_json(root / "material-plan.json", plan_sha)
    validate(plan, target, material, python)
    receipt = material.read_json(root / "tool-materials.json", material.file_fact(root / "tool-materials.json")["sha256"])
    need(receipt == {"schema": "morpheus-native-macos-data-staged-v2", "target": target,
        "plan_sha256": plan_sha, "original_archives_retained": True, "executed_tools": [],
        "environment_complete": False, "native_sources_complete": False, "build_ready": False,
        "distribution_ready": False})
    files, directories = {}, {"."}
    def entry(name, size, sha, mode):
        need(name not in files)
        files[name] = {"path": name, "bytes": size, "sha256": sha, "mode": mode}
        directories.update(str(p) for p in PurePosixPath(name).parents)
    for row in plan["materials"]:
        suffix = ".zip" if row["format"] == "zip" else ".tar.xz"
        entry("archives/" + row["name"] + suffix, row["bytes"], row["sha256"], 0o600)
        if row["seed_cache"]: entry("cipd-cache/instances/" + row["instance_id"], row["bytes"], row["sha256"], 0o600)
        if row["extraction"] == "tree":
            for member in row["members"]:
                name = "runtime/" + row["name"] + "/" + member["path"]
                if member["kind"] == "regular": entry(name, member["bytes"], member["sha256"], member["output_mode"])
                elif member["kind"] == "directory":
                    directories.add(name); directories.update(str(p) for p in PurePosixPath(name).parents)
            for alias in row["alias_copies"]:
                entry("runtime/" + row["name"] + "/" + alias["path"], alias["bytes"], alias["sha256"], alias["output_mode"])
    for spec in plan["specs"]: entry("specs/" + spec["name"] + "/.vpython3", spec["bytes"], spec["sha256"], 0o600)
    for name in ("material-plan.json", "tool-materials.json"):
        fact = material.file_fact(root / name); entry(name, fact["bytes"], fact["sha256"], 0o600)
    material.topology(root, files, directories)
    return plan, files, directories

def core_profile(root, target, material, transport):
    manifest = material.read_json(root / "selected-tool-manifest.json", CORE_PLAN)
    tools = transport.manifest_validate(manifest, TARGETS[target][0])
    receipt = material.read_json(root / "tool-materials.json", material.file_fact(root / "tool-materials.json")["sha256"])
    need(receipt["schema"] == 1 and receipt["status"] == "tool_data_staged"
         and receipt["target"] == TARGETS[target][0] and receipt["manifest_sha256"] == CORE_PLAN
         and receipt["executed_tools"] == [] and receipt["environment_complete"] is False)
    files, directories = {}, {"."}
    def entry(name, size, sha, mode):
        need(name not in files); files[name] = {"path": name, "bytes": size, "sha256": sha, "mode": mode}
        directories.update(str(p) for p in PurePosixPath(name).parents)
    for row in tools:
        suffix = ".zip" if row["format"] == "zip" else ".tar.xz"
        entry("archives/" + row["name"] + suffix, row["bytes"], row["sha256"], 0o600)
        for member in row["members"]:
            name = "tools/" + row["name"] + "/" + member["path"]
            if member["kind"] == "regular": entry(name, member["bytes"], member["sha256"], member["output_mode"])
            elif member["kind"] == "directory":
                directories.add(name); directories.update(str(p) for p in PurePosixPath(name).parents)
        for alias in row["alias_copies"]:
            entry("tools/" + row["name"] + "/" + alias["destination"], alias["bytes"], alias["sha256"], alias["output_mode"])
    for name in ("selected-tool-manifest.json", "tool-materials.json"):
        fact = material.file_fact(root / name); entry(name, fact["bytes"], fact["sha256"], 0o600)
    material.topology(root, files, directories)
    return files, directories

def project(base, files, directories, markers, core, runtime, target, material):
    """Add only genuine GN/Ninja/Clang/Rust. Apple SDK stays host-only."""
    roots = [(core, "tools/gn/", "buildtools/mac/"),
             (core, "tools/ninja/", "third_party/ninja/"),
             (core, "tools/clang/", "third_party/llvm-build/Release+Asserts/"),
             (runtime, "runtime/rust/", "third_party/rust-toolchain/"),
             (runtime, "runtime/llvmobjdump/", "third_party/llvm-build/Release+Asserts/")]
    markers = {"checkout/" + material.relative(name) for name in markers}
    overlay = []
    for root_spec, prefix, mount in roots:
        root, rows = root_spec
        for name, row in sorted(rows.items()):
            if not name.startswith(prefix): continue
            output = "checkout/src/" + mount + name[len(prefix):]
            need(output not in files and all(not (output == m or output.startswith(m + "/") or m.startswith(output + "/")) for m in markers))
            parents = list(PurePosixPath(output).parents)
            for parent in reversed(parents):
                part = str(parent)
                need(part not in files)
                if part not in directories:
                    (base / part).mkdir(mode=0o700); directories.add(part)
            material.file_fact(root / name, row, row["mode"], base / output)
            mode = 0o755 if row["mode"] & 0o111 else 0o644
            os.chmod(base / output, mode)
            files[output] = {"path": output, "bytes": row["bytes"], "sha256": row["sha256"], "mode": mode}
            overlay.append({"path": output.removeprefix("checkout/src/"), "type": "file", "mode": "100755" if mode == 0o755 else "100644", "bytes": row["bytes"], "sha256": row["sha256"]})
    need(overlay)
    material.topology(base, files, directories)
    return sorted(overlay, key=lambda row: row["path"])

def sdk_inventory(root):
    """Read-only complete selected host Xcode inventory. Never copy SDK bytes."""
    need(root.is_absolute() and root == root.resolve(strict=True) and root.parent == Path("/Applications")
         and re.fullmatch(r"Xcode(?:_[A-Za-z0-9_.-]+)?\.app", root.name))
    files, directories, total, started = [], [], 0, time.monotonic()
    def failed(_): raise Refused() from None
    def name(path):
        value = path.relative_to(root).as_posix()
        need("\\" not in value and not any(ord(c) < 32 or ord(c) == 127 for c in value)
             and all(p not in ("", ".", "..", ".git") for p in value.split("/")))
        return value
    for directory, children, names in os.walk(root, followlinks=False, onerror=failed):
        here = Path(directory); info = here.lstat()
        need(stat.S_ISDIR(info.st_mode) and info.st_uid == 0 and info.st_mode & 0o022 == 0
             and time.monotonic() - started <= 240)
        directories.append("." if here == root else name(here))
        for child in list(children):
            path = here / child; info = path.lstat()
            if stat.S_ISLNK(info.st_mode):
                need(path.resolve(strict=True).is_relative_to(root))
                files.append({"path": name(path), "type": "symlink", "target": os.readlink(path)})
                children.remove(child)
            else: need(stat.S_ISDIR(info.st_mode) and info.st_uid == 0 and info.st_mode & 0o022 == 0)
        for child in names:
            path = here / child; before = path.lstat()
            if stat.S_ISLNK(before.st_mode):
                need(path.resolve(strict=True).is_relative_to(root))
                files.append({"path": name(path), "type": "symlink", "target": os.readlink(path)})
                continue
            need(stat.S_ISREG(before.st_mode) and before.st_uid == 0 and before.st_mode & 0o022 == 0
                 and before.st_size <= 2*1024**3)
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
            try:
                identity = lambda st: (st.st_dev,st.st_ino,st.st_size,st.st_mode,st.st_uid,st.st_mtime_ns,st.st_ctime_ns)
                need(identity(before) == identity(os.fstat(fd)))
                h, count = hashlib.sha256(), 0
                while True:
                    block = os.read(fd, 1024**2)
                    if not block: break
                    count += len(block); total += len(block)
                    need(count <= before.st_size and total <= 64*1024**3 and time.monotonic()-started <= 240)
                    h.update(block)
                need(count == before.st_size and identity(before) == identity(os.fstat(fd)) == identity(path.lstat()))
                files.append({"path": name(path), "type": "file", "bytes": count, "sha256": h.hexdigest()})
            finally: os.close(fd)
        need(len(files) + len(directories) <= 1000000)
    files.sort(key=lambda row: row["path"].encode("utf-8")); directories.sort(key=lambda row: row.encode("utf-8"))
    return files, directories

def canonical(value):
    return (json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"))+"\n").encode("utf-8")
