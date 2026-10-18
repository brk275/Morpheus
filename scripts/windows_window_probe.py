#!/usr/bin/env python3
"""Bounded, account-free Win32 window inventory of the Slint probe example.

Hosted Windows only, with an explicit opt-in. Launches the separately built
crates/morpheus-app/examples/windows_window_probe.rs executable inside the
reviewed kill-on-close job, waits for its first visible top-level window, then
takes bounded snapshots of that process's top-level windows. It records only
fixed facts: a closed class family, named style and extended-style flags,
owner kind and zero or nonzero area. It never reads or prints titles, UI text,
raw class names, paths, handles or process IDs, sends no input or window
messages, starts no Morpheus application code and lets the example close
itself. See docs/WINDOWS_WINDOW_PROBE.md.
"""

import ctypes
import json
import os
from pathlib import Path
import sys
import tempfile
import time

if __package__:
    from . import run_windows_capture_acceptance as capture
else:
    import run_windows_capture_acceptance as capture

REPO = Path(__file__).resolve().parents[1]
CHECK = "windows_window_probe"
OPT_IN = "MORPHEUS_WINDOWS_WINDOW_PROBE"
# Kept equal to windows_package_ui.HOSTED_WINDOWS_IMAGES by a contract test.
HOSTED_WINDOWS_IMAGES = ("win25", "win25-vs2026")
EXAMPLE = "windows_window_probe.exe"
RUN_SECONDS = 100
FIRST_WINDOW_SECONDS = 20
POLL_SECONDS = 0.25
SNAPSHOTS = 8
SNAPSHOT_SECONDS = 0.5
# The example closes itself after 30 seconds; its own watchdog fires at 45.
# A contract test keeps these bounds consistent with the example's source.
EXIT_SECONDS = 50
# WindowsJob.close bounds job drainage and its single retained process wait.
CLEANUP_RESERVE_SECONDS = 20
WINDOW_BOUND = 64
KIND_BOUND = 16
CLASS_CHARS = 256
GWL_STYLE = -16
GWL_EXSTYLE = -20
GW_OWNER = 4
DWMWA_CLOAKED = 14
STILL_ACTIVE = 258

# Exact Win32 class names mapped to closed families; any other name is
# "other" and is never kept or printed.
FAMILIES = {
    "Window Class": "winit_window",  # winit's default class; Slint sets none
    "Winit Thread Event Target": "winit_event_target",
    "IME": "ime",
    "MSCTFIME UI": "text_services",
    "OleMainThreadWndClass": "ole",
}
STYLE_FLAGS = (("popup", 0x80000000), ("child", 0x40000000), ("minimize", 0x20000000),
               ("visible", 0x10000000), ("disabled", 0x08000000), ("caption", 0x00C00000),
               ("sysmenu", 0x00080000), ("thickframe", 0x00040000), ("minimizebox", 0x00020000),
               ("maximizebox", 0x00010000))
EXSTYLE_FLAGS = (("topmost", 0x00000008), ("transparent", 0x00000020), ("toolwindow", 0x00000080),
                 ("appwindow", 0x00040000), ("layered", 0x00080000),
                 ("noredirectionbitmap", 0x00200000), ("noactivate", 0x08000000))
EXITS = {0: "normal", 1: "platform_error", 3: "watchdog", 4: "watchdog_unavailable",
         5: "watchdog_failed"}
FAILURES = frozenset({"none", "guard", "arguments", "executable", "job", "process_start",
                      "process_id", "window_enumeration", "window_bound", "window_facts",
                      "first_window_timeout", "app_exited", "exit_timeout", "exit_status", "process_wait",
                      "window_unowned",
                      "process_remaining", "deadline", "cleanup", "unclassified"})
STAGES = frozenset({"setup", "launch", "first_window", "snapshots", "exit", "complete"})


class ProbeFailure(RuntimeError):
    def __init__(self, code):
        self.code = code if code in FAILURES else "unclassified"
        super().__init__("Windows window probe failed")


