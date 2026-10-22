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
import tempfile
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

    def assert_containment(self):
        containment.assert_contained()

    def run(self, stage, argv, *, cwd, env, timeout, output_limit,
            binary_output=False, expected_status=(0,), stdin_bytes=None,
            stderr_limit=8 * 1024 * 1024):
        self.assert_containment()
        if (not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", stage)
                or not argv or not all(isinstance(value, str) and "\0" not in value for value in argv)
                or not argv[0].startswith("/")
                or not 0 < timeout <= 3600 or not 0 <= output_limit <= 64 * 1024 * 1024
                or type(stderr_limit) is not int or not 0 <= stderr_limit <= 8 * 1024 * 1024
                or (stdin_bytes is not None and
                    (type(stdin_bytes) is not bytes or len(stdin_bytes) > 65536))):
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
        input_file = None
        deadline = time.monotonic() + timeout
        try:
            if stdin_bytes is not None:
                # Raw Git object queries only. A regular private input file
                # avoids a producer thread/pipe and cannot outlive this owner.
                input_file = tempfile.TemporaryFile(dir=self.log_root)
                full_write(input_file, stdin_bytes)
                input_file.seek(0)
            for path in (stdout_path, stderr_path):
                descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                descriptors.append(os.fdopen(descriptor, "wb", buffering=0))
            process = subprocess.Popen(argv, cwd=cwd, env=env,
                                       stdin=input_file if input_file is not None else subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                       start_new_session=True, umask=0o077)
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
                        if stderr_size > stderr_limit:
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
            if input_file is not None:
                input_file.close()
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
    api_asset = parsed.hostname == "api.github.com"
    if (parsed.scheme != "https" or parsed.username is not None or parsed.password is not None
            or parsed.hostname not in {"github.com", "api.github.com", "archive.ubuntu.com", "zlib.net"}
            or parsed.port not in (None, 443) or parsed.fragment or parsed.query
            or not 1 <= maximum <= 64 * 1024 * 1024
            or (parsed.hostname == "zlib.net" and parsed.path != "/fossils/zlib-1.3.2.tar.gz")
            or (api_asset and not re.fullmatch(
                r"/repos/(?:linuxdeploy/linuxdeploy|linuxdeploy/linuxdeploy-plugin-appimage|"
                r"AppImage/type2-runtime)/releases/assets/[1-9][0-9]{0,19}", parsed.path))):
        raise CommandFailure("input")
    # Redirects never carry credentials; HTTPS-only redirection plus an exact
    # required final digest bounds release-asset indirection without accepting a
    # mutable tag as trust. No API Authorization header or ambient curlrc.
    # Public asset-ID requests require this constant media type to return
    # bytes instead of JSON metadata; no caller-supplied headers are accepted.
    headers = ["--header", "Accept: application/octet-stream"] if api_asset else []
    output = runner.run("download", ["/usr/bin/curl", "--disable", "--fail", "--silent", "--show-error",
                        "--location", "--max-redirs", "3", "--noproxy", "*", "--proto", "=https", "--proto-redir", "=https",
                        "--connect-timeout", "10", "--max-time", "120", "--max-filesize", str(maximum), *headers, url],
                        cwd=root, env=environment, timeout=125, output_limit=maximum, binary_output=True)
    if ((expected_size is not None and len(output) != expected_size)
            or (expected_sha256 is not None and hashlib.sha256(output).hexdigest() != expected_sha256)):
        raise CommandFailure("input")
    return output


# Package inputs have a fixed status envelope. The native client downloader
# above keeps its existing command contract and source verification.
PACKAGE_INPUT_STAGES = frozenset((
    "download", "download-linuxdeploy", "download-appimage-plugin",
    "download-pipewire-dsc", "download-pipewire-original", "download-pipewire-debian", "download-pipewire-patch",
    "download-runtime-fuse", "download-runtime-mimalloc", "download-runtime-musl", "download-runtime-runtime",
    "download-runtime-squashfuse", "download-runtime-zlib", "download-runtime-zstd"))
PACKAGE_INPUT_REASONS = frozenset((
    "http_forbidden", "http_not_found", "http_rate_limited", "http_unavailable", "http_rejected",
    "download_transport", "download_tls"))
PACKAGE_INPUT_FRAME = "\nMORPHEUS_INPUT_STATUS:%{http_code}:%{exitcode}\n"
PACKAGE_INPUT_FRAME_LIMIT = 64
PACKAGE_INPUT_STATUS = re.compile(rb"\nMORPHEUS_INPUT_STATUS:([0-9]{3}):([0-9]{1,2})\n\Z")


class PackageDownloadFailure(CommandFailure):
    def __init__(self, reason):
        super().__init__("exit")
        self.reason = reason if type(reason) is str and reason in PACKAGE_INPUT_REASONS else "input"


