#!/usr/bin/env python3
"""Linux CI process containment; no repository code executes with root privileges.

Keep this entry point stdlib-only: sudo executes trusted util-linux programs,
then setpriv drops privileges before starting this interpreter with -I.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import time

REPO = Path(__file__).resolve().parent.parent
TRUSTED = ("/usr/bin/sudo", "/usr/bin/unshare", "/usr/bin/setpriv")
CAPABILITIES = ("CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb")


def failure():
    return RuntimeError("Element process containment failed")


def trusted_program(path: str):
    resolved = Path(path).resolve(strict=True)
    for item in (resolved, *resolved.parents):
        info = item.stat()
        if info.st_uid != 0 or info.st_mode & 0o022:
            raise failure()
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise failure()


def namespace_command(args: list[str], directory: Path, env: dict[str, str]) -> list[str]:
    # Only the unprivileged parent imports repository helpers.
    from fixture_paths import private_directory, write_private

    if sys.platform != "linux" or env.get("GITHUB_ACTIONS") != "true" or env.get("RUNNER_OS") != "Linux":
        raise failure()
    uid, gid, groups = os.getuid(), os.getgid(), sorted(set(os.getgroups()))
    if uid == 0 or gid == 0 or 0 in groups or os.geteuid() != uid or os.getegid() != gid:
        raise failure()
    for path in TRUSTED:
        trusted_program(path)
    directory = private_directory(directory)
    control = directory / "control.json"
    write_private(control, json.dumps({"uid": uid, "gid": gid, "groups": groups,
                                      "outer_namespace": os.readlink("/proc/self/ns/pid"),
                                      "env": env, "args": args}), create=True)
    group_option = "--groups=" + ",".join(map(str, groups)) if groups else "--clear-groups"
    return [TRUSTED[0], "--non-interactive", "--", TRUSTED[1], "--pid", "--fork",
            "--kill-child=KILL", "--mount-proc", TRUSTED[2], f"--reuid={uid}", f"--regid={gid}",
            group_option, "--inh-caps=-all", "--ambient-caps=-all", "--bounding-set=-all",
            "--no-new-privs", "--pdeathsig=KILL", str(Path(sys.executable).resolve()), "-I",
            str(Path(__file__).resolve()), "--inside", str(control)]


def status(process):
    if process.returncode is not None:
        raise failure()
    observed = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    if observed is None:
        return None
    return observed.si_status if observed.si_code == os.CLD_EXITED else -observed.si_status


def wait_owned(process, seconds):
    deadline = time.monotonic() + seconds
    while (result := status(process)) is None:
        if time.monotonic() >= deadline:
            raise subprocess.TimeoutExpired("Element contained process", seconds)
        time.sleep(0.05)
    return result


def stop_owned(process):
    # A waitable direct child anchors this group ID, even after its exit.
    # Never signal a cached PID after another observer has reaped the child.
    status(process)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=10)


def read_control(path: Path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "r") as source:
        info = os.fstat(source.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077
                or info.st_uid != os.getuid() or info.st_size > 1_048_576):
            raise failure()
        value = json.load(source)
    if not isinstance(value, dict):
        raise failure()
    return value


def assert_init_identity(control):
    uid, gid, groups = control.get("uid"), control.get("gid"), control.get("groups")
    if (not isinstance(uid, int) or not isinstance(gid, int) or uid <= 0 or gid <= 0
            or not isinstance(groups, list) or any(type(group) is not int or group <= 0 for group in groups)
            or os.getresuid() != (uid, uid, uid) or os.getresgid() != (gid, gid, gid)
            or sorted(set(os.getgroups())) != groups or os.getpid() != 1 or os.getppid() != 0):
        raise failure()
    namespace = os.readlink("/proc/self/ns/pid")
    if (not re.fullmatch(r"pid:\[\d+\]", namespace) or namespace == control.get("outer_namespace")
            or namespace != os.readlink("/proc/1/ns/pid")):
        raise failure()
    fields = dict(line.split(":", 1) for line in Path("/proc/self/status").read_text().splitlines() if ":" in line)
    if (fields.get("NSpid", "").split() != ["1"] or fields.get("NoNewPrivs", "").strip() != "1"
            or any(int(fields.get(key, "1").strip(), 16) != 0 for key in CAPABILITIES)):
        raise failure()
    return namespace


def assert_contained():
    # The actual runner refuses direct execution outside the guarded PID1.
    control = read_control(Path(os.environ.get("MORPHEUS_ELEMENT_NAMESPACE_CONTROL", "")))
    return assert_init_identity(control)


def inside(path: Path):
    control = read_control(path)
    namespace = assert_init_identity(control)
    env, args = control.get("env"), control.get("args")
    if (not isinstance(env, dict) or any(not isinstance(k, str) or not isinstance(v, str) for k, v in env.items())
            or not isinstance(args, list) or not args or any(not isinstance(arg, str) for arg in args)):
        raise failure()
    os.environ.clear()
    os.environ.update(env)
    os.environ["MORPHEUS_ELEMENT_NAMESPACE_CONTROL"] = str(path)
    if args == ["namespace-process-probe"]:
        # This deliberate detached descendant models Chromium/native sessions.
        child = subprocess.Popen([sys.executable, "-I", "-c", "import time; time.sleep(60)"],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                 stderr=subprocess.DEVNULL, start_new_session=True)
        fd = os.open(path.parent / "ready.tmp", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as output:
            json.dump({"namespace": namespace, "child": child.pid}, output)
        os.replace(path.parent / "ready.tmp", path.parent / "ready.json")
        # Fail-safe if even the outer regression is killed before cleanup.
        time.sleep(30)
        return
    os.execvpe(args[0], args, os.environ)


def namespace_members(namespace: str):
    """Read-only observations, never use these numeric PIDs as signal targets."""
    found = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            if os.readlink(path / "ns/pid") != namespace:
                continue
            fields = dict(line.split(":", 1) for line in (path / "status").read_text().splitlines() if ":" in line)
            found[int(path.name)] = int(fields["NSpid"].split()[-1])
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
    return found


def open_init_handle(init_pid, namespace):
    candidate_fd = os.pidfd_open(init_pid)
    try:
        if namespace_members(namespace).get(init_pid) != 1:
            raise failure()
    except BaseException:
        os.close(candidate_fd)
        raise
    return candidate_fd


def containment_check(directory: Path):
    """Mandatory hosted regression: kill the exact sudo path with no TTY."""
    args = namespace_command(["namespace-process-probe"], directory, dict(os.environ))
    log_fd = os.open(directory / "probe.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    init_fd = namespace_fd = None
    process = None
    try:
        with os.fdopen(log_fd, "wb") as output:
            process = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=output,
                                       stderr=subprocess.STDOUT, start_new_session=True)
        deadline = time.monotonic() + 10
        ready = directory / "ready.json"
        while not ready.is_file():
            if status(process) is not None or time.monotonic() >= deadline:
                raise failure()
            time.sleep(0.05)
        value = read_control(ready)
        namespace = value.get("namespace", "")
        if not re.fullmatch(r"pid:\[\d+\]", namespace) or namespace == os.readlink("/proc/self/ns/pid"):
            raise failure()
        members = namespace_members(namespace)
        if 1 not in members.values() or value.get("child") not in members.values() or value.get("child") == 1:
            raise failure()
        init_pid = next(pid for pid, inner in members.items() if inner == 1)
        # Stable kernel handle is only a regression-failure cleanup fallback;
        # it is never used to make the group-kill assertion pass.
        namespace_fd = os.open(f"/proc/{init_pid}/ns/pid", os.O_RDONLY)
        if f"pid:[{os.fstat(namespace_fd).st_ino}]" != namespace:
            raise failure()
        init_fd = open_init_handle(init_pid, namespace)
        if os.getpgid(init_pid) != process.pid:
            raise failure()
        status(process)  # Must still be our waitable, unreaped direct child.
        os.killpg(process.pid, signal.SIGKILL)
        deadline = time.monotonic() + 5
        while namespace_members(namespace):
            if time.monotonic() >= deadline:
                raise failure()
            time.sleep(0.05)
        wait_owned(process, 5)
        print("MORPHEUS_ELEMENT_CONTAINMENT passed", flush=True)
    finally:
        try:
            if init_fd is not None:
                try:
                    signal.pidfd_send_signal(init_fd, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                finally:
                    os.close(init_fd)
        finally:
            if namespace_fd is not None:
                os.close(namespace_fd)
            if process is not None:
                stop_owned(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--inside", type=Path)
    group.add_argument("--check", type=Path)
    args = parser.parse_args()
    if args.inside:
        inside(args.inside)
    else:
        containment_check(args.check)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        print("Element process containment failed; raw diagnostics withheld", file=sys.stderr)
        raise SystemExit(1)
