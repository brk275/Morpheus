#!/usr/bin/env python3
"""Hosted-only native-vault fixture wrapper with isolated owned credentials.

Never publishes raw command output, generated values, paths, or keychain names.
An executed fixture is core vault acceptance, not Matrix login or installed UI.
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import signal
import selectors
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid

try:
    from scripts import package_smoke
except ModuleNotFoundError:
    import package_smoke

REPO = Path(__file__).resolve().parents[1]
SECURITY = "/usr/bin/security"
BOUND = 16384
PREPARE_CHECKS = frozenset((
    "initial_empty", "password_enabled", "wrong_current_rejected", "password_changed",
    "synthetic_accounts_seeded", "protected_direct_switch_blocked", "wrong_authorization_rejected",
    "correct_authorization_granted", "cancel_before_verify", "cancel_after_verify",
    "stale_policy_grant", "reset_with_accounts_blocked", "prepare_inactive",
))
VERIFY_CHECKS = frozenset((
    "restart_protected_inactive", "old_password_rejected", "persisted_password_authorized",
    "distinct_native_entries", "remove_b_complete", "remove_b_absent_a_unchanged",
    "reset_with_one_account_blocked", "remove_a_complete", "all_accounts_absent",
    "password_reset_complete", "empty_reopen",
))
CLEANUP_CHECKS = frozenset(("cleanup_app_entry", "cleanup_account_a", "cleanup_account_b", "cleanup_profile_absent"))
CHECKS = {"prepare": PREPARE_CHECKS, "verify_remove": VERIFY_CHECKS, "cleanup": CLEANUP_CHECKS}
STAGES = {
    "prepare": frozenset(("input_validated", "profile_open", "password_enable", "wrong_current", "seed_inactive_accounts",
        "protected_authorization", "cancel_before_verify", "cancel_after_verify", "policy_revocation", "prepare_complete")),
    "verify_remove": frozenset(("input_validated", "restart_open", "restart_authorization", "remove_b", "remove_a",
        "password_reset", "empty_reopen", "verify_remove_complete")),
    "cleanup": frozenset(("input_validated", "cleanup_validate_journal", "cleanup_owned_entries", "cleanup_owned_profile", "cleanup_complete")),
}
TERMINAL = {"prepare": "prepare_complete", "verify_remove": "verify_remove_complete", "cleanup": "cleanup_complete"}


class Failure(Exception):
    def __init__(self, code, *, cleanup=False, created=False):
        # Only literals from reviewed callers may be constructed here.
        super().__init__("Native vault acceptance failed; private output withheld")
        self.code = code
        self.cleanup = cleanup
        self.created = created


def require(condition, code):
    if not condition:
        raise Failure(code)


class Evidence:
    def __init__(self):
        self.first = None
        self.cleanup_failures = []
        self.phases = {}

    def fail(self, stage, code):
        if self.first is None:
            self.first = {"stage": stage, "code": code}

    def cleanup_failed(self, code):
        if code not in self.cleanup_failures:
            self.cleanup_failures.append(code)
        self.fail("cleanup", code)


@dataclass(frozen=True)
class CommandResult:
    status: int
    output: bytes


class MacRunner:
    """Retain an unreaped process group until bounded teardown has completed."""
    def __init__(self):
        self.quiescent = True

    def run(self, command, *, environment=None, payload=b"", seconds=15, output_limit=BOUND):
        require(self.quiescent, "children_unconfirmed")
        require(sys.platform == "darwin", "process_platform")
        require(type(payload) is bytes and len(payload) <= 4096, "process_input_bound")
        require(0 < seconds <= 300 and 0 < output_limit <= 262144, "process_bounds")
        try:
            package_smoke.require_nonreaping_wait()
        except (OSError, RuntimeError):
            raise Failure("process_ownership") from None
        process = None
        failure = None
        status = None
        data = bytearray()
        selector = selectors.DefaultSelector()
        try:
            # Anonymous temporary input avoids secret-bearing arguments or
            # persistent files and cannot block the supervisor while writing.
            with tempfile.TemporaryFile() as private_input:
                os.fchmod(private_input.fileno(), 0o600)
                private_input.write(payload)
                private_input.seek(0)
                process = subprocess.Popen(command, stdin=private_input, stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT, env=environment, start_new_session=True)
                self.quiescent = False
                os.set_blocking(process.stdout.fileno(), False)
                selector.register(process.stdout, selectors.EVENT_READ)
                deadline = time.monotonic() + seconds
                eof = False
                while True:
                    status = package_smoke.process_status(process)
                    if status is not None and eof:
                        break
                    if time.monotonic() >= deadline:
                        raise Failure("process_deadline")
                    for key, _ in selector.select(min(0.05, max(0, deadline - time.monotonic()))):
                        block = os.read(key.fd, min(65536, output_limit + 1))
                        if not block:
                            selector.unregister(key.fileobj)
                            eof = True
                        else:
                            require(len(data) + len(block) <= output_limit, "process_output_bound")
                            data.extend(block)
                require(type(status) is int, "process_status")
        except Failure as error:
            failure = error
        except KeyboardInterrupt:
            failure = Failure("interrupted")
        except (OSError, RuntimeError, ValueError):
            failure = Failure("process_io")
        finally:
            selector.close()
            if process is not None:
                try:
                    self.stop_owned(process)
                    self.quiescent = True
                except (Failure, OSError, RuntimeError, subprocess.SubprocessError):
                    if failure is None:
                        failure = Failure("process_cleanup", cleanup=True)
                    else:
                        failure.cleanup = True
                finally:
                    if process.stdout is not None:
                        process.stdout.close()
        if failure is not None:
            raise failure
        return CommandResult(status, bytes(data))


    def stop_owned(self, process):
        # The reviewed fixture/security tools do not detach or start new sessions.
        # Retain the child PID until every member of its original group is dead.
        package_smoke.process_status(process)
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            # XNU may return EPERM for a zombie-only group. Do not infer success
            # from that error: the bounded metadata snapshot below must prove it.
            pass
        deadline = time.monotonic() + 5
        while True:
            snapshot = _MacSnapshotRunner().snapshot()
            if not group_has_live_members(snapshot, process.pid):
                break
            require(time.monotonic() < deadline, "process_descendants")
            time.sleep(0.05)
        process.wait(timeout=5)
        require(process.returncode is not None, "process_cleanup")


def group_has_live_members(raw, group):
    require(type(raw) is bytes and len(raw) <= 65536 and type(group) is int and group > 0, "process_snapshot")
    try:
        text = raw.decode("ascii")
    except UnicodeError:
        raise Failure("process_snapshot") from None
    states = []
    for line in text.splitlines():
        match = re.fullmatch(r"\s*([0-9]{1,10})\s+([A-Za-z0-9+<=>-]{1,24})\s*", line)
        require(match is not None, "process_snapshot")
        if int(match.group(1)) == group:
            states.append(match.group(2))
    # Our unreaped leader anchors the group and must appear even as a zombie.
    require(bool(states), "process_snapshot")
    return any(not state.startswith("Z") for state in states)


class _MacSnapshotRunner(MacRunner):
    """Only the trusted system ps leaf; never a fixture or caller command."""
    def snapshot(self):
        result = super().run(["/bin/ps", "-axo", "pgid=,stat="], seconds=3, output_limit=65536)
        require(result.status == 0, "process_snapshot")
        return result.output

    def stop_owned(self, process):
        # Apple ps source contains no subprocess execution in this fixed mode.
        # This terminates/reaps the inspector, without recursive inspection.
        if package_smoke.process_status(process) is None:
            process.kill()
        process.wait(timeout=5)
        require(process.returncode is not None, "process_cleanup")



class _PipePump:
    """Own only two parent pipe ends; cap retained output and signal all errors."""
    def __init__(self, input_fd, output_fd, payload, limit):
        self.input_fd, self.output_fd = input_fd, output_fd
        self.payload, self.limit = payload, limit
        self.output = bytearray()
        self.input_error = threading.Event()
        self.output_error = threading.Event()
        self.overflow = threading.Event()
        self.input_done = threading.Event()
        self.output_done = threading.Event()
        self.sent = self.eof = False
        self.writer = threading.Thread(target=self.write, daemon=True)
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.writer_started = self.reader_started = False
        self.start_uncertain = False

    def start(self):
        try:
            self.writer.start()
            self.writer_started = True
            self.reader.start()
            self.reader_started = True
        except BaseException:
            # An asynchronous interruption during Thread.start may leave its
            # native start outcome unknown. Never claim drained/allow deletion.
            self.start_uncertain = True
            raise

    def write(self):
        try:
            position = 0
            while position < len(self.payload):
                count = os.write(self.input_fd, self.payload[position:])
                require(count > 0, "process_input")
                position += count
            self.sent = True
        except (OSError, Failure):
            self.input_error.set()
        finally:
            try:
                os.close(self.input_fd)  # EOF for the child even on write error.
            except OSError:
                self.input_error.set()
            self.input_done.set()

    def read(self):
        try:
            while True:
                block = os.read(self.output_fd, 65536)
                if not block:
                    self.eof = True
                    break
                if len(self.output) + len(block) > self.limit:
                    self.overflow.set()
                elif not self.overflow.is_set():
                    self.output.extend(block)
                # Keep draining after overflow until Job termination closes all
                # child writers; no further bytes are retained.
        except OSError:
            self.output_error.set()
        finally:
            try:
                os.close(self.output_fd)
            except OSError:
                self.output_error.set()
            self.output_done.set()

    def finish(self):
        completed = not self.start_uncertain
        for thread, started, descriptor in ((self.writer, self.writer_started, self.input_fd),
                                             (self.reader, self.reader_started, self.output_fd)):
            if started:
                thread.join(timeout=2)
                completed = completed and not thread.is_alive()
            elif not self.start_uncertain:
                try:
                    os.close(descriptor)
                except OSError:
                    completed = False
        return completed


class WindowsRunner:
    """Suspended Job containment, noninheritable pipes, bounded private drains."""
    def __init__(self, root):
        self.root = root
        self.quiescent = True

    def run(self, command, *, environment=None, payload=b"", seconds=15, output_limit=BOUND):
        require(self.quiescent and sys.platform == "win32", "process_platform")
        require(type(payload) is bytes and len(payload) <= 4096, "process_input_bound")
        require(0 < seconds <= 300 and 0 < output_limit <= 262144, "process_bounds")
        try:
            from scripts import native_vault_windows as windows_root
            from scripts import run_windows_capture_acceptance as windows_job
        except ModuleNotFoundError:
            import native_vault_windows as windows_root
            import run_windows_capture_acceptance as windows_job
        windows_root.verify_private_root(self.root)
        job = pump = None
        descriptors = []
        streams = []
        failure = None
        status = None
        try:
            job = windows_job.WindowsJob()
            self.quiescent = False
            input_read, input_write = os.pipe(); descriptors.extend((input_read, input_write))
            output_read, output_write = os.pipe(); descriptors.extend((output_read, output_write))
            for descriptor in descriptors:
                os.set_inheritable(descriptor, False)
                require(not os.get_inheritable(descriptor), "process_pipe")
            input_stream = os.fdopen(input_read, "rb", buffering=0)
            descriptors.remove(input_read); streams.append(input_stream)
            output_stream = os.fdopen(output_write, "wb", buffering=0)
            descriptors.remove(output_write); streams.append(output_stream)
            handle = job.spawn(command, dict(os.environ) if environment is None else environment,
                               output_stream, stdin_stream=input_stream)
            # Child owns only inheritable duplicates. Close both parent copies
            # before starting pumps so complete Job teardown always produces EOF.
            input_stream.close(); output_stream.close()
            streams.clear()
            pump = _PipePump(input_write, output_read, payload, output_limit)
            descriptors.remove(input_write); descriptors.remove(output_read)
            pump.start()
            deadline = time.monotonic() + seconds
            while True:
                require(not pump.input_error.is_set(), "process_input")
                require(not pump.output_error.is_set(), "process_output")
                require(not pump.overflow.is_set(), "process_output_bound")
                try:
                    status = job.wait(handle, 0)
                except windows_job.Failure as error:
                    if error.code != "deadline":
                        raise
                    status = None
                if status is not None and pump.input_done.is_set() and pump.output_done.is_set():
                    require(pump.sent and pump.eof, "process_pipe")
                    break
                require(time.monotonic() < deadline, "process_deadline")
                time.sleep(0.02)
        except Failure as error:
            failure = error
        except windows_job.Failure:
            failure = Failure("process_job")
        except KeyboardInterrupt:
            failure = Failure("interrupted")
        except (OSError, RuntimeError, ValueError):
            failure = Failure("process_io")
        finally:
            cleanup_ok = True
            for stream in streams:
                try:
                    stream.close()
                except OSError:
                    cleanup_ok = False
            for descriptor in descriptors:
                try:
                    os.close(descriptor)
                except OSError:
                    cleanup_ok = False
            if job is not None:
                try:
                    job.close()  # Requires all assigned descendants inactive.
                except (windows_job.Failure, OSError):
                    cleanup_ok = False
            if pump is not None:
                try:
                    if not pump.finish():
                        cleanup_ok = False
                except (OSError, RuntimeError):
                    cleanup_ok = False
            self.quiescent = cleanup_ok
            if not cleanup_ok:
                if failure is None:
                    failure = Failure("process_cleanup", cleanup=True)
                else:
                    failure.cleanup = True
        if failure is not None:
            raise failure
        # Completion can race the last polling iteration. Inspect final pump
        # facts after all threads have joined, including close/write errors.
        require(not pump.input_error.is_set(), "process_input")
        require(not pump.output_error.is_set(), "process_output")
        require(not pump.overflow.is_set(), "process_output_bound")
        require(pump.sent and pump.eof and type(status) is int, "process_pipe")
        return CommandResult(status, bytes(pump.output))

def security_secret_payload(operation, password, path):
    require(operation in ("create-keychain", "unlock-keychain")
        and type(password) is str and re.fullmatch(r"[a-f0-9]{64}", password), "keychain_secret_input")
    text = str(path)
    require(path.is_absolute() and all(ord(char) >= 32 and ord(char) != 127 for char in text), "keychain_path")
    # Apple's split_line handles backslash escaping inside double quotes. This
    # is its literal token grammar, not shell quoting or shell execution.
    quoted = '"' + text.replace('\\', '\\\\').replace('"', '\\"') + '"'
    payload = (operation + ' -p ' + password + ' ' + quoted + '\n').encode("utf-8")
    require(len(payload) < 4096, "keychain_secret_input")
    return payload


def private_object(path, *, directory=False):
    info = path.lstat()
    require(not stat.S_ISLNK(info.st_mode), "private_path")
    require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode), "private_path")
    if sys.platform == "win32":
        # The absent root was atomically created with a protected owner/SYSTEM
        # inheritable DACL. Only reviewed fixture writes occur below it.
        require(not info.st_file_attributes & 0x400, "private_path")
    else:
        require(info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) & 0o077 == 0, "private_permissions")
    if not directory:
        require(info.st_nlink == 1, "private_links")
    return info


def make_mac_root(path, parent):
    require(path.parent == parent and re.fullmatch(r"morpheus-native-vault-[a-f0-9]{64}", path.name), "root_identity")
    path.mkdir(mode=0o700, exist_ok=False)
    try:
        info = private_object(path, directory=True)
        require(stat.S_IMODE(info.st_mode) == 0o700 and path.resolve(strict=True) == path, "root_permissions")
    except Failure as error:
        error.created = True
        raise
    except OSError:
        raise Failure("root_permissions", created=True) from None


def unique_json(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate_json_field")
        result[key] = value
    return result


def read_json(path):
    require(private_object(path).st_size <= BOUND, "private_json_bound")
    with path.open("rb") as source:
        raw = source.read(BOUND + 1)
    require(len(raw) <= BOUND, "private_json_bound")
    try:
        return json.loads(raw, object_pairs_hook=unique_json)
    except (UnicodeError, json.JSONDecodeError):
        raise Failure("private_json_schema") from None


def phase_record(path, phase, *, final=False, expected_cleanup=None):
    value = read_json(path)
    require(type(value) is dict and set(value) == {"version", "phase", "status", "stage", "failure", "checks"}, "phase_schema")
    require(type(value["version"]) is int and value["version"] == 1 and value["phase"] == phase, "phase_identity")
    require(type(value["status"]) is str and type(value["stage"]) is str
        and value["status"] in ("running", "passed", "failed") and value["stage"] in STAGES[phase], "phase_state")
    # The native failure text is never copied into public evidence. It is only
    # accepted as a bounded machine field; publication reduces it to one enum.
    require(value["failure"] is None or (type(value["failure"]) is str and re.fullmatch(r"[a-z_]{1,64}", value["failure"])), "phase_failure_schema")
    checks = value["checks"]
    require(type(checks) is dict and set(checks) <= CHECKS[phase]
        and all(type(result) is bool for result in checks.values()), "phase_checks")
    require((value["status"] == "failed") == (value["failure"] is not None), "phase_failure_state")
    if final:
        expected = expected_cleanup if phase == "cleanup" else CHECKS[phase]
        require(value["status"] == "passed" and value["stage"] == TERMINAL[phase]
            and value["failure"] is None and set(checks) == expected and all(checks.values()), "phase_incomplete")
    return {"phase": phase, "status": value["status"], "stage": value["stage"],
            "failure": None if value["failure"] is None else "fixture_failure", "checks": checks}


def metadata_paths(raw, *, default=False):
    require(type(raw) is bytes and len(raw) <= BOUND, "keychain_metadata")
    try:
        value = raw.decode("utf-8")
    except UnicodeError:
        raise Failure("keychain_metadata") from None
    if default and value.strip() == "<NULL>":
        return None
    paths = []
    for line in value.splitlines():
        match = re.fullmatch(r'\s*"([^"\r\n\x00-\x1f]+)"\s*', line)
        require(match is not None, "keychain_metadata")
        path = Path(match.group(1))
        require(path.is_absolute(), "keychain_metadata")
        paths.append(path)
    require(len(paths) <= 32 and (not default or len(paths) == 1), "keychain_metadata")
    return paths[0] if default else paths


class MacKeychain:
    def __init__(self, root, runner, evidence):
        self.root, self.runner, self.evidence = root, runner, evidence
        self.path = root / "native-vault.keychain-db"
        self.original_default = None
        self.original_list = None
        self.snapshotted = False
        self.may_have_changed = False
        self.owned_create_attempted = False
        self.ready = False

    def command(self, arguments):
        result = self.runner.run([SECURITY, *map(str, arguments)], seconds=15)
        require(result.status == 0, "keychain_command")
        return result.output

    def secret_command(self, operation, password):
        payload = security_secret_payload(operation, password, self.path)
        # Exactly one command then EOF; quit or a second command would replace
        # security's exit status and could conceal a failed create/unlock.
        result = self.runner.run([SECURITY, "-q", "-i"], payload=payload, seconds=15)
        require(result.status == 0, "keychain_command")

    def snapshot(self):
        current_default = metadata_paths(self.command(["default-keychain", "-d", "user"]), default=True)
        current_list = metadata_paths(self.command(["list-keychains", "-d", "user"]))
        self.original_default, self.original_list = current_default, current_list
        self.snapshotted = True

    def setup(self):
        # Validate interactive path spelling before any global preference change.
        password = secrets.token_hex(32)
        security_secret_payload("create-keychain", password, self.path)
        self.snapshot()
        require(not self.path.exists() and not self.path.is_symlink(), "keychain_exists")
        self.may_have_changed = True
        self.owned_create_attempted = True
        self.secret_command("create-keychain", password)
        private_object(self.path)
        self.secret_command("unlock-keychain", password)
        self.command(["list-keychains", "-d", "user", "-s", self.path])
        self.command(["default-keychain", "-d", "user", "-s", self.path])
        require(metadata_paths(self.command(["default-keychain", "-d", "user"]), default=True) == self.path
            and metadata_paths(self.command(["list-keychains", "-d", "user"])) == [self.path], "keychain_selection")
        self.ready = True

    def restore(self):
        if not self.may_have_changed:
            return True
        if not self.runner.quiescent:
            self.evidence.cleanup_failed("children_unconfirmed")
            return False
        if not self.snapshotted:
            self.evidence.cleanup_failed("keychain_snapshot_missing")
            return False
        failed = False
        commands = [["list-keychains", "-d", "user", "-s", *self.original_list],
                    ["default-keychain", "-d", "user", "-s"] + ([] if self.original_default is None else [self.original_default])]
        for arguments in commands:
            try:
                self.command(arguments)
            except (Failure, OSError):
                failed = True
                self.evidence.cleanup_failed("keychain_restore")
        try:
            self.verify_restored()
        except (Failure, OSError):
            failed = True
            self.evidence.cleanup_failed("keychain_restore_verify")
        # Delete only the new exact keychain. No original path is a target,
        # even when restoring its preference metadata reports an error.
        if self.owned_create_attempted and self.runner.quiescent:
            try:
                if self.path.exists():
                    private_object(self.path)
                    self.command(["delete-keychain", self.path])
                require(not self.path.exists() and not self.path.is_symlink(), "keychain_delete")
            except (Failure, OSError):
                failed = True
                self.evidence.cleanup_failed("keychain_delete")
        else:
            failed = True
            self.evidence.cleanup_failed("children_unconfirmed")
        try:
            self.verify_restored()
        except (Failure, OSError):
            failed = True
            self.evidence.cleanup_failed("keychain_restore_verify")
        return not failed

    def verify_restored(self):
        require(self.runner.quiescent, "children_unconfirmed")
        require(metadata_paths(self.command(["default-keychain", "-d", "user"]), default=True) == self.original_default
            and metadata_paths(self.command(["list-keychains", "-d", "user"])) == self.original_list, "keychain_restore")


def journal_cleanup_checks(root, nonce, account_a, account_b):
    value = read_json(root / "owned-items.json")
    require(type(value) is dict and set(value) == {"version", "run_nonce", "profile_uuid", "account_a", "account_b", "app_password", "account_a_item", "account_b_item"}, "journal_schema")
    require(type(value["version"]) is int and value["version"] == 1 and value["run_nonce"] == nonce
        and value["account_a"] == account_a and value["account_b"] == account_b, "journal_identity")
    profile = value["profile_uuid"]
    if profile is not None:
        try:
            parsed = uuid.UUID(profile)
        except (ValueError, TypeError, AttributeError):
            raise Failure("journal_profile") from None
        require(parsed.version == 4 and str(parsed) == profile, "journal_profile")
    expected = {"cleanup_profile_absent"}
    for name, fact in (("app_password", "cleanup_app_entry"), ("account_a_item", "cleanup_account_a"), ("account_b_item", "cleanup_account_b")):
        claim = value[name]
        require(type(claim) is dict and set(claim) == {"absent_confirmed", "write_claimed"}
            and all(type(flag) is bool for flag in claim.values()), "journal_claim")
        require(not claim["write_claimed"] or claim["absent_confirmed"], "journal_claim")
        if claim["write_claimed"]:
            require(name != "app_password" or profile is not None, "journal_claim")
            expected.add(fact)
    return expected


def executable_identity(path, digest):
    require(re.fullmatch(r"[a-f0-9]{64}", digest) is not None, "binary_hash")
    require(path.is_absolute() and not path.is_symlink() and path.is_file(), "binary_path")
    path = path.resolve(strict=True)
    require(path.is_relative_to((REPO / "target").resolve()), "binary_path")
    with path.open("rb") as binary:
        found = hashlib.file_digest(binary, "sha256").hexdigest()
    require(found == digest, "binary_hash")
    return path


class Acceptance:
    def __init__(self, binary, digest, runner, evidence):
        self.binary, self.digest, self.evidence = binary, digest, evidence
        self.nonce = secrets.token_hex(32)
        self.account_a, self.account_b = str(uuid.uuid4()), str(uuid.uuid4())
        self.values = {name: secrets.token_hex(32) for name in (
            "password_initial", "password_changed", "password_restart", "wrong_password", "sentinel_a", "sentinel_b")}
        require(len(set(self.values.values())) == 6, "random_collision")
        self.parent = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
        self.root = self.parent / ("morpheus-native-vault-" + self.nonce)
        self.runner = runner if runner is not None else (WindowsRunner(self.root) if sys.platform == "win32" else MacRunner())
        self.created = False
        self.root_verified = False
        self.phase_started = False
        self.owned_items_resolved = False
        self.platform_restored = False
        self.profile_removed = False

    def phase(self, phase, expected_cleanup=None):
        if sys.platform == "win32":
            self.windows_root.verify_private_root(self.root)
        private_object(self.root, directory=True)
        executable_identity(self.binary, self.digest)
        for suffix in ("-progress.json", "-result.json"):
            path = self.root / (phase + suffix)
            require(not path.exists() and not path.is_symlink(), "phase_replay")
        payload = {"version": 1, "phase": phase, "run_nonce": self.nonce, "root": str(self.root),
            "profile": str(self.root / "profile"), "journal": str(self.root / "owned-items.json"),
            "progress": str(self.root / (phase + "-progress.json")), "result": str(self.root / (phase + "-result.json")),
            "executable": str(self.binary), "account_a": self.account_a, "account_b": self.account_b, **self.values}
        raw = json.dumps(payload, separators=(",", ":")).encode()
        require(len(raw) <= 4096, "fixture_input_bound")
        environment = dict(os.environ)
        environment["MORPHEUS_NATIVE_VAULT_ISOLATED"] = self.nonce
        try:
            self.phase_started = True
            result = self.runner.run([str(self.binary), "--ignored", "--exact", "native_vault_" + phase,
                "--test-threads=1", "--nocapture"], environment=environment, payload=raw, seconds=300, output_limit=262144)
            require(result.status == 0, "fixture_exit")
            # Exact sealed facts + successful process exit are mandatory; libtest
            # text alone (including zero tests) cannot satisfy this acceptance.
            record = phase_record(self.root / (phase + "-result.json"), phase, final=True, expected_cleanup=expected_cleanup)
            self.evidence.phases[phase] = record
            return record
        except (Failure, OSError) as original:
            error = original if isinstance(original, Failure) else Failure("phase_receipt_io")
            try:
                record = phase_record(self.root / (phase + "-progress.json"), phase)
                self.evidence.phases[phase] = record
                self.evidence.fail(record["stage"], record["failure"] or error.code)
            except (Failure, OSError):
                self.evidence.fail(phase, error.code)
            if error.cleanup or not self.runner.quiescent:
                self.evidence.cleanup_failed("children_unconfirmed")
            raise error from None

    def run(self):
        windows = sys.platform == "win32"
        keychain = None if windows else MacKeychain(self.root, self.runner, self.evidence)
        previous_nonce = os.environ.get("MORPHEUS_NATIVE_VAULT_ISOLATED")
        windows_error = ()
        if windows:
            try:
                from scripts import native_vault_windows as windows_root
            except ModuleNotFoundError:
                import native_vault_windows as windows_root
            self.windows_root = windows_root
            windows_error = (windows_root.NativeVaultWindowsError,)
        try:
            forbidden = [REPO, REPO / "target"]
            for name in ("CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR"):
                if os.environ.get(name):
                    configured = Path(os.environ[name])
                    require(configured.is_absolute(), "cache_path")
                    forbidden.append(configured.resolve())
            require(all(not self.root.is_relative_to(path) for path in forbidden), "root_checkout")
            if windows:
                # Only the nonsecret nonce enters the parent environment. The
                # ACL creator must see an ABSENT root; no log or file precedes it.
                os.environ["MORPHEUS_NATIVE_VAULT_ISOLATED"] = self.nonce
                windows_root.prepare_private_root(self.root)
            else:
                make_mac_root(self.root, self.parent)
            self.created = self.root_verified = True
            if keychain is not None:
                keychain.setup()
            self.phase("prepare")
            self.phase("verify_remove")
            self.owned_items_resolved = True
        except (Failure, OSError, *windows_error) as error:
            if getattr(error, "created", False):
                self.created = True
            code = error.code if isinstance(error, Failure) else "windows_isolation" if isinstance(error, windows_error) else "private_io"
            self.evidence.fail("setup_or_fixture", code)
            if isinstance(error, Failure) and error.cleanup:
                self.evidence.cleanup_failed("children_unconfirmed")
            if self.root_verified and (windows or keychain.ready) and self.runner.quiescent and (self.root / "owned-items.json").exists():
                try:
                    expected = journal_cleanup_checks(self.root, self.nonce, self.account_a, self.account_b)
                    self.phase("cleanup", expected)
                    self.owned_items_resolved = True
                except (Failure, OSError, *windows_error):
                    self.evidence.cleanup_failed("owned_items_cleanup")
        finally:
            if windows:
                # No existing credential or Windows preference was changed.
                # Still require confirmed deletion of every claimed own target
                # before deleting the journal/profile that proves ownership.
                self.platform_restored = self.runner.quiescent and (not self.phase_started or self.owned_items_resolved)
                if not self.platform_restored:
                    self.evidence.cleanup_failed("owned_items_unconfirmed")
            else:
                self.platform_restored = keychain.restore()
            if self.created and self.runner.quiescent and self.platform_restored:
                try:
                    if not self.root_verified:
                        self.root.rmdir()  # Partial ACL/root setup authorizes empty removal only.
                    else:
                        if windows:
                            windows_root.verify_private_root(self.root)
                        private_object(self.root, directory=True)
                        shutil.rmtree(self.root)
                    self.profile_removed = not self.root.exists()
                    require(self.profile_removed, "root_cleanup")
                except (Failure, OSError, *windows_error):
                    self.evidence.cleanup_failed("root_cleanup")
            elif self.created:
                self.evidence.cleanup_failed("root_cleanup_unconfirmed")
            if windows:
                if previous_nonce is None:
                    os.environ.pop("MORPHEUS_NATIVE_VAULT_ISOLATED", None)
                else:
                    os.environ["MORPHEUS_NATIVE_VAULT_ISOLATED"] = previous_nonce
            self.values.clear()
        require(self.evidence.first is None and not self.evidence.cleanup_failures
            and self.profile_removed and self.platform_restored and self.runner.quiescent, "acceptance_failed")


def main(argv=None):
    class PrivateParser(argparse.ArgumentParser):
        def error(self, message):
            raise Failure("arguments")
    evidence = Evidence()
    acceptance = None
    provenance = None
    try:
        parser = PrivateParser(description=__doc__)
        parser.add_argument("--binary", required=True, type=Path)
        parser.add_argument("--binary-sha256", required=True)
        args = parser.parse_args(argv)
        require(os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
            and os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus"
            and os.environ.get("GITHUB_REF") == "refs/heads/main", "host_guard")
        require(sys.platform in ("darwin", "win32"), "platform_guard")
        require(re.fullmatch(r"[a-f0-9]{40}", os.environ.get("GITHUB_SHA", "")) is not None, "source_guard")
        require(os.environ.get("RUNNER_ARCH") in (("X64",) if sys.platform == "win32" else ("ARM64", "X64")), "architecture_guard")
        require(all(re.fullmatch(r"[1-9][0-9]{0,19}", os.environ.get(name, ""))
            for name in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT")), "run_identity")
        binary = executable_identity(args.binary, args.binary_sha256)
        source_inputs = {}
        for name in ("Cargo.lock", "rust-toolchain.toml"):
            with (REPO / name).open("rb") as source:
                source_inputs[name] = hashlib.file_digest(source, "sha256").hexdigest()
        provenance = {"source_commit": os.environ["GITHUB_SHA"], "binary_sha256": args.binary_sha256,
            "run_id": os.environ["GITHUB_RUN_ID"], "run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
            "platform": "windows" if sys.platform == "win32" else "macos", "architecture": os.environ["RUNNER_ARCH"], "source_inputs": source_inputs}
        acceptance = Acceptance(binary, args.binary_sha256, None, evidence)
        acceptance.run()
    except (Failure, OSError, KeyError) as error:
        evidence.fail("wrapper", error.code if isinstance(error, Failure) else "private_io")
    passed = evidence.first is None and not evidence.cleanup_failures
    print(json.dumps({"check": "native_vault_core", "status": "passed" if passed else "failed",
        "failure": evidence.first, "cleanup_failures": evidence.cleanup_failures, "phases": evidence.phases,
        "children_stopped": acceptance.runner.quiescent if acceptance else False,
        "platform_restored": acceptance.platform_restored if acceptance else False,
        "profile_removed": acceptance.profile_removed if acceptance else False,
        "provenance": provenance,
        "private_output_published": False}, sort_keys=True), flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
