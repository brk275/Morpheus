"""Prepare the pinned fixed PipeWire client for explicit native/AppImage builds.

This module does not download executables, launch apps, set process-global loader
variables, or enable packaging. The Linux packaging adapter must own subprocess
containment, private bounded logs and installed acceptance before enabling it.
Native uses a distinct SONAME; the original AppImage deployment stays separate.
"""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shlex
import stat
import sys
import time


ROOT = Path(__file__).resolve().parents[2]
_SPEC = importlib.util.spec_from_file_location(
    "morpheus_pipewire_recipe", ROOT / "scripts/pipewire_startup_probe.py"
)
R = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = R
_SPEC.loader.exec_module(R)

SONAME = R.SONAME
DESTINATION = "usr/lib/" + SONAME
NOTICE_DIR = "usr/share/doc/morpheus-app/pipewire-client"
LICENSES = {
    "LICENSE": "be4be5d77424833edf31f53fc1f1cecb6996b9e2d747d9e6fb8f878362ebc92b",
    "COPYING": "8909c319a7e27dbb33a15b9035f89ab3b7b2f6a12f8bcddc755206a8db1ada44",
}
TOOL_NAMES = frozenset({"linuxdeploy-x86_64.AppImage",
                       "linuxdeploy-plugin-appimage.AppImage", "runtime-x86_64"})


class PackageFailure(RuntimeError):
    """Fixed public message; do not expose filenames, logs or child exceptions."""

    def __init__(self, reason):
        allowed = {"private_root", "input", "tool", "deadline", "command", "output",
                   "source", "license", "client", "configuration", "deployment"}
        self.reason = reason if reason in allowed else "input"
        super().__init__("AppImage client preparation failed: " + self.reason)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def read_regular(path, bound):
    try:
        return R.read_regular(Path(path), bound, "paired_provenance")
    except (OSError, R.ProbeFailure):
        raise PackageFailure("input") from None


def private_directory(path):
    path = Path(path).absolute()
    info = path.lstat()
    if (path.resolve() != path or not stat.S_ISDIR(info.st_mode)
            or info.st_uid != os.getuid() or info.st_mode & 0o077
            or ROOT == path or ROOT in path.parents or path in ROOT.parents):
        raise PackageFailure("private_root")
    return path


def require_build_platform(os_release=None):
    if (sys.platform != "linux" or platform.machine() != "x86_64"
            or os.getuid() == 0 or os.getuid() != os.geteuid()):
        raise PackageFailure("configuration")
    if os_release is None:
        os_release = read_regular(Path("/etc/os-release").resolve(strict=True), 16384).decode("ascii")
    fields = dict(re.findall(r'^([A-Z_]+)="?([^"\n]+)"?$', os_release, re.MULTILINE))
    if fields.get("ID") != "ubuntu" or fields.get("VERSION_ID") != "24.04":
        raise PackageFailure("configuration")


def write_new(path, data, mode=0o600):
    with Path(path).open("xb") as output:
        output.write(data)
    Path(path).chmod(mode)


def pinned_tools():
    data = json.loads(read_regular(Path(__file__).with_name("appimage-tools.json"), 16384))
    tools = data.get("tools", [])
    if (data.get("schema") != 1 or data.get("cargo_packager") != "0.11.8"
            or len(tools) != 3 or {item.get("cache_name") for item in tools} != TOOL_NAMES):
        raise PackageFailure("tool")
    for item in tools:
        if (not re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", ""))
                or type(item.get("size")) is not int or not 1 <= item["size"] <= 32 * 1024 * 1024
                or type(item.get("asset_id")) is not int or item["asset_id"] <= 0
                or not item.get("url", "").startswith("https://github.com/")):
            raise PackageFailure("tool")
    return tools


def seed_tools(root, payloads):
    """Verify all three authoritative assets before writing any executable.

    Caller supplies bounded downloaded bytes. A changed continuous-tag asset
    fails its pinned digest; there is no latest-version or cache fallback.
    Cargo Packager mutates three linuxdeploy header bytes: only this new private
    invocation copy may be changed. Never share it with a persistent cache.
    """
    root = private_directory(root)
    pins = pinned_tools()
    if set(payloads) != TOOL_NAMES:
        raise PackageFailure("tool")
    for pin in pins:
        data = payloads[pin["cache_name"]]
        if type(data) is not bytes or len(data) != pin["size"] or sha256(data) != pin["sha256"]:
            raise PackageFailure("tool")
    cache = root / "xdg-cache"
    directory = cache / ".cargo-packager/AppImage"
    try:
        # Every ancestor is exclusively created. mkdir(parents=True) would
        # follow a pre-existing symlink underneath the otherwise private root.
        cache.mkdir(mode=0o700)
        (cache / ".cargo-packager").mkdir(mode=0o700)
        directory.mkdir(mode=0o700)
    except OSError:
        raise PackageFailure("tool") from None
    for pin in pins:
        write_new(directory / pin["cache_name"], payloads[pin["cache_name"]], 0o700)
    # Cargo Packager copies this original source launcher to AppDir/AppRun.
    # There is no opaque provider AppRun or ambient LD_LIBRARY_PATH injection.
    write_new(directory / "AppRun-x86_64", launcher_bytes(), 0o700)
    # The runtime pin prevents appimagetool from choosing/downloading its own
    # runtime. It is an explicit per-child plugin argument, not a loader override.
    return {"XDG_CACHE_HOME": str(cache), "LDAI_RUNTIME_FILE": str(directory / "runtime-x86_64")}


