#!/usr/bin/env python3
"""Run the actual Wayland ScreenCast/PipeWire fixture in a private desktop.

Requires Sway, swaybg, PipeWire, WirePlumber, xdg-desktop-portal and the xdpw
build dependencies documented in docs/WAYLAND_FIXTURE.md. Builds a pinned,
CI-only xdpw 0.7.1 with a SHM compatibility adaptation in the private fixture;
no system backend is replaced. No user desktop, account or display permission
is used. No pixels or raw service logs are uploaded.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

# Import by sibling path as well when this script is loaded by unit tests.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from wayland_fixture_backend import prepare_backend
from element_process_namespace import (
    assert_contained, containment_check, namespace_command, read_control,
    stop_owned, wait_owned,
)

REPO = Path(__file__).resolve().parents[1]
TARGET_TEST = "wayland_selected_display_streams_changes_and_stops"
# Only these literal facts may leave the private fixture. Never copy matched
# text: service messages also contain session paths, output names and app IDs.
DIAGNOSTIC_MARKERS = {
    "portal-wlr": {
        "create_session_received": b"dbus: create session method invoked",
        "select_sources_received": b"dbus: select sources method invoked",
        "start_received": b"dbus: start method invoked",
        "no_valid_screencopy_format": b"wlroots: unable to receive a valid format from wlr_screencopy",
        "shm_buffer_announced": b"wlroots: buffer event handler",
        "dmabuf_buffer_announced": b"wlroots: linux_dmabuf event handler",
        "screencopy_failed": b"wlroots: failed event handler",
        "chooser_cancelled": b"wlroots: output chooser canceled",
        "no_output_selected": b"wlroots: no output found",
        "pipewire_iteration_failed": b"pipewire_loop_iterate failed:",
    },
    "portal": {
        "backend_call_failed": b"A backend call failed:",
        "session_export_failed": b"Failed to export session:",
        "result_processing_failed": b"Failed to process results:",
        "screencast_proxy_failed": b"Failed to create screen cast proxy:",
    },
}
DIAGNOSTIC_STAGES = frozenset({
    "backend_build", "compositor", "media_services", "portal", "outputs", "test_discovery", "capture_test",
})
DIAGNOSTIC_LOG_LIMIT = 4 * 1024 * 1024
CAPTURE_STAGES = frozenset({
    "before_test", "pipewire_binding_origin", "pipewire_initializer",
    "capabilities", "opening_display_1", "opening_display_2",
    "first_frame_1", "first_frame_2", "checking_pixels_1", "checking_pixels_2",
    "stopping_display_1", "stopping_display_2", "reopening_display", "reopen_frame",
    "changing_source", "awaiting_changed_frame", "removing_source", "awaiting_source_loss",
    "stopping_lost_source", "restoring_source", "denying_selection", "cancelling_selection",
    "complete",
})


def binding_facts(path: Path) -> dict:
    """Allow only bounded counts; loader paths/addresses never leave the test."""
    try:
        with path.open("rb") as source:
            raw = source.read(257)
        if len(raw) > 256:
            return {"status": "invalid"}
        value = json.loads(raw)
    except (OSError, ValueError):
        return {"status": "unavailable"}
    keys = {"checked", "system_pipewire", "executable", "other", "unavailable"}
    if (not isinstance(value, dict) or set(value) != keys
            or any(type(count) is not int or not 0 <= count <= 33 for count in value.values())
            or value["checked"] != 33
            or sum(value[key] for key in keys - {"checked"}) != 33):
        return {"status": "invalid"}
    return {"status": "valid", **value}


def bindings_passed(facts: dict) -> bool:
    return facts == {"status": "valid", "checked": 33, "system_pipewire": 33,
                     "executable": 0, "other": 0, "unavailable": 0}


def capture_stage(path: Path) -> str:
    try:
        with path.open("rb") as source:
            value = source.read(65).decode("ascii").strip()
    except (OSError, UnicodeError):
        return "unavailable"
    return value if value in CAPTURE_STAGES else "unknown"


def diagnostic_facts(root: Path, stage: str, services: dict[str, subprocess.Popen]) -> dict:
    """Return only fixed enums/booleans; unknown log text is never emitted."""
    facts = {"stage": stage if stage in DIAGNOSTIC_STAGES else "unknown"}
    for name in ("sway", "pipewire", "wireplumber", "portal-wlr", "portal"):
        process = services.get(name)
        if process is None:
            facts[f"{name}.process"] = "not_started"
        else:
            try:
                code = peek_returncode(process)
                facts[f"{name}.process"] = "running" if code is None else "exited"
            except (OSError, RuntimeError):
                facts[f"{name}.process"] = "unavailable"
        if name not in DIAGNOSTIC_MARKERS:
            continue
        try:
            with (root / f"{name}.log").open("rb") as log:
                contents = log.read(DIAGNOSTIC_LOG_LIMIT)
                truncated = bool(log.read(1))
                if truncated:
                    log.seek(-DIAGNOSTIC_LOG_LIMIT, os.SEEK_END)
                    contents += log.read(DIAGNOSTIC_LOG_LIMIT)
            facts[f"{name}.log_unavailable"] = False
            facts[f"{name}.log_truncated"] = truncated
        except OSError:
            contents = b""
            facts[f"{name}.log_unavailable"] = True
        for fact, marker in DIAGNOSTIC_MARKERS[name].items():
            facts[f"{name}.{fact}"] = marker in contents
    return facts


def capture_test_passed(output: str) -> bool:
    """An existing but no-longer-ignored test must not produce false evidence."""
    target_passed = re.search(rf"^test {re.escape(TARGET_TEST)} \.\.\. ok$", output, re.MULTILINE)
    summary_passed = re.search(
        r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out;",
        output, re.MULTILINE,
    )
    return target_passed is not None and summary_passed is not None


def capture_test_facts(output: str, status: int | None, stage: str = "unknown") -> dict:
    """Do not publish libtest panic payloads, even when the test fails."""
    facts = {
        "command": "timed_out" if status is None else "succeeded" if status == 0 else "failed",
        "required_test_passed": capture_test_passed(output),
        "zero_tests_passed": "test result: ok. 0 passed;" in output,
        "last_stage": stage if stage in CAPTURE_STAGES else "unknown",
    }
    # Cargo commonly exits 101 when its child dies by signal. Keep the
    # wrapper status and the child's reported termination distinct. Never
    # echo the cargo command, native error line, process ID or signal text.
    if status is not None and 0 <= status <= 255:
        facts["command_exit_code"] = status
    elif status is not None and -64 <= status < 0:
        facts["command_signal"] = -status
    reported_signal = re.search(r"\(signal: ([1-9][0-9]?), SIG[A-Z0-9]+(?::|\))", output)
    if reported_signal and int(reported_signal[1]) <= 64:
        facts["test_signal"] = int(reported_signal[1])
    reported_exit = re.search(r"\(exit status: ([0-9]{1,3})\)", output)
    if reported_exit and int(reported_exit[1]) <= 255:
        facts["test_exit_code"] = int(reported_exit[1])
    for category, markers in {
        "non_unwinding_panic": ("panic in a function that cannot unwind", "non-unwinding panic"),
        "borrow_panic": ("already borrowed", "RefCell already"),
        "pointer_precondition_panic": ("unsafe precondition(s) violated", "misaligned pointer dereference", "null pointer dereference"),
        "runtime_context_panic": ("there is no reactor running", "Cannot drop a runtime in a context"),
        "stack_overflow": ("has overflowed its stack", "fatal runtime error: stack overflow"),
        "pipewire_library_frame": ("pipewire::stream::", "pipewire::buffer::", "pipewire-0.10.1/src/"),
        "libspa_library_frame": ("libspa::", "libspa-0.10.1/src/"),
        "libspa_chunk_frame": ("libspa::buffer::Data::chunk",),
    }.items():
        facts[category] = any(marker in output for marker in markers)
    for phase, marker in {
        "create_session": "creating the screen-sharing session:",
        "select_sources": "selecting the screen-sharing source:",
        "start_stream": "starting the screen-sharing stream:",
        "portal_request_failed": "system screen-sharing portal could not complete the request",
        "initial_selection": "portal display selection",
        "first_frame": "first PipeWire frame",
        "wrong_selected_source": "selected red HEADLESS-2 was not captured",
        "stop_timeout": "session shutdown exceeded stop budget",
        "reopen": "portal reopen",
        "continuous_frame": "continuous PipeWire frame",
        "changed_frame_timeout": "stream did not deliver changed selected-source pixels",
        "unexpected_source_switch": "capture silently changed to another display",
        "source_loss_timeout": "lost source did not terminate capture",
        "denial_failed": "denied portal must not capture",
        "cancel_failed": "cancelled selection must not capture",
        "cancel_timeout": "selection cancellation exceeded worker stop budget",
    }.items():
        facts[phase] = marker in output
    location = re.search(r"panicked at crates/morpheus-rtc/tests/wayland_capture\.rs:([0-9]{1,5}):[0-9]{1,5}", output)
    if location:
        facts["fixture_panic_line"] = int(location[1])
    return facts


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
        "MORPHEUS_WAYLAND_STAGE_PATH": str(root / "capture-stage"),
        "MORPHEUS_WAYLAND_BINDING_PATH": str(root / "binding-facts"),
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
    """Read-only identity observations for process-tree regression assertions."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
        return stat.rsplit(")", 1)[1].split()[19]
    except (FileNotFoundError, ProcessLookupError, IndexError):
        return None


