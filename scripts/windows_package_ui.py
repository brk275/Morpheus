#!/usr/bin/env python3
"""Bounded, account-free installed Windows keyboard/UI Automation check on CI only.

package_smoke proves the plain installed launch first. This second launch of the
same installed release executable runs inside a kernel job object, drives the
real window through Windows UI Automation and real keyboard input, and then
asks for a normal close. It is accessibility API/keyboard evidence on a fresh
hosted runner profile, not Narrator, live-account or visual acceptance.
"""

import contextlib
import ctypes
import json
import os
import queue
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from pathlib import Path

if __package__:
    from . import package_smoke as smoke
    from . import run_windows_capture_acceptance as capture
else:
    import package_smoke as smoke
    import run_windows_capture_acceptance as capture

CHECK = "installed_windows_keyboard_uia"
PROFILE_RELATIVE = Path("brk275") / "Morpheus"
# directories 6 resolves io.github/brk275/Morpheus to <RoamingAppData>\brk275\Morpheus
# (config, data) and <LocalAppData>\brk275\Morpheus (local data, cache).
KNOWN_FOLDERS = {
    "roaming": "{3EB685DB-65F9-4CF6-A03A-E3EF65729F3D}",
    "local": "{F1B32785-6FBA-4FCF-9D55-7B8E7F157091}",
}
SEED_CONFIG = "schema_version = 1\n[appearance]\nscale_percent = 110\n"
ACCEPTANCE_SECONDS = 180
BRIDGE_REPLY_SECONDS = 20
WAIT_SECONDS = 12
BRIDGE_LINE_BOUND = 4 * 1024 * 1024
TREE_BOUND = 5000
KEYS = frozenset({"Return", "Space", "Tab", "ShiftTab", "Escape"})
SETTINGS_TAB_BOUND = 160
WM_CLOSE = 0x0010
FILE_ATTRIBUTE_REPARSE_POINT = 0x400


class DiagnosticFailure(RuntimeError):
    def __init__(self, code):
        self.code = code
        super().__init__("Installed Windows UI assertion failed")


def require_ui(condition, code):
    if not condition:
        raise DiagnosticFailure(code)


class Diagnostics:
    """Only fixed tokens/booleans/bounded counts; never names, values, handles, PIDs or errors."""
    def __init__(self, report):
        self.value = report["diagnostic"] = {
            "step": "setup", "operation": "none", "failure": "none", "bridge_error": "none",
            "owned_window_seen": False, "uia_root_seen": False, "homeserver_seen": False,
            "username_seen": False, "password_seen": False,
            "selector_matches": 0, "incomplete_passes": 0, "foreground_recoveries": 0,
            "bounded_snapshots": 0, "password_semantics": "unchecked",
            "traversed_node_matched": False, "cleanup": "not_started", "profile_removed": False,
        }
        self.deadline = time.monotonic() + ACCEPTANCE_SECONDS

    def step(self, step):
        self.value["step"] = step
        self.value["operation"] = "none"

    def operation(self, operation):
        self.value["operation"] = operation

    def count(self, key):
        self.value[key] = min(self.value[key] + 1, 2)

    def bridge_error(self, code):
        if self.value["bridge_error"] == "none":
            self.value["bridge_error"] = code if code in smoke.WINDOWS_BRIDGE_ERRORS else "bridge_error"

    def check_deadline(self):
        require_ui(time.monotonic() < self.deadline, "deadline")

    def failed(self, error):
        if self.value["failure"] != "none":
            return
        if isinstance(error, DiagnosticFailure):
            code = error.code
        elif isinstance(error, capture.Failure):
            code = error.code
        elif isinstance(error, AttributeError):
            code = "attribute_error"
        elif isinstance(error, TypeError):
            code = "type_error"
        elif isinstance(error, LookupError):
            code = "lookup_error"
        elif isinstance(error, ValueError):
            code = "value_error"
        elif isinstance(error, RuntimeError):
            code = "runtime_error"
        elif isinstance(error, OSError):
            code = "os_error"
        else:
            code = "unclassified"
        self.value["failure"] = code if code in smoke.WINDOWS_KEYBOARD_FAILURES else "unclassified"


class IncompleteTraversal(LookupError):
    """A snapshot skipped or lost an owned node; its result is not evidence."""