def launcher_bytes():
    data = read_regular(Path(__file__).with_name("AppRun"), 4096)
    if (not data.startswith(b"#!/bin/sh\n") or not data.endswith(b'exec "$MORPHEUS_EXECUTABLE" "$@"\n')
            or b"LD_LIBRARY_PATH=" in data or b"LD_PRELOAD=" in data or b"eval " in data):
        raise PackageFailure("tool")
    return data


def verify_tools_after(root, original_payloads):
    """Only Cargo Packager's documented three-byte header edit is accepted."""
    root = private_directory(root)
    directory = root / "xdg-cache/.cargo-packager/AppImage"
    for ancestor in (root / "xdg-cache", root / "xdg-cache/.cargo-packager", directory):
        try:
            info = ancestor.lstat()
        except OSError:
            raise PackageFailure("tool") from None
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or ancestor.resolve(strict=True) != ancestor):
            raise PackageFailure("tool")
    receipts = {}
    launcher = directory / "AppRun-x86_64"
    launcher_info = launcher.lstat()
    if (not stat.S_ISREG(launcher_info.st_mode) or launcher_info.st_uid != os.getuid()
            or stat.S_IMODE(launcher_info.st_mode) != 0o700 or launcher_info.st_nlink != 1
            or read_regular(launcher, 4096) != launcher_bytes()):
        raise PackageFailure("tool")
    receipts["AppRun-x86_64"] = sha256(launcher_bytes())
    for pin in pinned_tools():
        name = pin["cache_name"]
        original = original_payloads[name]
        if len(original) != pin["size"] or sha256(original) != pin["sha256"]:
            raise PackageFailure("tool")
        path = directory / name
        try:
            info = path.lstat()
        except OSError:
            raise PackageFailure("tool") from None
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o700 or info.st_nlink != 1):
            raise PackageFailure("tool")
        actual = read_regular(path, pin["size"])
        expected = original[:8] + bytes(3) + original[11:] if name == "linuxdeploy-x86_64.AppImage" else original
        if actual != expected:
            raise PackageFailure("tool")
        receipts[name] = sha256(actual)
    return receipts


NATIVE_MESON_SHA256 = "edf88a811c34f4f301d30321128bb23570d6ed39cd9c4db47dd0f0982c091639"
NATIVE_MESON_PATCHED_SHA256 = "01e57c7f2b10a615cb07f64c2dbf3eac63383c1bcd242c36882aecfbd8c7b4ce"


def native_library_name(source):
    # Native source builds need an ELF dependency stock PipeWire cannot satisfy.
    # Only the target name changes; headers, protocol, C sources and plugins do
    # not. AppImage keeps its original name and relocated deployment contract.
    path = source / "src/pipewire/meson.build"
    original = read_regular(path, 16384)
    before = b"libpipewire = shared_library(pipewire_name, pipewire_sources,"
    after = b"libpipewire = shared_library('morpheus-pipewire-0.3', pipewire_sources,"
    if sha256(original) != NATIVE_MESON_SHA256 or original.count(before) != 1:
        raise PackageFailure("source")
    modified = original.replace(before, after, 1)
    if sha256(modified) != NATIVE_MESON_PATCHED_SHA256:
        raise PackageFailure("source")
    path.write_bytes(modified)


