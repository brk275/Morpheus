#!/usr/bin/env python3
"""Pre-synced encrypted Sticky2025 interop with an unmodified official RC peer.

Runs only on ephemeral Linux GitHub runners after the shared MatrixRTC fixture
is ready. Browser/SDK logs, accounts, captures and tool installations remain in
the private external root. No screenshots, traces or raw logs are published.
"""
from __future__ import annotations

import argparse
import contextlib
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import struct
import subprocess
import sys
import time
import wave

from fixture_paths import external_path, private_directory, write_private
from element_peer_fixture import down, up
from element_process_namespace import assert_contained

REPO = Path(__file__).resolve().parent.parent
TEST_NAME = "element_call_browser_peer_encrypted_media_and_departure"
CHECKS = frozenset((
    "official_image",
    "browser_identity", "browser_presynced", "browser_roster", "browser_video", "browser_audio", "browser_leave",
    "native_presynced_baseline", "native_roster", "native_audio", "native_video", "native_departure",
    "native_group_roster", "native_late_join_media", "native_group_departure", "native_key_rotation",
    "browser_group_roster", "browser_group_audio", "browser_screen", "browser_group_departure",
    "browser_key_rotation", "browser_post_rotation_screen", "browser_post_rotation_audio",
))
STAGES = frozenset((
    "configuration", "tool_install", "browser_install", "native_test_build", "media_fixture",
    "browser_launch", "browser_login", "browser_identity", "browser_room_sync", "native_join_wait",
    "browser_join", "browser_roster", "browser_receive_video", "browser_receive_audio", "native_media_wait", "browser_leave",
    "browser_group_roster", "browser_group_audio", "browser_receive_screen",
    "browser_group_departure", "browser_key_rotation",
    "element_fixture_root", "element_configuration", "element_image_pull", "element_image_verify",
    "element_container_create", "element_container_start", "element_ready", "element_cleanup",
    "native_test", "log_scan", "complete",
))
FAILURES = frozenset(("fixture_operation", "browser_acceptance", "native_acceptance", "deadline", "evidence", "log_scan"))
NATIVE_STAGES = ("microphone_cryptor", "late_login", "late_sync", "late_bootstrap", "late_join", "group_roster")
NATIVE_STAGE_PREFIX = "MORPHEUS_ELEMENT_NATIVE_STAGE "
MAX_LOG_BYTES = 64 * 1024 * 1024
MAX_LOG_FILES = 128
MAX_LOG_ENTRIES = 65_536
MAX_LOG_DEPTH = 32
CHECK_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_CHECK ([a-z_]+)=(true|false)$", re.MULTILINE)
STAGE_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_STAGE ([a-z_]+)$", re.MULTILINE)
FAILURE_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_FAILURE ([a-z_]+)$", re.MULTILINE)


GROUP_PREFIX = "MORPHEUS_ELEMENT_GROUP "
GROUP_FIELDS = ("original_roster", "late_roster", "continuity_detail", "late_current_bob", "late_bob_pending")
GROUP_ROSTERS = frozenset(("exact", "missing_alice", "missing_bob", "missing_carol", "missing_multiple",
                           "unexpected_or_duplicate", "unavailable", "unknown"))
GROUP_VALUES = (GROUP_ROSTERS, GROUP_ROSTERS,
                frozenset(("same_live_event", "changed_live_event", "not_unique", "unknown")),
                frozenset(("present", "missing", "ambiguous", "unknown")),
                frozenset(("pending", "absent", "retrying", "unknown",
                           "pending_missing_key", "pending_withheld_key", "pending_unknown_index", "pending_untrusted_sender", "pending_malformed", "pending_megolm_failure", "pending_identity_mismatch", "pending_no_crypto", "pending_internal_error", "pending_other")))
GROUP_MAX_LINE = len(GROUP_PREFIX) + len(GROUP_FIELDS) - 1 + sum(
    len(key) + 1 + max(map(len, values)) for key, values in zip(GROUP_FIELDS, GROUP_VALUES))


def group_discriminator(output: str) -> dict[str, str]:
    """One bounded complete fixed-label line; never copy an unbounded token."""
    result = None
    cursor = 0
    while (start := output.find("MORPHEUS_ELEMENT_GROUP", cursor)) >= 0:
        cursor = start + len("MORPHEUS_ELEMENT_GROUP")
        if start and output[start - 1] != "\n":
            continue
        end = output.find("\n", start)
        end = len(output) if end < 0 else end
        unknown = dict.fromkeys(GROUP_FIELDS, "unknown")
        if result is not None or end - start > GROUP_MAX_LINE:
            return unknown
        line = output[start:end]
        if not line.startswith(GROUP_PREFIX):
            return unknown
        fields = line[len(GROUP_PREFIX):].split(" ")
        if len(fields) != len(GROUP_FIELDS):
            return unknown
        result = {}
        for field, key, values in zip(fields, GROUP_FIELDS, GROUP_VALUES):
            name, sep, value = field.partition("=")
            if not sep or name != key or value not in values:
                return unknown
            result[key] = value
    return result or {}


