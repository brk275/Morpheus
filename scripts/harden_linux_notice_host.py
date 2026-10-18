#!/usr/bin/env python3
"""Tighten notice-tree permissions on a fresh GitHub-hosted Ubuntu 24.04 runner.

The installed-notice inventory (packaging/linux/appimage_notices.py) accepts a
copyright or common-license file only when every component of its path is
owned by root and, unless it is a symlink, not group- or world-writable. The
hosted Ubuntu 24.04 image ships /usr/share, /usr/share/doc and
/usr/share/common-licenses group- and world-writable, so CI runs this as root
before any other checkout code.

It refuses anything but that exact hosted image with real and effective root.
It verifies that / and /usr are root-owned directories nobody else can write,
then removes only the group and world write bits from /usr/share itself and
from each directory and regular file of the doc and common-licenses trees.
Every entry, owner, content and other mode bit stays. Each step opens one name
beneath an already open directory with O_NOFOLLOW and checks the descriptor
with fstat before fchmod, so no symlink is ever followed or changed. It refuses
an entry owned by another user, any other file type, a mount crossing or an
entry replaced while it runs, within fixed time, depth and entry bounds. It
also refuses a writable regular file with more than one hard link before
changing it, since its mode is shared with aliases that may lie outside these
trees; an already tight one is left alone. After each fchmod it rechecks the
descriptor's identity, owner, link count and exact mode.
Output is one JSON line of fixed tokens and counts, never a name or path.

This trusts the official hosted image as it is before candidate code runs.
Tightening permissions is not a repair for a tampered host. Importing this
module changes nothing.
"""

import contextlib
import errno
import json
import os
import re
import stat
import sys
import time

CHECK = "linux_notice_host_hardening"
HOSTED = (("GITHUB_ACTIONS", "true"), ("RUNNER_ENVIRONMENT", "github-hosted"), ("RUNNER_OS", "Linux"))
IMAGE = "ubuntu24"
RELEASE = (("ID", "ubuntu"), ("VERSION_ID", "24.04"))
OS_RELEASE = "/etc/os-release"
OS_RELEASE_LIMIT = 16 * 1024
ROOT = "/"
# Fixed names beneath /usr/share and the stage each one reports.
TREES = (("doc", "doc"), ("common-licenses", "common_licenses"))
WRITE_BITS = stat.S_IWGRP | stat.S_IWOTH
# Directory levels in each tree, counting the tree root itself as 1.
MAX_DEPTH = 32
MAX_ENTRIES = 250_000
MAX_SECONDS = 90.0
# Listed entries plus /usr/share and the two tree roots, which are not listed.
MAX_COUNT = MAX_ENTRIES + 3

# Statuses are hardened, surveyed, refused (guard only) and failed.
STAGES = ("guard", "ancestors", "share", "doc", "common_licenses", "complete")
GUARD_REASONS = ("arguments", "platform", "hosted", "image", "arch", "release", "root")
PATH_REASONS = ("missing", "symlink", "type", "owner", "mode", "mount", "replaced", "hardlink",
                "depth", "count", "time", "access", "io", "verify", "internal")
# Counts cover /usr/share and both trees; symlinks are counted, never opened.
COUNTS = ("directories", "files", "symlinks", "linked_files",
          "group_writable", "world_writable", "tightened")
# Each of these counts at most one per directory or regular file.
PER_ENTRY = ("group_writable", "world_writable", "tightened")
KEYS = frozenset(("check", "status", "stage", "reason", *COUNTS))
FALLBACK = {"check": CHECK, "status": "failed"}
ERRNO_REASONS = {errno.ENOENT: "missing", errno.ELOOP: "replaced", errno.ENOTDIR: "replaced",
                 errno.EACCES: "access", errno.EPERM: "access"}


class Failure(Exception):
    """A closed reason; never carries a name, path or OS message."""

    def __init__(self, reason):
        super().__init__(reason)
        self.reason = reason


def os_release(path=OS_RELEASE):
    """Bounded distribution release fields; empty when unreadable."""
    try:
        with open(path, "rb") as handle:
            data = handle.read(OS_RELEASE_LIMIT + 1)
        text = data.decode("utf-8") if len(data) <= OS_RELEASE_LIMIT else ""
    except (OSError, UnicodeError):
        return {}
    return dict(re.findall(r'^([A-Z_]+)="?([^"\n]*)"?$', text, re.MULTILINE))


