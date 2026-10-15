"""One fresh Python/AT-SPI client per private installed capture session.

libatspi caches its bus for the process lifetime. Each worker owns exactly one
run_case; switching environment variables cannot safely recycle that client.
The existing outer PID namespace contains detached children on hard failure.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import appimage_client as client
import appimage_diagnostics as D
from appimage_process import CommandFailure, OwnedRunner
import element_process_namespace as containment

MODES = {"ordinary": False, "eager_bindings": True}
BOUNDS = {"artifact": 512 * 1024 * 1024, "peer": 512 * 1024 * 1024,
          "backend": 64 * 1024 * 1024, "fixture": 65536,
          "executable": 512 * 1024 * 1024, "library": 64 * 1024 * 1024}
HASH = re.compile(r"[0-9a-f]{64}")


class CaptureFailure(RuntimeError):
    def __init__(self):
        super().__init__("Installed AppImage capture worker failed")


def require(value):
    if not value:
        raise CaptureFailure()


def parent_command():
    with open("/proc/1/cmdline", "rb") as source:
        return source.read(32769)


def assert_worker_contained(owner_root):
    # The outer runner remains the validated unprivileged PID1. This fresh
    # direct child needs an explicit descendant check; relaxing PID1's global
    # assert_contained would silently weaken every other fixture runner.
    control = containment.read_control(Path(os.environ.get("MORPHEUS_ELEMENT_NAMESPACE_CONTROL", "")))
    args = control.get("args")
    require(type(args) is list and len(args) == 6 and all(type(arg) is str for arg in args)
            and args[:5] == [str(Path(sys.executable).resolve()), str(Path(__file__).with_name("package_appimage.py").resolve()),
                            "--inside", str(owner_root), "--cargo"]
            and Path(args[5]).is_absolute() and Path(args[5]).name == "cargo"
            and all(0 < len(arg) <= 4096 and "\0" not in arg for arg in args))
    require(parent_command() == b"\0".join(arg.encode() for arg in args) + b"\0")
    uid, gid, groups = control.get("uid"), control.get("gid"), control.get("groups")
    require(sys.platform == "linux" and type(uid) is int and uid > 0 and type(gid) is int and gid > 0
            and type(groups) is list and all(type(group) is int and group > 0 for group in groups)
            and groups == sorted(set(groups)) and os.getresuid() == (uid, uid, uid)
            and os.getresgid() == (gid, gid, gid) and sorted(set(os.getgroups())) == groups
            and os.getpid() > 1 and os.getppid() == 1)
    namespace = os.readlink("/proc/self/ns/pid")
    require(re.fullmatch(r"pid:\[\d+\]", namespace) and namespace != control.get("outer_namespace")
            and namespace == os.readlink("/proc/1/ns/pid"))
    for path, expected_pid in (("/proc/self/status", os.getpid()), ("/proc/1/status", 1)):
        fields = dict(line.split(":", 1) for line in Path(path).read_text().splitlines() if ":" in line)
        require(fields.get("NSpid", "").split() == [str(expected_pid)]
                and fields.get("Uid", "").split() == [str(uid)] * 4
                and fields.get("Gid", "").split() == [str(gid)] * 4
                and sorted(map(int, fields.get("Groups", "").split())) == groups
                and fields.get("NoNewPrivs", "").strip() == "1"
                and all(int(fields.get(key, "1").strip(), 16) == 0 for key in containment.CAPABILITIES))
    return namespace


class WorkerRunner(OwnedRunner):
    def __init__(self, log_root, owner_root, progress=None):
        self.owner_root = owner_root
        super().__init__(log_root, progress=progress)

    def assert_containment(self):
        assert_worker_contained(self.owner_root)


def canonical_path(value):
    require(type(value) is str and 0 < len(value) <= 4096 and not any(c in value for c in "\r\n\0"))
    path = Path(value)
    require(path.is_absolute() and path.resolve(strict=True) == path)
    return path


def private_json(path, maximum):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
            and info.st_mode & 0o077 == 0 and info.st_nlink == 1)
    return json.loads(client.read_regular(path, maximum))


def file_record(path, maximum):
    path = canonical_path(str(path))
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and not info.st_mode & 0o022)
    return {"path": str(path), "sha256": hashlib.sha256(client.read_regular(path, maximum)).hexdigest()}


def validate_input(path):
    path = canonical_path(str(path))
    require(path.name == "case-input.json")
    case_root = client.private_directory(path.parent)
    owner_root = client.private_directory(case_root.parent)
    value = private_json(path, 32768)
    require(type(value) is dict and set(value) == {"schema", "mode", "appdir", "files"}
            and type(value["schema"]) is int and value["schema"] == 1
            and type(value["mode"]) is str and value["mode"] in MODES)
    require(case_root.name == ("bindings" if MODES[value["mode"]] else "ordinary"))
    appdir = canonical_path(value["appdir"])
    require(appdir.is_dir() and appdir.is_relative_to(owner_root)
            and type(value["files"]) is dict and set(value["files"]) == set(BOUNDS))
    paths = {}
    for name, maximum in BOUNDS.items():
        item = value["files"][name]
        require(type(item) is dict and set(item) == {"path", "sha256"}
                and type(item["sha256"]) is str and HASH.fullmatch(item["sha256"]))
        candidate = canonical_path(item["path"])
        require(candidate.is_relative_to(ROOT / "target" if name == "peer" else owner_root))
        require(file_record(candidate, maximum) == item)
        if name == "fixture":
            require(not candidate.stat().st_mode & 0o077)
        if name in {"artifact", "peer", "backend", "executable"}:
            require(candidate.stat().st_mode & 0o111)
        paths[name] = candidate
    require(paths["executable"] == appdir / "usr/bin/morpheus-app"
            and paths["library"] == appdir / "usr/lib/libpipewire-0.3.so.0")
    return case_root, appdir, paths, value


def validate_result(value, expected):
    from appimage_capture import FACTS
    fields = {"mode", "actual_gui_capture", "peer", "untouched_static_reopen_verified",
              "executable_sha256", "pipewire_sha256"}
    bindings = MODES[expected["mode"]]
    if bindings:
        fields.add("binding_evidence")
    require(type(value) is dict and set(value) == fields and value["mode"] == expected["mode"]
            and value["actual_gui_capture"] is True and value["untouched_static_reopen_verified"] is False
            and type(value["peer"]) is dict and set(value["peer"]) == set(FACTS)
            and all(item is True for item in value["peer"].values())
            and value["executable_sha256"] == expected["files"]["executable"]["sha256"]
            and value["pipewire_sha256"] == expected["files"]["library"]["sha256"])
    if bindings:
        proof = value["binding_evidence"]
        require(type(proof) is dict and set(proof) == {"app_pipewire_imports", "app_pipewire_bindings_verified",
                "unexpected_providers", "executable_sha256", "pipewire_sha256", "actual_imports", "collision_symbols_not_imported"})
        for key in ("actual_imports", "collision_symbols_not_imported"):
            names = proof[key]
            require(type(names) is list and len(names) <= 256
                    and all(type(name) is str and re.fullmatch(r"pw_[a-z0-9_]{1,120}", name) for name in names)
                    and names == sorted(set(names)))
        require(0 < len(proof["actual_imports"]) <= 256
                and not set(proof["actual_imports"]).intersection(proof["collision_symbols_not_imported"]))
        for key in ("app_pipewire_imports", "app_pipewire_bindings_verified"):
            require(type(proof[key]) is int and proof[key] == len(proof["actual_imports"]))
        require(type(proof["unexpected_providers"]) is int and proof["unexpected_providers"] == 0
                and proof["executable_sha256"] == value["executable_sha256"]
                and proof["pipewire_sha256"] == value["pipewire_sha256"])
    return value


def adopt_progress(progress, case_root, mode):
    """Copy only the fixed validated schema after the worker is fully stopped."""
    value = private_json(case_root / "fixed-progress.json", 8192)
    require(D.valid_record(value) and value["wrapper_phase"] == "capture" and value["capture_mode"] == mode)
    if progress is not None:
        for name in (*D.POSITIONS, "peer_facts"):
            progress.value[name] = value[name]
        if progress.value["failure"] is None:
            progress.value["failure"] = value["failure"]
        if progress.value["inner_cleanup"] != "failed":
            progress.value["inner_cleanup"] = value["inner_cleanup"]
        progress.save()
    return value


class WorkerSupervision:
    """Keep supervisor cleanup failure separate from the worker's first failure."""
    def __init__(self):
        self.cleanup_failed = False

    def command(self, _stage):
        pass

    def failed(self, _error):
        pass

    def cleanup(self, passed):
        if passed is not True:
            self.cleanup_failed = True


