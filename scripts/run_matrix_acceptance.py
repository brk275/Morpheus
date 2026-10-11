#!/usr/bin/env python3
"""Exercise real disposable Matrix/RTC services on an ephemeral Linux runner.

Use inside dbus-run-session with gnome-keyring-daemon and Gitleaks installed.
Credentials, keyrings and raw logs stay outside the checkout and are never
uploaded. This checks protocols and persistence, not physical devices or UI.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from fixture_paths import external_path, private_directory, write_private


REPO = Path(__file__).resolve().parent.parent
CHAT_TESTS = (
    "live_local", "drafts_local", "room_actions_local", "formatted_local",
    "media_local", "discovery_local", "moderation_local", "search_local",
)
RESULT = re.compile(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;")
LATE_JOIN_EVIDENCE = re.compile(r"^MORPHEUS_RTC_LATE_JOIN historical_member=(true|false)$", re.MULTILINE)


def passed_tests(output: str) -> int:
    """A skipped opt-in test returning Ok is not acceptance evidence."""
    if re.search(r"\bskipped\b", output, re.IGNORECASE):
        raise RuntimeError("An opt-in acceptance test skipped its live checks")
    results = RESULT.findall(output)
    if not results or any(int(failed) or int(ignored) for _, failed, ignored in results):
        raise RuntimeError("Missing or incomplete acceptance test result")
    count = sum(int(passed) for passed, _, _ in results)
    if count == 0:
        raise RuntimeError("Acceptance command executed no tests")
    return count


def fixture_from_output(output: str, root: Path) -> Path:
    path = external_path(Path(output.strip()))
    if not path.is_relative_to(root.resolve()) or not path.is_file():
        raise RuntimeError("Fixture command did not return a private fixture path")
    if path.stat().st_mode & 0o077:
        raise RuntimeError("Fixture file has group or world permissions")
    return path


class Acceptance:
    def __init__(self, root: Path, gitleaks: str):
        self.root = private_directory(root)
        self.logs = private_directory(root / "logs")
        self.gitleaks = gitleaks
        self.env = dict(os.environ)
        self.env.update({
            "MORPHEUS_MATRIX_TEST_HOME": str(root),
            "XDG_DATA_HOME": str(private_directory(root / "desktop-data")),
            "XDG_CACHE_HOME": str(private_directory(root / "desktop-cache")),
            "RUST_BACKTRACE": "0",
        })
        self.results = []
        self.sequence = 0

    def run(self, label: str, args: list[str], *, extra=None, test=False,
            stdin=None, seconds=900) -> str:
        self.sequence += 1
        log = self.logs / f"{self.sequence:02d}-{label}.log"
        started = time.monotonic()
        print(f"Starting {label}", flush=True)
        # Create the private log before the child can write anything to it.
        fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as output:
            process = subprocess.Popen(
                args, cwd=REPO, env=self.env | (extra or {}),
                stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
                stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
            )
            try:
                process.communicate(stdin, timeout=seconds)
            except subprocess.TimeoutExpired:
                # Cargo and fixture scripts spawn children. Stop the whole
                # owned group so a timed-out test cannot outlive its services.
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise RuntimeError(f"{label} exceeded its {seconds}-second deadline") from None
        output = log.read_text(errors="replace")
        # Scan bytes through stdin: avoid reading the adjacent credential files
        # and avoid source allowlists/inline exclusions applying to fresh logs.
        scan = subprocess.run(
            [self.gitleaks, "stdin", "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
            input=output, text=True, capture_output=True, cwd=self.root,
        )
        if scan.returncode:
            raise RuntimeError(f"{label} log failed credential scanning; raw log withheld")
        if process.returncode:
            # Scanner coverage is not a guarantee: arbitrary server errors can
            # reflect secrets without a recognizable prefix. Never publish raw
            # child logs, including on failure. Only source locations and Rust
            # test identifiers are useful public diagnostics here.
            failed = re.findall(r"^test ([A-Za-z0-9_:]+) \.\.\. FAILED$", output, re.MULTILINE)
            locations = re.findall(r"panicked at (crates/[A-Za-z0-9_./-]+\.rs:\d+:\d+)", output)
            evidence = {"check": label, "failed_tests": failed, "panic_locations": locations}
            # This exact boolean-only marker says whether a fresh device could
            # already decrypt the old membership. Never print event/account
            # identifiers, arbitrary server text, ciphertext, or raw logs.
            late_join = LATE_JOIN_EVIDENCE.findall(output)
            if late_join:
                evidence["late_join_historical_member"] = [value == "true" for value in late_join]
            print(json.dumps(evidence), flush=True)
            raise RuntimeError(f"{label} failed with exit status {process.returncode}")
        record = {"check": label, "seconds": round(time.monotonic() - started, 2)}
        if test:
            record["passed"] = passed_tests(output)
        print(json.dumps(record | {"status": "passed"}), flush=True)
        self.results.append(record)
        write_private(self.root / "results.json", json.dumps(self.results, indent=2) + "\n")
        return output

    def script(self, name: str, command: str) -> str:
        return self.run(f"{name}-{command}", [sys.executable, f"scripts/{name}.py", command],
                        seconds=300)

    def test(self, name: str, fixture: Path, *, extra=None, rtc=False):
        env = {"MORPHEUS_MATRIX_TEST_ACCOUNTS": str(fixture)} | (extra or {})
        args = ["cargo", "test", "--locked", "-p", "morpheus-rtc" if rtc else "morpheus-matrix"]
        if rtc:
            args += ["--features", "native-devices"]
        args += ["--test", name, "--", "--nocapture", "--test-threads=1"]
        label = name + ("-classic" if env.get("MORPHEUS_MATRIX_SEARCH_CLASSIC") else "")
        if env.get("MORPHEUS_RTC_LATE_JOIN") == "1":
            label += "-late-join"
        self.run(label, args, extra=env, test=True)

    def rtc_cases(self):
        # The second run gets different users/room: existing devices and keys
        # would hide a regression in encrypted sticky membership for late join.
        for late_join in (False, True):
            fixture = fixture_from_output(self.script("local_matrix_rtc", "rtc-fixture"), self.root)
            extra = {"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": str(fixture)}
            if late_join:
                extra["MORPHEUS_RTC_LATE_JOIN"] = "1"
            self.test("local_rtc", fixture, rtc=True, extra=extra)

    def start_keyring(self):
        # This D-Bus session and empty-password keyring exist only on a fresh
        # ephemeral runner. No desktop user's existing vault is unlocked.
        self.run("unlock-disposable-keyring", ["gnome-keyring-daemon", "--unlock", "--components=secrets"],
                 stdin=b"\n", seconds=30)
        marker = "morpheus-ephemeral-vault-probe"
        self.run("vault-store", ["secret-tool", "store", "--label=Morpheus CI probe", "morpheus-ci", "probe"],
                 stdin=marker.encode(), seconds=30)
        found = self.run("vault-read", ["secret-tool", "lookup", "morpheus-ci", "probe"], seconds=30)
        if found.strip() != marker:
            raise RuntimeError("Disposable OS credential vault failed its round trip")
        self.run("vault-clear", ["secret-tool", "clear", "morpheus-ci", "probe"], seconds=30)

    def summary(self):
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as output:
                output.write("### Disposable Matrix integration\n\n")
                output.write("Real Synapse, OS Secret Service and LiveKit; synthetic call media.\n\n")
                output.write("| Check | Passed tests | Seconds |\n| --- | ---: | ---: |\n")
                for item in self.results:
                    if "passed" in item:
                        output.write(f"| {item['check']} | {item['passed']} | {item['seconds']} |\n")
                output.write("\nNo fixture credentials or installers were uploaded. Physical-device, UI, "
                             "Element Call, and installed-package gates remain separate.\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gitleaks", required=True)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("RUNNER_OS") != "Linux":
        raise RuntimeError("This harness is restricted to ephemeral Linux GitHub runners")
    if not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
        raise RuntimeError("Run the harness inside dbus-run-session")
    for command in ("docker", "cargo", "gnome-keyring-daemon", "secret-tool", args.gitleaks):
        if not shutil.which(command):
            raise RuntimeError(f"Required command unavailable: {command}")
    # Refuse to claim or stop a pre-existing fixture on a reused/self-hosted host.
    for kind, names in {
        "container": ("morpheus-synapse-a", "morpheus-synapse-b", "morpheus-rtc-livekit",
                      "morpheus-rtc-auth", "morpheus-rtc-federation", "morpheus-rtc-sfu-loopback"),
        "volume": ("morpheus-synapse-a-data", "morpheus-synapse-b-data"),
        "network": ("morpheus-matrix-test",),
    }.items():
        for name in names:
            if subprocess.run(["docker", kind, "inspect", name], capture_output=True).returncode == 0:
                raise RuntimeError("Pre-existing Matrix fixture found; refusing to modify it")
    root = Path(tempfile.mkdtemp(prefix="morpheus-hosted-acceptance-", dir=os.environ.get("RUNNER_TEMP")))
    acceptance = Acceptance(root, args.gitleaks)
    try:
        acceptance.start_keyring()
        acceptance.script("local_matrix_env", "up")
        acceptance.script("local_matrix_rtc", "up")
        acceptance.script("local_matrix_rtc", "probe")
        fixture = fixture_from_output(str(root / "accounts.json"), root)
        for name in CHAT_TESTS:
            acceptance.test(name, fixture)
        acceptance.test("search_local", fixture, extra={"MORPHEUS_MATRIX_SEARCH_CLASSIC": "1"})
        recovery = fixture_from_output(acceptance.script("local_matrix_env", "recovery-fixture"), root)
        acceptance.test("recovery_local", recovery, extra={"MORPHEUS_MATRIX_TEST_RECOVERY": "1"})
        gate = fixture_from_output(acceptance.script("local_matrix_env", "rtc-gate-fixture"), root)
        acceptance.test("unsupported_transport_local", gate, rtc=True,
                        extra={"MORPHEUS_MATRIX_RTC_GATE_TEST_ACCOUNTS": str(gate)})
        acceptance.rtc_cases()
    finally:
        acceptance.summary()
        # Only these containers belong to this fresh, explicitly checked run.
        # Always attempt both cleanups; preserve the original test failure.
        for name in ("local_matrix_rtc", "local_matrix_env"):
            try:
                acceptance.script(name, "down")
            except (RuntimeError, OSError) as error:
                print(f"Cleanup incomplete: {error}", file=sys.stderr)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError) as error:
        print(f"Matrix acceptance failed: {error}", file=sys.stderr)
        raise SystemExit(1)
