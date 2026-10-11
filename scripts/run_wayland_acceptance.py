#!/usr/bin/env python3
"""Run the actual Wayland ScreenCast/PipeWire fixture in a private desktop.

Requires Sway, swaybg, PipeWire, WirePlumber, xdg-desktop-portal and the Ubuntu
24.04 wlr 0.7.1 backend. No user desktop, screen recording, account or display
permission is used. No pixels or raw service logs are uploaded.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]
TARGET_TEST = "wayland_selected_display_streams_changes_and_stops"


def chooser(mode_path: Path) -> int:
    mode = mode_path.read_text(encoding="utf-8").strip()
    if mode == "allow":
        # wlr 0.7.1 expects a raw output name. The later 'Monitor:' syntax is
        # deliberately not used with Ubuntu 24.04's backend.
        print("HEADLESS-2", flush=True)
    elif mode == "delay":
        time.sleep(5)
    elif mode != "deny":
        return 2
    return 0


def configs(root: Path) -> tuple[Path, Path]:
    config = root / "config"
    (config / "xdg-desktop-portal").mkdir(parents=True)
    (config / "xdg-desktop-portal" / "portals.conf").write_text(
        "[preferred]\ndefault=wlr;\norg.freedesktop.impl.portal.ScreenCast=wlr;\n",
        encoding="utf-8",
    )
    sway = config / "sway.conf"
    sway.write_text(
        'xwayland disable\n'
        'output HEADLESS-1 mode 1280x720 bg #2020f0 solid_color\n'
        'output HEADLESS-2 mode 1280x720 position 1280 0 bg #f02020 solid_color\n'
        'seat seat0 fallback true\n', encoding="utf-8",
    )
    mode = root / "chooser-mode"
    mode.write_text("allow", encoding="utf-8")
    chooser_script = root / "choose-output"
    chooser_script.write_text(
        "#!/bin/sh\nexec " + shlex.join([sys.executable, str(Path(__file__).resolve()), "--chooser", str(mode)]) + "\n",
        encoding="utf-8",
    )
    chooser_script.chmod(0o700)
    wlr = config / "wlr.ini"
    wlr.write_text(
        "[screencast]\nmax_fps=15\nchooser_type=simple\nchooser_cmd=" + shlex.quote(str(chooser_script)) + "\n",
        encoding="utf-8",
    )
    return sway, wlr


def private_environment(root: Path) -> dict[str, str]:
    env = os.environ.copy()
    for field in ("DISPLAY", "WAYLAND_DISPLAY", "SWAYSOCK", "DBUS_SESSION_BUS_ADDRESS"):
        env.pop(field, None)
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    env.update({
        "XDG_RUNTIME_DIR": str(runtime), "PIPEWIRE_RUNTIME_DIR": str(runtime),
        "XDG_CONFIG_HOME": str(root / "config"), "XDG_CURRENT_DESKTOP": "sway",
        "XDG_SESSION_TYPE": "wayland", "WLR_BACKENDS": "headless",
        "WLR_RENDERER": "pixman", "WLR_HEADLESS_OUTPUTS": "2",
        "WLR_LIBINPUT_NO_DEVICES": "1", "MORPHEUS_WAYLAND_ACCEPTANCE": "1",
        "MORPHEUS_WAYLAND_CHOOSER_MODE": str(root / "chooser-mode"),
    })
    return env


def wait_for(predicate, processes: list[subprocess.Popen], description: str, seconds: int = 15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if any(peek_returncode(process) is not None for process in processes):
            raise RuntimeError(f"a fixture service exited while waiting for {description}")
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {description}")


def peek_returncode(process: subprocess.Popen) -> int | None:
    """Observe a Linux child without reaping its process-group identity.

    Keep exited leaders as zombies until every member of their group has been
    killed. Popen.poll/wait/communicate would free the leader PID too early.
    """
    if process.returncode is not None:
        raise RuntimeError("fixture process was reaped before group cleanup")
    result = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    if result is None:
        return None
    if result.si_code == os.CLD_EXITED:
        return result.si_status
    return -result.si_status


def wait_owned_process(process: subprocess.Popen, timeout: float) -> int:
    deadline = time.monotonic() + timeout
    while True:
        result = peek_returncode(process)
        if result is not None:
            return result
        if time.monotonic() >= deadline:
            raise subprocess.TimeoutExpired(process.args, timeout)
        time.sleep(0.05)


def stop_owned_groups(processes: list[subprocess.Popen]) -> None:
    """Terminate each complete group before releasing its anchored leader PID."""
    for process in processes:
        # Fail closed if a caller accidentally reintroduces a reaping wait.
        peek_returncode(process)
    try:
        for process in reversed(processes):
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if all(peek_returncode(process) is not None for process in processes):
                break
            time.sleep(0.05)
    finally:
        # A group leader can exit on TERM while a child ignores it. Send KILL
        # to every owned group, even exited leaders, before reaping any leader.
        for process in reversed(processes):
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
        for process in processes:
            process.wait(timeout=3)


def process_identity(pid: int) -> str | None:
    """Linux start time prevents the timeout fallback from killing reused PIDs."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
        return stat.rsplit(")", 1)[1].split()[19]
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return None