def assert_session_namespace() -> None:
    """The D-Bus child must remain inside the previously guarded PID namespace."""
    control = read_control(Path(os.environ.get("MORPHEUS_ELEMENT_NAMESPACE_CONTROL", "")))
    namespace = os.readlink("/proc/self/ns/pid")
    if (namespace == control.get("outer_namespace")
            or namespace != os.readlink("/proc/1/ns/pid")):
        raise RuntimeError("Wayland process containment failed")


def session(root: Path) -> int:
    assert_session_namespace()
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    processes: list[subprocess.Popen] = []
    services: dict[str, subprocess.Popen] = {}
    stage = "backend_build"
    with contextlib.ExitStack() as files:
        def start(name: str, command: list[str], *, service: bool = True):
            log = files.enter_context((root / f"{name}.log").open("wb"))
            os.chmod(root / f"{name}.log", 0o600)
            process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            processes.append(process)
            if service:
                services[name] = process
            return process

        def build_command(name: str, command: list[str]):
            process = start(name, command, service=False)
            try:
                status = wait_owned_process(process, timeout=180)
            except subprocess.TimeoutExpired:
                status = -1
            if status:
                # No desktop has started yet. These are compiler diagnostics,
                # never service logs or source-selection payloads.
                print((root / f"{name}.log").read_text(errors="replace")[-24000:], file=sys.stderr)
                raise RuntimeError("pinned Wayland fixture backend did not build")

        try:
            backend = prepare_backend(root, build_command)
            print("Wayland fixture: pinned xdpw 0.7.1 with CI-only SHM adaptation; no system installation.")
            stage = "compositor"
            start("sway", ["sway", "--config", str(root / "config" / "sway.conf")])
            display = wait_for(lambda: next((p.name for p in runtime.glob("wayland-*") if p.is_socket()), None), list(services.values()), "Wayland socket")
            sway_socket = wait_for(lambda: next(runtime.glob("sway-ipc.*.sock"), None), list(services.values()), "Sway control socket")
            os.environ.update(WAYLAND_DISPLAY=display, SWAYSOCK=str(sway_socket))
            subprocess.run(["dbus-update-activation-environment", "WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR", "PIPEWIRE_RUNTIME_DIR"], check=True, timeout=10)
            stage = "media_services"
            start("pipewire", ["pipewire"])
            wait_for(lambda: (runtime / "pipewire-0").is_socket(), list(services.values()), "PipeWire socket")
            start("wireplumber", ["wireplumber"])
            stage = "portal"
            # Trace remains in the mode-0700 fixture and is deleted at exit.
            # It distinguishes SHM-only backend startup from chooser failure.
            backend_process = start("portal-wlr", [str(backend), "--loglevel", "TRACE", "--config", str(root / "config" / "wlr.ini")])

            def backend_owned():
                probe = subprocess.run(["gdbus", "call", "--session", "--dest", "org.freedesktop.DBus", "--object-path", "/org/freedesktop/DBus", "--method", "org.freedesktop.DBus.GetConnectionUnixProcessID", "org.freedesktop.impl.portal.desktop.wlr"], capture_output=True, timeout=3)
                match = re.fullmatch(rb"\(uint32 ([0-9]+),\)\s*", probe.stdout)
                return probe.returncode == 0 and match is not None and int(match[1]) == backend_process.pid

            # Establish this process's ownership before the frontend can
            # auto-activate the unadapted installed backend through D-Bus.
            wait_for(backend_owned, list(services.values()), "private portal backend")
            start("portal", ["/usr/libexec/xdg-desktop-portal", "--replace"])

            def portal_ready():
                probe = subprocess.run(["gdbus", "call", "--session", "--dest", "org.freedesktop.portal.Desktop", "--object-path", "/org/freedesktop/portal/desktop", "--method", "org.freedesktop.DBus.Properties.Get", "org.freedesktop.portal.ScreenCast", "AvailableSourceTypes"], capture_output=True, timeout=3)
                return probe.returncode == 0
            wait_for(portal_ready, list(services.values()), "ScreenCast portal", seconds=30)
            stage = "outputs"
            outputs = subprocess.run(["swaymsg", "-t", "get_outputs", "-r"], capture_output=True, check=True, timeout=5)
            active = {item["name"] for item in json.loads(outputs.stdout) if item["active"]}
            if active != {"HEADLESS-1", "HEADLESS-2"}:
                raise RuntimeError("fixture requires exactly two active headless outputs")
            # Listing first catches accidental cfg/feature changes that would
            # otherwise make a zero-test run look like acceptance evidence.
            stage = "test_discovery"
            command = ["cargo", "test", "--locked", "-p", "morpheus-rtc", "--features", "native-devices", "--test", "wayland_capture"]
            listing_log = files.enter_context((root / "test-list.log").open("wb"))
            listing = subprocess.Popen(command + ["--", "--list"], cwd=REPO, stdout=listing_log, stderr=subprocess.STDOUT, start_new_session=True)
            processes.append(listing)
            listing_status = wait_owned_process(listing, timeout=300)
            listing_output = (root / "test-list.log").read_text(errors="replace")
            if listing_status or f"{TARGET_TEST}: test" not in listing_output:
                # Compiler diagnostics contain no fixture pixels or secrets.
                print(listing_output[-24000:], file=sys.stderr)
                raise RuntimeError("Wayland acceptance test did not compile or was not discovered")
            stage = "capture_test"
            stage_path = root / "capture-stage"
            stage_path.write_text("before_test", encoding="ascii")
            stage_path.chmod(0o600)
            binding_path = root / "binding-facts"
            binding_path.write_text("{}", encoding="ascii")
            binding_path.chmod(0o600)
            test_log = files.enter_context((root / "capture-test.log").open("wb"))
            os.chmod(root / "capture-test.log", 0o600)
            test = subprocess.Popen(command + ["--", "--ignored", "--exact", TARGET_TEST, "--test-threads=1"], cwd=REPO, stdout=test_log, stderr=subprocess.STDOUT, start_new_session=True)
            processes.append(test)
            try:
                status = wait_owned_process(test, timeout=180)
            except subprocess.TimeoutExpired:
                status = None
            test_output = (root / "capture-test.log").read_text(errors="replace")
            origins = binding_facts(binding_path)
            print("Wayland PipeWire binding facts: " + json.dumps(origins, sort_keys=True))
            if status != 0 or not capture_test_passed(test_output) or not bindings_passed(origins):
                print("Wayland capture test facts: " + json.dumps(capture_test_facts(test_output, status, capture_stage(stage_path)), sort_keys=True), file=sys.stderr)
                raise RuntimeError("Wayland portal acceptance did not execute and pass its required test")
            print("Actual Wayland display selection/stream/cancel tests passed; native window and GNOME/KDE remain unverified.")
            return 0
        except Exception:
            print("Wayland fixture diagnostic facts: " + json.dumps(diagnostic_facts(root, stage, services), sort_keys=True), file=sys.stderr)
            raise
        finally:
            stop_owned_groups(processes)