def evidence(output: str, *, complete=False) -> dict[str, bool]:
    result = {}
    for key, value in CHECK_PATTERN.findall(output):
        if key in CHECKS:
            result[key] = result.get(key, True) and value == "true"
    if complete and (set(result) != CHECKS or not all(result.values())
                     or any(value in FAILURES for value in FAILURE_PATTERN.findall(output))):
        raise RuntimeError("evidence")
    return result


def native_stages(output: str) -> tuple[str, ...]:
    """Bound each candidate line before copying a fixed native stage."""
    result = []
    cursor = 0
    while (start := output.find("MORPHEUS_ELEMENT_NATIVE_STAGE", cursor)) >= 0:
        cursor = start + len("MORPHEUS_ELEMENT_NATIVE_STAGE")
        if start and output[start - 1] != "\n":
            continue
        end = output.find("\n", start)
        end = len(output) if end < 0 else end
        if (len(result) >= len(NATIVE_STAGES) or end - start > 96
                or output[start:end] != NATIVE_STAGE_PREFIX + NATIVE_STAGES[len(result)]):
            return ("invalid",)
        result.append(NATIVE_STAGES[len(result)])
    return tuple(result)


def diagnostics(output: str, *, include_native=True) -> dict[str, str]:
    result = {}
    stages = [value for value in STAGE_PATTERN.findall(output) if value in STAGES]
    failures = [value for value in FAILURE_PATTERN.findall(output) if value in FAILURES]
    if stages:
        result["stage"] = stages[-1]
    if failures:
        result["failure"] = failures[-1]
    if include_native and (native := native_stages(output)):
        result["native_stage"] = native[-1]
    if (include_native and result.get("native_stage") == "group_roster"
            and result.get("failure") == "native_acceptance"):
        result.update(group_discriminator(output))
    return result


def exact_native_pass(output: str) -> bool:
    return (not re.search(r"\bskipped\b", output, re.I)
            and re.search(r"^test " + TEST_NAME + r" \.\.\. ok$", output, re.M) is not None
            and re.search(r"^test result: ok\. 1 passed; 0 failed; 0 ignored;", output, re.M) is not None)


def native_executable(output: str) -> Path:
    matches = []
    for line in output.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (value.get("reason") == "compiler-artifact" and value.get("target", {}).get("name") == "element_call_peer"
                and value.get("profile", {}).get("test") is True and value.get("executable")):
            path = Path(value["executable"]).resolve()
            if path.is_file() and path.is_relative_to((REPO / "target").resolve()):
                matches.append(path)
    if len(matches) != 1:
        raise RuntimeError("native_acceptance")
    return matches[0]