def prepare_client(root, archives, upstream_patch, run_command, *, variant):
    """Build only the pinned fixed client; never install anything globally.

    ``run_command(stage, argv, cwd=, env=, timeout=, output_limit=)`` must drain
    bounded private output and kill/reap its owned process tree before returning
    or raising, including on interrupts. All commands share a ten-minute budget.
    This is a recipe with pinned sources, not a claim of bit-reproducible output:
    exact tool versions/configuration/object hashes are retained for comparison.
    """
    if variant not in ("appimage", "native"):
        raise PackageFailure("configuration")
    require_build_platform()
    root = private_directory(root)
    if set(archives) != set(R.ARCHIVES):
        raise PackageFailure("source")
    if (type(upstream_patch) is not bytes or len(upstream_patch) > 32768
            or sha256(upstream_patch) != R.UPSTREAM_PATCH_SHA256):
        raise PackageFailure("source")
    try:
        verified = {key: R.verified_archive(archives[key], key) for key in R.ARCHIVES}
        environment = R.clean_environment(dict(os.environ))
        tools = {name: R.trusted_tool(name) for name in ("patch", "meson", "ninja", "cc", "pkg-config")}
        deadline = time.monotonic() + 600

        def run(stage, command, *, cwd=root, budget=60, bound=256 * 1024):
            remaining = deadline - time.monotonic()
            if remaining < 1:
                raise PackageFailure("deadline")
            try:
                output = run_command(stage, command, cwd=cwd, env=dict(environment),
                                     timeout=min(budget, remaining), output_limit=bound)
            except Exception as error:
                reason = "deadline" if isinstance(error, TimeoutError) or type(error).__name__ == "TimeoutExpired" else "command"
                raise PackageFailure(reason) from None
            if type(output) is not bytes or len(output) > bound:
                raise PackageFailure("output")
            return output

        versions = {name: sha256(run("tool-version", [path, "--version"], bound=16384))
                    for name, path in tools.items()}
        source = root / "source"
        source.mkdir(mode=0o700)
        R.extract_archive(verified["original"], source, prefix="pipewire-1.0.5", strip_prefix=True)
        R.extract_archive(verified["debian"], source, prefix="debian", strip_prefix=False)
        for patch in R.patch_series(source):
            run("distro-patch", [tools["patch"], "--batch", "--forward", "--fuzz=0", "--get=0", "-p1", "--input", str(patch)], cwd=source)
        R.apply_upstream_patch(source)
        if variant == "native":
            native_library_name(source)
        target = R.NATIVE_TARGET if variant == "native" else R.TARGET
        soname = R.NATIVE_SONAME if variant == "native" else SONAME
        build = root / "build"
        # Strip source/build path differences from file/debug macros. No host
        # compiler flags or package-loader overrides are accepted.
        environment["CFLAGS"] = " ".join(shlex.quote(f"{flag}={root}=/usr/src/morpheus-pipewire")
                                         for flag in ("-ffile-prefix-map", "-fdebug-prefix-map"))
        run("configure", [tools["meson"], "setup", str(build), str(source), *R.CONFIGURE], budget=120)
        run("client-build", [tools["ninja"], "-C", str(build), "-j2", target], budget=300)
        library = read_regular(build / target, R.MAX_LIBRARY)
        build_id, needed = R.elf_identity(library, library=True, expected_soname=soname)
        objects = R.object_inventory(build, target=target)
        stage = root / "stage"
        stage.mkdir(mode=0o700)
        client = stage / soname
        write_new(client, library, 0o500)
        if variant == "native":
            # The Rust sys crate still links -lpipewire-0.3. GNU ld reads the
            # distinct DT_SONAME from this file, so it cannot fall back to stock.
            (stage / "libpipewire-0.3.so").symlink_to(soname)
            (stage / SONAME).symlink_to(soname)
        for name, expected in LICENSES.items():
            data = read_regular(source / name, 8192)
            if sha256(data) != expected:
                raise PackageFailure("license")
            write_new(stage / name, data)
        # Ship exact original source inputs and upstream patch identity alongside
        # notices; these are source archives, never executable package tools.
        for key, data in verified.items():
            write_new(stage / R.ARCHIVES[key][0], data)
        write_new(stage / "exported-node-startup.patch", upstream_patch)
        metadata = {"schema": 1, "source_version": R.VERSION, "upstream_commit": R.COMMIT,
                    "upstream_patch_sha256": R.UPSTREAM_PATCH_SHA256,
                    "patched_source_sha256": R.PATCHED_SOURCE_SHA256,
                    "source_archives": {key: value[1] for key, value in R.ARCHIVES.items()},
                    "source_client_sha256": sha256(library), "build_id": build_id,
                    "needed": list(needed), "objects": objects, "tool_version_output_sha256": versions,
                    "configure": list(R.CONFIGURE),
                    "config_sha256": sha256(read_regular(build / "config.h", 65536)),
                    "recipe_sha256": sha256(read_regular(build / "build.ninja", 4 * 1024 * 1024))}
        if variant == "native":
            metadata.update(variant="native", soname=soname,
                            native_meson_sha256=NATIVE_MESON_PATCHED_SHA256)
        write_new(stage / "SOURCE.json", json.dumps(metadata, sort_keys=True, indent=2).encode() + b"\n")
        return stage, metadata
    except R.ProbeFailure:
        raise PackageFailure("source") from None
    except OSError:
        raise PackageFailure("input") from None