class ApiError(OSError):
    """A Win32 query failed; carries no handle, name or system text."""


def require(condition, code):
    if not condition:
        raise ProbeFailure(code)


def guard(environment=None, platform=None):
    environment = os.environ if environment is None else environment
    require((sys.platform if platform is None else platform) == "win32"
            and environment.get(OPT_IN) == "1"
            and environment.get("GITHUB_ACTIONS") == "true"
            and environment.get("RUNNER_ENVIRONMENT") == "github-hosted"
            and environment.get("RUNNER_OS") == "Windows"
            and environment.get("ImageOS") in HOSTED_WINDOWS_IMAGES, "guard")


def example_executable(value, root=REPO):
    """Only the cargo-built example under this checkout's target directory."""
    path = Path(value)
    require(path.is_absolute(), "executable")
    try:
        resolved = path.resolve(strict=True)
        target = (Path(root) / "target").resolve(strict=True)
    except (OSError, RuntimeError):
        raise ProbeFailure("executable") from None
    require(resolved.name == EXAMPLE and resolved.parent.name == "examples"
            and resolved.parent.parent.name in ("debug", "release")
            and resolved.parent.parent.parent == target and resolved.is_file(), "executable")
    return resolved


# Standard machine-wide variables only; never a user profile, account or app one.
SYSTEM_VARIABLES = ("ALLUSERSPROFILE", "CommonProgramFiles", "ComSpec", "NUMBER_OF_PROCESSORS", "OS",
                    "PATHEXT", "PROCESSOR_ARCHITECTURE", "ProgramData", "ProgramFiles", "SystemDrive")


def child_environment(temporary, environment=None):
    """A minimal system environment with a private temporary directory."""
    environment = os.environ if environment is None else environment
    system_root = environment.get("SystemRoot", r"C:\Windows")
    child = {name: environment[name] for name in SYSTEM_VARIABLES if name in environment}
    child.update(SystemRoot=system_root, windir=system_root, PATH=system_root + r"\System32;" + system_root,
                 TEMP=str(temporary), TMP=str(temporary))
    return child


def flags(value, table):
    return sorted(name for name, mask in table if value & mask == mask)


def family(class_name):
    return FAMILIES.get(class_name, "other") if type(class_name) is str else "other"


def inspect(api, window, pid):
    """Fixed facts for one owned top-level window, or None if it vanished."""
    try:
        require(api.process_of(window) == pid, "window_unowned")
        name = api.class_name(window)
        style = api.long(window, GWL_STYLE)
        exstyle = api.long(window, GWL_EXSTYLE)
        owner = api.owner(window)
        left, top, right, bottom = api.rect(window)
        visible = api.is_visible(window)
        owner_pid = api.process_of(owner) if owner else None
        cloaked = api.cloaked(window)
        # EnumWindows supplied a borrowed handle. Recheck ownership around all
        # queries so a destroyed/reused handle cannot supply foreign facts.
        require(api.process_of(window) == pid, "window_unowned")
    except ApiError:
        if not api.is_window(window):
            return None
        raise ProbeFailure("window_facts") from None
    return {
        "visible": bool(visible),
        "family": family(name),
        "style": flags(style, STYLE_FLAGS),
        "exstyle": flags(exstyle, EXSTYLE_FLAGS),
        "owner": "none" if not owner else ("same_process" if owner_pid == pid else "other_process"),
        "area": "nonzero" if right > left and bottom > top else "zero",
        "cloaked": cloaked,
    }


def snapshot(api, pid):
    """Every owned top-level window, with visible ones counted as
    package_smoke.visible_windows counts them: same process, IsWindowVisible."""
    windows = api.top_level(pid)
    require(len(windows) <= WINDOW_BOUND, "window_bound")
    visible, hidden, vanished = [], [], 0
    for window in windows:
        facts = inspect(api, window, pid)
        if facts is None:
            vanished += 1
        elif facts["visible"]:
            visible.append(facts)
        else:
            hidden.append(facts["family"])
    return {"visible": visible, "hidden": sorted(set(hidden)), "hidden_count": len(hidden),
            "vanished": vanished}


