"""Prepare an explicitly requested, hosted-only PipeWire client A/B experiment.

This never installs a library or runs capture. The caller owns a private root,
process containment, bounded private logs, and final cleanup. ``run_command``
must accept (stage, argv, *, cwd, env, timeout, output_limit), return captured
bytes on success, and kill/reap its owned process tree before raising on error.
No command output, path, environment or exception payload is printed here.

The normal system-library acceptance gate must remain mandatory. These paired
artifacts are diagnostic evidence only, not a replacement for that gate.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import stat
import struct
import sys
import tarfile
import time


VERSION = "1.0.5-1ubuntu3.3"
COMMIT = "800a25a01f99156d7d954f4d7fe1b0a6cf8ae7e3"
UPSTREAM_PATCH_SHA256 = "d46d38c905bf18e82515ade96fb4a8ab3fbd62fd0784b92655b21266de343584"
LIBRARY = "libpipewire-0.3.so.0.1005.0"
SONAME = "libpipewire-0.3.so.0"
TARGET = "src/pipewire/" + LIBRARY
SOURCE_FILE = "src/pipewire/impl-node.c"
DISTRO_SOURCE_SHA256 = "8b20d2b364fa3fc2d85d54741e9a11827e66b2795585f30cfb096556567c9fe6"
PATCHED_SOURCE_SHA256 = "9b47ad0083b39ef83fcb8bae4beb0d9dfc462689862eac46127f67e631d62a45"
SERIES_SHA256 = "82482f14b961779b0ab55c7940fdfc37781ce4764cad0e440355cab92da66eb3"
ORIGINAL = (
    b"\t\timpl->pending_id = pw_work_queue_add(impl->work,\n"
    b"\t\t\t\tnode, res == EBUSY ? -EBUSY : res,\n"
    b"\t\t\t\ton_state_complete, SPA_INT_TO_PTR(state));"
)
REPLACEMENT = (
    b"\t\tif (node->exported) {\n"
    b"\t\t\t/* exported nodes must complete immediately. This is important\n"
    b"\t\t\t * because the server sends ping to check completion. The server\n"
    b"\t\t\t * will only send Start to driver nodes when all clients are\n"
    b"\t\t\t * ready for processing. */\n"
    b"\t\t\ton_state_complete(node, SPA_INT_TO_PTR(state), -EBUSY, 0);\n"
    b"\t\t} else {\n"
    b"\t\t\timpl->pending_id = pw_work_queue_add(impl->work,\n"
    b"\t\t\t\t\tnode, res == EBUSY ? -EBUSY : res,\n"
    b"\t\t\t\t\ton_state_complete, SPA_INT_TO_PTR(state));\n"
    b"\t\t}"
)
SOURCES = (
    "buffers", "impl-core", "impl-client", "conf", "context", "control", "core",
    "data-loop", "impl-device", "filter", "global", "introspect", "impl-link",
    "log", "loop", "main-loop", "mem", "impl-module", "impl-node", "impl-factory",
    "impl-metadata", "pipewire", "impl-port", "properties", "protocol", "proxy",
    "resource", "settings", "stream", "thread", "thread-loop", "utils", "work-queue",
)
PACKAGES = (
    "libpipewire-0.3-0t64:amd64", "libpipewire-0.3-modules:amd64",
    "libspa-0.2-modules:amd64", "pipewire-bin:amd64", "libpipewire-0.3-common",
)
BASE_URL = "https://archive.ubuntu.com/ubuntu/pool/main/p/pipewire/"
ARCHIVES = {
    "dsc": (f"pipewire_{VERSION}.dsc", "8c9f84687253a01bd787431bf8ef04f4c9306db1a7c0dda6c9564f17da6a7c02", 16 * 1024),
    "original": ("pipewire_1.0.5.orig.tar.bz2", "d5e5f3d0b8460e5711c1571c500156fd61768ba55a062eaeb94356abdb955a56", 3 * 1024 * 1024),
    "debian": (f"pipewire_{VERSION}.debian.tar.xz", "a1650f8e95859b3a8057ea95dad7e0509bfed9896db5b0d17c70910c4e9a3ff0", 128 * 1024),
}
CONFIGURE = (
    "--buildtype=release", "--wrap-mode=nofallback", "--prefix=/usr",
    "--libdir=lib/x86_64-linux-gnu", "--sysconfdir=/etc", "-Dauto_features=disabled",
    "-Db_lto=false", "-Db_ndebug=false", "-Dc_link_args=-Wl,--build-id=sha1",
    "-Ddocs=disabled", "-Dman=disabled", "-Dexamples=disabled", "-Dtests=disabled",
    "-Dinstalled_tests=disabled", "-Dsession-managers=[]", "-Dpipewire-jack=disabled",
    "-Dpipewire-v4l2=disabled", "-Ddbus=enabled", "-Dsystemd=enabled",
    "-Dsystemd-user-service=disabled", "-Dflatpak=enabled",
    # audioconvert_dep is referenced unconditionally by src/modules/meson.build.
    # Configure these targets, but compile only TARGET, never the plugin graph.
    "-Dspa-plugins=enabled", "-Daudioconvert=enabled", "-Dsupport=enabled",
)
STAGES = frozenset({
    "eligibility", "packages", "loader", "download", "extract", "distro_patch",
    "configure", "control_build", "control_artifact", "upstream_patch",
    "patched_build", "paired_provenance", "patched_artifact", "complete",
})
REASONS = frozenset({
    "ineligible", "private_root", "tool_identity", "package_identity", "loader_identity",
    "command", "deadline", "output_bound", "archive_identity", "archive_path",
    "archive_bound", "patch_series", "patch_path", "source_identity", "patch_anchor",
    "artifact_identity", "elf_identity", "object_identity", "configuration_changed",
    "filesystem", "unexpected",
})
REPO = Path(__file__).resolve().parents[1]
MAX_LIBRARY = 64 * 1024 * 1024


class ProbeFailure(RuntimeError):
    """Public diagnostics are fixed enums, never underlying exception strings."""

    def __init__(self, stage: str, reason: str, status: int | None = None):
        self.stage = stage if stage in STAGES else "eligibility"
        self.reason = reason if reason in REASONS else "unexpected"
        self.status = status if type(status) is int and -255 <= status <= 255 else None
        super().__init__(f"PipeWire startup probe: {self.stage}/{self.reason}")

    def facts(self) -> dict:
        facts = {"eligible": self.reason != "ineligible", "prepared": False,
                 "stage": self.stage, "failure": self.reason}
        if self.status is not None:
            facts["status"] = self.status
        return facts


@dataclass(frozen=True)
class PreparedClients:
    control: dict
    patched: dict
    control_metadata_path: Path
    patched_metadata_path: Path
    loader_path: Path
    provenance: dict

    def facts(self) -> dict:
        return {"eligible": True, "prepared": True, "stage": "complete"}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def clean_environment(environment: dict[str, str]) -> dict[str, str]:
    """Do not inherit compiler/loader/script overrides into trusted commands."""
    prefixes = ("LD_", "DYLD_", "GLIBC_", "MALLOC_", "GCONV_", "PYTHON", "PERL",
                "BASH_FUNC_", "PKG_CONFIG", "CCACHE", "SCCACHE", "MESON", "NINJA",
                "PIPEWIRE_", "SPA_", "QUILT_", "DEB_", "DPKG_")
    names = {"CC", "CXX", "CPP", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "AR",
             "AS", "LD", "NM", "STRIP", "RANLIB", "OBJCOPY", "OBJDUMP", "BASH_ENV",
             "ENV", "CONFIG_SITE", "CDPATH", "GCC_EXEC_PREFIX", "COMPILER_PATH",
             "LIBRARY_PATH", "CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH"}
    result = {k: v for k, v in environment.items() if k not in names and not k.startswith(prefixes)}
    result.update(PATH="/usr/bin:/bin", LC_ALL="C", LANG="C", CC="/usr/bin/cc",
                  PKG_CONFIG="/usr/bin/pkg-config", SOURCE_DATE_EPOCH="1727740800")
    return result


def check_eligibility(environment: dict[str, str], os_release: str | None = None) -> None:
    expected = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                "RUNNER_OS": "Linux", "MORPHEUS_WAYLAND_STARTUP_PROBE": "1"}
    if (sys.platform != "linux" or platform.machine() != "x86_64" or os.getuid() == 0
            or os.getuid() != os.geteuid() or any(environment.get(k) != v for k, v in expected.items())):
        raise ProbeFailure("eligibility", "ineligible")
    if os_release is None:
        os_release = Path("/etc/os-release").read_text()
    fields = dict(re.findall(r'^([A-Z_]+)="?([^"\n]+)"?$', os_release, re.MULTILINE))
    if fields.get("ID") != "ubuntu" or fields.get("VERSION_ID") != "24.04":
        raise ProbeFailure("eligibility", "ineligible")


def private_root(root: Path) -> Path:
    root = Path(root).absolute()
    metadata = root.lstat()
    if (root.resolve() != root or not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077
            or root == REPO or root in REPO.parents or REPO in root.parents):
        raise ProbeFailure("eligibility", "private_root")
    return root


def trusted_tool(name: str) -> str:
    path = Path("/usr/bin") / name
    resolved = path.resolve(strict=True)
    metadata = resolved.stat()
    if (resolved.parent != Path("/usr/bin") or not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != 0 or metadata.st_mode & 0o022 or not metadata.st_mode & 0o111):
        raise ProbeFailure("eligibility", "tool_identity")
    return str(path)


def read_regular(path: Path, limit: int, stage: str) -> bytes:
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > limit:
            raise ProbeFailure(stage, "artifact_identity")
        with os.fdopen(descriptor, "rb", closefd=False) as source:
            data = source.read(limit + 1)
        if len(data) != metadata.st_size:
            raise ProbeFailure(stage, "artifact_identity")
        return data
    finally:
        os.close(descriptor)


def validate_packages(output: bytes) -> None:
    expected = {f"{name.removesuffix(':amd64')}\t{VERSION}\tpipewire\t{VERSION}\t"
                f"{'amd64' if name.endswith(':amd64') else 'all'}\tinstalled" for name in PACKAGES}
    try:
        lines = output.decode("ascii").splitlines()
    except UnicodeError:
        raise ProbeFailure("packages", "package_identity") from None
    if len(lines) != len(expected) or set(lines) != expected:
        raise ProbeFailure("packages", "package_identity")


def safe_relative(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if (not name or "\\" in name or "\x00" in name or path.is_absolute()
            or ".." in path.parts or str(path) != name.rstrip("/") or not path.parts):
        raise ProbeFailure("extract", "archive_path")
    return path


def verified_archive(data: bytes, key: str) -> bytes:
    _, expected, bound = ARCHIVES[key]
    if len(data) > bound or digest(data) != expected:
        raise ProbeFailure("download", "archive_identity")
    return data


def extract_archive(data: bytes, destination: Path, *, prefix: str, strip_prefix: bool) -> None:
    """Validate the entire bounded file map before extracting regular files."""
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as archive:
        entries = []
        paths = {}
        expanded = 0
        for member in archive:
            expanded += member.size
            if len(entries) >= 1600 or expanded > 20 * 1024 * 1024 or member.size < 0:
                raise ProbeFailure("extract", "archive_bound")
            path = safe_relative(member.name)
            if (path.parts[0] != prefix or path in paths or not (member.isfile() or member.isdir())):
                raise ProbeFailure("extract", "archive_path")
            paths[path] = member.isdir()
            entries.append((member, path))
        for path in paths:
            if any(parent in paths and not paths[parent] for parent in path.parents):
                raise ProbeFailure("extract", "archive_path")
            relative = path.parts[1:] if strip_prefix else path.parts
            target = destination.joinpath(*relative)
            if target != destination and (target.exists() or target.is_symlink()):
                raise ProbeFailure("extract", "archive_path")
            for parent in target.parents:
                if parent == destination:
                    break
                if parent.is_symlink() or (parent.exists() and not parent.is_dir()):
                    raise ProbeFailure("extract", "archive_path")
        if destination.is_symlink() or not destination.is_dir():
            raise ProbeFailure("extract", "archive_path")
        for member, path in entries:
            relative = path.parts[1:] if strip_prefix else path.parts
            target = destination.joinpath(*relative)
            if member.isdir():
                target.mkdir(mode=0o700, parents=True, exist_ok=True)
            else:
                target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                with archive.extractfile(member) as source, target.open("xb") as output:
                    content = source.read(member.size + 1)
                    if len(content) != member.size:
                        raise ProbeFailure("extract", "archive_bound")
                    output.write(content)
                target.chmod(0o700 if member.mode & 0o111 else 0o600)


def patch_series(source: Path) -> list[Path]:
    series = read_regular(source / "debian/patches/series", 8192, "distro_patch")
    if digest(series) != SERIES_SHA256:
        raise ProbeFailure("distro_patch", "patch_series")
    paths = []
    for line in series.decode("ascii").splitlines():
        if not line or line.startswith("#"):
            continue
        if not re.fullmatch(r"[A-Za-z0-9._/-]+\.patch", line):
            raise ProbeFailure("distro_patch", "patch_path")
        relative = safe_relative(line)
        patch = source / "debian/patches" / str(relative)
        text = read_regular(patch, 1024 * 1024, "distro_patch").decode("utf-8")
        headers = 0
        for header in text.splitlines():
            if header.startswith(("--- ", "+++ ")):
                name = header[4:].split("\t", 1)[0].split(" ", 1)[0]
                if name == "/dev/null":
                    continue
                if not name.startswith(("a/", "b/")):
                    raise ProbeFailure("distro_patch", "patch_path")
                safe_relative(name[2:])
                headers += 1
        if not headers or patch in paths:
            raise ProbeFailure("distro_patch", "patch_path")
        paths.append(patch)
    if not paths or len(paths) > 64:
        raise ProbeFailure("distro_patch", "patch_series")
    return paths


def apply_upstream_patch(source: Path) -> None:
    path = source / SOURCE_FILE
    contents = read_regular(path, 256 * 1024, "upstream_patch")
    if digest(contents) != DISTRO_SOURCE_SHA256:
        raise ProbeFailure("upstream_patch", "source_identity")
    if contents.count(ORIGINAL) != 1:
        raise ProbeFailure("upstream_patch", "patch_anchor")
    modified = contents.replace(ORIGINAL, REPLACEMENT, 1)
    if digest(modified) != PATCHED_SOURCE_SHA256:
        raise ProbeFailure("upstream_patch", "source_identity")
    path.write_bytes(modified)


def elf_identity(data: bytes, *, library: bool, allow_origin_runpath: bool = False) -> tuple[str, tuple[str, ...]]:
    """Bounded ELF64/x86-64 PT_NOTE/DT_SONAME reader; never execute readelf/ldd."""
    def fail():
        raise ProbeFailure("paired_provenance", "elf_identity")
    if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
        fail()
    kind, machine, version = struct.unpack_from("<HHI", data, 16)
    phoff = struct.unpack_from("<Q", data, 32)[0]
    ehsize, phsize, phnum = struct.unpack_from("<HHH", data, 52)
    if (kind != 3 or machine != 62 or version != 1 or ehsize != 64 or phsize != 56
            or not 1 <= phnum <= 128 or phoff < 64 or phoff + phsize * phnum > len(data)):
        fail()
    segments = []
    build_ids = []
    for index in range(phnum):
        values = struct.unpack_from("<IIQQQQQQ", data, phoff + index * phsize)
        kind, _, offset, address, _, size, _, _ = values
        if offset + size > len(data):
            fail()
        segments.append((kind, offset, address, size))
        if kind == 4:
            cursor, end = offset, offset + size
            while cursor < end:
                if cursor + 12 > end:
                    fail()
                namesz, descsz, note_type = struct.unpack_from("<III", data, cursor)
                cursor += 12
                desc = cursor + ((namesz + 3) & ~3)
                following = desc + ((descsz + 3) & ~3)
                if following > end:
                    fail()
                if data[cursor:cursor + namesz] == b"GNU\0" and note_type == 3:
                    if not 1 <= descsz <= 64:
                        fail()
                    build_ids.append(data[desc:desc + descsz].hex())
                cursor = following
    if len(build_ids) != 1:
        fail()
    if not library:
        return build_ids[0], ()
    dynamic = [item for item in segments if item[0] == 2]
    if len(dynamic) != 1 or dynamic[0][3] > 64 * 1024 or dynamic[0][3] % 16:
        fail()
    entries = []
    for cursor in range(dynamic[0][1], dynamic[0][1] + dynamic[0][3], 16):
        tag, value = struct.unpack_from("<qQ", data, cursor)
        if tag == 0:
            break
        entries.append((tag, value))
    else:
        fail()
    def only(tag):
        values = [value for found, value in entries if found == tag]
        if len(values) != 1:
            fail()
        return values[0]
    strings, string_size = only(5), only(10)
    matches = [offset + strings - address for kind, offset, address, size in segments
               if kind == 1 and address <= strings and strings + string_size <= address + size]
    if len(matches) != 1 or string_size > 4 * 1024 * 1024 :
        fail()
    table = data[matches[0]:matches[0] + string_size]
    def string(index):
        if index >= len(table):
            fail()
        end = table.find(b"\0", index)
        if end < 0:
            fail()
        try:
            return table[index:end].decode("ascii")
        except UnicodeError:
            fail()
    search_paths = [(tag, string(value)) for tag, value in entries if tag in (15, 29)]
    if search_paths and (not allow_origin_runpath or search_paths != [(29, "$ORIGIN")]):
        fail()
    needed = tuple(sorted(string(value) for tag, value in entries if tag == 1))
    if (string(only(14)) != SONAME or "libc.so.6" not in needed
            or set(needed) - {"libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "libatomic.so.1"}):
        fail()
    return build_ids[0], needed


def object_inventory(build: Path) -> dict[str, str]:
    directory = build / (TARGET + ".p")
    expected = {name + ".c.o" for name in SOURCES}
    paths = {path.name: path for path in directory.iterdir() if path.name.endswith(".o")}
    if set(paths) != expected:
        raise ProbeFailure("paired_provenance", "object_identity")
    return {name: digest(read_regular(path, MAX_LIBRARY, "paired_provenance"))
            for name, path in sorted(paths.items())}


def verify_object_pair(control: dict, patched: dict) -> None:
    changed = {name for name in control if control[name] != patched.get(name)}
    if set(control) != set(patched) or changed != {"impl-node.c.o"}:
        raise ProbeFailure("paired_provenance", "object_identity")


def loader_identity(run, dpkg_query: str) -> tuple[Path, bytes, str]:
    loader = Path("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2")
    metadata = loader.lstat()
    if (loader.resolve(strict=True) != loader or not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != 0 or metadata.st_mode & 0o022 or not metadata.st_mode & 0o111):
        raise ProbeFailure("loader", "loader_identity")
    # Noble's merged-/usr package database may retain /lib spellings. Accept
    # only these two exact installed paths resolving to the trusted file.
    listing = run("loader", [dpkg_query, "--listfiles", "libc6:amd64"], limit=256 * 1024)
    allowed = {str(loader), "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"}
    matches = [line for line in listing.decode("ascii").splitlines() if line in allowed]
    if len(matches) != 1 or Path(matches[0]).resolve(strict=True) != loader:
        raise ProbeFailure("loader", "loader_identity")
    data = read_regular(loader, 4 * 1024 * 1024, "loader")
    build_id, _ = elf_identity(data, library=False)
    return loader, data, build_id


def artifact(build: Path, root: Path, variant: str) -> tuple[dict, Path, tuple[str, ...]]:
    if variant not in ("control", "patched"):
        raise ProbeFailure("paired_provenance", "artifact_identity")
    contents = read_regular(build / TARGET, MAX_LIBRARY, variant + "_artifact")
    build_id, needed = elf_identity(contents, library=True)
    destination = root / variant
    destination.mkdir(mode=0o700)
    path = destination / LIBRARY
    with path.open("xb") as output:
        output.write(contents)
    path.chmod(0o500)
    (destination / SONAME).symlink_to(LIBRARY)
    metadata = {"schema": 1, "variant": variant, "library_path": str(path.resolve(strict=True)),
                "sha256": digest(contents), "build_id": build_id}
    metadata_path = destination / "artifact.json"
    with metadata_path.open("x", encoding="ascii") as output:
        json.dump(metadata, output, sort_keys=True)
    metadata_path.chmod(0o600)
    return metadata, metadata_path, needed


def prepare_clients(root: Path, run_command) -> PreparedClients:
    """Build one control and one patched client, never launch or install them.

    The callback's timeout is a total wall-clock deadline, not an idle timeout.
    It must bound output while draining and complete owned-child cleanup before
    returning/raising, including interrupts. Files remain private for caller
    cleanup on either outcome. The whole preparation has a 15-minute budget.
    """
    stage = "eligibility"
    try:
        check_eligibility(dict(os.environ))
        root = private_root(root)
        environment = clean_environment(dict(os.environ))
        deadline = time.monotonic() + 900
        tools = {name: trusted_tool(name) for name in ("dpkg-query", "curl", "patch", "meson", "ninja", "cc", "pkg-config")}
        def run(label, argv, *, cwd=root, timeout=30, limit=64 * 1024):
            nonlocal stage
            stage = label
            remaining = int(deadline - time.monotonic())
            if remaining < 1:
                raise ProbeFailure(stage, "deadline")
            try:
                output = run_command(stage, argv, cwd=cwd, env=dict(environment),
                                     timeout=min(timeout, remaining), output_limit=limit)
            except Exception as error:
                reason = "deadline" if isinstance(error, TimeoutError) or type(error).__name__ == "TimeoutExpired" else "command"
                raise ProbeFailure(stage, reason, getattr(error, "returncode", None)) from None
            if type(output) is not bytes or len(output) > limit:
                raise ProbeFailure(stage, "output_bound")
            return output
        query = "${Package}\t${Version}\t${source:Package}\t${source:Version}\t${Architecture}\t${db:Status-Status}\n"
        validate_packages(run("packages", [tools["dpkg-query"], "--show", "--showformat=" + query, *PACKAGES]))
        stage = "loader"
        loader, loader_bytes, loader_build_id = loader_identity(run, tools["dpkg-query"])
        workspace = root / "pipewire-client-probe"
        workspace.mkdir(mode=0o700)
        payloads = {}
        for key, (name, _, bound) in ARCHIVES.items():
            payloads[key] = verified_archive(run("download", [tools["curl"], "--disable", "--fail", "--silent", "--show-error",
                "--proto", "=https", "--connect-timeout", "10", "--max-time", "30", "--max-filesize", str(bound),
                BASE_URL + name], timeout=35, limit=bound), key)
        stage = "extract"
        source = workspace / "source"
        source.mkdir(mode=0o700)
        extract_archive(payloads["original"], source, prefix="pipewire-1.0.5", strip_prefix=True)
        extract_archive(payloads["debian"], source, prefix="debian", strip_prefix=False)
        patches = patch_series(source)
        for path in patches:
            run("distro_patch", [tools["patch"], "--batch", "--forward", "--fuzz=0", "--get=0", "-p1", "--input", str(path)], cwd=source)
        if digest(read_regular(source / SOURCE_FILE, 256 * 1024, "distro_patch")) != DISTRO_SOURCE_SHA256:
            raise ProbeFailure("distro_patch", "source_identity")
        build = workspace / "build"
        run("configure", [tools["meson"], "setup", str(build), str(source), *CONFIGURE], timeout=120)
        configuration = read_regular(build / "config.h", 64 * 1024, "configure")
        recipe = read_regular(build / "build.ninja", 4 * 1024 * 1024, "configure")
        compile_argv = [tools["ninja"], "-C", str(build), "-j2", TARGET]
        run("control_build", compile_argv, timeout=300)
        control_objects = object_inventory(build)
        control, control_path, control_needed = artifact(build, workspace, "control")
        stage = "upstream_patch"
        apply_upstream_patch(source)
        run("patched_build", compile_argv, timeout=300)
        stage = "paired_provenance"
        if (read_regular(build / "config.h", 64 * 1024, stage) != configuration
                or read_regular(build / "build.ninja", 4 * 1024 * 1024, stage) != recipe):
            raise ProbeFailure(stage, "configuration_changed")
        patched_objects = object_inventory(build)
        verify_object_pair(control_objects, patched_objects)
        patched, patched_path, patched_needed = artifact(build, workspace, "patched")
        if (control["sha256"] == patched["sha256"] or control["build_id"] == patched["build_id"]
                or control_needed != patched_needed):
            raise ProbeFailure(stage, "artifact_identity")
        provenance = {"source_version": VERSION, "upstream_commit": COMMIT,
            "upstream_patch_sha256": UPSTREAM_PATCH_SHA256,
            "archives": {key: value[1] for key, value in ARCHIVES.items()},
            "distro_series_sha256": SERIES_SHA256, "control_source_sha256": DISTRO_SOURCE_SHA256,
            "patched_source_sha256": PATCHED_SOURCE_SHA256, "configuration_sha256": digest(configuration),
            "build_recipe_sha256": digest(recipe), "configure_options": list(CONFIGURE),
            "control_objects": control_objects, "patched_objects": patched_objects,
            "needed": list(control_needed), "loader_sha256": digest(loader_bytes), "loader_build_id": loader_build_id}
        return PreparedClients(control, patched, control_path, patched_path, loader, provenance)
    except ProbeFailure:
        raise
    except OSError:
        raise ProbeFailure(stage, "filesystem") from None
    except Exception:
        raise ProbeFailure(stage, "unexpected") from None