def run_session(command: list[str], env: dict[str, str], timeout: float = 840) -> int:
    process = subprocess.Popen(command, cwd=REPO, env=env, stdin=subprocess.DEVNULL,
                               start_new_session=True)
    try:
        return wait_owned(process, timeout)
    finally:
        # The unreaped sudo child anchors this group until KILL. The mandatory
        # preflight proves this exact non-TTY path kills namespace PID1 too;
        # Linux then tears down even detached service/cargo descendants.
        # Never signal process IDs recovered from a stale registry file.
        stop_owned(process)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inside-session", type=Path)
    parser.add_argument("--contained-session", type=Path)
    parser.add_argument("--chooser", type=Path)
    args = parser.parse_args()
    if args.chooser:
        return chooser(args.chooser)
    if args.contained_session:
        assert_contained()
        # Already unprivileged, capability-free PID1. D-Bus and the desktop
        # clients must see the same /proc IDs for portal ownership checks.
        os.execv("/usr/bin/dbus-run-session", ["/usr/bin/dbus-run-session", "--",
                   sys.executable, str(Path(__file__).resolve()), "--inside-session",
                   str(args.contained_session)])
    if args.inside_session:
        return session(args.inside_session)
    if sys.platform != "linux":
        raise RuntimeError("the Wayland acceptance fixture requires Linux")
    for command in ("sway", "swaymsg", "swaybg", "pipewire", "wireplumber", "dbus-run-session", "dbus-update-activation-environment", "gdbus", "cargo", "meson", "ninja", "pkg-config", "cc"):
        if not shutil.which(command):
            raise RuntimeError(f"missing Wayland acceptance dependency: {command}")
    with tempfile.TemporaryDirectory(prefix="morpheus-wayland-") as directory:
        root = Path(directory)
        root.chmod(0o700)
        configs(root)
        containment_check(root / "containment-probe")
        env = private_environment(root)
        command = namespace_command([sys.executable, str(Path(__file__).resolve()),
                                     "--contained-session", str(root)], root / "namespace", env)
        return run_session(command, env)


if __name__ == "__main__":
    def interrupted(_signal, _frame):
        raise RuntimeError("fixture interrupted; stopping its owned processes")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.SubprocessError) as error:
        print(f"Wayland acceptance failed: {error}", file=sys.stderr)
        raise SystemExit(1)
