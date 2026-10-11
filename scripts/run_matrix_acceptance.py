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
from element_peer_fixture import VERSION as ELEMENT_VERSION, IMAGE_DIGEST as ELEMENT_IMAGE_DIGEST
from run_element_acceptance import diagnostics as element_diagnostics, evidence as element_evidence
from matrix_fixture_diagnostics import is_progress_line, matrix_fixture_diagnostics
from element_process_namespace import namespace_command, stop_owned, wait_owned


REPO = Path(__file__).resolve().parent.parent
FIXTURE_CONTAINERS = (
    "morpheus-synapse-a", "morpheus-synapse-b", "morpheus-rtc-livekit",
    "morpheus-rtc-auth", "morpheus-rtc-federation", "morpheus-rtc-sfu-loopback",
)
FIXTURE_VOLUMES = ("morpheus-synapse-a-data", "morpheus-synapse-b-data")
FIXTURE_NETWORK = "morpheus-matrix-test"
CHAT_TESTS = (
    "live_local", "drafts_local", "room_actions_local", "formatted_local",
    "media_local", "discovery_local", "moderation_local", "search_local",
)
RESULT = re.compile(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;")
LIVE_STAGES = frozenset((
    "fixture", "login_alice", "sync_alice", "send_alice", "login_carol", "sync_carol",
    "switch_alice", "cached_alice", "switch_carol", "stop_primary", "restore_carol",
    "sync_restored", "restore_alice", "cached_restored", "login_bob", "sync_bob",
    "encrypted_send", "encrypted_edit", "encrypted_reply", "encrypted_reaction",
    "encrypted_thread_send", "encrypted_thread_receive", "action_isolation", "return_alice",
    "stop_peer", "restore_peer", "encrypted_restart", "logout_bob", "logout_alice",
    "logout_carol", "stop_restored", "classic_login", "classic_sync", "classic_logout",
    "classic_stop", "profile_cleanup", "complete",
))
LIVE_STAGE = re.compile(r"^MORPHEUS_LIVE_STAGE ([a-z_]+)$", re.MULTILINE)
LATE_JOIN_EVIDENCE = re.compile(r"^MORPHEUS_RTC_LATE_JOIN historical_member=(true|false)$", re.MULTILINE)
MODERATION_STAGES = frozenset(("invite_initial", "invite_after_kick", "invite_after_unban"))
MODERATION_FAILURES = frozenset((
    "selected_account_changed", "room_not_found", "invalid_user_id",
    "history_sender_unverified", "history_cross_signing_missing", "sdk", "other",
))
MODERATION_CHECKS = frozenset((
    "reinvite_identity_unverified", "reinvite_trust_unchanged", "reinvite_after_kick", "reinvite_after_unban",
))
MODERATION_STAGE = re.compile(r"^MORPHEUS_MODERATION_STAGE ([a-z_]+)$", re.MULTILINE)
MODERATION_FAILURE = re.compile(r"^MORPHEUS_MODERATION_FAILURE ([a-z_]+)$", re.MULTILINE)
MODERATION_EVIDENCE = re.compile(r"^MORPHEUS_MODERATION_CHECK ([a-z_]+)=(true|false)$", re.MULTILINE)
OAUTH_PHASES = {
    "login_refresh": (
        "cancelled", "login", "callback_replay_rejected", "refresh", "vaulted",
        "no_plaintext_credentials",
    ),
    "restore_refresh": (
        "restored", "expired_token_rejected", "automatic_refresh", "vaulted", "remote_logout",
        "account_removed", "vault_removed", "logout",
    ),
}
OAUTH_CHECK_NAMES = frozenset(name for names in OAUTH_PHASES.values() for name in names)
OAUTH_EVIDENCE = re.compile(r"^MORPHEUS_OAUTH_CHECK ([a-z_]+)=(true|false)$", re.MULTILINE)
OAUTH_STAGES = frozenset((
    "fixture_root", "ports", "certificate", "configuration", "mas_config", "synapse_config", "merge_config",
    "postgres_start", "postgres_ready", "synapse_start", "mas_start", "mas_ready", "synapse_ready",
    "registration", "cleanup", "browser_authorize", "browser_login", "browser_consent", "browser_callback",
    "fixture_load", "profile_open", "cancel_begin", "cancel_verify", "login_begin", "browser_forms",
    "login_finish", "login_identity", "callback_replay", "manual_refresh", "vault_persist", "profile_scan",
    "restore", "restore_identity", "expiry_wait", "expired_token_probe", "automatic_refresh", "logout",
    "logout_remote", "logout_account", "logout_vault",
))
OAUTH_FAILURES = frozenset(("fixture_operation", "browser_protocol", "rust_acceptance", "deadline",
                           "logout_timeout", "logout_operation", "logout_remote", "logout_account",
                           "logout_vault_lookup", "logout_vault_retained",
                           "browser_callback_fragment", "browser_callback_query", "browser_callback_state",
                           "browser_callback_code", "browser_oauth_access_denied", "browser_oauth_login_required",
                           "browser_oauth_consent_required", "browser_oauth_interaction_required",
                           "browser_oauth_invalid_request", "browser_oauth_server_error", "browser_oauth_other",
                           "browser_helper_exit", "browser_helper_output_size", "browser_helper_output_schema",
                           "browser_callback_url", "browser_callback_match"))
OAUTH_STAGE = re.compile(r"^MORPHEUS_OAUTH_STAGE ([a-z_]+)$", re.MULTILINE)
OAUTH_FAILURE = re.compile(r"^MORPHEUS_OAUTH_FAILURE ([a-z_]+)$", re.MULTILINE)
RING_TEST = "outgoing_ring::live_tests::actual_outgoing_invitation_encrypted_receipt_and_redaction"
RING_CHECKS = frozenset((
    "sticky_sync", "own_crypto", "peer_trust", "joined_without_media", "encrypted_membership",
    "invitation_receipt", "fetched_decrypted_notification", "targeted_reference", "redaction_receipt",
    "redaction_observed", "cleanup",
))
RING_CHECK = re.compile(r"^MORPHEUS_RING_CHECK ([a-z_]+)=(true|false)$", re.MULTILINE)
RING_RESULT = re.compile(r"^MORPHEUS_RING_RESULT passed=(true|false)$", re.MULTILINE)
RING_STAGES = frozenset((
    "fixture", "login", "sticky_sync", "own_crypto", "peer_trust", "targets", "join", "membership",
    "send", "fetch", "validate", "redact", "redaction", "cleanup",
))
RING_STAGE = re.compile(r"^MORPHEUS_RING_STAGE ([a-z_]+)$", re.MULTILINE)
RING_FAILURE = re.compile(r"^MORPHEUS_RING_FAILURE ([a-z_]+)$", re.MULTILINE)


def live_diagnostics(output: str) -> dict:
    """Distinguish an entered test from compile/startup stalls without raw logs."""
    stages = [stage for stage in LIVE_STAGE.findall(output) if stage in LIVE_STAGES]
    result = {"test_entered": "fixture" in stages}
    if stages:
        result["stage"] = stages[-1]
    return result


def ring_evidence(output: str) -> dict[str, bool]:
    """Allow only fixed checks; a later true cannot erase an earlier false."""
    checks = {}
    for name, value in RING_CHECK.findall(output):
        if name in RING_CHECKS:
            checks[name] = checks.get(name, True) and value == "true"
    return checks


def ring_diagnostics(output: str) -> dict:
    """Only source-defined stages, booleans and failure categories are public."""
    diagnostics = {"checks": ring_evidence(output)}
    stages = [stage for stage in RING_STAGE.findall(output) if stage in RING_STAGES]
    failures = [stage for stage in RING_FAILURE.findall(output) if stage in RING_STAGES]
    if stages:
        diagnostics["stage"] = stages[-1]
    if failures:
        diagnostics["failure"] = failures[-1]
    results = RING_RESULT.findall(output)
    if results:
        diagnostics["passed"] = all(value == "true" for value in results)
    return diagnostics


def ring_passed_tests(output: str) -> int:
    """The opt-in app test, complete fixed facts and its final result must agree."""
    named = re.findall(rf"^test {re.escape(RING_TEST)} \.\.\. ok$", output, re.MULTILINE)
    checks = ring_evidence(output)
    if (passed_tests(output) != 1 or len(RESULT.findall(output)) != 1 or len(named) != 1
            or any(checks.get(name) is not True for name in RING_CHECKS)
            or "failure" in ring_diagnostics(output)
            or RING_RESULT.findall(output) != ["true"]):
        raise RuntimeError("Outgoing ring acceptance evidence is incomplete")
    return 1


def moderation_diagnostics(output: str) -> dict:
    result = {}
    stages = [value for value in MODERATION_STAGE.findall(output) if value in MODERATION_STAGES]
    failures = [value for value in MODERATION_FAILURE.findall(output) if value in MODERATION_FAILURES]
    if stages:
        result["stage"] = stages[-1]
    if failures:
        result["failure"] = failures[-1]
    for name, value in MODERATION_EVIDENCE.findall(output):
        if name in MODERATION_CHECKS:
            checks = result.setdefault("checks", {})
            checks[name] = checks.get(name, True) and value == "true"
    return result


def moderation_evidence(output: str) -> dict[str, bool]:
    evidence = moderation_diagnostics(output)
    checks = evidence.get("checks", {})
    if "failure" in evidence or any(checks.get(name) is not True for name in MODERATION_CHECKS):
        raise RuntimeError("Moderation acceptance evidence is incomplete")
    return checks


def oauth_diagnostics(output: str) -> dict[str, str]:
    """Report the last recognized attempted stage and fixed failure category."""
    result = {}
    stages = [stage for stage in OAUTH_STAGE.findall(output) if stage in OAUTH_STAGES]
    failures = [failure for failure in OAUTH_FAILURE.findall(output) if failure in OAUTH_FAILURES]
    if stages:
        result["stage"] = stages[-1]
    if failures:
        result["failure"] = failures[-1]
    return result


def oauth_evidence(output: str, required=()) -> dict[str, bool]:
    """Only exact allowlisted booleans can leave private OAuth test logs."""
    evidence = {}
    for name, value in OAUTH_EVIDENCE.findall(output):
        if name in OAUTH_CHECK_NAMES:
            # A false result cannot be hidden by a later successful marker.
            evidence[name] = evidence.get(name, True) and value == "true"
    if any(evidence.get(name) is not True for name in required):
        raise RuntimeError("Delegated OAuth acceptance evidence is incomplete")
    return evidence


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
    # The OAuth helper keeps its sole fixture path on stdout and emits only
    # fixed progress codes on stderr. This runner deliberately captures both.
    lines = []
    for line in output.strip().splitlines():
        if is_progress_line(line):
            continue
        match = OAUTH_STAGE.fullmatch(line)
        if match and match[1] in OAUTH_STAGES:
            continue
        lines.append(line)
    if len(lines) != 1:
        raise RuntimeError("Fixture command did not return a single private fixture path")
    path = external_path(Path(lines[0]))
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

    def run(self, label: str, args: list[str], *, extra=None, test=False, oauth_checks=(), element_checks=False,
            ring_case=None, stdin=None, seconds=900, pid_namespace=False) -> str:
        if ring_case is not None and ring_case not in (
                ("current", "dm"), ("current", "room"), ("sticky-2025", "dm"), ("sticky-2025", "room")):
            raise RuntimeError("Invalid outgoing ring acceptance case")
        self.sequence += 1
        log = self.logs / f"{self.sequence:02d}-{label}.log"
        started = time.monotonic()
        print(f"Starting {label}", flush=True)
        timed_out = False
        if pid_namespace:
            if stdin is not None:
                raise RuntimeError("Contained commands require non-TTY stdin")
            args = namespace_command(args, self.root / f"namespace-{self.sequence:02d}", self.env | (extra or {}))
        # Create the private log before the child can write anything to it.
        fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as output:
            process = subprocess.Popen(
                args, cwd=REPO, env=self.env | (extra or {}),
                stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL,
                stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
            )
            try:
                if pid_namespace:
                    wait_owned(process, seconds)
                else:
                    process.communicate(stdin, timeout=seconds)
            except subprocess.TimeoutExpired:
                # Cargo and fixture scripts spawn children. Stop the whole
                # owned group so a timed-out test cannot outlive its services.
                if not pid_namespace:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                timed_out = True
            finally:
                if pid_namespace:
                    # Keep the sudo leader unreaped through the group kill.
                    # PID1 death also kills detached browser/native sessions.
                    stop_owned(process)
        output = log.read_text(errors="replace")
        # Scan bytes through stdin: avoid reading the adjacent credential files
        # and avoid source allowlists/inline exclusions applying to fresh logs.
        scan = subprocess.run(
            [self.gitleaks, "stdin", "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
            input=output, text=True, capture_output=True, cwd=self.root,
        )
        if scan.returncode:
            raise RuntimeError(f"{label} log failed credential scanning; raw log withheld")
        if process.returncode or timed_out:
            # Scanner coverage is not a guarantee: arbitrary server errors can
            # reflect secrets without a recognizable prefix. Never publish raw
            # child logs, including on failure. Only source locations and Rust
            # test identifiers are useful public diagnostics here.
            failed = re.findall(r"^test ([A-Za-z0-9_:]+) \.\.\. FAILED$", output, re.MULTILINE)
            locations = re.findall(r"panicked at (crates/[A-Za-z0-9_./-]+\.rs:\d+:\d+)", output)
            evidence = {"check": label, "failed_tests": failed, "panic_locations": locations}
            if label == "live_local":
                evidence["live_diagnostics"] = live_diagnostics(output)
                if timed_out:
                    evidence["live_diagnostics"]["failure"] = "deadline"
            if fixture_diagnostics := matrix_fixture_diagnostics(output):
                if timed_out:
                    fixture_diagnostics["failure"] = "deadline"
                evidence["matrix_fixture_diagnostics"] = fixture_diagnostics
            # This exact boolean-only marker says whether a fresh device could
            # already decrypt the old membership. Never print event/account
            # identifiers, arbitrary server text, ciphertext, or raw logs.
            late_join = LATE_JOIN_EVIDENCE.findall(output)
            if late_join:
                evidence["late_join_historical_member"] = [value == "true" for value in late_join]
            if oauth_checks:
                evidence["oauth_checks"] = oauth_evidence(output)
            if diagnostics := oauth_diagnostics(output):
                if timed_out:
                    diagnostics["failure"] = "deadline"
                evidence["oauth_diagnostics"] = diagnostics
            if label == "moderation_local":
                evidence["moderation_diagnostics"] = moderation_diagnostics(output)
            if element_checks:
                evidence["element_checks"] = element_evidence(output)
                evidence["element_diagnostics"] = element_diagnostics(output)
                if timed_out:
                    evidence["element_diagnostics"]["failure"] = "deadline"
            if ring_case is not None:
                evidence["ring_diagnostics"] = ring_diagnostics(output)
                if timed_out:
                    evidence["ring_diagnostics"]["failure"] = "deadline"
            print(json.dumps(evidence), flush=True)
            if timed_out:
                raise RuntimeError(f"{label} exceeded its {seconds}-second deadline")
            raise RuntimeError(f"{label} failed with exit status {process.returncode}")
        record = {"check": label, "seconds": round(time.monotonic() - started, 2)}
        if ring_case is not None:
            try:
                record["passed"] = ring_passed_tests(output)
            except RuntimeError:
                print(json.dumps({"check": label, "ring_diagnostics": ring_diagnostics(output)}), flush=True)
                raise
            record["ring_checks"] = ring_evidence(output)
            record["ring_mode"], record["ring_target"] = ring_case
        elif test:
            record["passed"] = passed_tests(output)
        if label == "live_local":
            diagnostics = live_diagnostics(output)
            if not diagnostics["test_entered"] or diagnostics.get("stage") != "complete":
                print(json.dumps({"check": label, "live_diagnostics": diagnostics}), flush=True)
                raise RuntimeError("Live Matrix acceptance completion evidence is incomplete")
            record["live_diagnostics"] = diagnostics
        if label == "moderation_local":
            # A green moderation result must exercise an existing, unverified
            # account identity on this fresh device without changing trust.
            try:
                record["moderation_checks"] = moderation_evidence(output)
            except RuntimeError:
                print(json.dumps({"check": label, "moderation_diagnostics": moderation_diagnostics(output)}), flush=True)
                raise
        if oauth_checks:
            record["oauth_checks"] = oauth_evidence(output, oauth_checks)
        if element_checks:
            record["element_checks"] = element_evidence(output, complete=True)
            record["element_peer"] = {"version": ELEMENT_VERSION, "image_digest": ELEMENT_IMAGE_DIGEST,
                                      "protocol": "matrix_2_0_sticky2025", "baseline": "presynced"}
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

    def outgoing_ring_cases(self):
        # Each mode/target uses new users and an encrypted room. Existing
        # devices, memberships or notification events cannot satisfy its gate.
        for mode in ("current", "sticky-2025"):
            for target in ("dm", "room"):
                fixture = fixture_from_output(self.script("local_matrix_rtc", "rtc-fixture"), self.root)
                self.run(
                    f"outgoing-ring-{mode}-{target}",
                    ["cargo", "test", "--locked", "-p", "morpheus-app", "--features", "native-calls",
                     "--bin", "morpheus-app", RING_TEST, "--", "--ignored", "--exact", "--show-output",
                     "--test-threads=1"],
                    extra={"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": str(fixture),
                           "MORPHEUS_RING_TEST_MODE": mode, "MORPHEUS_RING_TEST_TARGET": target},
                    ring_case=(mode, target), seconds=300,
                )

    def oauth_cases(self):
        """Separate processes prove that refresh tokens survive vaulted restart."""
        services = self.root / "oauth-services"
        profile = private_directory(self.root / "oauth-profile")
        helper = [sys.executable, "scripts/local_matrix_auth.py"]
        failed = False
        try:
            output = self.run("oauth-services-up", helper + ["up", "--root", str(services)], seconds=300)
            fixture = fixture_from_output(output, services)
            for phase, required in OAUTH_PHASES.items():
                self.run(
                    "oauth-local-" + phase.replace("_", "-"),
                    ["cargo", "test", "--locked", "-p", "morpheus-matrix", "--test", "oauth_local",
                     "--", "--nocapture", "--test-threads=1"],
                    extra={"MORPHEUS_MATRIX_OAUTH_FIXTURE": str(fixture),
                           "MORPHEUS_MATRIX_OAUTH_PROFILE": str(profile),
                           "MORPHEUS_MATRIX_OAUTH_PHASE": phase},
                    test=True, oauth_checks=required, seconds=600,
                )
        except BaseException:
            failed = True
            raise
        finally:
            try:
                cleanup = helper + ["down", "--root", str(services)]
                if failed:
                    cleanup.append("--keep-data")
                self.run("oauth-services-down", cleanup, seconds=120)
            except (RuntimeError, OSError):
                # Keep the failed test as the primary diagnostic, but do not
                # claim a passing run when its owned service cleanup failed.
                if not failed:
                    raise
                print("OAuth fixture cleanup incomplete; private state retained", file=sys.stderr)

    def element_case(self):
        self.run("element-process-containment", [sys.executable, "scripts/element_process_namespace.py",
                 "--check", str(self.root / "element-containment-probe")], seconds=60)
        fixture = fixture_from_output(self.script("local_matrix_rtc", "rtc-fixture"), self.root)
        root = self.root / "element-interop"
        failed = False
        try:
            self.run("element-call-rc-sticky2025-presynced", [
                sys.executable, "scripts/run_element_acceptance.py", "--root", str(root),
                "--fixture", str(fixture), "--gitleaks", self.gitleaks,
            ], element_checks=True, seconds=1800, pid_namespace=True)
        except BaseException:
            failed = True
            raise
        finally:
            # A separate cleanup also covers a killed helper, using only its
            # private nonce ownership record. It never touches shared services.
            try:
                self.run("element-fixture-down", [sys.executable, "scripts/element_peer_fixture.py", "down",
                         "--root", str(root / "service"), "--keep-data"], seconds=100)
            except (RuntimeError, OSError):
                if not failed:
                    raise
                print("Element peer cleanup incomplete; private state retained", file=sys.stderr)

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

    def verify_fixture_absence(self):
        """Prove cleanup using checked inventories, not unchecked inspect failures.

        Raw Docker output uses the same private logs and credential scanning
        as the acceptance commands. Retained volumes and private fixture data
        are intentional; this checks only the owned containers and network.
        """
        query_failed = False
        resource_remains = False
        for label, command, names in (
            ("verify-fixture-container-cleanup",
             ["docker", "container", "ls", "--all", "--format", "{{.Names}}"],
             FIXTURE_CONTAINERS),
            ("verify-fixture-network-cleanup",
             ["docker", "network", "ls", "--format", "{{.Name}}"],
             (FIXTURE_NETWORK,)),
        ):
            try:
                output = self.run(label, command, seconds=30)
            except (RuntimeError, OSError):
                query_failed = True
                continue
            resource_remains |= bool(set(output.splitlines()).intersection(names))
        if query_failed:
            raise RuntimeError("Disposable Matrix fixture absence could not be verified")
        if resource_remains:
            raise RuntimeError("Disposable Matrix fixture resources remain after cleanup")

    def summary(self):
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as output:
                output.write("### Disposable Matrix integration\n\n")
                output.write("Real Synapse, Matrix Authentication Service, OS Secret Service and LiveKit; "
                             "synthetic call media and OAuth browser-form protocol automation.\n\n")
                output.write("| Check | Passed tests | Seconds |\n| --- | ---: | ---: |\n")
                for item in self.results:
                    if "passed" in item:
                        output.write(f"| {item['check']} | {item['passed']} | {item['seconds']} |\n")
                    elif "element_checks" in item:
                        output.write(f"| {item['check']} | {len(item['element_checks'])} protocol assertions | {item['seconds']} |\n")
                if any("element_checks" in item for item in self.results):
                    output.write(f"\nElement peer passed: `{ELEMENT_VERSION}`, `{ELEMENT_IMAGE_DIGEST}`. "
                                 "Only its pre-synced Sticky2025 mode was tested, with two-way synthetic video, "
                                 "browser-to-native audio, roster and departure. This does not verify stable "
                                 "v0.26.1 Matrix 2.0, Morpheus's Current default, fresh-device late join, "
                                 "native-to-browser audio, or full call release acceptance.\n")
                if any("ring_checks" in item for item in self.results):
                    output.write("\nOutgoing invitation rows verify the actual app sender, fetched and decrypted "
                                 "encrypted events by receipt, and server redaction for the listed modes and targets. "
                                 "They do not verify incoming ringing, notification presentation, or Element receipt.\n")
                output.write("\nNo fixture credentials or installers were uploaded. Physical-device, UI, "
                             "OS-browser handoff, broader Element Call interoperability, and installed-package gates remain separate.\n")


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
    # Prove absence before taking cleanup ownership. An unavailable daemon is
    # not evidence of absence, and arbitrary inventory/daemon output is private.
    for command, names in (
        (["docker", "container", "ls", "--all", "--format", "{{.Names}}"], FIXTURE_CONTAINERS),
        (["docker", "volume", "ls", "--format", "{{.Name}}"], FIXTURE_VOLUMES),
        (["docker", "network", "ls", "--format", "{{.Name}}"], (FIXTURE_NETWORK,)),
    ):
        try:
            result = subprocess.run(command, capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.SubprocessError, UnicodeError):
            raise RuntimeError("Could not verify absence of a pre-existing Matrix fixture") from None
        if result.returncode:
            raise RuntimeError("Could not verify absence of a pre-existing Matrix fixture")
        if set(result.stdout.splitlines()).intersection(names):
            raise RuntimeError("Pre-existing Matrix fixture found; refusing to modify it")
    root = Path(tempfile.mkdtemp(prefix="morpheus-hosted-acceptance-", dir=os.environ.get("RUNNER_TEMP")))
    acceptance = Acceptance(root, args.gitleaks)
    body_failed = False
    try:
        acceptance.start_keyring()
        acceptance.script("local_matrix_env", "up")
        fixture = fixture_from_output(str(root / "accounts.json"), root)
        for name in CHAT_TESTS:
            acceptance.test(name, fixture)
        acceptance.test("search_local", fixture, extra={"MORPHEUS_MATRIX_SEARCH_CLASSIC": "1"})
        recovery = fixture_from_output(acceptance.script("local_matrix_env", "recovery-fixture"), root)
        acceptance.test("recovery_local", recovery, extra={"MORPHEUS_MATRIX_TEST_RECOVERY": "1"})
        acceptance.oauth_cases()
        # Message, draft, recovery and OAuth checks use their own services and
        # do not require RTC. Collect that evidence before RTC bootstrap, while
        # preserving fatal failures and the same unconditional cleanup below.
        acceptance.script("local_matrix_rtc", "up")
        acceptance.script("local_matrix_rtc", "probe")
        gate = fixture_from_output(acceptance.script("local_matrix_env", "rtc-gate-fixture"), root)
        acceptance.test("unsupported_transport_local", gate, rtc=True,
                        extra={"MORPHEUS_MATRIX_RTC_GATE_TEST_ACCOUNTS": str(gate)})
        acceptance.outgoing_ring_cases()
        acceptance.rtc_cases()
        acceptance.element_case()
    except BaseException:
        body_failed = True
        raise
    finally:
        summary_failed = False
        try:
            acceptance.summary()
        except BaseException:
            summary_failed = True
        # Only these containers belong to this fresh, explicitly checked run.
        # Summary errors and interrupts must not prevent either cleanup. Keep
        # any original body exception; otherwise cleanup failure is fatal.
        cleanup_failures = []
        for name in ("local_matrix_rtc", "local_matrix_env"):
            try:
                acceptance.script(name, "down")
            except BaseException:
                cleanup_failures.append(name)
        try:
            acceptance.verify_fixture_absence()
        except BaseException:
            cleanup_failures.append("fixture_absence")
        try:
            if summary_failed:
                print("Acceptance summary could not be written", file=sys.stderr)
            for name in cleanup_failures:
                # Labels are fixed literals above, never exception payloads.
                print(f"Cleanup incomplete: {name}", file=sys.stderr)
        except (OSError, ValueError):
            # A broken diagnostic stream cannot replace the original failure.
            pass
        if not body_failed:
            if cleanup_failures:
                raise RuntimeError("Disposable Matrix service cleanup did not complete") from None
            if summary_failed:
                raise RuntimeError("Matrix acceptance summary could not be written") from None


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError) as error:
        print(f"Matrix acceptance failed: {error}", file=sys.stderr)
        raise SystemExit(1)
