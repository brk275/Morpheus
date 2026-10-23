"""Inventory AppImage dependencies without executing a target or calling ldd.

This records exact host package provenance and ships its copyright notices.
It deliberately does not equate notices or a GNU build ID with complete source
compliance. Static runtime/Rust dependencies and corresponding-source assembly
remain release gates; this adapter permits private QA, never publication.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat

import appimage_client as client


MAX_ELF = 512 * 1024 * 1024
MAX_NOTICE = 4 * 1024 * 1024
MAX_COMPONENTS = 256
NOTICE_ROOT = "usr/share/doc/morpheus-app/bundled-components"
CPAL_LICENSE_SHA256 = "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4"
ALSA_LICENSES = {
    "LICENSE-APACHE": "0d542e0c8804e39aa7f37eb00da5a762149dc682d7829451287e11b938e94594",
    "LICENSE-MIT": "3cc33f8e76680b9cba2e2a5bc94de5a92420045863ab2fadbef290a55b4d8530",
}
SYSTEM_DIRS = (Path("/usr/lib/x86_64-linux-gnu"), Path("/lib/x86_64-linux-gnu"),
               Path("/usr/lib"), Path("/lib"))
PACKAGE = re.compile(r"[a-z0-9][a-z0-9+.-]*(?::[a-z0-9][a-z0-9-]*)?")
SONAME = re.compile(r"[A-Za-z0-9_+.-]+")
HEX = re.compile(r"[0-9a-f]{64}")
VERSION = re.compile(r"[A-Za-z0-9.+:~_-]{1,160}")
# One GNU build-ID note as GNU readelf --wide --notes prints it: the owner,
# the descriptor size and the note type, then the description on the SAME
# line (without --wide it would start on the next line). Separators are
# matched structurally as runs of spaces/tabs; the owner and type text carry
# the note's identity. The hex is lowercase, as readelf prints it.
BUILD_ID_NOTE = re.compile(
    r"^[ \t]*GNU[ \t]+0x([0-9a-f]{8})[ \t]+NT_GNU_BUILD_ID \(unique build ID bitstring\)"
    r"[ \t]+Build ID:[ \t]*([0-9a-f]+)[ \t]*$", re.MULTILINE)


class InventoryFailure(RuntimeError):
    def __init__(self, reason):
        allowed = {"input", "tool", "elf", "dependency", "package", "notice", "deployment", "limit",
                   "elf_build_id", "elf_needed", "elf_soname", "elf_search", "search"}
        self.reason = reason if reason in allowed else "input"
        super().__init__("AppImage inventory failed: " + self.reason)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def regular(path, bound):
    try:
        return client.read_regular(Path(path), bound)
    except (OSError, client.PackageFailure):
        raise InventoryFailure("input") from None


def root_owned(path):
    """Follow only a root-controlled system path; preserve no ambient overrides."""
    try:
        path = Path(path).absolute()
        for candidate in (path, path.resolve(strict=True)):
            for item in (candidate, *candidate.parents):
                info = item.lstat()
                if info.st_uid != 0 or (not stat.S_ISLNK(info.st_mode) and info.st_mode & 0o022):
                    raise InventoryFailure("tool")
        resolved = path.resolve(strict=True)
        if not resolved.is_file():
            raise InventoryFailure("tool")
        return resolved
    except OSError:
        raise InventoryFailure("tool") from None


def command(runner, label, argv, root, environment, *, limit=1024 * 1024, expected_status=(0,)):
    try:
        output = runner.run(label, argv, cwd=root, env=environment, timeout=30,
                            output_limit=limit, expected_status=expected_status)
        if type(output) is not bytes or len(output) > limit:
            raise InventoryFailure("limit")
        return output.decode("utf-8", errors="strict")
    except (OSError, UnicodeError):
        raise InventoryFailure("input") from None


def parse_build_id(output):
    """The single GNU build ID in a `readelf --wide --notes` report.

    Exactly one well-formed GNU build-ID note line is required, and the text
    `Build ID:` and the type `NT_GNU_BUILD_ID` must each occur exactly once in
    the whole report, so a duplicate, malformed or stray occurrence is never
    ignored. The ID is bounded lowercase even-length hex whose length agrees
    with the note's reported descriptor size.
    """
    notes = BUILD_ID_NOTE.findall(output)
    if (len(notes) != 1 or output.count("Build ID:") != 1
            or output.count("NT_GNU_BUILD_ID") != 1):
        raise InventoryFailure("elf_build_id")
    size, build_id = notes[0]
    if (not 8 <= len(build_id) <= 128 or len(build_id) % 2
            or int(size, 16) * 2 != len(build_id)):
        raise InventoryFailure("elf_build_id")
    return build_id


def parse_elf_report(output):
    build_id = parse_build_id(output)
    needed = re.findall(r"\(NEEDED\).*?Shared library: \[([^\]\n]+)\]", output)
    names = re.findall(r"\(SONAME\).*?Library soname: \[([^\]\n]+)\]", output)
    searches = re.findall(r"\((?:RPATH|RUNPATH)\).*?: \[([^\]\n]*)\]", output)
    if (len(needed) > 128 or len(needed) != len(set(needed))
            or any(not SONAME.fullmatch(value) for value in needed)):
        raise InventoryFailure("elf_needed")
    if len(names) > 1 or any(not SONAME.fullmatch(value) for value in names):
        raise InventoryFailure("elf_soname")
    if len(searches) > 1:
        raise InventoryFailure("elf_search")
    return {"build_id": build_id, "needed": sorted(needed), "soname": names[0] if names else None,
            "search": searches[0].split(":") if searches else []}


def elf_record(runner, path, root, environment, label="inventory-elf"):
    if type(label) is not str or label not in {"inventory-elf", "inventory-elf-client", "inventory-elf-dependency", "inventory-elf-deployed"}:
        raise InventoryFailure("input")
    data = regular(path, MAX_ELF)
    # This distribution is x86-64 little-endian ELF, ET_EXEC or ET_DYN only.
    if (len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01"
            or int.from_bytes(data[16:18], "little") not in (2, 3)
            or int.from_bytes(data[18:20], "little") != 62):
        raise InventoryFailure("elf")
    readelf = root_owned("/usr/bin/readelf")
    output = command(runner, label, [str(readelf), "--wide", "--dynamic", "--notes", str(path)],
                     root, environment)
    return {**parse_elf_report(output), "sha256": digest(data)}


def resolve_library(name):
    if not SONAME.fullmatch(name):
        raise InventoryFailure("dependency")
    found = set()
    for directory in SYSTEM_DIRS:
        path = directory / name
        if path.exists() or path.is_symlink():
            found.add(root_owned(path))
    if len(found) != 1:
        raise InventoryFailure("dependency")
    return found.pop()


def parse_package_record(output, expected):
    lines = output.splitlines()
    if len(lines) != 1:
        raise InventoryFailure("package")
    fields = lines[0].split("\t")
    if len(fields) != 4:
        raise InventoryFailure("package")
    package, version, source, source_version = fields
    if (package != expected or not PACKAGE.fullmatch(package) or not PACKAGE.fullmatch(source)
            or not VERSION.fullmatch(version) or not VERSION.fullmatch(source_version)):
        raise InventoryFailure("package")
    return {"package": package, "version": version, "source_package": source,
            "source_version": source_version}


def package_record(runner, path, root, environment):
    dpkg = root_owned("/usr/bin/dpkg-query")
    # dpkg may retain the pre-usrmerge spelling in its file list. Query both
    # owned aliases and accept only the same single owning package.
    aliases = [str(path)]
    if str(path).startswith("/usr/lib/"):
        aliases.append(str(path)[4:])
    owners = set()
    for alias in aliases:
        # dpkg --search uses status 1 with empty stdout for an absent alias.
        # Ownership/deadline/output failures must propagate, never try another
        # alias after cleanup failed.
        output = command(runner, "inventory-owner", [str(dpkg), "--search", alias],
                         root, environment, expected_status=(0, 1))
        for line in output.splitlines():
            match = re.fullmatch(r"([a-z0-9][a-z0-9+.-]*(?::[a-z0-9][a-z0-9-]*)?): (/.+)", line)
            if not match or match[2] != alias:
                raise InventoryFailure("package")
            owners.add(match[1])
    if len(owners) != 1:
        raise InventoryFailure("package")
    owner = owners.pop()
    output = command(runner, "inventory-package", [str(dpkg), "--show",
                     "--showformat=${binary:Package}\t${Version}\t${source:Package}\t${source:Version}\n",
                     "--", owner], root, environment)
    record = parse_package_record(output, owner)
    notice = root_owned(Path("/usr/share/doc") / owner.split(":")[0] / "copyright")
    data = regular(notice, MAX_NOTICE)
    if not data.strip():
        raise InventoryFailure("notice")
    # Retain complete copyright prose, including its common-license references.
    common = {}
    for name in set(re.findall(rb"/usr/share/common-licenses/([A-Za-z0-9.+-]+)", data)):
        text = name.rstrip(b".").decode("ascii")
        if not text:
            raise InventoryFailure("notice")
        common[text] = regular(root_owned(Path("/usr/share/common-licenses") / text), MAX_NOTICE)
    return record, data, common


def add_notice(directory, config, notices, name, data):
    if not re.fullmatch(r"[A-Za-z0-9_+.-]+", name) or not 0 < len(data) <= MAX_NOTICE:
        raise InventoryFailure("notice")
    destination = NOTICE_ROOT + "/" + name
    path = directory / name
    if destination in notices:
        if notices[destination] != digest(data):
            raise InventoryFailure("notice")
        return destination
    files = config["appimage"]["files"]
    if destination in files.values() or str(path) in files:
        raise InventoryFailure("notice")
    client.write_new(path, data)
    files[str(path)] = destination
    notices[destination] = digest(data)
    return destination


def cpal_notices(directory, config, notices):
    """Copy pinned local audio licenses/provenance, not a compliance claim."""
    source = client.ROOT / "vendor/cpal"
    license_text = regular(source / "LICENSE", MAX_NOTICE)
    provenance = regular(source / "OWNED_COREAUDIO.md", MAX_NOTICE)
    alsa_provenance = regular(source / "OWNED_ALSA.md", MAX_NOTICE)
    alsa_licenses = {name: regular(client.ROOT / "vendor/alsa" / name, MAX_NOTICE)
                     for name in ALSA_LICENSES}
    # Validate every required source before staging any local-audio notice.
    if (digest(license_text) != CPAL_LICENSE_SHA256
            or any(digest(data) != ALSA_LICENSES[name] for name, data in alsa_licenses.items())):
        raise InventoryFailure("notice")
    add_notice(directory, config, notices, "CPAL-0.18.2-LICENSE", license_text)
    add_notice(directory, config, notices, "CPAL-0.18.2-PATCH.md", provenance)
    add_notice(directory, config, notices, "ALSA-0.11.0-PATCH.md", alsa_provenance)
    for name, data in alsa_licenses.items():
        add_notice(directory, config, notices, "ALSA-0.11.0-" + name, data)


def prepare(runner, root, binary, stage, source_metadata, config, environment, *, runtime_metadata):
    root = client.private_directory(root)
    stage = Path(stage)
    if not isinstance(config.get("appimage", {}).get("files"), dict):
        raise InventoryFailure("input")
    directory = root / "component-notices"
    directory.mkdir(mode=0o700)
    notices = {}
    components = {}
    main = elf_record(runner, binary, root, environment)
    if main["search"]:
        raise InventoryFailure("search")
    components["morpheus-app"] = {**main, "kind": "project"}
    pending = list(main["needed"])
    while pending:
        name = pending.pop()
        if name in components:
            continue
        if len(components) >= MAX_COMPONENTS:
            raise InventoryFailure("limit")
        if name == client.SONAME:
            path = stage / name
            record = elf_record(runner, path, root, environment, "inventory-elf-client")
            if (record["sha256"] != source_metadata.get("source_client_sha256")
                    or record["build_id"] != source_metadata.get("build_id")):
                raise InventoryFailure("input")
            record.update(kind="pipewire", source_version=source_metadata["source_version"])
        else:
            path = resolve_library(name)
            record = elf_record(runner, path, root, environment, "inventory-elf-dependency")
            package, copyright_text, common = package_record(runner, path, root, environment)
            key = package["package"].replace(":", "_")
            notice = add_notice(directory, config, notices, key + ".copyright", copyright_text)
            for common_name, content in common.items():
                add_notice(directory, config, notices, "common-" + common_name, content)
            record.update(kind="system-package", **package, copyright_notice=notice)
        if record["soname"] != name:
            raise InventoryFailure("dependency")
        if record["search"]:
            raise InventoryFailure("search")
        components[name] = record
        pending.extend(record["needed"])
    add_notice(directory, config, notices, "Morpheus-LICENSE", regular(client.ROOT / "LICENSE", MAX_NOTICE))
    cpal_notices(directory, config, notices)
    # PipeWire provenance/notices are staged by appimage_config; pin every byte.
    for source, destination in config["appimage"]["files"].items():
        if destination.startswith((client.NOTICE_DIR + "/", "usr/share/doc/morpheus-app/appimage-runtime/")):
            data = regular(source, 64 * 1024 * 1024)
            notices[destination] = digest(data)
    runtime = runtime_metadata.get("runtime", {})
    if (runtime_metadata.get("schema") != 1 or runtime_metadata.get("fresh_private_prefix") is not True
            or runtime_metadata.get("malformedtest_returned_zero") is not True
            or runtime_metadata.get("upstream_runtime_binary_downloaded") is not False
            or not HEX.fullmatch(runtime.get("sha256", ""))):
        raise InventoryFailure("input")
    return {"schema": 1, "components": components, "notices": notices,
            "launcher_sha256": digest(client.launcher_bytes()),
            "runtime_pin": runtime, "runtime_sources": runtime_metadata["sources"],
            "runtime_source_patches": runtime_metadata["patches"]}


def walk_regular_files(appdir):
    """Bound traversal and reject symlinks outside the exact extracted AppDir."""
    appdir = Path(appdir).absolute()
    if appdir.resolve(strict=True) != appdir or not appdir.is_dir():
        raise InventoryFailure("deployment")
    count = 0
    for folder, dirs, files in os.walk(appdir, followlinks=False):
        for name in [*dirs, *files]:
            count += 1
            if count > 10000:
                raise InventoryFailure("limit")
            path = Path(folder) / name
            info = path.lstat()
            if (info.st_uid != os.getuid() or info.st_mode & (stat.S_ISUID | stat.S_ISGID)
                    or (not stat.S_ISLNK(info.st_mode) and info.st_mode & 0o022)):
                raise InventoryFailure("deployment")
            if stat.S_ISLNK(info.st_mode):
                resolved = path.resolve(strict=True)
                if appdir not in resolved.parents or not resolved.is_file():
                    raise InventoryFailure("deployment")
            elif stat.S_ISREG(info.st_mode):
                if info.st_nlink != 1:
                    raise InventoryFailure("deployment")
                yield path
            elif not stat.S_ISDIR(info.st_mode):
                raise InventoryFailure("deployment")


def validate_search(record, path, appdir):
    for entry in record["search"]:
        # No ambient paths, empty components, LD override or unresolved tokens.
        if entry != "$ORIGIN" and not entry.startswith("$ORIGIN/"):
            raise InventoryFailure("deployment")
        suffix = entry[len("$ORIGIN"):].lstrip("/")
        if "$" in suffix or "\\" in suffix:
            raise InventoryFailure("deployment")
        resolved = (path.parent / suffix).resolve(strict=True)
        if (resolved != appdir and appdir not in resolved.parents) or not resolved.is_dir():
            raise InventoryFailure("deployment")


def verify(runner, appdir, notice_record, shipped_pipewire, environment):
    appdir = Path(appdir).absolute()
    components = notice_record["components"]
    seen = set()
    deployed = []
    files = list(walk_regular_files(appdir))
    for path in files:
        with path.open("rb") as stream:
            prefix = stream.read(4)
        if prefix != b"\x7fELF":
            continue
        relative = path.relative_to(appdir).as_posix()
        record = elf_record(runner, path, appdir, environment, "inventory-elf-deployed")
        key = "morpheus-app" if relative == "usr/bin/morpheus-app" else record["soname"]
        original = components.get(key)
        if (original is None or key in seen or record["build_id"] != original["build_id"]
                or record["needed"] != original["needed"] or record["soname"] != original["soname"]):
            raise InventoryFailure("deployment")
        validate_search(record, path, appdir)
        if key == client.SONAME and (relative != shipped_pipewire["relative_path"]
                                    or record["sha256"] != shipped_pipewire["sha256"]):
            raise InventoryFailure("deployment")
        seen.add(key)
        deployed.append({"relative_path": relative, "source_sha256": original["sha256"],
                         "deployed_sha256": record["sha256"], "build_id": record["build_id"],
                         "needed": record["needed"], "kind": original["kind"],
                         **{field: original[field] for field in ("package", "version", "source_package", "source_version")
                            if field in original}})
    if not {"morpheus-app", client.SONAME}.issubset(seen):
        raise InventoryFailure("deployment")
    launcher = appdir / "AppRun"
    if (launcher not in files or not launcher.stat().st_mode & stat.S_IXUSR
            or digest(regular(launcher, 4096)) != notice_record["launcher_sha256"]
            or not (appdir / "usr/bin/morpheus-app").stat().st_mode & stat.S_IXUSR):
        raise InventoryFailure("deployment")
    for relative, expected in notice_record["notices"].items():
        pure = PurePosixPath(relative)
        if pure.is_absolute() or ".." in pure.parts or not HEX.fullmatch(expected):
            raise InventoryFailure("notice")
        path = appdir / relative
        if path not in files or digest(regular(path, 64 * 1024 * 1024)) != expected:
            raise InventoryFailure("notice")
    return {"schema": 1, "deployed": sorted(deployed, key=lambda item: item["relative_path"]),
            "external_dependencies": sorted(set(components) - seen),
            "notice_sha256": notice_record["notices"], "launcher_sha256": notice_record["launcher_sha256"],
            "runtime_input_sha256": notice_record["runtime_pin"]["sha256"],
            "runtime_sources": notice_record["runtime_sources"],
            "runtime_source_patches": notice_record["runtime_source_patches"],
            "identity_basis": "owned deployment; pre/post hashes and matching build IDs, SONAMEs, DT_NEEDED",
            "identity_is_reproducible_build_proof": False, "distribution_ready": False,
            "unresolved_release_inputs": ["compiler-runtime-corresponding-source-and-exception-compliance",
                                           "rust-and-vendored-static-dependency-notices-and-sources",
                                           "bundled-system-package-corresponding-sources"]}
