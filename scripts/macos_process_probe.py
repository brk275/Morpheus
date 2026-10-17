# Owner-authored hosted-only native process contract. No app or profile.
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import signal
import subprocess
import sys
import tempfile
import time

CHILD = r"""
import os, signal, sys, time
r, w = os.pipe()
child = os.fork()
if child == 0:
    os.close(r)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    os.write(w, b"1")
    os.close(w)
    while True:
        time.sleep(1)
os.close(w)
assert os.read(r, 1) == b"1"
os.close(r)
print("ready", flush=True)
if sys.argv[1] == "exited_leader":
    os._exit(0)
while True:
    time.sleep(1)
"""


def require(value):
    if not value:
        raise RuntimeError("Native process contract failed")


def main():
    report = {"check": "macos_process_contract", "status": "failed", "phase": "guard",
              "architecture": platform.machine(), "app_launched": False, "profile_created": False,
              "vault_accessed": False, "raw_output_public": False, "source_verified": False,
              "driver_sha256": "", "smoke_sha256": "", "cases": []}
    module = None
    try:
        require(sys.platform == "darwin" and platform.machine() in {"arm64", "x86_64"})
        require(all(os.environ.get(k) == v for k, v in {
            "GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "macOS",
            "ImageOS": "macos15", "GITHUB_REPOSITORY": "brk275/Morpheus",
            "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/codex/macos-process-preflight",
        }.items()))
        report["phase"] = "source"
        root = Path(__file__).resolve().parents[1]
        for name, variable in (("macos_package_ui.py", "DRIVER_SHA256"), ("package_smoke.py", "SMOKE_SHA256")):
            require(hashlib.sha256((root / "scripts" / name).read_bytes()).hexdigest() == os.environ[variable])
        report["driver_sha256"] = os.environ["DRIVER_SHA256"]
        report["smoke_sha256"] = os.environ["SMOKE_SHA256"]
        report["source_verified"] = True
        report["phase"] = "import"
        sys.path.insert(0, str(root))
        from scripts import macos_package_ui as module
        module.guard()
        module.smoke.require_nonreaping_wait()
        for name in ("owned_identity", "running_leader", "exited_leader"):
            case = {"name": name, "session_verified": False, "group_verified": False,
                    "executable_verified": False, "survivor_verified": False,
                    "empty_before_reap": False, "leader_reaped": False, "cleanup_verified": False}
            report["cases"].append(case)
            process = None
            original_wait = None
            try:
                with tempfile.TemporaryFile() as private_log:
                    report["phase"] = "launch"
                    command = ["/bin/sleep", "60"] if name == "owned_identity" else [sys.executable, "-I", "-c", CHILD, name]
                    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                               stderr=private_log, start_new_session=True)
                    original_wait = process.wait
                    if name != "owned_identity":
                        report["phase"] = "ready"
                        with selectors.DefaultSelector() as selector:
                            selector.register(process.stdout, selectors.EVENT_READ)
                            require(bool(selector.select(timeout=5)))
                        require(process.stdout.readline(64) == b"ready\n")
                    report["phase"] = "identity"
                    if name != "exited_leader":
                        require(module.session_of(process.pid) == process.pid)
                        case["session_verified"] = True
                    if name == "owned_identity":
                        deadline = time.monotonic() + 5
                        while not module.owned_mac_pid(process.pid, process.pid, Path("/bin/sleep").resolve()):
                            require(time.monotonic() < deadline and module.smoke.process_status(process) is None)
                            time.sleep(0.02)
                        require(not module.owned_mac_pid(process.pid, process.pid, Path("/bin/false").resolve()))
                        case["executable_verified"] = True
                    report["phase"] = "survivor"
                    if name == "exited_leader":
                        deadline = time.monotonic() + 5
                        while module.smoke.process_status(process) is None:
                            require(time.monotonic() < deadline)
                            time.sleep(0.02)
                        require(module.smoke.process_status(process) == 0 and process.returncode is None)
                    members = module.group_members(process.pid)
                    require(len(members) == (2 if name == "running_leader" else 1))
                    if name == "exited_leader":
                        # A zombie leader need not answer getsid; its live child
                        # still belongs to the generated session anchored by it.
                        require(module.session_of(members[0][0]) == process.pid)
                        case["session_verified"] = True
                    case["group_verified"] = True
                    case["survivor_verified"] = name != "owned_identity"

                    def guarded_wait(*args, **kwargs):
                        require(process.returncode is None and module.group_members(process.pid) == [])
                        case["empty_before_reap"] = True
                        return original_wait(*args, **kwargs)

                    process.wait = guarded_wait
                    report["phase"] = "stop"
                    module.stop_group(process)
                    require(case["empty_before_reap"] and process.returncode is not None)
                    if name == "exited_leader":
                        require(process.returncode == 0)
                    case["leader_reaped"] = True
                    case["cleanup_verified"] = True
            finally:
                if process is not None:
                    process.wait = original_wait
                    if process.returncode is None:
                        report["phase"] = "cleanup"
                        # No signal after releasing the owned leader PID.
                        module.smoke.process_status(process)
                        try:
                            module.stop_group(process)
                        except Exception:
                            # Still anchored: a bounded last kill of only this
                            # generated group; never touch any app or profile.
                            if process.returncode is None:
                                module.smoke.process_status(process)
                                os.killpg(process.pid, signal.SIGKILL)
                                original_wait(timeout=5)
                            raise
                    if process.stdout is not None:
                        process.stdout.close()
        report.update(status="passed", phase="complete")
    except Exception:
        pass  # Fixed phase and booleans only; no exception/paths/PIDs/streams.
    print(json.dumps(report, sort_keys=True), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
