"""Owned, deadline-bound subprocess adapter for Linux package preparation.

Runs only inside the existing validated fixture PID namespace. Every direct
child has its own process group; the unreaped leader anchors group ownership.
The outer namespace supervisor also contains children which create new sessions.
No output is forwarded to the terminal. Tests replace process/kernel adapters.
"""

from __future__ import annotations

import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import element_process_namespace as containment


class CommandFailure(RuntimeError):
    def __init__(self, reason):
        allowed = {"deadline", "output", "exit", "pipe", "cleanup", "input"}
        self.reason = reason if reason in allowed else "input"
        super().__init__("Private package command failed: " + self.reason)


def full_write(stream, data):
    remaining = memoryview(data)
    while remaining:
        count = stream.write(remaining)
        if count is None or count <= 0 or count > len(remaining):
            raise CommandFailure("pipe")
        remaining = remaining[count:]


class OwnedRunner:
    def __init__(self, log_root, progress=None):
        self.log_root = Path(log_root)
        self.log_root.mkdir(mode=0o700)
        self.sequence = 0
        self.progress = progress
        self.last_stage = "none"

    def run(self, stage, argv, *, cwd, env, timeout, output_limit,
            binary_output=False, expected_status=(0,)):
        containment.assert_contained()
        if (not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", stage)
                or not argv or not all(isinstance(value, str) and "\0" not in value for value in argv)
                or not argv[0].startswith("/")
                or not 0 < timeout <= 3600 or not 0 <= output_limit <= 64 * 1024 * 1024):
            raise CommandFailure("input")
        self.last_stage = stage
        if self.progress is not None:
            self.progress.command(stage)
        self.sequence += 1
        name = f"{self.sequence:04d}-{stage}"
        # Binary downloads are kept only in the returned bounded bytes. Stderr
        # remains private and scanned even when a server rejects the request.
        stdout_path = self.log_root / (name + ".stdout.log")
        stderr_path = self.log_root / (name + ".stderr.log")
        descriptors = []
        process = None
        selector = selectors.DefaultSelector()
        stdout = bytearray()
        stderr_size = 0
        failed = None
        code = None
        cleanup_failed = False
        deadline = time.monotonic() + timeout
        try:
            for path in (stdout_path, stderr_path):
                descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                descriptors.append(os.fdopen(descriptor, "wb", buffering=0))
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                       start_new_session=True)
            for stream, kind in ((process.stdout, "stdout"), (process.stderr, "stderr")):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, kind)
            while selector.get_map() or code is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise CommandFailure("deadline")
                for event, _ in selector.select(min(remaining, 0.05)):
                    try:
                        chunk = os.read(event.fileobj.fileno(), 65536)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(event.fileobj)
                        continue
                    if event.data == "stdout":
                        if len(stdout) + len(chunk) > output_limit:
                            raise CommandFailure("output")
                        stdout.extend(chunk)
                        if not binary_output:
                            full_write(descriptors[0], chunk)
                    else:
                        stderr_size += len(chunk)
                        if stderr_size > 8 * 1024 * 1024:
                            raise CommandFailure("output")
                        full_write(descriptors[1], chunk)
                if code is None:
                    # WNOWAIT: do not reap before final group cleanup, even on
                    # success; otherwise a reused numeric group could be hit.
                    code = containment.status(process)
            if code not in expected_status:
                raise CommandFailure("exit")
        except BaseException as error:
            failed = error
            if self.progress is not None:
                try:
                    self.progress.failed(error)
                except BaseException:
                    pass  # Diagnostic I/O cannot skip the owned-group cleanup.
        finally:
            if process is not None:
                try:
                    containment.stop_owned(process)
                except BaseException:
                    # Preserve an interrupt while making cleanup failure fatal.
                    cleanup_failed = True
                    if failed is None:
                        failed = CommandFailure("cleanup")
                for stream in (process.stdout, process.stderr):
                    if stream is not None:
                        stream.close()
            selector.close()
            for output in descriptors:
                output.close()
        if cleanup_failed and self.progress is not None:
            try:
                self.progress.cleanup(False)
            except BaseException:
                pass
        if failed is not None:
            if isinstance(failed, (KeyboardInterrupt, SystemExit, CommandFailure)):
                raise failed
            raise CommandFailure("pipe") from None
        return bytes(stdout)


def download(runner, *, url, maximum, root, environment, expected_size=None, expected_sha256=None):
    """HTTPS-only bounded download; pin verification precedes executable use."""
    import hashlib
    from urllib.parse import urlsplit
    parsed = urlsplit(url)
    if (parsed.scheme != "https" or parsed.username is not None or parsed.password is not None
            or parsed.hostname not in {"github.com", "archive.ubuntu.com"}
            or parsed.port not in (None, 443) or parsed.fragment or parsed.query
            or not 1 <= maximum <= 64 * 1024 * 1024):
        raise CommandFailure("input")
    # Redirects never carry credentials; HTTPS-only redirection plus an exact
    # required final digest bounds release-asset indirection without accepting a
    # mutable tag as trust. No API Authorization header or ambient curlrc.
    output = runner.run("download", ["/usr/bin/curl", "--disable", "--fail", "--silent", "--show-error",
                        "--location", "--max-redirs", "3", "--noproxy", "*", "--proto", "=https", "--proto-redir", "=https",
                        "--connect-timeout", "10", "--max-time", "120", "--max-filesize", str(maximum), url],
                        cwd=root, env=environment, timeout=125, output_limit=maximum, binary_output=True)
    if ((expected_size is not None and len(output) != expected_size)
            or (expected_sha256 is not None and hashlib.sha256(output).hexdigest() != expected_sha256)):
        raise CommandFailure("input")
    return output