def kind_key(facts):
    return json.dumps({key: value for key, value in facts.items() if key != "visible"}, sort_keys=True)


def summarize(snapshots):
    counts = [len(item["visible"]) for item in snapshots]
    kinds = {}
    for item in snapshots:
        for key in {kind_key(facts) for facts in item["visible"]}:
            kinds[key] = kinds.get(key, 0) + 1
    visible_kinds = [{**json.loads(key), "snapshots": seen} for key, seen in sorted(kinds.items())]
    return {
        "snapshots": len(snapshots),
        "visible_count_min": min(counts) if counts else None,
        "visible_count_max": max(counts) if counts else None,
        "hidden_count_max": max((item["hidden_count"] for item in snapshots), default=None),
        "hidden_families": sorted({name for item in snapshots for name in item["hidden"]}),
        "vanished": sum(item["vanished"] for item in snapshots),
        "visible_kinds": visible_kinds[:KIND_BOUND],
        "visible_kinds_truncated": len(visible_kinds) > KIND_BOUND,
        # Whether the keyboard driver's "exactly one" rule would have held.
        "driver_rule": "exactly_one" if counts and set(counts) == {1} else "not_one",
    }


class Win32:
    """Read-only user32/dwmapi queries; x64 only, every result checked."""

    def __init__(self):
        require(sys.platform == "win32" and ctypes.sizeof(ctypes.c_void_p) == 8, "guard")
        from ctypes import wintypes as w
        self.w = w
        self.callback = ctypes.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        signatures = {
            "EnumWindows": ([self.callback, w.LPARAM], w.BOOL),
            "GetWindowThreadProcessId": ([w.HWND, ctypes.POINTER(w.DWORD)], w.DWORD),
            "IsWindow": ([w.HWND], w.BOOL),
            "IsWindowVisible": ([w.HWND], w.BOOL),
            "GetClassNameW": ([w.HWND, w.LPWSTR, ctypes.c_int], ctypes.c_int),
            "GetWindowLongPtrW": ([w.HWND, ctypes.c_int], ctypes.c_ssize_t),
            "GetWindow": ([w.HWND, w.UINT], w.HWND),
            "GetWindowRect": ([w.HWND, ctypes.POINTER(w.RECT)], w.BOOL),
        }
        for name, (arguments, result) in signatures.items():
            function = getattr(user32, name)
            function.argtypes, function.restype = arguments, result
        self.user32 = user32
        dwmapi = ctypes.WinDLL("dwmapi", use_last_error=True)
        dwmapi.DwmGetWindowAttribute.argtypes = [w.HWND, w.DWORD, ctypes.c_void_p, w.DWORD]
        dwmapi.DwmGetWindowAttribute.restype = ctypes.c_long
        self.dwmapi = dwmapi

    def process_of(self, window):
        owner = self.w.DWORD()
        if not self.user32.GetWindowThreadProcessId(window, ctypes.byref(owner)):
            raise ApiError()
        return owner.value

    def top_level(self, pid):
        found, overflow = [], False

        @self.callback
        def collect(window, _):
            nonlocal overflow
            owner = self.w.DWORD()
            if self.user32.GetWindowThreadProcessId(window, ctypes.byref(owner)) and owner.value == pid:
                if len(found) >= WINDOW_BOUND:
                    overflow = True
                else:
                    found.append(window)
            return True

        require(bool(self.user32.EnumWindows(collect, 0)), "window_enumeration")
        require(not overflow, "window_bound")
        return found

    def is_window(self, window):
        return bool(self.user32.IsWindow(window))

    def is_visible(self, window):
        return bool(self.user32.IsWindowVisible(window))

    def class_name(self, window):
        buffer = ctypes.create_unicode_buffer(CLASS_CHARS + 1)
        length = self.user32.GetClassNameW(window, buffer, CLASS_CHARS + 1)
        if length <= 0:
            raise ApiError()
        return buffer.value[:length]

    def long(self, window, index):
        ctypes.set_last_error(0)
        value = self.user32.GetWindowLongPtrW(window, index)
        if value == 0 and ctypes.get_last_error():
            raise ApiError()
        return value & 0xFFFFFFFF

    def owner(self, window):
        ctypes.set_last_error(0)
        owner = self.user32.GetWindow(window, GW_OWNER)
        if not owner and ctypes.get_last_error():
            raise ApiError()
        return owner or None

    def rect(self, window):
        rect = self.w.RECT()
        if not self.user32.GetWindowRect(window, ctypes.byref(rect)):
            raise ApiError()
        return rect.left, rect.top, rect.right, rect.bottom

    def cloaked(self, window):
        value = self.w.DWORD()
        if self.dwmapi.DwmGetWindowAttribute(window, DWMWA_CLOAKED, ctypes.byref(value), ctypes.sizeof(value)):
            return None
        return value.value != 0