def download_package_input(runner, *, url, maximum, root, environment,
                           expected_size=None, expected_sha256=None, stage="download"):
    """Fetch a pinned package input with bounded, credential-free retries.

    Curl appends constant numeric status to its binary stdout; nothing reaches
    the text log. Parse that final envelope, discard failed/partial bytes, and
    verify the original size/digest before returning a successful payload.
    Up to three attempts share a 125-second command budget. Mandatory owned
    group cleanup still completes before return, including after a deadline.
    """
    import hashlib
    from urllib.parse import urlsplit
    try:
        parsed = urlsplit(url)
        api_asset = parsed.hostname == "api.github.com"
        valid_url = (type(url) is str and parsed.scheme == "https"
                     and parsed.username is None and parsed.password is None
                     and parsed.hostname in {"github.com", "api.github.com", "archive.ubuntu.com", "zlib.net"}
                     and parsed.port in (None, 443) and not parsed.fragment and not parsed.query
                     and (parsed.hostname != "zlib.net" or parsed.path == "/fossils/zlib-1.3.2.tar.gz")
                     and (not api_asset or re.fullmatch(
                         r"/repos/(?:linuxdeploy/linuxdeploy|linuxdeploy/linuxdeploy-plugin-appimage|"
                         r"AppImage/type2-runtime)/releases/assets/[1-9][0-9]{0,19}", parsed.path)))
    except (TypeError, ValueError):
        raise CommandFailure("input") from None
    if (not valid_url or type(maximum) is not int
            or not 1 <= maximum <= 64 * 1024 * 1024 - PACKAGE_INPUT_FRAME_LIMIT
            or type(stage) is not str or stage not in PACKAGE_INPUT_STAGES
            or type(expected_sha256) is not str or not re.fullmatch(r"[0-9a-f]{64}", expected_sha256)
            or (expected_size is not None and
                (type(expected_size) is not int or not 1 <= expected_size <= maximum))):
        raise CommandFailure("input")
    headers = (["--header", "Accept: application/octet-stream", "--header", "Cache-Control: no-cache"]
               if api_asset else [])
    deadline = time.monotonic() + 125
    transport = {18, 28, 52, 55, 56}
    transient_http = {408, 500, 502, 503, 504}
    # Normal curl failures return their final fixed status envelope. Other
    # exits and all ownership/deadline/output/cleanup failures stay fatal in
    # OwnedRunner, before retry policy can consume any bytes.
    statuses = (0, 18, 22, 28, 35, 51, 52, 55, 56, 60)
    for attempt in range(3):
        remaining = deadline - time.monotonic()
        if remaining <= 1:
            raise CommandFailure("deadline")
        curl_seconds = min(120.0, remaining - 1.0)
        output = runner.run(stage, ["/usr/bin/curl", "--disable", "--fail", "--silent", "--show-error",
                            "--location", "--max-redirs", "3", "--noproxy", "*", "--proto", "=https", "--proto-redir", "=https",
                            "--connect-timeout", "10", "--max-time", format(curl_seconds, ".3f"),
                            "--max-filesize", str(maximum), "--write-out", PACKAGE_INPUT_FRAME, *headers, url],
                            cwd=root, env=environment, timeout=remaining,
                            output_limit=maximum + PACKAGE_INPUT_FRAME_LIMIT, binary_output=True,
                            expected_status=statuses)
        match = PACKAGE_INPUT_STATUS.search(output)
        if time.monotonic() >= deadline:
            raise CommandFailure("deadline")
        if match is None or len(output) > maximum + PACKAGE_INPUT_FRAME_LIMIT:
            raise CommandFailure("input")
        http, code = (int(value) for value in match.groups())
        if code not in statuses or (http != 0 and not 100 <= http <= 599):
            raise CommandFailure("input")
        payload = output[:match.start()]
        if code == 0:
            if (not 200 <= http < 300 or len(payload) > maximum
                    or (expected_size is not None and len(payload) != expected_size)
                    or hashlib.sha256(payload).hexdigest() != expected_sha256):
                raise CommandFailure("input")
            return payload
        if code in {35, 51, 60}:
            reason, retry = "download_tls", False
        elif http in {403, 404, 429}:
            # Curl may retain an HTTP status while a transfer times out.
            # A permanent HTTP refusal must not become a transport retry.
            reason = {403: "http_forbidden", 404: "http_not_found", 429: "http_rate_limited"}[http]
            retry = False
        elif code == 22:
            if http < 400:
                raise CommandFailure("input")
            reason = "http_unavailable" if http in transient_http else "http_rejected"
            retry = http in transient_http
        elif code in transport:
            reason, retry = "download_transport", True
        else:
            raise CommandFailure("input")
        if not retry or attempt == 2:
            raise PackageDownloadFailure(reason)
        # Do not retry forbidden/not-found/rate-limited requests, TLS failures,
        # corrupt payloads or command ownership failures.
        remaining = deadline - time.monotonic()
        if remaining <= 1:
            raise CommandFailure("deadline")
        time.sleep(min(float(attempt + 1), remaining - 1.0))
    raise CommandFailure("input")
