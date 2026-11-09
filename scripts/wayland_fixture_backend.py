"""Pinned, CI-only xdpw build for a SHM-only software Wayland display.

The SHM adaptation and bounded producer diagnostics are not shipped with
Morpheus or installed in the host. See docs/WAYLAND_FIXTURE.md for its provenance and evidence limits.
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

PRODUCER_SOURCE_SHA256 = "e5dd5063d50494efaf82295807b43b20a8c5e68a389e89dbcfca63a724d60d4f"
PRODUCER_BEGIN = (
    'void xdpw_pwr_enqueue_buffer(struct xdpw_screencast_instance *cast) {\n'
    '\tlogprint(TRACE, "pipewire: enqueueing buffer");'
)
PRODUCER_DIAGNOSTIC_BEGIN = PRODUCER_BEGIN + (
    '\n\t/* Morpheus CI only: fixed literals, at most 16 attempts per process. */'
    '\n\tstatic unsigned diagnostic_samples = 0;'
    '\n\tconst bool diagnose = diagnostic_samples < 16;'
    '\n\tif (diagnose) {'
    '\n\t\tdiagnostic_samples++;'
    '\n\t\tlogprint(TRACE, "%s", cast->current_frame.pw_buffer ?'
    '\n\t\t\t"morpheus-fixture-producer: buffer-present" :'
    '\n\t\t\t"morpheus-fixture-producer: buffer-absent");'
    '\n\t}'
)
PRODUCER_QUEUE = '\tpw_stream_queue_buffer(cast->stream, pw_buf);'
PRODUCER_INCLUDE = '#include <assert.h>'
PRODUCER_CLOCK_HELPER = '''static void morpheus_fixture_order(const char *event) {
	struct timespec now;
	if (clock_gettime(CLOCK_MONOTONIC_RAW, &now) != 0) {
		logprint(TRACE, "morpheus-fixture-order: clock-unavailable");
		return;
	}
	/* Match PipeWire 1.0.5 support/logger.c, including its folded seconds. */
	logprint(TRACE, "morpheus-fixture-order: raw=%05lu.%06lu event=%s",
		((unsigned long)now.tv_sec & 0x1FFFFFFF) % 100000,
		(unsigned long)now.tv_nsec / 1000, event);
}

'''
PRODUCER_DIAGNOSTIC_QUEUE = (
    '\tconst bool diagnostic_driving = diagnose && pw_stream_is_driving(cast->stream);'
    '\n\tif (diagnose) {'
    '\n\t\tlogprint(TRACE, "%s", buffer_corrupt ?'
    '\n\t\t\t"morpheus-fixture-producer: frame-corrupt" :'
    '\n\t\t\t"morpheus-fixture-producer: frame-valid");'
    '\n\t\tlogprint(TRACE, "%s", diagnostic_driving ?'
    '\n\t\t\t"morpheus-fixture-producer: queue-while-driving" :'
    '\n\t\t\t"morpheus-fixture-producer: queue-while-not-driving");'
    '\n\t}'
    '\n\tif (diagnose) { morpheus_fixture_order("queue-begin"); }'
    '\n\tconst int queue_result = pw_stream_queue_buffer(cast->stream, pw_buf);'
    '\n\tif (diagnose) {'
    '\n\t\tmorpheus_fixture_order(queue_result < 0 ? "queue-rejected" :'
    '\n\t\t\tbuffer_corrupt ? "queue-corrupt-accepted" : "queue-valid-accepted");'
    '\n\t\tlogprint(TRACE, "%s", queue_result < 0 ?'
    '\n\t\t\t"morpheus-fixture-producer: queue-rejected" :'
    '\n\t\t\t"morpheus-fixture-producer: queue-accepted");'
    '\n\t\tlogprint(TRACE, "morpheus-fixture-producer: queued frame=%s driving=%s result=%s",'
    '\n\t\t\tbuffer_corrupt ? "corrupt" : "valid", diagnostic_driving ? "yes" : "no",'
    '\n\t\t\tqueue_result < 0 ? "rejected" : "accepted");'
    '\n\t\tif (diagnostic_samples == 16) {'
    '\n\t\t\tlogprint(TRACE, "morpheus-fixture-producer: sample-limit");'
    '\n\t\t}'
    '\n\t}'
)


def add_producer_diagnostics(source: Path) -> None:
    """Observe the existing queue call once, without changing queue semantics."""
    contents = source.read_bytes()
    if hashlib.sha256(contents).hexdigest() != PRODUCER_SOURCE_SHA256:
        raise RuntimeError("Wayland fixture producer source checksum mismatch")
    text = contents.decode("utf-8")
    if any(text.count(anchor) != 1 for anchor in (PRODUCER_BEGIN, PRODUCER_QUEUE, PRODUCER_INCLUDE)):
        raise RuntimeError("Wayland fixture producer diagnostic anchors did not match exactly")
    text = text.replace(PRODUCER_INCLUDE, PRODUCER_INCLUDE + '\n#include <time.h>', 1)
    text = text.replace(PRODUCER_BEGIN, PRODUCER_CLOCK_HELPER + PRODUCER_DIAGNOSTIC_BEGIN, 1)
    text = text.replace(PRODUCER_QUEUE, PRODUCER_DIAGNOSTIC_QUEUE, 1)
    source.write_text(text, encoding="utf-8")

# CI-only fresh-cast initial snapshot; not an upstream backport.
INITIAL_HEADER_SHA256 = '11dcf2c033c22b4e83db900ac310004bee204ce215a43f16161e3dd3b6451ec3'
INITIAL_COPY_SHA256 = '788d334c6a7c315a741ef8caaafffe61ad80f0f1d7a78eaed96d993d9328c644'
INITIAL_PRODUCER_SHA256 = '9c3d1ef9d2b6a8ec46b0dc4a95f994905c759a194af4849214b501c2f92f7e36'
INITIAL_HEADER_ANCHOR = '\tbool initialized;'
INITIAL_COPY_ANCHOR = '\tzwlr_screencopy_frame_v1_copy_with_damage(frame, cast->current_frame.xdpw_buffer->buffer);'
INITIAL_QUEUE_ANCHOR = '\tconst int queue_result = pw_stream_queue_buffer(cast->stream, pw_buf);'
INITIAL_HEADER_REPLACEMENT = '\tbool initialized;\n\tbool morpheus_initial_frame_queued;'
INITIAL_COPY_REPLACEMENT = '\tif (cast->morpheus_initial_frame_queued) {\n\t\tzwlr_screencopy_frame_v1_copy_with_damage(frame, cast->current_frame.xdpw_buffer->buffer);\n\t} else {\n\t\tzwlr_screencopy_frame_v1_copy(frame, cast->current_frame.xdpw_buffer->buffer);\n\t}'
INITIAL_QUEUE_REPLACEMENT = '\tconst int queue_result = pw_stream_queue_buffer(cast->stream, pw_buf);\n\tif (!buffer_corrupt && queue_result >= 0) {\n\t\tcast->morpheus_initial_frame_queued = true;\n\t}'


def adapt_initial_frame(header: Path, capture: Path, producer: Path) -> None:
    """Request real pixels until the first valid producer queue is accepted."""
    inputs = (
        (header, INITIAL_HEADER_SHA256, INITIAL_HEADER_ANCHOR, INITIAL_HEADER_REPLACEMENT),
        (capture, INITIAL_COPY_SHA256, INITIAL_COPY_ANCHOR, INITIAL_COPY_REPLACEMENT),
        (producer, INITIAL_PRODUCER_SHA256, INITIAL_QUEUE_ANCHOR, INITIAL_QUEUE_REPLACEMENT),
    )
    changes = []
    for path, expected, anchor, replacement in inputs:
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != expected:
            raise RuntimeError("Wayland fixture initial-frame source checksum mismatch")
        text = data.decode("utf-8")
        if text.count(anchor) != 1:
            raise RuntimeError("Wayland fixture initial-frame anchor did not match exactly")
        changes.append((path, text.replace(anchor, replacement, 1)))
    # Check all three identities and anchors before changing any source file.
    for path, text in changes:
        path.write_text(text, encoding="utf-8")


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
    add_producer_diagnostics(source / "src" / "screencast" / "pipewire_screencast.c")
    adapt_initial_frame(source / "include" / "screencast_common.h",
                        source / "src" / "screencast" / "wlr_screencast.c",
                        source / "src" / "screencast" / "pipewire_screencast.c")
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