class BridgeFailure(RuntimeError):
    """The bridge answered with a fixed error token (kept in diagnostics only)."""
    def __init__(self, code):
        self.code = code
        super().__init__("UI Automation bridge command failed")


def guard(environment=None, platform=None):
    environment = os.environ if environment is None else environment
    require_ui((sys.platform if platform is None else platform) == "win32"
               and environment.get("GITHUB_ACTIONS") == "true"
               and environment.get("RUNNER_ENVIRONMENT") == "github-hosted"
               and environment.get("RUNNER_OS") == "Windows"
               and environment.get("ImageOS") == "win25", "guard")


def powershell_path(environment=None):
    environment = os.environ if environment is None else environment
    return Path(environment.get("SystemRoot", r"C:\Windows")) / "System32" / "WindowsPowerShell" / "v1.0" / "powershell.exe"


def bridge_command(script, pid, window):
    require_ui(type(pid) is int and pid > 0 and type(window) is int and window > 0, "window_count")
    # A native CreateProcess argument list, never a shell string; the bridge
    # receives only the owned process and window identifiers.
    return [str(powershell_path()), "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
            "-File", str(script), "-ProcessId", str(pid), "-WindowHandle", str(window)]


def known_folder(name):
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    ole32 = ctypes.WinDLL("ole32", use_last_error=True)
    guid = ctypes.create_string_buffer(16)
    smoke.require(ole32.CLSIDFromString(KNOWN_FOLDERS[name], guid) == 0, "Invalid known folder identifier")
    result = ctypes.c_wchar_p()
    status = shell32.SHGetKnownFolderPath(guid, 0, None, ctypes.byref(result))
    smoke.require(status == 0 and result.value, "Could not resolve the application profile folder")
    try:
        return Path(result.value)
    finally:
        ole32.CoTaskMemFree(result)


def profile_roots():
    # The release executable (directories 6 / dirs-sys) resolves its profile
    # through the shell known folders, never APPDATA or LOCALAPPDATA, so no
    # environment variable can redirect it. The driver resolves the same
    # folders and treats the fresh hosted runner account as the isolation
    # boundary: it refuses any existing Morpheus material there, creates only
    # the synthetic profile, and removes only what it created.
    return [known_folder(name) / PROFILE_RELATIVE for name in ("roaming", "local")]


def reparse_point(path):
    """A symbolic link or junction, present or dangling; never followed."""
    try:
        status = os.lstat(path)
    except FileNotFoundError:
        return False
    return path.is_symlink() or bool(getattr(status, "st_file_attributes", 0) & FILE_ATTRIBUTE_REPARSE_POINT)


def seed_profile(roots):
    for root in roots:
        # A fresh runner has no brk275 organization folder at all. An existing
        # folder, file or reparse point at either level may hold or redirect to
        # real profile or vault material, so it is refused, not inspected.
        require_ui(not reparse_point(root.parent) and not root.parent.exists()
                   and not reparse_point(root) and not root.exists(), "profile_exists")
    config = roots[0] / "config" / "config.toml"
    try:
        config.parent.mkdir(parents=True)
        for created in (roots[0].parent, roots[0], config.parent):
            require_ui(created.is_dir() and not reparse_point(created), "profile_exists")
        config.write_text(SEED_CONFIG, encoding="utf-8")
    except Exception:
        remove_profile(roots)
        raise
    return config


def remove_profile(roots):
    for root in roots:
        # Only the directories this run created are removed; a reparse point
        # here would mean the tree is not ours and is never followed.
        smoke.require(not reparse_point(root) and not reparse_point(root.parent),
                      "Refusing to remove a redirected profile root")
        if root.exists():
            shutil.rmtree(root)
        with contextlib.suppress(OSError):
            root.parent.rmdir()  # Only when the organization folder is empty.
    return all(not root.exists() and not reparse_point(root) for root in roots)


def environment_for(nonce):
    env = os.environ.copy()
    env.pop("MORPHEUS_QA_PROFILE_DIR", None)
    env[smoke.NONCE_ENV] = nonce
    return env


def installed_executable(executable):
    entries = smoke.uninstall_registry()
    require_ui(len(entries) == 1 and entries[0]["DisplayName"] == smoke.PRODUCT
               and Path(entries[0]["InstallLocation"].strip('"')).resolve() == executable.parent.resolve()
               and executable.name == f"{smoke.BINARY}.exe", "install_state")