def build_receipt(stage, metadata, *, variant):
    """Explicit build input; not a signature or proof against its owner changing it.

    The native link uses a unique SONAME. AppImage validates its original client
    here, then keeps its existing deployment-time byte/build-ID measurement.
    """
    if variant not in ("appimage", "native"):
        raise PackageFailure("configuration")
    stage = private_directory(stage)
    soname = R.NATIVE_SONAME if variant == "native" else SONAME
    data = read_regular(stage / soname, R.MAX_LIBRARY)
    identity, needed = R.elf_identity(data, library=True, expected_soname=soname)
    if (metadata.get("schema") != 1
            or sha256(data) != metadata.get("source_client_sha256")
            or identity != metadata.get("build_id") or list(needed) != metadata.get("needed")
            or metadata.get("source_version") != R.VERSION
            or metadata.get("upstream_patch_sha256") != R.UPSTREAM_PATCH_SHA256
            or metadata.get("upstream_commit") != R.COMMIT
            or metadata.get("patched_source_sha256") != R.PATCHED_SOURCE_SHA256
            or metadata.get("source_archives") != {k: v[1] for k, v in R.ARCHIVES.items()}
            or variant == "native" and (metadata.get("variant") != "native"
                or metadata.get("soname") != soname
                or metadata.get("native_meson_sha256") != NATIVE_MESON_PATCHED_SHA256)):
        raise PackageFailure("client")
    values = {"schema": "1", "variant": variant, "soname": soname,
              "source_version": R.VERSION, "upstream_commit": R.COMMIT,
              "upstream_patch_sha256": R.UPSTREAM_PATCH_SHA256,
              "patched_source_sha256": R.PATCHED_SOURCE_SHA256,
              "original_archive_sha256": R.ARCHIVES["original"][1],
              "debian_archive_sha256": R.ARCHIVES["debian"][1],
              "dsc_sha256": R.ARCHIVES["dsc"][1],
              "sha256": sha256(data), "build_id": identity,
              "native_meson_sha256": NATIVE_MESON_PATCHED_SHA256 if variant == "native" else "none"}
    receipt = stage / "build-client.txt"
    write_new(receipt, "".join(k + "=" + v + "\n" for k, v in sorted(values.items())).encode("ascii"), 0o400)
    if variant == "native":
        write_new(stage / "artifact.json", json.dumps({
            "schema": 1, "variant": "native", "library_path": str(stage / soname),
            "sha256": sha256(data), "build_id": identity,
        }, sort_keys=True).encode("ascii") + b"\n", 0o400)
    return receipt


def appimage_config(base, stage):
    """Add the fixed client only to AppImage custom files; DEB stays unchanged."""
    config = copy.deepcopy(base)
    if "appimage" in config:
        raise PackageFailure("configuration")
    stage = Path(stage)
    files = {str(stage / SONAME): DESTINATION}
    for name in [*LICENSES, "SOURCE.json", "exported-node-startup.patch", *(item[0] for item in R.ARCHIVES.values())]:
        files[str(stage / name)] = NOTICE_DIR + "/" + name
    # The source is staged directly under its SONAME. No custom-file symlink is
    # used: Cargo Packager follows symlinks when copying appimage.files.
    config["appimage"] = {"files": files, "excluded-libs": ["libpipewire-0.3.so*"]}
    return config


def verify_deployed_client(appdir, source_metadata):
    """Identify the exact post-linuxdeploy library before installed acceptance.

    Pinned linuxdeploy adds local RUNPATH, so SHA need not equal input SHA.
    Build ID/SONAME/NEEDED must match, and only literal $ORIGIN RUNPATH is allowed.
    No empty/current-directory, absolute or parent path search entries are allowed.
    """
    appdir = Path(appdir).resolve(strict=True)
    expected = appdir / DESTINATION
    paths = list(appdir.rglob("libpipewire-0.3.so*"))
    if paths != [expected] or expected.is_symlink():
        raise PackageFailure("deployment")
    if expected.resolve(strict=True) != expected:
        raise PackageFailure("deployment")
    contents = read_regular(expected, R.MAX_LIBRARY)
    try:
        build_id, needed = R.elf_identity(contents, library=True, allow_origin_runpath=True)
    except R.ProbeFailure:
        raise PackageFailure("deployment") from None
    if build_id != source_metadata["build_id"] or list(needed) != source_metadata["needed"]:
        raise PackageFailure("deployment")
    info = expected.stat()
    if info.st_mode & 0o022:
        raise PackageFailure("deployment")
    return {"schema": 1, "relative_path": DESTINATION, "sha256": sha256(contents),
            "build_id": build_id, "needed": list(needed),
            "source_client_sha256": source_metadata["source_client_sha256"]}
