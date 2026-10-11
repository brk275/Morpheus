"""Pinned, CI-only xdpw build for a SHM-only software Wayland display.

The three-line adaptation below is not shipped with Morpheus or installed in
the host. See docs/WAYLAND_FIXTURE.md for its provenance and evidence limits.
"""

from __future__ import annotations

import hashlib
import io
import os
from pathlib import Path, PurePosixPath
import tarfile
import urllib.request

COMMIT = "74428f2a8fa7f252e2a46fdf5b697536c66c8a1c"  # upstream v0.7.1
ARCHIVE_URL = f"https://codeload.github.com/emersion/xdg-desktop-portal-wlr/tar.gz/{COMMIT}"
ARCHIVE_SHA256 = "10050be0b2675ffe02f9d09102a23e972d170628049b91fff6b5433f5eab7a7e"
SOURCE_SHA256 = "6680ac11bcd349c7819853b57b27ea310606bdd92497f206d0f6f813777bae9a"
ARCHIVE_LIMIT = 2 * 1024 * 1024
EXTRACTED_LIMIT = 8 * 1024 * 1024
ORIGINAL_CHECK = (
    "\tif (cast->screencopy_frame_info[WL_SHM].format == DRM_FORMAT_INVALID ||\n"
    "\t\t\t(cast->ctx->state->screencast_version >= 3 &&\n"
    "\t\t\t cast->screencopy_frame_info[DMABUF].format == DRM_FORMAT_INVALID)) {"
)
SHM_CHECK = "\tif (cast->screencopy_frame_info[WL_SHM].format == DRM_FORMAT_INVALID) {"


def extract_verified_archive(data: bytes, destination: Path) -> None:
    if len(data) > ARCHIVE_LIMIT or hashlib.sha256(data).hexdigest() != ARCHIVE_SHA256:
        raise RuntimeError("Wayland fixture backend archive checksum mismatch")
    prefix = f"xdg-desktop-portal-wlr-{COMMIT}"
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        members = archive.getmembers()
        if len(members) > 500 or sum(member.size for member in members) > EXTRACTED_LIMIT:
            raise RuntimeError("Wayland fixture backend archive exceeds extraction bounds")
        paths = set()
        for member in members:
            path = PurePosixPath(member.name)
            if (
                path.is_absolute() or ".." in path.parts or not path.parts
                or path.parts[0] != prefix or member.name in paths
                or not (member.isdir() or member.isfile())
            ):
                raise RuntimeError("Wayland fixture backend archive contains an unsafe entry")
            paths.add(member.name)
        destination.mkdir(mode=0o700)
        for member in members:
            relative = PurePosixPath(member.name).relative_to(prefix)
            target = destination.joinpath(*relative.parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True, mode=0o700)
            else:
                target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                with archive.extractfile(member) as source, target.open("xb") as output:
                    output.write(source.read())
                target.chmod(0o700 if member.mode & 0o111 else 0o600)


def adapt_shm_check(source: Path) -> None:
    contents = source.read_bytes()
    if hashlib.sha256(contents).hexdigest() != SOURCE_SHA256:
        raise RuntimeError("Wayland fixture backend source checksum mismatch")
    text = contents.decode("utf-8")
    if text.count(ORIGINAL_CHECK) != 1:
        raise RuntimeError("Wayland fixture backend startup check did not match exactly")
    # The software allocator legitimately reports only SHM. Retain its valid
    # format check. Upstream 21288533ce0ad39ceed055ed0af457062515a392 replaced
    # this both-formats check during its buffer-constraints refactor; this is a
    # smaller adapted CI-only change, not a claim of an upstream backport.
    source.write_text(text.replace(ORIGINAL_CHECK, SHM_CHECK, 1), encoding="utf-8")


def prepare_backend(root: Path, run_command) -> Path:
    with urllib.request.urlopen(ARCHIVE_URL, timeout=30) as response:
        data = response.read(ARCHIVE_LIMIT + 1)
    source = root / "backend-source"
    extract_verified_archive(data, source)
    adapt_shm_check(source / "src" / "screencast" / "screencast.c")
    build = root / "backend-build"
    run_command("backend-configure", [
        "meson", "setup", str(build), str(source), "--buildtype=release",
        "--wrap-mode=nodownload", "-Dsd-bus-provider=libsystemd",
        "-Dsystemd=disabled", "-Dman-pages=disabled",
    ])
    run_command("backend-compile", ["meson", "compile", "-C", str(build), "--jobs", "2"])
    executable = build / "xdg-desktop-portal-wlr"
    if not executable.is_file() or not os.access(executable, os.X_OK):
        raise RuntimeError("Wayland fixture backend executable was not produced")
    return executable