class Bridge:
    """One PowerShell UI Automation bridge; its whole stream is private."""
    def __init__(self, command, log, diagnostics):
        self.diagnostics = diagnostics
        self.replies = queue.Queue()
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
                                        encoding="utf-8", errors="replace", bufsize=1)
        self.reader = threading.Thread(target=self.pump, daemon=True)
        self.reader.start()
        ready = self.reply()
        require_ui(ready.get("ready") is True, "bridge_invalid")

    def pump(self):
        try:
            while True:
                line = self.process.stdout.readline()
                if not line:
                    break
                self.replies.put(line if len(line) <= BRIDGE_LINE_BOUND else "")
        except (OSError, ValueError):
            pass
        finally:
            self.replies.put(None)

    def reply(self):
        remaining = min(BRIDGE_REPLY_SECONDS, max(0.0, self.diagnostics.deadline - time.monotonic()))
        try:
            line = self.replies.get(timeout=remaining)
        except queue.Empty:
            self.diagnostics.check_deadline()
            raise DiagnosticFailure("bridge_timeout") from None
        require_ui(line is not None, "bridge_exited")
        try:
            value = json.loads(line)
        except ValueError:
            raise DiagnosticFailure("bridge_invalid") from None
        require_ui(type(value) is dict and type(value.get("ok")) is bool, "bridge_invalid")
        return value

    def call(self, command, allow_error=False):
        self.diagnostics.check_deadline()
        require_ui(self.process.poll() is None, "bridge_exited")
        try:
            self.process.stdin.write(json.dumps(command, separators=(",", ":")) + "\n")
            self.process.stdin.flush()
        except (OSError, ValueError):
            raise DiagnosticFailure("bridge_exited") from None
        value = self.reply()
        if not value["ok"]:
            code = value.get("error")
            code = code if code in smoke.WINDOWS_BRIDGE_ERRORS else "bridge_error"
            self.diagnostics.bridge_error(code)
            if allow_error:
                raise BridgeFailure(code)
            raise DiagnosticFailure("bridge_invalid")
        return value

    def close(self):
        try:
            if self.process.poll() is None:
                with contextlib.suppress(OSError, ValueError):
                    self.process.stdin.write('{"op":"exit"}\n')
                    self.process.stdin.flush()
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
        finally:
            with contextlib.suppress(OSError, ValueError):
                self.process.stdin.close()
            with contextlib.suppress(OSError, ValueError):
                self.process.stdout.close()


class Node:
    __slots__ = ("id", "parent", "pid", "name", "type", "password", "focused", "error")

    def __init__(self, raw):
        require_ui(type(raw) is dict and type(raw.get("id")) is int and type(raw.get("parent")) is int
                   and type(raw.get("pid")) is int and type(raw.get("name")) is str
                   and type(raw.get("type")) is str and all(type(raw.get(key)) is bool
                                                            for key in ("password", "focused", "error")),
                   "bridge_invalid")
        for key in self.__slots__:
            setattr(self, key, raw[key])


