#!/usr/bin/env python3
"""Private, explicit free-hosted Mac bundle transaction acceptance.

Two nonce-separated release packages exercise production signature/staging,
bundle/helper/trial/health/recovery code. No UI, account, vault or network client
is initialized. The synthetic old parent actually exits under owned observation;
this does not prove normal AppWindow/shipping shutdown, feed/download/TLS,
Developer ID, power loss or device behavior.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import secrets
import select
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import time
import tomllib

if __package__:
    from . import macos_package_ui as owned, package as packaging
    from . import package_smoke as smoke, prepare_macos_update as payload
    from .package_revision import expected_revision, verify_checkout, versioned_source_bytes, verify_versioned_export
else:
    import macos_package_ui as owned
    import package as packaging
    import package_smoke as smoke
    import prepare_macos_update as payload
    from package_revision import expected_revision, verify_checkout, versioned_source_bytes, verify_versioned_export

CASES = ("upgrade", "invalid_signature", "tampered_stage", "prepared_restart",
         "after_apply_recovery", "trial_failure_rollback")
MARKERS = {
    "upgrade": {"prepared", "helper_ready_lease_held", "synthetic_shutdown", "trial_committed_before_store",
                "committed_restart_before_store", "upgrade_and_committed_restart"},
    "invalid_signature": {"invalid_signature_rejected"},
    "tampered_stage": {"prepared", "helper_ready_lease_held", "synthetic_shutdown", "continue_tampered_stage",
                       "old_restart_before_store", "rollback_preserved_old"},
    "prepared_restart": {"prepared", "old_restart_before_store"},
    "after_apply_recovery": {"prepared", "helper_ready_lease_held", "synthetic_shutdown", "after_apply_before_trial",
                             "force_exit_after_apply", "owned_helper_interrupted", "old_restart_before_store", "recovery_relaunched_old",
                             "interrupted_recovery_preserved_old"},
    "trial_failure_rollback": {"prepared", "helper_ready_lease_held", "synthetic_shutdown", "trial_failed_before_health",
                               "old_restart_before_store", "rollback_preserved_old"},
}
HELPER_CASES = frozenset(("upgrade", "tampered_stage", "after_apply_recovery", "trial_failure_rollback"))
for _case in HELPER_CASES:
    MARKERS[_case] |= {"helper_ticket_ready", "helper_observer_armed", "old_actor_exit_observed",
                      "helper_parent_exit_observed"}
STAGES = frozenset(("preflight", "export", "build_old", "build_new", "signing", "log_scan", "cleanup", "complete", *CASES))
SCANNER_SHA256 = {
    "arm64": "ba52fb1bfabbcde42f032afad3d6e0b19dff8ed105229a16e7caa338bbc0e84f",
    "x86_64": "cee01fea7173f1b779dff188e1c26ecbcb4027d394acc573b23aaf0be260e291",
}
MAX_LOG = 64 * 1024 * 1024
MAX_ARCHIVE = 64 * 1024 * 1024
MARKER_BYTES = b'{"passed":true}\n'
APP = Path("Contents/MacOS/morpheus-app")
STATE = ".Morpheus.app.morpheus-update"
ROOT = Path(__file__).resolve().parents[1]
# Darwin SDK sys/event.h constants not exported by CPython's select module.
# The native observer still requires receipt + exact returned kernel payload.
DARWIN_EV_RECEIPT = 0x0040
DARWIN_NOTE_EXITSTATUS = 0x04000000
FORCED_AFTER_APPLY_EXIT = 86


class Refused(RuntimeError):
    """Fixed public diagnostics only; raw paths, logs and keys stay private."""


def require(condition):
    if not condition:
        raise Refused("mac_update_acceptance")


def digest(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def bounded(path, limit):
    path = Path(path)
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid()
            and before.st_nlink == 1 and before.st_size <= limit and not before.st_mode & 0o7022)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        require((os.fstat(source.fileno()).st_dev, os.fstat(source.fileno()).st_ino)
                == (before.st_dev, before.st_ino))
        data = source.read(limit + 1)
        require(len(data) <= limit)
        return data


def private_new(path):
    path = Path(path)
    require(path.is_absolute() and path.parent.resolve(strict=True) == path.parent)
    path.mkdir(mode=0o700)
    path.chmod(0o700)
    return path


def private_write(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())


def hosted_guard(explicit, environment=None, event=None):
    environment = os.environ if environment is None else environment
    require(explicit and sys.platform == "darwin" and platform.machine() in {"arm64", "x86_64"}
            and os.getuid() == os.geteuid() and os.geteuid() != 0)
    for name, value in {
        "GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "macOS",
        "ImageOS": "macos15", "GITHUB_REPOSITORY": "brk275/Morpheus", "GITHUB_EVENT_NAME": "workflow_dispatch",
        "GITHUB_ACTOR": "brk275", "GITHUB_TRIGGERING_ACTOR": "brk275",
    }.items():
        require(environment.get(name) == value)
    event = json.loads(bounded(environment["GITHUB_EVENT_PATH"], 1024 * 1024)) if event is None else event
    repository, sender = event.get("repository", {}), event.get("sender", {})
    owner = repository.get("owner", {})
    require(repository.get("private") is False and repository.get("full_name") == "brk275/Morpheus"
            and owner.get("login") == sender.get("login") == "brk275"
            and type(owner.get("id")) is int and owner["id"] > 0 and sender.get("id") == owner["id"]
            and environment.get("GITHUB_ACTOR_ID") == str(owner["id"]))
    forbidden = {"CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD",
                 "MORPHEUS_UPDATE_FEED", "MORPHEUS_UPDATE_PUBLIC_KEY", "MORPHEUS_TIMELINE_ACCEPTANCE_BUILD",
                 "MORPHEUS_MAC_UPDATE_ACCEPTANCE_BUILD", "MORPHEUS_MAC_UPDATE_ACCEPTANCE_PLAN",
                 "APPLE_SIGNING_IDENTITY", "APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD",
                 "APPLE_API_KEY", "APPLE_API_ISSUER", "APPLE_ID", "APPLE_PASSWORD"}
    require(not forbidden.intersection(environment))


def execution_environment(environment):
    # Keep public runner/toolchain context; never forward broad secrets or
    # developer loader settings. Actors still cannot enter UI/profile code.
    allowed = {"PATH", "HOME", "USER", "LOGNAME", "SHELL", "CARGO_HOME", "RUSTUP_HOME", "DEVELOPER_DIR",
               "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "RUNNER_TEMP", "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT",
               "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "GITHUB_REPOSITORY", "GITHUB_EVENT_NAME",
               "GITHUB_EVENT_PATH", "GITHUB_SHA", "GITHUB_ACTOR", "GITHUB_ACTOR_ID", "GITHUB_TRIGGERING_ACTOR"}
    return {name: value for name, value in environment.items() if name in allowed}


class Commands:
    def __init__(self, root, environment):
        self.root, self.environment = root, environment
        self.logs = private_new(root / "logs")
        self.sequence = 0
        self.pending = []
        self.helper_observers = []
        self.cleanup_failed = False

    def start(self, argv, *, cwd=None, extra=None):
        self.sequence += 1
        path = self.logs / f"{self.sequence:03d}.log"
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as output:
            process = subprocess.Popen(argv, cwd=cwd, env=self.environment | (extra or {}),
                                       stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=True)
        self.pending.append(process)
        return process, path

    def finish(self, process, path, *, seconds, allow_stop=False):
        failure, terminal = None, False
        try:
            deadline = time.monotonic() + seconds
            # Do not reap until owned-group cleanup proves empty. A poll or
            # log-bound failure is just as terminal for a build/signer command
            # as a deadline; every such failure must stop its retained group.
            while True:
                status = smoke.process_status(process)
                if status is not None:
                    terminal = True
                    break
                require(path.lstat().st_size <= MAX_LOG and time.monotonic() < deadline)
                time.sleep(0.05)
            require(path.lstat().st_size <= MAX_LOG and status == 0)
        except BaseException as error:
            failure = error
        finally:
            # Nonactor tools may always be stopped through their still-owned
            # group. A live synthetic actor may own helpers in other groups:
            # preserve its monitor on failure, never substitute orphan PID kills.
            if allow_stop or terminal:
                try:
                    owned.stop_group(process)
                    self.pending.remove(process)
                except BaseException as error:
                    self.cleanup_failed = True
                    if failure is None:
                        failure = error
        if failure is not None:
            raise failure

    def run(self, argv, *, cwd=None, extra=None, seconds=180):
        process, path = self.start(argv, cwd=cwd, extra=extra)
        self.finish(process, path, seconds=seconds, allow_stop=True)
        return path

    def finish_actor(self, process, path, *, seconds=180, expected=0):
        # These fixed synthetic actors never create same-group descendants.
        # Observe/reap only this actual Popen; helper exit has its separate
        # retained kernel registration. This is not a group-drain assertion.
        failure, terminal = None, False
        try:
            deadline = time.monotonic() + seconds
            while True:
                status = smoke.process_status(process)
                if status is not None:
                    terminal = True
                    break
                require(path.lstat().st_size <= MAX_LOG and time.monotonic() < deadline)
                time.sleep(0.05)
            require(path.lstat().st_size <= MAX_LOG)
        except BaseException as error:
            failure = error
        finally:
            if terminal:
                try:
                    status = process.wait(timeout=5)
                    self.pending.remove(process)
                    require(status == expected)
                except BaseException as error:
                    if process in self.pending:
                        self.cleanup_failed = True
                    if failure is None:
                        failure = error
        if failure is not None:
            raise failure


def closed_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result


def helper_ticket(root, nonce, parent, old, new):
    require(type(parent) is int and parent > 1 and type(nonce) is str
            and re.fullmatch(r"[a-f0-9]{32}", nonce) is not None)
    path = root / "helper-observer.json"
    require(path.lstat().st_mode & 0o077 == 0)
    ticket = json.loads(bounded(path, 4096), object_pairs_hook=closed_object)
    require(type(ticket) is dict and set(ticket) == {"schema", "nonce", "transaction", "parent", "helper"}
            and type(ticket["schema"]) is int and ticket["schema"] == 1 and ticket["nonce"] == nonce
            and type(ticket["parent"]) is int and ticket["parent"] == parent
            and type(ticket["helper"]) is int and 1 < ticket["helper"] < 2**31
            and ticket["helper"] != parent and type(ticket["transaction"]) is str
            and re.fullmatch(r"[A-Za-z0-9]{1,64}", ticket["transaction"]) is not None)
    journal = json.loads(bounded(root / STATE / "journal.json", 16 * 1024), object_pairs_hook=closed_object)
    require(type(journal) is dict and set(journal) == {"protocol", "id", "phase", "old_hash", "new_hash", "candidate"}
            and type(journal.get("protocol")) is int and journal.get("protocol") == 1 and journal.get("phase") == "Prepared"
            and journal.get("id") == ticket["transaction"] and journal.get("old_hash") == old["bundle_sha256"]
            and journal.get("new_hash") == new["bundle_sha256"])
    candidate = journal["candidate"]
    require(type(candidate) is dict and set(candidate) == {"protocol", "product", "version", "arch", "persistence"}
            and type(candidate["protocol"]) is int and candidate["protocol"] == 2
            and candidate["product"] == "io.github.brk275.morpheus" and candidate["version"] == new["version"]
            and candidate["arch"] == new["arch"] and candidate["persistence"] == new["persistence"])
    return ticket


class HelperExit:
    """Owned kernel observation, armed while the real helper Child is anchored."""
    def __init__(self, commands, parent, root, nonce, old, new):
        require(sys.platform == "darwin" and hasattr(select, "kqueue") and hasattr(select, "kevent")
                and parent in commands.pending and smoke.process_status(parent) is None)
        ticket = helper_ticket(root, nonce, parent.pid, old, new)
        self.commands, self.pid = commands, ticket["helper"]
        self.cookie = int(nonce[:15], 16) + 1
        self.queue = select.kqueue()
        self.terminal = False
        try:
            os.set_inheritable(self.queue.fileno(), False)
            require(not os.get_inheritable(self.queue.fileno()))
            change = select.kevent(self.pid, filter=select.KQ_FILTER_PROC,
                                   flags=select.KQ_EV_ADD | select.KQ_EV_ENABLE | select.KQ_EV_ONESHOT | DARWIN_EV_RECEIPT,
                                   fflags=select.KQ_NOTE_EXIT | DARWIN_NOTE_EXITSTATUS, udata=self.cookie)
            receipt = self.queue.control([change], 1, 0)
            require(len(receipt) == 1 and self.matches(receipt[0], receipt=True)
                    and smoke.process_status(parent) is None and not self.queue.control([], 1, 0))
        except BaseException:
            self.queue.close()
            raise
        commands.helper_observers.append(self)

    def matches(self, event, *, receipt=False):
        identity = (event.ident == self.pid and event.filter == select.KQ_FILTER_PROC and event.udata == self.cookie
                    and event.fflags == (select.KQ_NOTE_EXIT | DARWIN_NOTE_EXITSTATUS))
        if receipt:
            return identity and event.flags & select.KQ_EV_ERROR != 0 and event.data == 0
        return (identity and event.flags & select.KQ_EV_ERROR == 0
                and event.flags & (select.KQ_EV_EOF | select.KQ_EV_ONESHOT) == (select.KQ_EV_EOF | select.KQ_EV_ONESHOT)
                and type(event.data) is int and 0 <= event.data <= 0xffff)

    def record_terminal(self, events):
        require(len(events) == 1 and self.matches(events[0]))
        status = os.waitstatus_to_exitcode(events[0].data)
        self.terminal = True
        try:
            self.queue.close()
        except BaseException:
            self.commands.cleanup_failed = True
            raise
        self.commands.helper_observers.remove(self)
        return status

    def finish(self, *, expected, seconds=420):
        try:
            deadline = time.monotonic() + seconds
            while True:
                remaining = deadline - time.monotonic()
                require(remaining > 0)
                events = self.queue.control([], 1, min(remaining, 1))
                if not events:
                    continue
                status = self.record_terminal(events)
                require(status == expected)
                return
        except BaseException:
            if not self.terminal:
                # Preserve the observation handle and private fixture. No
                # orphan PID signal or invented successful cleanup is allowed.
                self.commands.cleanup_failed = True
            raise


def export_source(commands, source, revision):
    verify_checkout(source, revision)
    archive = commands.root / "source.tar"
    commands.run(["git", "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "archive",
                  "--format=tar", "--output=" + str(archive), revision], cwd=source)
    data = bounded(archive, MAX_ARCHIVE)
    # Authenticate the complete exported source before any build. The existing
    # validator rejects symlinks, foreign paths and arbitrary version exclusions.
    exports = []
    for index, variant in enumerate(app_versions(source), 1):
        destination = private_new(commands.root / f"source-{index}")
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as tar:
            names, expanded = set(), 0
            for member in tar:
                pure = Path(member.name)
                require(not pure.is_absolute() and ".." not in pure.parts and "\\" not in member.name
                        and str(pure) == member.name.rstrip("/") and (member.isdir() or member.isfile())
                        and member.name not in names and len(names) < 5000)
                names.add(member.name)
                path = destination / pure
                if member.isdir():
                    path.mkdir(mode=0o755, parents=True, exist_ok=True)
                else:
                    expanded += member.size
                    require(member.size <= 8 * 1024 * 1024 and expanded <= MAX_ARCHIVE)
                    path.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
                    blob = tar.extractfile(member).read(member.size + 1)
                    require(len(blob) == member.size)
                    private_write(path, versioned_source_bytes(member.name, blob, variant))
                    path.chmod(0o755 if member.mode & 0o111 else 0o644)
        verify_versioned_export(destination, data, variant)
        exports.append(destination)
    archive.unlink()
    return exports


def app_versions(source):
    version = tomllib.loads(bounded(source / "crates/morpheus-app/Cargo.toml", 1024 * 1024).decode())["package"]["version"]
    match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version)
    require(match is not None)
    return match[1] + "-updateqa.1", match[1] + "-updateqa.2"


def build_bundle(commands, source, target, version, nonce, index):
    artifact = private_new(commands.root / f"package-{index}")
    config = packaging.package_config(source, platform="darwin")
    require(config["before-packaging-command"]
            == "cargo build --release --locked -p morpheus-app --features native-calls")
    require(config["macos"] == {"info-plist-path": "packaging/macos/Info.plist",
                               "entitlements": "packaging/macos/entitlements.plist"})
    config.update({"version": version, "out-dir": str(artifact), "binaries-dir": str(target / "release")})
    formats = packaging.macos_update_formats(config, "app")
    # Exactly one existing before-packaging build for this version. DMG/UI QA
    # remains unchanged and separate; this route retains its actual .app bytes.
    commands.run(["cargo", "packager", "--release", "--config", json.dumps(config), "--formats", formats],
                 cwd=source, extra={"CARGO_TARGET_DIR": str(target), "MORPHEUS_MAC_UPDATE_ACCEPTANCE_BUILD": nonce},
                 seconds=3300)
    arch = {"arm64": "aarch64", "x86_64": "x86_64"}[platform.machine()]
    receipt = payload.prepare_update(artifact / "Morpheus.app", artifact / "Morpheus.app.tar.gz",
                                     version=version, arch=arch, root=source)
    metadata = payload.binary_identity(payload._read(artifact / "Morpheus.app" / APP, payload.MAX_ARCHIVE_BYTES), version, arch)
    require(receipt["bundle_sha256"] == payload._snapshot(artifact / "Morpheus.app")["tree_sha256"]
            and receipt["persistence"] == metadata["persistence"])
    return artifact, receipt, metadata


def sign_payload(commands, archive):
    # Official pinned Packager signer; no private key is read by this driver,
    # passed on argv, included in a report or used outside the private root.
    keys = private_new(commands.root / "keys")
    key = keys / "disposable"
    commands.run(["cargo", "packager", "signer", "generate", "--ci", "--path", str(key)],
                 extra={"CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD": ""})
    for path in (key, key.with_name(key.name + ".pub")):
        require(path.is_file() and not path.is_symlink())
        path.chmod(0o600)
    commands.run(["cargo", "packager", "signer", "sign", str(archive)],
                 extra={"CARGO_PACKAGER_SIGN_PRIVATE_KEY": str(key), "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD": ""})
    public = bounded(key.with_name(key.name + ".pub"), 4096).decode("ascii").strip()
    signature_path = archive.with_name(archive.name + ".sig")
    signature_path.chmod(0o600)
    signature = bounded(signature_path, 4096).decode("ascii").strip()
    require(all(re.fullmatch(r"[A-Za-z0-9+/=]+", value) for value in (public, signature)))
    return public, signature


def copy_bundle(source, destination, expected):
    snapshot = payload._snapshot(source)
    require(snapshot["tree_sha256"] == expected and not destination.exists() and not destination.is_symlink())
    destination.mkdir(mode=0o700)
    # Preserve exact validated file/directory/link modes, including Darwin's
    # no-follow link mode. Never apply chmod to a symlink target.
    for record in sorted(snapshot["records"], key=lambda item: (len(Path(item["path"]).parts), item["path"])):
        path, original = destination / record["path"], source / record["path"]
        if record["kind"] == "directory":
            path.mkdir(mode=0o700)
        elif record["kind"] == "file":
            with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as output:
                output.write(payload._read(original, payload.MAX_ARCHIVE_BYTES))
                os.fchmod(output.fileno(), record["mode"])
        else:
            os.symlink(record["target"], path)
            os.chmod(path, record["mode"], follow_symlinks=False)
    for record in reversed(snapshot["records"]):
        if record["kind"] == "directory":
            (destination / record["path"]).chmod(record["mode"])
    destination.chmod(snapshot["mode"])
    require(payload._snapshot(destination)["tree_sha256"] == expected and payload._snapshot(source) == snapshot)


def event_seen(root, token):
    try:
        return bounded(root / "events" / token, 64) == MARKER_BYTES
    except FileNotFoundError:
        return False


def wait_event(root, token, process, log, seconds=160):
    deadline = time.monotonic() + seconds
    while not event_seen(root, token):
        require(smoke.process_status(process) is None and log.lstat().st_size <= MAX_LOG
                and time.monotonic() < deadline)
        time.sleep(0.05)


def tamper_staged_resource(root):
    path = root / STATE / "next.app/Contents/Resources/aurora.toml"
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid() and before.st_nlink == 1
            and before.st_size < 16 * 1024 * 1024 and not before.st_mode & 0o7022)
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
    with os.fdopen(fd, "wb") as output:
        require((os.fstat(output.fileno()).st_dev, os.fstat(output.fileno()).st_ino) == (before.st_dev, before.st_ino))
        output.write(b"\n# synthetic staged-byte tamper\n")
        output.flush()
        os.fsync(output.fileno())
    private_write(root / "events/continue_tampered_stage", MARKER_BYTES)


def run_case(commands, case, old, old_receipt, new_receipt, public, signature, nonce):
    require(case in CASES)
    root = private_new(commands.root / case)
    private_new(root / "events")
    installed = root / "Morpheus.app"
    copy_bundle(old / "Morpheus.app", installed, old_receipt["bundle_sha256"])
    plan = {"schema": 1, "nonce": nonce, "case": case, "old_version": old_receipt["version"],
            "new_version": new_receipt["version"], "old_tree_sha256": old_receipt["bundle_sha256"],
            "new_tree_sha256": new_receipt["bundle_sha256"], "archive_sha256": new_receipt["sha256"],
            "public_key": public, "signature": signature}
    private_write(root / "run.json", json.dumps(plan, sort_keys=True).encode("ascii"))
    actor_env = {"MORPHEUS_MAC_UPDATE_ACCEPTANCE_PLAN": str(root / "run.json")}
    process, log = commands.start([str(installed / APP), "--morpheus-mac-qa-prepare"], extra=actor_env)
    observer = None
    if case in {"tampered_stage", "prepared_restart"}:
        wait_event(root, "prepared", process, log)
        require(not event_seen(root, "helper_ready_lease_held"))
        if case == "tampered_stage":
            tamper_staged_resource(root)
        else:
            # This fixed checkpoint has no helper/descendant yet. Signal only
            # this still-owned unreaped direct child, never an orphan PID/group.
            require(process in commands.pending and smoke.process_status(process) is None)
            os.kill(process.pid, signal.SIGKILL)
            commands.finish_actor(process, log, expected=-signal.SIGKILL)
            process, log = commands.start([str(installed / APP)], extra=actor_env)
    if case in HELPER_CASES:
        wait_event(root, "helper_ticket_ready", process, log)
        observer = HelperExit(commands, process, root, nonce, old_receipt, new_receipt)
        private_write(root / "events/helper_observer_armed", MARKER_BYTES)
    commands.finish_actor(process, log, seconds=420)
    if observer is not None:
        # This is an actual owned synthetic parent exit, not normal UI cleanup.
        private_write(root / "events/old_actor_exit_observed", MARKER_BYTES)
        if case == "after_apply_recovery":
            wait_helper_event(root, "after_apply_before_trial", observer)
            private_write(root / "events/force_exit_after_apply", MARKER_BYTES)
            observer.finish(expected=FORCED_AFTER_APPLY_EXIT)
            private_write(root / "events/owned_helper_interrupted", MARKER_BYTES)
        else:
            observer.finish(expected=0 if case == "upgrade" else 1)
        require(event_seen(root, "helper_parent_exit_observed"))
        if case == "upgrade":
            journal = json.loads(bounded(root / STATE / "journal.json", 16 * 1024), object_pairs_hook=closed_object)
            require(journal["phase"] == "Committed" and journal["new_hash"] == new_receipt["bundle_sha256"]
                    and payload._snapshot(root / STATE / "previous.app")["tree_sha256"] == old_receipt["bundle_sha256"])
        if case in {"upgrade", "tampered_stage", "after_apply_recovery"}:
            restarted, restart_log = commands.start([str(installed / APP)], extra=actor_env)
            commands.finish_actor(restarted, restart_log, seconds=420)
    expected = new_receipt if case == "upgrade" else old_receipt
    require(payload._snapshot(installed)["tree_sha256"] == expected["bundle_sha256"]
            and not (root / STATE / "journal.json").exists()
            and not (root / STATE / "previous.app").exists()
            and not (root / STATE / "next.app").exists())
    if case == "upgrade":
        private_write(root / "events/upgrade_and_committed_restart", MARKER_BYTES)
    elif case in {"tampered_stage", "trial_failure_rollback"}:
        private_write(root / "events/rollback_preserved_old", MARKER_BYTES)
    elif case == "after_apply_recovery":
        private_write(root / "events/interrupted_recovery_preserved_old", MARKER_BYTES)
    entries = {path.name for path in (root / "events").iterdir()}
    require(entries == MARKERS[case] and all(event_seen(root, token) for token in entries))
    return True


def wait_helper_event(root, token, observer):
    deadline = time.monotonic() + 180
    while not event_seen(root, token):
        require(time.monotonic() < deadline)
        # The helper has a retained immutable registration. An early terminal
        # event or error is a failure, never permission to signal its old PID.
        early = observer.queue.control([], 1, 0)
        if early:
            observer.record_terminal(early)
            raise Refused("mac_update_acceptance")
        time.sleep(0.05)


def scan_logs(commands, scanner):
    require(digest(scanner) == SCANNER_SHA256[platform.machine()])
    # External default rules and an empty ignore file: no repository scanner
    # suppression or allow comment may admit private command/app output.
    config, ignore = commands.root / "default-gitleaks.toml", commands.root / "empty-gitleaksignore"
    if not config.exists():
        private_write(config, b"[extend]\nuseDefault = true\n")
        private_write(ignore, b"")
    passed = True
    for log in sorted(commands.logs.iterdir()):
        try:
            data = bounded(log, MAX_LOG)
            result = subprocess.run([str(scanner), "stdin", "--no-banner", "--config", str(config),
                                     "--gitleaks-ignore-path", str(ignore), "--ignore-gitleaks-allow", "--redact=100"],
                                    input=data, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                    cwd=commands.root, timeout=30, check=False)
            passed = result.returncode == 0 and passed
        except Exception:
            # One over-bound/unreadable log must not prevent scan attempts for
            # the remaining bounded logs. No prefix scan is called a full scan.
            passed = False
    require(passed)


def scan_final_logs(commands, scanner, report):
    report["checks"]["bounded_log_scan_attempted"] = True
    try:
        scan_logs(commands, scanner)
        final = not commands.pending and not commands.helper_observers and not commands.cleanup_failed
    except Exception:
        final = False
    # With live/unproved ownership these are snapshots, never final log or
    # private-cleanup acceptance. The fixture is retained on every such failure.
    report["checks"]["default_log_scan"] = final
    return final


def processes_in_fixture(root):
    # Observation only. Never signal a PID discovered in this snapshot.
    result = []
    for pid, _, state in owned.process_table():
        if state.startswith("Z"):
            continue
        executable = owned.executable_of(pid)
        if executable is not None and executable.is_relative_to(root):
            result.append(pid)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execute-on-free-hosted-runner", action="store_true")
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--gitleaks", type=Path, required=True)
    args = parser.parse_args()
    report = {"schema": 1, "check": "installed_macos_transaction_synthetic", "status": "failed", "stage": "preflight",
              "checks": {}, "normal_ui_shutdown": False, "actual_old_parent_exit": False,
              "production_group_cleanup": False, "power_loss": False, "installation_enabled": False}
    commands, root, completed = None, None, False
    try:
        hosted_guard(args.execute_on_free_hosted_runner)
        smoke.require_nonreaping_wait()
        source = args.source.resolve(strict=True)
        require(source == ROOT and source.is_dir())
        revision = expected_revision(os.environ)
        verify_checkout(source, revision)
        scanner = args.gitleaks.resolve(strict=True)
        require(scanner.is_file() and os.access(scanner, os.X_OK) and digest(scanner) == SCANNER_SHA256[platform.machine()])
        temporary = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
        require(temporary.is_dir() and not temporary.is_relative_to(source))
        target = args.target_dir.resolve(strict=True)
        require(target.is_dir() and not target.is_relative_to(source) and target.lstat().st_uid == os.geteuid())
        nonce = secrets.token_hex(16)
        root = private_new(temporary / ("morpheus-mac-update-qa-" + nonce))
        private_new(root / "tmp")
        private_new(root / "payload")
        environment = execution_environment(os.environ) | {"TMPDIR": str(root / "tmp"), "RUST_LOG": "warn", "RUST_BACKTRACE": "0"}
        commands = Commands(root, environment)
        commands.scanner = scanner
        version_log = commands.run(["cargo", "packager", "--version"])
        scan_logs(commands, scanner)
        require(bounded(version_log, 4096).strip() == b"cargo-packager 0.11.8")
        report["stage"] = "export"
        exports = export_source(commands, source, revision)
        versions = app_versions(source)
        built = []
        for index, (export, version) in enumerate(zip(exports, versions), 1):
            report["stage"] = "build_old" if index == 1 else "build_new"
            built.append(build_bundle(commands, export, target, version, nonce, index))
        old, old_receipt, old_identity = built[0]
        new, new_receipt, new_identity = built[1]
        require(old_identity["persistence"] == new_identity["persistence"]
                and old_receipt["bundle_sha256"] != new_receipt["bundle_sha256"])
        report["checks"]["same_instrumented_cohort"] = True
        archive = root / "payload/Morpheus.app.tar.gz"
        shutil.copyfile(new / "Morpheus.app.tar.gz", archive)
        archive.chmod(0o600)
        require(digest(archive) == new_receipt["sha256"])
        report["stage"] = "signing"
        public, signature = sign_payload(commands, archive)
        report["checks"]["official_disposable_signer"] = True
        for case in CASES:
            report["stage"] = case
            report["checks"][case] = run_case(commands, case, old, old_receipt, new_receipt, public, signature, nonce)
        report["checks"]["synthetic_parent_kernel_exit"] = True
        report["checks"]["owned_helper_kernel_exit"] = True
        report["checks"]["no_cleanup_after_apply_interruption"] = True
        # Fixed immutable public identities only; never plan paths or signatures.
        report["source_revision"] = revision
        report["artifacts"] = [{key: receipt[key] for key in ("version", "arch", "persistence", "sha256", "bundle_sha256")}
                               for receipt in (old_receipt, new_receipt)]
        completed = True
    except Exception:
        # Details and exceptions can contain user/toolchain paths. The private
        # scanned logs remain diagnostic evidence; no arbitrary exception text.
        pass
    finally:
        if commands is not None:
            try:
                report["stage"] = "log_scan" if completed else report["stage"]
                final_logs = scan_final_logs(commands, commands.scanner, report)
                # Only successful, completely reaped scenarios may delete the
                # root. On any failure preserve private data and do not signal
                # discovered/orphan PIDs or race a still-running monitor.
                if completed and final_logs and not commands.pending and not commands.helper_observers and not commands.cleanup_failed and not processes_in_fixture(root):
                    report["stage"] = "cleanup"
                    require(root.parent == Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
                            and root.name.startswith("morpheus-mac-update-qa-") and not root.is_symlink()
                            and root.lstat().st_uid == os.geteuid() and root.lstat().st_mode & 0o077 == 0)
                    shutil.rmtree(root)
                    report["checks"]["private_cleanup"] = True
                    report["stage"], report["status"] = "complete", "passed"
            except Exception:
                report["status"] = "failed"
    print("MORPHEUS_MAC_UPDATE_QA " + json.dumps(report, sort_keys=True))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