def run_isolated(root, artifact, appdir, peer, backend, fixture, environment, *, bindings, diagnostics=None):
    containment.assert_contained()
    require(type(bindings) is bool)
    root.mkdir(mode=0o700)
    client.private_directory(root)
    mode = "eager_bindings" if bindings else "ordinary"
    paths = {"artifact": artifact, "peer": peer, "backend": backend, "fixture": fixture,
             "executable": appdir / "usr/bin/morpheus-app", "library": appdir / "usr/lib/libpipewire-0.3.so.0"}
    value = {"schema": 1, "mode": mode, "appdir": str(appdir),
             "files": {name: file_record(path, BOUNDS[name]) for name, path in paths.items()}}
    input_path = root / "case-input.json"
    client.write_new(input_path, json.dumps(value, sort_keys=True).encode() + b"\n")
    # A fresh exec avoids libatspi's process-global connection and cached tree.
    # The dedicated runner does not overwrite worker progress on command exit.
    supervision = WorkerSupervision()
    runner = OwnedRunner(root / "worker-logs", progress=supervision)
    if diagnostics is not None:
        # Reset peer facts against this fresh case before exec; a launch failure
        # must not inherit completed facts from the preceding ordinary case.
        if diagnostics.value["inner_cleanup"] != "failed":
            diagnostics.value["inner_cleanup"] = "not_started"
        diagnostics.capture(mode, "setup", root / "session" / "peer-control")
        diagnostics.command("capture-case")
    failure = None
    cleanup_confirmed = False
    try:
        runner.run("capture-case", [str(Path(sys.executable).resolve()), str(Path(__file__).resolve()), str(input_path)],
                   cwd=ROOT, env=environment, timeout=900, output_limit=0)
    except BaseException as error:
        failure = error
    try:
        state = adopt_progress(diagnostics, root, mode)
        cleanup_confirmed = state["inner_cleanup"] == "passed"
        require(state["failure"] is None and state["inner_cleanup"] == "passed"
                and state["capture_phase"] == "complete" and not supervision.cleanup_failed)
    except BaseException as error:
        if failure is None:
            failure = error
    if failure is not None:
        if diagnostics is not None:
            # The supervisor can fail cleanup even after the worker reported
            # success. A killed worker with no validated cleanup receipt is
            # also unconfirmed; later Docker teardown must not label it passed.
            if (not cleanup_confirmed or supervision.cleanup_failed
                    or (isinstance(failure, CommandFailure) and failure.reason == "cleanup")):
                diagnostics.cleanup(False)
            diagnostics.failed(failure)
        raise failure
    return validate_result(private_json(root / "case-result.json", 32768), value)


def main(arguments):
    require(len(arguments) == 1)
    input_path = canonical_path(arguments[0])
    assert_worker_contained(input_path.parent.parent)
    root, appdir, paths, value = validate_input(input_path)
    progress = D.Progress(root)
    progress.wrapper("capture")
    progress.capture(value["mode"], "setup")
    try:
        from appimage_capture import run_case
        result = run_case(WorkerRunner(root / "command-logs", root.parent, progress=progress), root / "session",
                          paths["artifact"], appdir, paths["peer"], paths["backend"], paths["fixture"],
                          dict(os.environ), bindings=MODES[value["mode"]], diagnostics=progress)
        validate_result(result, value)
        require(progress.value["failure"] is None and progress.value["inner_cleanup"] == "passed"
                and progress.value["capture_phase"] == "complete")
        client.write_new(root / "case-result.json", json.dumps(result, sort_keys=True).encode() + b"\n")
    except BaseException as error:
        progress.failed(error)
        raise


if __name__ == "__main__":
    def interrupted(_signum, _frame):
        raise SystemExit(1)
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main(sys.argv[1:])
    except BaseException:
        # Raw worker exceptions never escape; child service logs remain private
        # and the outer owner scans them before publishing fixed diagnostics.
        raise SystemExit(1)