class WindowsUI:
    """Selectors over bridge snapshots with the Linux driver's evidence rules."""
    def __init__(self, bridge, pid, job, handle, diagnostics):
        self.bridge, self.pid, self.job, self.handle, self.diagnostics = bridge, pid, job, handle, diagnostics
        self.pass_incomplete = False

    def alive(self):
        self.diagnostics.operation("process_status")
        return self.job.kernel.WaitForSingleObject(self.handle, 0) == 258

    def nodes(self):
        self.pass_incomplete = False
        self.diagnostics.operation("snapshot")
        value = self.bridge.call({"op": "snapshot"})
        raw = value.get("nodes")
        require_ui(type(raw) is list and type(value.get("bounded")) is bool, "bridge_invalid")
        if value["bounded"]:
            self.diagnostics.count("bounded_snapshots")
        require_ui(len(raw) <= TREE_BOUND and not value["bounded"], "tree_bound")
        nodes = [Node(item) for item in raw]
        require_ui(all(node.id == index for index, node in enumerate(nodes)), "bridge_invalid")
        # The root must be the owned window itself and read completely.
        require_ui(bool(nodes) and nodes[0].parent == -1, "bridge_invalid")
        self.diagnostics.operation("node_owner")
        require_ui(nodes[0].pid == self.pid and not nodes[0].error, "bridge_invalid")
        self.diagnostics.value["uia_root_seen"] = True
        for node in nodes:
            self.diagnostics.operation("node_owner")
            if node.error or node.pid != self.pid:
                # A node the bridge could not read, or one that no longer
                # belonged to the owned process, leaves its subtree unseen.
                self.pass_incomplete = True
                continue
            self.diagnostics.value["traversed_node_matched"] = False
            yield node

    def find(self, label, *, prefix=False):
        matches = []
        self.diagnostics.value["selector_matches"] = 0
        for node in self.nodes():
            self.diagnostics.operation("node_name")
            matched = node.name.startswith(label) if prefix else node.name == label
            self.diagnostics.value["traversed_node_matched"] = matched
            if matched:
                matches.append(node)
                self.diagnostics.value["selector_matches"] = min(len(matches), 2)
        require_ui(len(matches) <= 1, "selector_ambiguous")
        if self.pass_incomplete:
            self.diagnostics.count("incomplete_passes")
            raise IncompleteTraversal()
        return matches[0] if matches else None

    def wait_for(self, predicate, seconds=None):
        deadline = time.monotonic() + (WAIT_SECONDS if seconds is None else seconds)
        while time.monotonic() < deadline:
            self.diagnostics.check_deadline()
            require_ui(self.alive(), "app_exited")
            try:
                value = predicate()
                if value:
                    return value
            except (LookupError, BridgeFailure):
                pass
            time.sleep(0.1)
        raise DiagnosticFailure("wait_timeout")

    def wait(self, label, *, prefix=False):
        return self.wait_for(lambda: self.find(label, prefix=prefix))

    def focused_name(self):
        self.diagnostics.operation("focused_element")
        value = self.bridge.call({"op": "focused"})
        require_ui(all(type(value.get(key)) is bool for key in ("present", "owned"))
                   and type(value.get("name")) is str, "bridge_invalid")
        if not value["present"]:
            return None
        # Focus elsewhere on the desktop is never treated as our focus state
        # and no key is sent toward it.
        require_ui(value["owned"], "focus_unowned")
        return value["name"]

    def expect_focus(self, name, *, prefix=False):
        return self.wait_for(lambda: (focused := self.focused_name()) is not None
                             and (focused.startswith(name) if prefix else focused == name))

    def focus(self, label, *, prefix=False):
        node = self.wait(label, prefix=prefix)
        self.diagnostics.operation("focus")
        # The bridge rechecks ownership of the live element before SetFocus.
        self.bridge.call({"op": "focus", "id": node.id})
        self.expect_focus(label, prefix=prefix)

    def key(self, key):
        smoke.require(key in KEYS, "Unsupported acceptance key")
        # Only an owned focused element may receive input, and the bridge
        # requires the exact owned foreground window before SendInput.
        self.diagnostics.operation("focused_element")
        require_ui(self.focused_name() is not None, "focus_unowned")
        self.diagnostics.operation("key_input")
        try:
            value = self.bridge.call({"op": "key", "key": key}, allow_error=True)
        except BridgeFailure as error:
            raise DiagnosticFailure("foreground_unowned" if error.code in {"foreground_unowned", "window_gone"}
                                    else "bridge_invalid") from None
        if value.get("recovered") is True:
            self.diagnostics.count("foreground_recoveries")


def inspect_sign_in(ui, report, diagnostics):
    report["phase"] = "sign_in_semantics"
    diagnostics.step("homeserver")
    homeserver = ui.wait("Homeserver URL")
    diagnostics.value["homeserver_seen"] = True
    diagnostics.step("username")
    username = ui.wait("Matrix ID or username")
    diagnostics.value["username_seen"] = True
    diagnostics.step("password")
    password = ui.wait("Password")
    diagnostics.value["password_seen"] = True
    diagnostics.step("password_semantics")
    diagnostics.operation("node_role")
    # Only role/protection flags are read; no field value is ever requested.
    semantics = ("protected" if password.password else "unprotected") if password.type == "edit" else "not_edit"
    diagnostics.value["password_semantics"] = semantics
    require_ui(semantics == "protected", "password_semantics")
    require_ui(homeserver.type == "edit" and username.type == "edit"
               and not homeserver.password and not username.password, "password_semantics")
    report["sign_in_semantics"] = True


