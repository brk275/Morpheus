"""Private fixtures and complete owned-log scanning outside source checkouts."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parent.parent


def external_path(path: Path) -> Path:
    """Reject checkout paths, including existing symlinks into the checkout."""
    resolved = path.expanduser().resolve()
    if resolved.is_relative_to(REPOSITORY):
        raise ValueError("Disposable fixture data must be stored outside the repository")
    return resolved


def fixture_home() -> Path:
    return external_path(Path(os.environ.get(
        "MORPHEUS_MATRIX_TEST_HOME",
        str(Path(tempfile.gettempdir()) / "morpheus-matrix-test"),
    )))


def private_directory(path: Path) -> Path:
    path = external_path(path)
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(path, 0o700)
    return path


def write_private(path: Path, content: str, *, create: bool = False) -> None:
    """Atomically write owner-only data without following a destination link."""
    external_path(path)
    if path.is_symlink():
        raise ValueError("Refusing to write fixture data through a symbolic link")
    parent = private_directory(path.parent)
    fd, name = tempfile.mkstemp(prefix=".fixture-", dir=parent)
    temporary = Path(name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as file:
            file.write(content)
        os.chmod(temporary, 0o600)
        external_path(path)
        if create:
            os.link(temporary, path)
        else:
            temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


class PrivateLogScanError(ValueError):
    def __init__(self, *, process_stopped=True, interruption=None):
        self.process_stopped = process_stopped is True
        self.interruption = interruption if isinstance(interruption, (KeyboardInterrupt, SystemExit)) else None
        super().__init__("Private log scan failed")


def _run_log_scanner(arguments, *, cwd, deadline):
    """One bounded scanner, with the same direct-child timeout/kill ownership."""
    import select
    import subprocess
    import time
    process = None
    output, errors = bytearray(), bytearray()
    failure = None
    stopped = True
    try:
        if time.monotonic() >= deadline:
            raise PrivateLogScanError()
        process = subprocess.Popen(arguments, cwd=cwd, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, umask=0o077,
                                   env={k:v for k,v in os.environ.items() if not k.startswith("GITLEAKS")})
        stopped = False
        streams = {process.stdout:output, process.stderr:errors}
        while streams:
            seconds = deadline - time.monotonic()
            if seconds <= 0:
                raise PrivateLogScanError()
            ready, _, _ = select.select(list(streams), [], [], seconds)
            if not ready:
                raise PrivateLogScanError()
            for stream in ready:
                block = os.read(stream.fileno(), 65536)
                if not block:
                    del streams[stream]
                    continue
                value = streams[stream]
                if len(value) + len(block) > 65536:
                    raise PrivateLogScanError()
                value.extend(block)
        result = process.wait(timeout=max(.001, deadline - time.monotonic()))
        stopped = True
    except BaseException as error:
        failure = error
    finally:
        if process is not None:
            try:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
                stopped = True
            except BaseException:
                stopped = False
            finally:
                process.stdout.close()
                process.stderr.close()
    if failure is not None or not stopped or time.monotonic() >= deadline:
        # Carry interruption identity with the owned child's final stop receipt.
        # The snapshot owner must clean its exact files before re-raising it.
        raise PrivateLogScanError(process_stopped=stopped, interruption=failure) from None
    return result, bytes(output), bytes(errors)


def scan_owned_logs(root: Path, scanner: Path, *, deadline=None) -> None:
    """Scan exact separate owned log snapshots once, never repair source data."""
    import hashlib
    import json
    import re
    import stat
    import sys
    import time
    end = time.monotonic() + 30
    if deadline is not None:
        end = min(end, deadline)
    root, scanner = Path(root).absolute(), Path(scanner).absolute()
    snapshot = None
    root_fd = data_fd = source_fd = report_fd = None
    created = {}
    can_cleanup = True
    primary = None
    root_identity = data_identity = None

    def check_time():
        if time.monotonic() >= end:
            raise PrivateLogScanError()

    def identity(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_gid,
                info.st_nlink, info.st_size, info.st_mtime_ns, info.st_ctime_ns)

    def directory_identity(path):
        info = path.lstat()
        if (path.resolve(strict=True) != path or not stat.S_ISDIR(info.st_mode)
                or info.st_uid != os.getuid() or info.st_mode & 0o077):
            raise PrivateLogScanError()
        return info.st_dev, info.st_ino, info.st_mode, info.st_uid

    def file_info(info):
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or info.st_nlink != 1
                or not 0 <= info.st_size <= 8 * 1024 * 1024):
            raise PrivateLogScanError()
        return identity(info)

    def inventory():
        check_time()
        if directory_identity(root) != source_root:
            raise PrivateLogScanError()
        result, invalid = {}, {}
        def refused(_error):
            check_time()
            invalid["walk-error-" + str(len(invalid))] = None
        for directory, dirs, files, descriptor in os.fwalk(".", dir_fd=source_fd,
                                                          follow_symlinks=False, onerror=refused):
            check_time()
            for name in dirs:
                check_time()
                if name.endswith(".log"):
                    invalid[str(Path(directory) / name)] = None
            # Record unsafe leaves without opening them; still scan other exact
            # private command logs, but never accept this incomplete batch.
            for name in files:
                check_time()
                if not name.endswith(".log"):
                    continue
                path = str(Path(directory) / name)
                try:
                    details = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
                except OSError:
                    invalid[path] = None
                    continue
                try:
                    result[path] = file_info(details)
                except PrivateLogScanError:
                    invalid[path] = identity(details)
        return result, invalid

    def copy_or_hash(path, expected, destination=None):
        check_time()
        selected = Path(path)
        parent_fd = None
        if selected.is_absolute():
            descriptor = os.open(selected, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        else:
            parent_fd = os.dup(source_fd)
            try:
                for part in selected.parts[:-1]:
                    next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent_fd)
                    os.close(parent_fd)
                    parent_fd = next_fd
                descriptor = os.open(selected.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent_fd)
            except BaseException:
                os.close(parent_fd)
                raise
        digest = hashlib.sha256()
        total = 0
        try:
            if file_info(os.fstat(descriptor)) != expected:
                raise PrivateLogScanError()
            while True:
                check_time()
                block = os.read(descriptor, 65536)
                if not block:
                    break
                total += len(block)
                if total > expected[6]:
                    raise PrivateLogScanError()
                digest.update(block)
                if destination is not None:
                    remaining = memoryview(block)
                    while remaining:
                        count = os.write(destination, remaining)
                        if count <= 0:
                            raise PrivateLogScanError()
                        remaining = remaining[count:]
            if (total != expected[6] or file_info(os.fstat(descriptor)) != expected
                    or file_info(selected.lstat() if parent_fd is None else
                                 os.stat(selected.name, dir_fd=parent_fd, follow_symlinks=False)) != expected):
                raise PrivateLogScanError()
        finally:
            os.close(descriptor)
            if parent_fd is not None:
                os.close(parent_fd)
        return digest.hexdigest()

    def create(name, data):
        descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                             0o600, dir_fd=root_fd)
        try:
            with os.fdopen(descriptor, "wb", closefd=False) as stream:
                stream.write(data)
                stream.flush()
        finally:
            created[name] = identity(os.fstat(descriptor))
            os.close(descriptor)

    def snapshot_inventory():
        if directory_identity(snapshot) != root_identity or directory_identity(snapshot / "data") != data_identity:
            raise PrivateLogScanError()
        result = {}
        for name in os.listdir(data_fd):
            result[name] = file_info(os.stat(name, dir_fd=data_fd, follow_symlinks=False))
        return result

    def refresh_owned_report():
        # os.Create in pinned Gitleaks truncates the precreated regular inode.
        # A retained descriptor anchors that inode even across interruption.
        details = identity(os.fstat(report_fd))
        if (details[:6] != report_anchor[:6] or not 0 <= details[6] <= 4096
                or identity(os.stat("report.json", dir_fd=root_fd, follow_symlinks=False)) != details):
            raise PrivateLogScanError()
        created["report.json"] = details

    def cleanup():
        # First validate the WHOLE exact temporary inventory before any unlink.
        if snapshot is None:
            return True
        try:
            if (not can_cleanup or root_fd is None or data_fd is None
                    or directory_identity(snapshot) != root_identity):
                return False
            expected = set(created) | {"data"}
            if set(os.listdir(root_fd)) != expected:
                return False
            if directory_identity(snapshot / "data") != data_identity:
                return False
            if set(os.listdir(data_fd)) != set(copies):
                return False
            for name, details in copies.items():
                if identity(os.stat(name, dir_fd=data_fd, follow_symlinks=False)) != details:
                    return False
            for name, details in created.items():
                if identity(os.stat(name, dir_fd=root_fd, follow_symlinks=False)) != details:
                    return False
            for name in copies:
                os.unlink(name, dir_fd=data_fd)
            for name in created:
                os.unlink(name, dir_fd=root_fd)
            os.rmdir("data", dir_fd=root_fd)
            if directory_identity(snapshot) != root_identity or os.listdir(root_fd):
                return False
            snapshot.rmdir()
            return not snapshot.exists()
        except (OSError, PrivateLogScanError):
            return False

    copies, hashes, copied_paths = {}, {}, {}
    try:
        check_time()
        if sys.platform != "linux" or root.resolve(strict=True) != root or root.is_relative_to(REPOSITORY):
            raise PrivateLogScanError()
        source_root = directory_identity(root)
        source_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        if (os.fstat(source_fd).st_dev, os.fstat(source_fd).st_ino) != source_root[:2]:
            raise PrivateLogScanError()
        original, invalid_original = inventory()
        incomplete = bool(invalid_original)
        # Neutral fixed POSIX path avoids default filename/path allowlists.
        base = Path("/tmp")
        info = base.lstat()
        if (base.resolve(strict=True) != base or not stat.S_ISDIR(info.st_mode) or info.st_uid != 0
                or info.st_mode & 0o022 and not info.st_mode & stat.S_ISVTX):
            raise PrivateLogScanError()
        snapshot = Path(tempfile.mkdtemp(prefix="morpheus-private-scan-", dir=base))
        root_identity = directory_identity(snapshot)
        root_fd = os.open(snapshot, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        os.mkdir("data", mode=0o700, dir_fd=root_fd)
        data_identity = directory_identity(snapshot / "data")
        data_fd = os.open("data", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root_fd)
        create("gitleaks.toml", b"[extend]\nuseDefault = true\n")
        create("empty.ignore", b"")
        for index, (path, details) in enumerate(sorted(original.items())):
            check_time()
            name = str(index).zfill(8) + ".log"
            descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o600, dir_fd=data_fd)
            try:
                initial = file_info(os.fstat(descriptor))
                try:
                    digest = copy_or_hash(path, details, descriptor)
                except (OSError, PrivateLogScanError):
                    incomplete = True
                    # Never adopt partial bytes after a replaced/refused source.
                    # Unlink only this exact exclusively created private leaf.
                    current = file_info(os.fstat(descriptor))
                    if (current[:6] != initial[:6]
                            or identity(os.stat(name, dir_fd=data_fd, follow_symlinks=False)) != current):
                        can_cleanup = False
                        raise PrivateLogScanError()
                    os.unlink(name, dir_fd=data_fd)
                else:
                    hashes[name] = digest
                    copies[name] = file_info(os.fstat(descriptor))
                    copied_paths[name] = (path, details)
            finally:
                os.close(descriptor)
        if snapshot_inventory() != copies:
            raise PrivateLogScanError()
        if inventory() != (original, invalid_original):
            incomplete = True
        for name, details in copies.items():
            if copy_or_hash(snapshot / "data" / name, details) != hashes[name]:
                raise PrivateLogScanError()
        check_time()
        report_fd = os.open("report.json", os.O_RDONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_NONBLOCK,
                            0o600, dir_fd=root_fd)
        report_anchor = identity(os.fstat(report_fd))
        created["report.json"] = report_anchor
        try:
            result, output, errors = _run_log_scanner(
                [str(scanner), "dir", str(snapshot / "data"), "--config", str(snapshot / "gitleaks.toml"),
                 "--gitleaks-ignore-path", str(snapshot / "empty.ignore"), "--ignore-gitleaks-allow",
                 "--max-target-megabytes", "0", "--max-decode-depth", "5", "--max-archive-depth", "0",
                 "--log-level", "info", "--no-color", "--redact=100", "--no-banner",
                 "--report-format", "json", "--report-path", str(snapshot / "report.json")],
                cwd=snapshot, deadline=end)
        except PrivateLogScanError as error:
            can_cleanup = error.process_stopped
            raise
        except BaseException:
            can_cleanup = False
            raise
        report = snapshot / "report.json"
        refresh_owned_report()
        info = os.fstat(report_fd)
        # Pinned Gitleaks creates its report inside the private directory.
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o022:
            raise PrivateLogScanError()
        created["report.json"] = identity(info)
        # Pinned Gitleaks counts raw fragments once, before decoding/rules.
        # Its directory source can otherwise silently skip a failed file open.
        # Accept only the two fixed no-color INFO footer lines and the exact
        # captured byte total; a clean report alone is not coverage evidence.
        messages = errors.decode("utf-8").splitlines()
        stamp = r"(?:[1-9]|1[0-2]):[0-5][0-9](?:AM|PM) INF "
        summary = re.fullmatch(
            stamp + r"scanned ~([0-9]{1,20}) bytes "
            r"\((?:0|[0-9]+(?:\.[0-9]{2})? (?:bytes|KB|MB|GB))\) in "
            r"(?:[0-9]+(?:\.[0-9]+)?(?:ns|µs|ms|s|m|h)){1,4}",
            messages[0] if len(messages) == 2 else "")
        if (summary is None or summary[1] != str(sum(details[6] for details in copies.values()))
                or re.fullmatch(stamp + "no leaks found", messages[1]) is None):
            raise PrivateLogScanError()
        if result != 0 or output or not 0 < info.st_size <= 4096:
            raise PrivateLogScanError()
        raw = os.read(report_fd, 4097)
        if (len(raw) != info.st_size or identity(os.fstat(report_fd)) != created["report.json"]
                or identity(os.stat("report.json", dir_fd=root_fd, follow_symlinks=False)) != created["report.json"] or json.loads(raw) != []):
            raise PrivateLogScanError()
        if snapshot_inventory() != copies:
            raise PrivateLogScanError()
        if inventory() != (original, invalid_original):
            incomplete = True
        for name, (path, details) in copied_paths.items():
            try:
                if copy_or_hash(path, details) != hashes[name]:
                    incomplete = True
            except (OSError, PrivateLogScanError):
                incomplete = True
            if copy_or_hash(snapshot / "data" / name, copies[name]) != hashes[name]:
                raise PrivateLogScanError()
        if set(os.listdir(root_fd)) != set(created) | {"data"}:
            raise PrivateLogScanError()
        check_time()
        if incomplete:
            raise PrivateLogScanError()
    except BaseException as error:
        primary = error
    finally:
        if report_fd is not None and can_cleanup:
            try:
                refresh_owned_report()
            except BaseException:
                can_cleanup = False
        cleaned = cleanup()
        if report_fd is not None:
            os.close(report_fd)
        if root_fd is not None:
            os.close(root_fd)
        if data_fd is not None:
            os.close(data_fd)
        if source_fd is not None:
            os.close(source_fd)
    if primary is not None:
        if type(primary) is PrivateLogScanError and primary.interruption is not None:
            raise primary.interruption
        if isinstance(primary, (KeyboardInterrupt, SystemExit)):
            raise primary
        raise PrivateLogScanError() from None
    if not cleaned:
        raise PrivateLogScanError()
