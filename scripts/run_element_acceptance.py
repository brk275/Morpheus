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
))
STAGES = frozenset((
    "configuration", "tool_install", "browser_install", "native_test_build", "media_fixture",
    "browser_launch", "browser_login", "browser_identity", "browser_room_sync", "native_join_wait",
    "browser_join", "browser_roster", "browser_receive_video", "browser_receive_audio", "native_media_wait", "browser_leave",
    "element_fixture_root", "element_configuration", "element_image_pull", "element_image_verify",
    "element_container_create", "element_container_start", "element_ready", "element_cleanup",
    "native_test", "log_scan", "complete",
))
FAILURES = frozenset(("fixture_operation", "browser_acceptance", "native_acceptance", "deadline", "evidence", "log_scan"))
CHECK_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_CHECK ([a-z_]+)=(true|false)$", re.MULTILINE)
STAGE_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_STAGE ([a-z_]+)$", re.MULTILINE)
FAILURE_PATTERN = re.compile(r"^MORPHEUS_ELEMENT_FAILURE ([a-z_]+)$", re.MULTILINE)


def evidence(output: str, *, complete=False) -> dict[str, bool]:
    result = {}
    for key, value in CHECK_PATTERN.findall(output):
        if key in CHECKS:
            result[key] = result.get(key, True) and value == "true"
    if complete and (set(result) != CHECKS or not all(result.values())
                     or any(value in FAILURES for value in FAILURE_PATTERN.findall(output))):
        raise RuntimeError("evidence")
    return result


def diagnostics(output: str) -> dict[str, str]:
    result = {}
    stages = [value for value in STAGE_PATTERN.findall(output) if value in STAGES]
    failures = [value for value in FAILURE_PATTERN.findall(output) if value in FAILURES]
    if stages:
        result["stage"] = stages[-1]
    if failures:
        result["failure"] = failures[-1]
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
        fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as output:
            self.process = subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                            stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        self.closed = False

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

    def mark(self, value):
        assert value in STAGES
        self.stage = value
        print("MORPHEUS_ELEMENT_STAGE " + value, flush=True)

    def start(self, args, *, extra=None, cwd=REPO):
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
        if failed:
            raise RuntimeError("fixture_operation")

    def scan(self):
        combined = "\n".join(path.read_text(errors="replace") for path in sorted(self.root.rglob("*.log")))
        try:
            result = subprocess.run([self.gitleaks, "stdin", "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
                                    input=combined, text=True, capture_output=True, timeout=30, cwd=self.root)
        except (subprocess.TimeoutExpired, OSError):
            raise RuntimeError("log_scan") from None
        if result.returncode:
            raise RuntimeError("log_scan")
        return combined


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
    native_output = ""
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
        native_output = native.log.read_text(errors="replace")
        if not exact_native_pass(native_output):
            raise RuntimeError("evidence")
    except BaseException as error:
        failed_stage = runner.stage
        failure = str(error) if isinstance(error, RuntimeError) and str(error) in FAILURES else "fixture_operation"
    finally:
        try:
            runner.close()
        except BaseException:
            failure = failure or "fixture_operation"
        try:
            runner.service(lambda: down(root / "service", keep_data=True))
        except BaseException:
            failure = failure or "fixture_operation"
    try:
        output = runner.scan()
        facts = evidence(output, complete=not failure)
    except RuntimeError as error:
        failure = str(error)
        output, facts = "", {}
    for key, value in sorted(facts.items()):
        print(f"MORPHEUS_ELEMENT_CHECK {key}={str(value).lower()}", flush=True)
    # Cleanup is required but must not erase the last useful failure stage.
    diagnostic_output = output.replace("MORPHEUS_ELEMENT_STAGE element_cleanup\n", "")
    if found := diagnostics(diagnostic_output):
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