def enter_preview(ui):
    # Keyboard entry through the real focused control; this starts the
    # account-free fictional preview and never submits credentials.
    ui.focus("Open interactive preview without signing in")
    ui.key("Return")
    ui.wait("Account menu for ", prefix=True)


def menu_keyboard(ui):
    trigger = "Account menu for "
    ui.focus(trigger, prefix=True)
    ui.key("Return")
    ui.expect_focus("Switch to ", prefix=True)
    ui.wait("Manage accounts")
    ui.key("Escape")
    ui.wait_for(lambda: ui.find("Manage accounts") is None)
    ui.expect_focus(trigger, prefix=True)
    ui.key("Space")
    ui.expect_focus("Switch to ", prefix=True)
    for _ in range(16):
        focused = ui.focused_name()
        if focused == "Manage accounts":
            break
        smoke.require(focused is not None and (focused.startswith("Switch to ")
                      or focused == "Add another Matrix account"), "Account menu tab order escaped")
        ui.key("Tab")
        ui.wait_for(lambda: (name := ui.focused_name()) is not None and name != focused)
    else:
        raise RuntimeError("Account menu target was not keyboard reachable")
    ui.key("Return")
    ui.wait("Manage saved Matrix accounts")
    ui.expect_focus("Done")
    ui.key("Tab")
    ui.expect_focus("Add account")
    ui.key("ShiftTab")
    ui.expect_focus("Done")
    ui.key("Escape")
    ui.wait_for(lambda: ui.find("Manage saved Matrix accounts") is None)
    ui.expect_focus(trigger, prefix=True)


def appearance_keyboard(ui):
    ui.focus("Open settings")
    ui.key("Return")
    # Settings rows below the fold are clipped out of the accessibility tree
    # until revealed, so neither the scale label nor its control can be waited
    # for from here. Reach the control with bounded real Tab presses, checking
    # the owned focused name after each one; focus reveals the row, and only
    # then is its label queried.
    for _ in range(SETTINGS_TAB_BOUND):
        focused = ui.focused_name()
        require_ui(focused is not None, "focus_unowned")
        if focused == "Increase font scale":
            break
        ui.key("Tab")
        ui.wait_for(lambda: (name := ui.focused_name()) is not None and name != focused)
    else:
        raise DiagnosticFailure("scale_unreachable")
    ui.wait("110% · text throughout the app")
    ui.key("Space")
    ui.wait("120% · text throughout the app")