def process_id(job, handle):
    from ctypes import wintypes
    job.kernel.GetProcessId.argtypes = [wintypes.HANDLE]
    job.kernel.GetProcessId.restype = wintypes.DWORD
    pid = job.kernel.GetProcessId(handle)
    require(pid > 0, "process_id")
    return int(pid)


def alive(job, handle):
    return job.kernel.WaitForSingleObject(handle, 0) == STILL_ACTIVE


def observe(api, job, handle, pid, report, deadline):
    """First visible window, then bounded snapshots, then the example's own exit."""
    report["stage"] = "first_window"
    first = time.monotonic() + FIRST_WINDOW_SECONDS
    while True:
        require(time.monotonic() < deadline, "deadline")
        require(alive(job, handle), "app_exited")
        visible = snapshot(api, pid)["visible"]
        # The event target can become visible before the subject is created.
        # Start sampling only once the real, nonzero-area Slint window exists.
        if any(item["family"] == "winit_window" and item["area"] == "nonzero"
               for item in visible):
            report["first_window_seen"] = True
            break
        require(time.monotonic() < first, "first_window_timeout")
        time.sleep(POLL_SECONDS)
    report["stage"] = "snapshots"
    snapshots = []
    for index in range(SNAPSHOTS):
        require(time.monotonic() < deadline, "deadline")
        require(alive(job, handle), "app_exited")
        snapshots.append(snapshot(api, pid))
        if index + 1 < SNAPSHOTS:
            time.sleep(SNAPSHOT_SECONDS)
    report.update(summarize(snapshots))
    report["stage"] = "exit"
    remaining = deadline - time.monotonic() - CLEANUP_RESERVE_SECONDS
    require(remaining > 0, "deadline")
    try:
        code = job.wait(handle, min(EXIT_SECONDS, remaining))
    except capture.Failure as error:
        raise ProbeFailure("exit_timeout" if error.code == "deadline" else "process_wait") from None
    report["exit"] = EXITS.get(code, "other")
    require(code == 0, "exit_status")
    # The job accounts for every descendant, including re-parented ones.
    require(job.active() == 0, "process_remaining")


def run(executable, report, *, api_factory=Win32, job_factory=None):
    deadline = time.monotonic() + RUN_SECONDS
    guard()
    executable = example_executable(executable)
    api = api_factory()
    report["stage"] = "launch"
    cleanup = "not_started"
    try:
        with tempfile.TemporaryDirectory(prefix="morpheus-window-probe-") as temporary, \
                tempfile.TemporaryFile() as log:
            job = None
            try:
                try:
                    job = (job_factory or capture.WindowsJob)()
                except capture.Failure:
                    raise ProbeFailure("job") from None
                try:
                    handle = job.spawn([str(executable)], child_environment(temporary), log)
                except capture.Failure:
                    raise ProbeFailure("process_start") from None
                pid = process_id(job, handle)
                observe(api, job, handle, pid, report, deadline)
            finally:
                # Keep the inherited log handle and temp tree alive until all
                # children are stopped, including on an observation failure.
                cleanup = "passed"
                try:
                    if job is not None:
                        job.close()
                except Exception:  # noqa: BLE001 - fixed token only
                    cleanup = "failed"
                report["cleanup"] = cleanup
    except OSError:
        report["cleanup"] = "failed"
        raise ProbeFailure("cleanup") from None
    require(cleanup == "passed", "cleanup")
    report["stage"] = "complete"