def register_groups(root: Path, processes: list[subprocess.Popen]) -> None:
    records = [{"pid": process.pid, "start": process_identity(process.pid)} for process in processes]
    temporary = root / "owned-groups.tmp"
    temporary.write_text(json.dumps(records), encoding="utf-8")
    temporary.replace(root / "owned-groups.json")


def force_registered_groups(root: Path) -> None:
    try:
        records = json.loads((root / "owned-groups.json").read_text(encoding="utf-8"))
    except FileNotFoundError:
        return
    for record in records:
        pid, started = record["pid"], record["start"]
        if started is not None and process_identity(pid) == started:
            with contextlib.suppress(ProcessLookupError):
                if os.getpgid(pid) == pid:
                    os.killpg(pid, signal.SIGKILL)


def session(root: Path) -> int:
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    processes: list[subprocess.Popen] = []
    with contextlib.ExitStack() as files:
        def start(name: str, command: list[str]):
            log = files.enter_context((root / f"{name}.log").open("wb"))
            process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            processes.append(process)
            register_groups(root, processes)
            return process
        try:
            start("sway", ["sway", "--config", str(root / "config" / "sway.conf")])
            display = wait_for(lambda: next((p.name for p in runtime.glob("wayland-*") if p.is_socket()), None), processes, "Wayland socket")
            sway_socket = wait_for(lambda: next(runtime.glob("sway-ipc.*.sock"), None), processes, "Sway control socket")
            os.environ.update(WAYLAND_DISPLAY=display, SWAYSOCK=str(sway_socket))
            subprocess.run(["dbus-update-activation-environment", "WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR", "PIPEWIRE_RUNTIME_DIR"], check=True, timeout=10)
            start("pipewire", ["pipewire"])
            wait_for(lambda: (runtime / "pipewire-0").is_socket(), processes, "PipeWire socket")
            start("wireplumber", ["wireplumber"])
            start("portal-wlr", ["/usr/libexec/xdg-desktop-portal-wlr", "--config", str(root / "config" / "wlr.ini")])
            start("portal", ["/usr/libexec/xdg-desktop-portal", "--replace"])

            def portal_ready():
                probe = subprocess.run(["gdbus", "call", "--session", "--dest", "org.freedesktop.portal.Desktop", "--object-path", "/org/freedesktop/portal/desktop", "--method", "org.freedesktop.DBus.Properties.Get", "org.freedesktop.portal.ScreenCast", "AvailableSourceTypes"], capture_output=True, timeout=3)
                return probe.returncode == 0
            wait_for(portal_ready, processes, "ScreenCast portal", seconds=30)
            outputs = subprocess.run(["swaymsg", "-t", "get_outputs", "-r"], capture_output=True, check=True, timeout=5)
            active = {item["name"] for item in json.loads(outputs.stdout) if item["active"]}
            if active != {"HEADLESS-1", "HEADLESS-2"}:
                raise RuntimeError("fixture requires exactly two active headless outputs")
            # Listing first catches accidental cfg/feature changes that would
            # otherwise make a zero-test run look like acceptance evidence.
            command = ["cargo", "test", "--locked", "-p", "morpheus-rtc", "--features", "native-devices", "--test", "wayland_capture"]
            listing_log = files.enter_context((root / "test-list.log").open("wb"))
            listing = subprocess.Popen(command + ["--", "--list"], cwd=REPO, stdout=listing_log, stderr=subprocess.STDOUT, start_new_session=True)
            processes.append(listing)
            register_groups(root, processes)
            listing_status = wait_owned_process(listing, timeout=900)
            listing_output = (root / "test-list.log").read_text(errors="replace")
            if listing_status or f"{TARGET_TEST}: test" not in listing_output:
                # Compiler diagnostics contain no fixture pixels or secrets.
                print(listing_output[-24000:], file=sys.stderr)
                raise RuntimeError("Wayland acceptance test did not compile or was not discovered")
            test = subprocess.Popen(command + ["--", "--ignored", "--nocapture", "--test-threads=1"], cwd=REPO, start_new_session=True)
            processes.append(test)
            register_groups(root, processes)
            if wait_owned_process(test, timeout=180):
                raise RuntimeError("Wayland portal acceptance failed")
            print("Actual Wayland display selection/stream/cancel tests passed; native window and GNOME/KDE remain unverified.")
            return 0
        finally:
            stop_owned_groups(processes)
            (root / "cleanup.done").touch(mode=0o600)