def config_persisted(config, diagnostics, seconds=10):
    deadline = time.monotonic() + seconds
    while True:
        diagnostics.check_deadline()
        diagnostics.operation("config_read")
        try:
            value = tomllib.loads(config.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            value = {}
        if value.get("appearance", {}).get("scale_percent") == 120:
            return
        require_ui(time.monotonic() < deadline, "config_mismatch")
        time.sleep(0.2)


def owned_window(job, handle, pid, diagnostics, seconds=30):
    deadline = time.monotonic() + seconds
    while True:
        diagnostics.check_deadline()
        diagnostics.operation("process_status")
        require_ui(job.kernel.WaitForSingleObject(handle, 0) == 258, "app_exited")
        diagnostics.operation("window_enumeration")
        windows = smoke.visible_windows(pid)
        if windows:
            require_ui(len(windows) == 1, "window_count")
            diagnostics.value["owned_window_seen"] = True
            return int(windows[0])
        require_ui(time.monotonic() < deadline, "wait_timeout")
        time.sleep(0.25)


def request_close(pid, window):
    """A normal WM_CLOSE to the exact owned window, after reverifying its owner."""
    from ctypes import wintypes
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.IsWindow.argtypes = [wintypes.HWND]
    user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
    owner = wintypes.DWORD()
    user32.GetWindowThreadProcessId(window, ctypes.byref(owner))
    smoke.require(user32.IsWindow(window) and owner.value == pid, "Installed window is no longer owned")
    smoke.require(bool(user32.PostMessageW(window, WM_CLOSE, 0, 0)), "Could not request the installed window close")


def normal_close(job, handle, pid, window, diagnostics):
    diagnostics.operation("close_request")
    request_close(pid, window)
    diagnostics.operation("process_wait")
    try:
        code = job.wait(handle, 15)
    except capture.Failure as error:
        raise DiagnosticFailure("exit_status" if error.code == "deadline" else "process_wait") from None
    require_ui(code == 0, "exit_status")
    # The job accounts for every descendant, including ones that re-parented.
    diagnostics.operation("job_query")
    require_ui(job.active() == 0, "process_remaining")


def process_id(job, handle):
    from ctypes import wintypes
    job.kernel.GetProcessId.argtypes = [wintypes.HANDLE]
    job.kernel.GetProcessId.restype = wintypes.DWORD
    pid = job.kernel.GetProcessId(handle)
    smoke.require(pid > 0, "Could not identify the installed process")
    return int(pid)


def acceptance(executable, report, diagnostics=None):
    diagnostics = diagnostics or Diagnostics(report)
    guard()
    diagnostics.operation("registry")
    installed_executable(executable)
    diagnostics.step("profile")
    diagnostics.operation("known_folder")
    roots = profile_roots()
    diagnostics.operation("profile_seed")
    config = seed_profile(roots)
    diagnostics.step("launch")
    diagnostics.operation("job")
    job = None
    bridge = None
    try:
        job = capture.WindowsJob()
        with tempfile.TemporaryFile() as app_log, tempfile.TemporaryFile() as bridge_log:
            diagnostics.operation("process_create")
            handle = job.spawn([str(executable)], environment_for(secrets.token_hex(16)), app_log)
            diagnostics.operation("process_id")
            pid = process_id(job, handle)
            diagnostics.step("window")
            window = owned_window(job, handle, pid, diagnostics)
            diagnostics.step("bridge")
            diagnostics.operation("bridge_start")
            bridge = Bridge(bridge_command(Path(__file__).with_name("windows_package_ui.ps1"), pid, window),
                            bridge_log, diagnostics)
            ui = WindowsUI(bridge, pid, job, handle, diagnostics)
            inspect_sign_in(ui, report, diagnostics)
            diagnostics.step("preview")
            enter_preview(ui)
            report["phase"] = "account_menu_keyboard"
            diagnostics.step("account_menu")
            menu_keyboard(ui)
            report["account_menu_keyboard"] = True
            report["phase"] = "appearance_config"
            diagnostics.step("appearance")
            appearance_keyboard(ui)
            diagnostics.step("config_persisted")
            config_persisted(config, diagnostics)
            report["appearance_config"] = True
            report["phase"] = "normal_window_close"
            diagnostics.step("normal_close")
            bridge.close()
            bridge = None
            normal_close(job, handle, pid, window, diagnostics)
            report["normal_window_close"] = True
    except Exception as error:
        diagnostics.failed(error)
        raise
    finally:
        # Preserve the first failure position if bounded cleanup also fails.
        failure = None
        try:
            if bridge is not None:
                bridge.close()
        except Exception as error:  # noqa: BLE001 - every cleanup step must run
            failure = failure or error
        try:
            if job is not None:
                job.close()
        except Exception as error:  # noqa: BLE001
            failure = failure or error
        try:
            diagnostics.value["profile_removed"] = remove_profile(roots)
        except Exception as error:  # noqa: BLE001
            failure = failure or error
        if failure is None and diagnostics.value["profile_removed"]:
            diagnostics.value["cleanup"] = "passed"
        else:
            diagnostics.value["cleanup"] = "failed"
            if diagnostics.value["failure"] == "none":
                diagnostics.step("cleanup")
                diagnostics.operation("cleanup")
                diagnostics.failed(failure if failure is not None else DiagnosticFailure("cleanup"))
            if failure is not None:
                raise failure


def main():
    report = {"check": CHECK, "status": "failed", "phase": "setup",
              "sign_in_semantics": False, "account_menu_keyboard": False, "appearance_config": False,
              "normal_window_close": False, "app_log_public": False, "screen_reader_acceptance": False}
    diagnostics = Diagnostics(report)
    try:
        smoke.require(len(sys.argv) == 2, "Expected installed executable path")
        acceptance(Path(sys.argv[1]).resolve(), report, diagnostics)
        report.update(status="passed", phase="complete")
        diagnostics.step("complete")
    except Exception as error:  # noqa: BLE001 - only fixed tokens leave this process
        diagnostics.failed(error)
    finally:
        print(json.dumps(report, sort_keys=True), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