def hosted_reason(environ=None, system=None, release=None):
    """None on the exact standard hosted image, else a closed reason. Read-only."""
    if sys.platform != "linux":
        return "platform"
    environ = os.environ if environ is None else environ
    system = os.uname() if system is None else system
    if system.sysname != "Linux":
        return "platform"
    if any(environ.get(name) != value for name, value in HOSTED):
        return "hosted"
    if environ.get("ImageOS") != IMAGE:
        return "image"
    if system.machine != "x86_64":
        return "arch"
    fields = os_release() if release is None else release
    if any(fields.get(name) != value for name, value in RELEASE):
        return "release"
    return None


def guard_reason(environ=None, system=None, release=None, uids=None):
    """hosted_reason, then real and effective root. Changes nothing."""
    reason = hosted_reason(environ, system, release)
    if reason is not None:
        return reason
    real, effective = (os.getuid(), os.geteuid()) if uids is None else uids
    return None if real == 0 and effective == 0 else "root"


def _flags(directory):
    # Built at call time so importing works where these flags do not exist.
    base = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC
    # A nonblocking open cannot wait on a FIFO swapped in after the check.
    return (base | os.O_DIRECTORY) if directory else (base | os.O_NONBLOCK | os.O_NOCTTY)


def _identity(info):
    return info.st_dev, info.st_ino, stat.S_IFMT(info.st_mode)


def _same(before, after):
    """The opened descriptor is still the entry its name was checked as."""
    if _identity(before) != _identity(after):
        raise Failure("replaced")


def result(status, stage, reason, counts=None):
    value = {"check": CHECK, "status": status, "stage": stage, "reason": reason}
    value.update(dict.fromkeys(COUNTS, 0) if counts is None else counts)
    return value


class Walk:
    """One bounded pass over the fixed notice paths.

    With fix it removes group and world write bits; only main() constructs it
    that way, after guard_reason() passes. Without fix it only counts. Tests
    point root and owner at a temporary tree; production always uses / and 0.
    """

    def __init__(self, fix, *, root=ROOT, owner=0, depth=MAX_DEPTH, entries=MAX_ENTRIES,
                 seconds=MAX_SECONDS, clock=time.monotonic):
        self.fix = fix
        self.root = root
        self.owner = owner
        self.depth = depth
        self.entries = entries
        self.clock = clock
        self.deadline = clock() + seconds
        self.listed = 0
        self.device = None
        self.stage = "ancestors"
        self.counts = dict.fromkeys(COUNTS, 0)

    def report(self):
        """Run once; every descriptor is closed before this returns."""
        try:
            self.run()
            reason = None
        except Failure as failure:
            reason = failure.reason
        except OSError as error:
            reason = ERRNO_REASONS.get(error.errno, "io")
        except Exception:
            reason = "internal"
        status = ("hardened" if self.fix else "surveyed") if reason is None else "failed"
        return result(status, self.stage, reason, dict(self.counts))

    def run(self):
        with contextlib.ExitStack() as opened:
            root = os.open(self.root, _flags(True))
            opened.callback(os.close, root)
            self.ancestor(os.fstat(root))
            usr, info = self.directory(root, "usr", opened)
            self.ancestor(info)
            self.stage = "share"
            share, info = self.directory(usr, "share", opened)
            self.device = info.st_dev
            # Nobody else can rename or replace a tree once this is tight.
            self.settle(share, info)
            for name, stage in TREES:
                self.stage = stage
                tree, info = self.directory(share, name, opened)
                if info.st_dev != self.device:
                    raise Failure("mount")
                self.settle(tree, info)
                self.descend(tree, 1)
        self.stage = "complete"

    def tick(self):
        if self.clock() > self.deadline:
            raise Failure("time")

    def ancestor(self, info):
        """/ and /usr: root-owned directories nobody else can write. Never changed."""
        if not stat.S_ISDIR(info.st_mode):
            raise Failure("type")
        if info.st_uid != self.owner:
            raise Failure("owner")
        if info.st_mode & WRITE_BITS:
            raise Failure("mode")

    def directory(self, parent, name, opened):
        """A fixed directory beneath parent; never a symlink or another owner's."""
        before = os.stat(name, dir_fd=parent, follow_symlinks=False)
        if stat.S_ISLNK(before.st_mode):
            raise Failure("symlink")
        if not stat.S_ISDIR(before.st_mode):
            raise Failure("type")
        handle = os.open(name, _flags(True), dir_fd=parent)
        opened.callback(os.close, handle)
        info = os.fstat(handle)
        _same(before, info)
        if info.st_uid != self.owner:
            raise Failure("owner")
        return handle, info

    def names(self, handle):
        """An open directory's names, read once within the entry and time bounds."""
        found = []
        with os.scandir(handle) as entries:
            for entry in entries:
                self.listed += 1
                if self.listed > self.entries:
                    raise Failure("count")
                self.tick()
                found.append(entry.name)
        return found

    def descend(self, parent, depth):
        """Each entry of an open directory at depth (a tree root is 1), depth first.

        With fix the directory is already tight. A child directory beyond the
        depth bound is refused before it is opened or changed.
        """
        for name in self.names(parent):
            self.tick()
            before = os.stat(name, dir_fd=parent, follow_symlinks=False)
            if before.st_uid != self.owner:
                raise Failure("owner")
            if before.st_dev != self.device:
                raise Failure("mount")
            if stat.S_ISLNK(before.st_mode):
                self.counts["symlinks"] += 1
                continue
            directory = stat.S_ISDIR(before.st_mode)
            if not directory and not stat.S_ISREG(before.st_mode):
                raise Failure("type")
            if directory and depth + 1 > self.depth:
                raise Failure("depth")
            if not directory and not self.fix:
                self.settle(None, before)
                continue
            handle = os.open(name, _flags(directory), dir_fd=parent)
            try:
                info = os.fstat(handle)
                _same(before, info)
                if info.st_uid != self.owner:
                    raise Failure("owner")
                self.settle(handle, info)
                if directory:
                    self.descend(handle, depth + 1)
            finally:
                os.close(handle)

    def settle(self, handle, info):
        """Count the entry and its write bits; with fix, remove and confirm them.

        The deadline is checked before every change, so an expired budget
        changes nothing. A writable regular file with another hard link is
        refused unchanged: its mode is shared with every alias.
        """
        self.tick()
        regular = stat.S_ISREG(info.st_mode)
        self.counts["files" if regular else "directories"] += 1
        if regular and info.st_nlink > 1:
            self.counts["linked_files"] += 1
        mode = stat.S_IMODE(info.st_mode)
        self.counts["group_writable"] += 1 if mode & stat.S_IWGRP else 0
        self.counts["world_writable"] += 1 if mode & stat.S_IWOTH else 0
        if not self.fix or not mode & WRITE_BITS:
            return
        if regular and info.st_nlink > 1:
            raise Failure("hardlink")
        wanted = mode & ~WRITE_BITS
        os.fchmod(handle, wanted)
        after = os.fstat(handle)
        if (_identity(after) != _identity(info) or after.st_uid != self.owner
                or stat.S_IMODE(after.st_mode) != wanted
                or (regular and after.st_nlink != 1)):
            raise Failure("verify")
        self.counts["tightened"] += 1