def media_fixtures(root: Path) -> None:
    """Browser fake capture uses red/green YUV video and a real nonzero tone."""
    root = private_directory(root)
    camera = root / "camera.y4m"
    fd = os.open(camera, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    # Exactly the color pattern the native receiver verifies, planar 4:2:0.
    pixels = (bytes([82]) * 320 + bytes([145]) * 320) * 360
    pixels += (bytes([90]) * 160 + bytes([54]) * 160) * 180
    pixels += (bytes([240]) * 160 + bytes([34]) * 160) * 180
    with os.fdopen(fd, "wb") as stream:
        stream.write(b"YUV4MPEG2 W640 H360 F15:1 Ip A1:1 C420jpeg\n")
        for _ in range(30):
            stream.write(b"FRAME\n" + pixels)
    audio = root / "microphone.wav"
    fd = os.open(audio, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream, wave.open(stream, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(48_000)
        wav.writeframes(b"".join(struct.pack("<h", int(8_000 * math.sin(2 * math.pi * 440 * n / 48_000)))
                                  for n in range(96_000)))


class Child:
    """Keep the owned group leader unreaped until every group member is killed."""
    def __init__(self, args, log: Path, env, cwd):
        self.log = log
        self.started = time.monotonic()
        fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        self.log_anchor = fd
        try:
            self.log_identity = self.file_identity(os.fstat(fd))
            with os.fdopen(os.dup(fd), "wb") as output:
                self.process = subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                                stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        except BaseException:
            os.close(fd)
            self.log_anchor = None
            raise
        self.closed = False
        self.joined = False

    @staticmethod
    def file_identity(info):
        return (info.st_dev, info.st_ino, info.st_uid, info.st_gid, info.st_mode, info.st_nlink)

    def release_log_anchor(self):
        if self.log_anchor is not None:
            if self.file_identity(os.fstat(self.log_anchor))[:2] != self.log_identity[:2]:
                raise RuntimeError("fixture_operation")
            os.close(self.log_anchor)
            self.log_anchor = None

    def status(self):
        result = os.waitid(os.P_PID, self.process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        if result is None:
            return None
        return result.si_status if result.si_code == os.CLD_EXITED else -result.si_status

    def close(self):
        if self.closed:
            return
        # The waitable, unreaped direct child is the ownership anchor. Once
        # somebody else reaps it, a cached PID is no longer safe to signal.
        if self.process.returncode is not None:
            self.closed = True
            raise RuntimeError("fixture_operation")
        try:
            self.status()
        except ChildProcessError:
            self.closed = True
            raise RuntimeError("fixture_operation") from None
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        finally:
            self.process.wait(timeout=10)
            self.joined = True
            self.closed = True


def wait_peers(native: Child, browser: Child, seconds=360):
    while True:
        # Sample both terminal statuses even when both finish between polls.
        # A final browser.close error must not be hidden by earlier markers.
        native_status, browser_status = native.status(), browser.status()
        if native_status is not None and native_status != 0:
            raise RuntimeError("native_acceptance")
        if browser_status is not None and browser_status != 0:
            raise RuntimeError("browser_acceptance")
        if native_status == 0 and browser_status == 0:
            return
        if time.monotonic() - native.started >= seconds:
            raise RuntimeError("deadline")
        time.sleep(0.2)


class Runner:
    def __init__(self, root: Path, gitleaks: str):
        self.root = private_directory(root)
        self.logs = private_directory(root / "logs")
        self.gitleaks = gitleaks
        self.env = dict(os.environ)
        self.env.update({"RUST_BACKTRACE": "0", "PLAYWRIGHT_BROWSERS_PATH": str(root / "browsers"),
                         "npm_config_cache": str(root / "npm-cache"), "npm_config_userconfig": str(root / "npmrc")})
        write_private(root / "npmrc", "registry=https://registry.npmjs.org/\n")
        self.stage = "configuration"
        self.sequence = 0
        self.children = []
        self.root_identity = Child.file_identity(self.root.stat())[:-1]

    def mark(self, value):
        assert value in STAGES
        self.stage = value
        print("MORPHEUS_ELEMENT_STAGE " + value, flush=True)

    def start(self, args, *, extra=None, cwd=REPO):
        if len(self.children) >= MAX_LOG_FILES:
            raise RuntimeError("fixture_operation")
        self.sequence += 1
        child = Child(args, self.logs / f"{self.sequence:02}.log", self.env | (extra or {}), cwd)
        self.children.append(child)
        return child

    def wait(self, child, seconds):
        while (status := child.status()) is None:
            if time.monotonic() - child.started >= seconds:
                raise RuntimeError("deadline")
            time.sleep(0.1)
        child.close()
        if status:
            raise RuntimeError("fixture_operation")
        return child.log.read_text(errors="replace")

    def command(self, args, *, seconds=300, cwd=REPO):
        return self.wait(self.start(args, cwd=cwd), seconds)

    def service(self, action):
        self.sequence += 1
        fd = os.open(self.logs / f"{self.sequence:02}.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as output, contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            return action()

    def close(self):
        failed = False
        for child in reversed(self.children):
            try:
                child.close()
            except BaseException:
                failed = True
        if failed or any(child.joined is not True for child in self.children):
            raise RuntimeError("fixture_operation")

    def release_log_anchors(self):
        failed = False
        for child in self.children:
            try:
                child.release_log_anchor()
            except BaseException:
                failed = True
        if failed:
            raise RuntimeError("fixture_operation")

    def log_snapshots(self):
        """Read only bounded regular logs through pinned no-follow directory FDs."""
        expected = {child.log.relative_to(self.root).as_posix(): child for child in self.children}
        found = set()
        snapshots = []
        total = entries = 0

        def stable(info):
            return (*Child.file_identity(info), info.st_size, info.st_mtime_ns, info.st_ctime_ns)

        def visit(directory, relative, depth):
            nonlocal total, entries
            if depth > MAX_LOG_DEPTH:
                raise RuntimeError("log_scan")
            with os.scandir(directory) as listing:
                for entry in listing:
                    entries += 1
                    if entries > MAX_LOG_ENTRIES:
                        raise RuntimeError("log_scan")
                    info = entry.stat(follow_symlinks=False)
                    name = relative + entry.name
                    if stat.S_ISDIR(info.st_mode):
                        fd = os.open(entry.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                                     dir_fd=directory)
                        try:
                            if Child.file_identity(os.fstat(fd)) != Child.file_identity(info):
                                raise RuntimeError("log_scan")
                            visit(fd, name + "/", depth + 1)
                        finally:
                            os.close(fd)
                    elif entry.name.endswith(".log"):
                        if len(snapshots) >= MAX_LOG_FILES or not stat.S_ISREG(info.st_mode):
                            raise RuntimeError("log_scan")
                        fd = os.open(entry.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                                     dir_fd=directory)
                        try:
                            before = os.fstat(fd)
                            if (not stat.S_ISREG(before.st_mode) or stable(before) != stable(info)
                                    or not 0 <= before.st_size <= MAX_LOG_BYTES - total - bool(snapshots)):
                                raise RuntimeError("log_scan")
                            if child := expected.get(name):
                                if (child.log_anchor is None
                                        or Child.file_identity(os.fstat(child.log_anchor)) != child.log_identity
                                        or Child.file_identity(before) != child.log_identity):
                                    raise RuntimeError("log_scan")
                                found.add(name)
                            chunks, size = [], 0
                            while chunk := os.read(fd, min(65_536, before.st_size - size + 1)):
                                size += len(chunk)
                                if size > before.st_size:
                                    raise RuntimeError("log_scan")
                                chunks.append(chunk)
                            if (size != before.st_size or stable(os.fstat(fd)) != stable(before)
                                    or stable(os.stat(entry.name, dir_fd=directory, follow_symlinks=False)) != stable(before)):
                                raise RuntimeError("log_scan")
                            data = b"".join(chunks)
                            total += size + bool(snapshots)
                            snapshots.append((name, data))
                        finally:
                            os.close(fd)

        fd = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        try:
            if Child.file_identity(os.fstat(fd))[:-1] != self.root_identity:
                raise RuntimeError("log_scan")
            visit(fd, "", 0)
            if found != set(expected):
                raise RuntimeError("log_scan")
        finally:
            os.close(fd)
        return sorted(snapshots)

    def scan(self, native=None):
        if any(child.joined is not True for child in self.children):
            raise RuntimeError("fixture_operation")
        if native is not None and not any(child is native for child in self.children):
            raise RuntimeError("log_scan")
        try:
            snapshots = self.log_snapshots()
        except (OSError, ValueError):
            raise RuntimeError("log_scan") from None
        combined = b"\n".join(data for _, data in snapshots)
        native_output = next((data for name, data in snapshots
                              if native is not None and name == native.log.relative_to(self.root).as_posix()), b"")
        try:
            result = subprocess.run([self.gitleaks, "stdin", "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
                                    input=combined, capture_output=True, timeout=30, cwd=self.root)
        except (subprocess.TimeoutExpired, OSError):
            raise RuntimeError("log_scan") from None
        if result.returncode:
            raise RuntimeError("log_scan")
        return (combined.decode(errors="replace").replace("\r\n", "\n").replace("\r", "\n"),
                native_output.decode(errors="replace").replace("\r\n", "\n").replace("\r", "\n"))


def execute(root: Path, fixture: Path, gitleaks: str):
    if sys.platform != "linux" or os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("RUNNER_OS") != "Linux":
        raise RuntimeError("configuration")
    if "MORPHEUS_ELEMENT_DIAG_REFRESH" in os.environ:
        raise RuntimeError("configuration")
    assert_contained()
    fixture = external_path(fixture)
    if not fixture.is_file() or fixture.stat().st_mode & 0o077:
        raise RuntimeError("configuration")
    root = private_directory(root)
    runner = Runner(root, gitleaks)
    failure = None
    failed_stage = "configuration"
    native = None
    peers_complete = False
    children_joined = False
    cleanup_failure = None
    try:
        runner.mark("tool_install")
        tool = private_directory(root / "tool")
        for name in ("package.json", "package-lock.json"):
            write_private(tool / name, (REPO / "scripts/element-peer" / name).read_text(), create=True)
        runner.command(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund", "--omit=optional"], cwd=tool)
        runner.mark("browser_install")
        # Trusted CI apt installs OS dependencies before the privilege drop.
        runner.command(["node", str(tool / "node_modules/playwright-core/cli.js"), "install", "chromium"], seconds=360, cwd=tool)
        runner.mark("native_test_build")
        built = runner.command(["cargo", "test", "--locked", "-p", "morpheus-rtc", "--features", "native-devices",
                                "--test", "element_call_peer", "--no-run", "--message-format=json"], seconds=600)
        binary = native_executable(built)
        peer = runner.service(lambda: up(root / "service"))
        if peer.get("official_image_identity_verified") is not True:
            raise RuntimeError("evidence")
        write_private(runner.logs / "image.log", "MORPHEUS_ELEMENT_CHECK official_image=true\n", create=True)
        runner.mark("media_fixture")
        media_fixtures(root)
        extra = {"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": str(fixture), "MORPHEUS_ELEMENT_PEER_ROOT": str(root),
                 "MORPHEUS_ELEMENT_PLAYWRIGHT_ROOT": str(tool), "MORPHEUS_ELEMENT_PEER_URL": peer["base_url"],
                 "MORPHEUS_ELEMENT_INTEROP_MODE": "sticky-2025", "MORPHEUS_ELEMENT_PEER_GO_FILE": str(root / "go"),
                 "MORPHEUS_ELEMENT_PEER_DONE_FILE": str(root / "done")}
        browser = runner.start(["node", "scripts/element_call_stock_peer.mjs"], extra=extra)
        # Browser keys and room sync exist before the native device joins the
        # room/call. Historical-membership key recovery is a separate test.
        while not (root / "browser-ready").is_file():
            if browser.status() is not None:
                raise RuntimeError("browser_acceptance")
            if time.monotonic() - browser.started >= 180:
                raise RuntimeError("deadline")
            time.sleep(0.2)
        runner.mark("native_test")
        native = runner.start([str(binary), "--exact", TEST_NAME, "--show-output", "--test-threads=1"], extra=extra)
        wait_peers(native, browser)
        peers_complete = True
    except BaseException as error:
        failed_stage = runner.stage
        failure = str(error) if isinstance(error, RuntimeError) and str(error) in FAILURES else "fixture_operation"
    finally:
        try:
            runner.close()
            children_joined = True
        except BaseException:
            cleanup_failure = "fixture_operation"
        try:
            runner.service(lambda: down(root / "service", keep_data=True))
        except BaseException:
            cleanup_failure = "fixture_operation"
    try:
        if not children_joined or cleanup_failure is not None:
            raise RuntimeError("fixture_operation")
        output, native_output = runner.scan(native)
        # This is the exact native log snapshot included in the successful
        # scan, inspected only after owned group teardown and actual waits.
        if peers_complete and children_joined and not exact_native_pass(native_output):
            failure = "evidence"
        failure = failure or cleanup_failure
        facts = evidence(output, complete=not failure)
    except RuntimeError as error:
        failure = str(error)
        output, native_output, facts = "", "", {}
    finally:
        try:
            runner.release_log_anchors()
        except BaseException:
            failure = "fixture_operation"
            output, native_output, facts = "", "", {}
    for key, value in sorted(facts.items()):
        print(f"MORPHEUS_ELEMENT_CHECK {key}={str(value).lower()}", flush=True)
    if children_joined and cleanup_failure is None:
        for value in native_stages(native_output):
            print(NATIVE_STAGE_PREFIX + value, flush=True)
        if failure and (group := group_discriminator(native_output)):
            print(GROUP_PREFIX + " ".join(key + "=" + group[key] for key in GROUP_FIELDS), flush=True)
    # Cleanup is required but must not erase the last useful failure stage.
    diagnostic_output = output.replace("MORPHEUS_ELEMENT_STAGE element_cleanup\n", "")
    if found := diagnostics(diagnostic_output, include_native=False):
        if "stage" in found:
            runner.mark(found["stage"])
    elif failure:
        runner.mark(failed_stage)
    if failure:
        print("MORPHEUS_ELEMENT_FAILURE " + failure, flush=True)
        raise RuntimeError(failure)
    runner.mark("complete")
    return facts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--gitleaks", required=True)
    args = parser.parse_args()
    # The outer acceptance runner has a longer watchdog and also calls down.
    # A graceful cancellation still lets this process kill its owned groups.
    def interrupted(_signal, _frame):
        raise RuntimeError("deadline")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGALRM, interrupted)
    signal.alarm(1_740)
    execute(args.root, args.fixture, args.gitleaks)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # Exceptions can carry command arguments, passwords or URLs.
        print("Element Call acceptance failed; raw diagnostics withheld", file=sys.stderr)
        raise SystemExit(1)