def run_session(command: list[str], env: dict[str, str], root: Path, timeout: float = 1200) -> int:
    process = subprocess.Popen(command, cwd=REPO, env=env, start_new_session=True)
    try:
        return process.wait(timeout=timeout)
    finally:
        # The inside-session Python catches TERM and closes its separately
        # owned service/cargo groups before this outer group is force-killed.
        if process.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        # dbus-run-session may exit before its Python child's finally block.
        # Keep the private runtime and logs alive until cleanup completes.
        deadline = time.monotonic() + 12
        while not (root / "cleanup.done").exists() and time.monotonic() < deadline:
            time.sleep(0.1)
        if not (root / "cleanup.done").exists():
            force_registered_groups(root)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inside-session", type=Path)
    parser.add_argument("--chooser", type=Path)
    args = parser.parse_args()
    if args.chooser:
        return chooser(args.chooser)
    if args.inside_session:
        return session(args.inside_session)
    if sys.platform != "linux":
        raise RuntimeError("the Wayland acceptance fixture requires Linux")
    for command in ("sway", "swaymsg", "swaybg", "pipewire", "wireplumber", "dbus-run-session", "dbus-update-activation-environment", "gdbus", "cargo"):
        if not shutil.which(command):
            raise RuntimeError(f"missing Wayland acceptance dependency: {command}")
    version = subprocess.run(["dpkg-query", "-W", "-f=${Version}", "xdg-desktop-portal-wlr"], capture_output=True, text=True, check=True).stdout
    if not version.startswith("0.7.1-"):
        raise RuntimeError("this chooser fixture is qualified for Ubuntu 24.04 xdg-desktop-portal-wlr 0.7.1")
    with tempfile.TemporaryDirectory(prefix="morpheus-wayland-") as directory:
        root = Path(directory)
        root.chmod(0o700)
        configs(root)
        return run_session(["dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()), "--inside-session", str(root)], private_environment(root), root)


if __name__ == "__main__":
    def interrupted(_signal, _frame):
        raise RuntimeError("fixture interrupted; stopping its owned processes")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.SubprocessError) as error:
        print(f"Wayland acceptance failed: {error}", file=sys.stderr)
        raise SystemExit(1)