def survey(**options):
    """Read-only pass over the same paths; needs no root. Opens no regular file."""
    return Walk(False, **options).report()


def public(report):
    """Only a consistent report of fixed keys, tokens and bounded counts is printed."""
    if type(report) is not dict or set(report) != KEYS or report["check"] != CHECK:
        return False
    counts = {name: report[name] for name in COUNTS}
    if not all(type(value) is int and 0 <= value <= MAX_COUNT for value in counts.values()):
        return False
    entries = counts["directories"] + counts["files"]
    if (entries + counts["symlinks"] > MAX_COUNT or counts["linked_files"] > counts["files"]
            or any(counts[name] > entries for name in PER_ENTRY)):
        return False
    status, stage, reason = report["status"], report["stage"], report["reason"]
    if status in ("hardened", "surveyed"):
        return (stage == "complete" and reason is None
                and (status == "hardened" or counts["tightened"] == 0))
    if status == "refused":
        return stage == "guard" and reason in GUARD_REASONS and not any(counts.values())
    if status == "failed":
        if stage == "guard":
            return reason == "internal" and not any(counts.values())
        return stage in STAGES and stage != "complete" and reason in PATH_REASONS
    return False


def main(argv=None):
    arguments = sys.argv[1:] if argv is None else argv
    try:
        reason = "arguments" if arguments else guard_reason()
        report = result("refused", "guard", reason) if reason else Walk(True).report()
    except Exception:
        report = result("failed", "guard", "internal")
    valid = public(report)
    print(json.dumps(report if valid else FALLBACK, sort_keys=True), flush=True)
    return 0 if valid and report["status"] == "hardened" else 1


if __name__ == "__main__":
    raise SystemExit(main())