def new_report():
    return {"check": CHECK, "status": "failed", "failure": "none", "stage": "setup",
            "first_window_seen": False, "snapshots": 0, "visible_count_min": None,
            "visible_count_max": None, "hidden_count_max": None, "hidden_families": [],
            "vanished": 0, "visible_kinds": [], "visible_kinds_truncated": False,
            "driver_rule": "not_one", "exit": "not_observed", "cleanup": "not_started"}


def public(report):
    """Re-validate every key and value before anything is printed."""
    tokens = set(FAMILIES.values()) | {"other"}
    kind_keys = {"family", "style", "exstyle", "owner", "area", "cloaked", "snapshots"}

    def count(value, bound):
        return value is None or (type(value) is int and 0 <= value <= bound)

    def token(value, allowed):
        return type(value) is str and value in allowed

    def token_list(value, allowed):
        return (type(value) is list and len(value) <= len(allowed)
                and all(token(item, allowed) for item in value)
                and value == sorted(set(value)))

    def kind(value):
        return (type(value) is dict and set(value) == kind_keys and token(value["family"], tokens)
                and token_list(value["style"], {name for name, _ in STYLE_FLAGS})
                and token_list(value["exstyle"], {name for name, _ in EXSTYLE_FLAGS})
                and token(value["owner"], {"none", "same_process", "other_process"})
                and token(value["area"], {"zero", "nonzero"})
                and (value["cloaked"] is None or type(value["cloaked"]) is bool)
                and type(value["snapshots"]) is int and 1 <= value["snapshots"] <= SNAPSHOTS)

    valid = (type(report) is dict and set(report) == set(new_report()) and report["check"] == CHECK
             and token(report["status"], {"passed", "failed"}) and token(report["failure"], FAILURES)
             and token(report["stage"], STAGES) and type(report["first_window_seen"]) is bool
             and type(report["snapshots"]) is int and 0 <= report["snapshots"] <= SNAPSHOTS
             and type(report["vanished"]) is int and count(report["vanished"], WINDOW_BOUND * SNAPSHOTS)
             and all(count(report[key], WINDOW_BOUND) for key in
                     ("visible_count_min", "visible_count_max", "hidden_count_max"))
             and token_list(report["hidden_families"], tokens)
             and type(report["visible_kinds"]) is list and len(report["visible_kinds"]) <= KIND_BOUND
             and all(kind(item) for item in report["visible_kinds"])
             and type(report["visible_kinds_truncated"]) is bool
             and token(report["driver_rule"], {"exactly_one", "not_one"})
             and token(report["exit"], {*EXITS.values(), "other", "not_observed"})
             and token(report["cleanup"], {"not_started", "passed", "failed"}))
    if valid and report["status"] == "passed":
        valid = (report["failure"] == "none" and report["stage"] == "complete"
                 and report["first_window_seen"] is True and report["snapshots"] == SNAPSHOTS
                 and report["exit"] == "normal" and report["cleanup"] == "passed"
                 and all(type(report[key]) is int for key in
                         ("visible_count_min", "visible_count_max", "hidden_count_max")))
    return report if valid else {"check": CHECK, "status": "failed", "failure": "unclassified",
                                 "diagnostics": "withheld"}


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    report = new_report()
    try:
        require(len(argv) == 1, "arguments")
        run(argv[0], report)
        report["status"] = "passed"
    except ProbeFailure as error:
        report["failure"] = error.code
    except Exception:  # noqa: BLE001 - never print another exception's text
        report["failure"] = "unclassified"
    output = public(report)
    print(json.dumps(output, sort_keys=True), flush=True)
    return 0 if output["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
