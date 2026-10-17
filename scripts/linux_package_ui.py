#!/usr/bin/env python3
"""Bounded, account-free installed AppImage keyboard/AT-SPI check on CI only.

The first package_smoke launch proves the minimal runtime before these test
dependencies are installed. This second launch uses the same unchanged package.
It is accessibility API/keyboard evidence, not an Orca or visual contrast test.
"""

import contextlib
import ctypes
import json
import os
import secrets
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

if __package__:
    from . import package_smoke as smoke
else:
    import package_smoke as smoke


class DiagnosticFailure(RuntimeError):
    def __init__(self, code):
        self.code = code
        super().__init__("Installed UI assertion failed")


def require_ui(condition, code):
    if not condition:
        raise DiagnosticFailure(code)


class Diagnostics:
    """Only fixed tokens/booleans; never record names, values, PIDs or errors."""
    def __init__(self, report):
        self.value = report["diagnostic"] = {
            "step": "setup", "operation": "none", "failure": "none",
            "desktop_app_seen": False, "owned_app_seen": False,
            "owned_node_seen": False, "homeserver_seen": False,
            "username_seen": False, "password_seen": False,
            "selector_matches": 0, "password_role": "unchecked",
            "traversed_node_matched": False,
            "child_count_unavailable": 0, "child_unavailable": 0, "incomplete_passes": 0,
            "focus_kind": "unavailable", "focused_nodes": 0,
            "cleanup": "not_started",
        }

    def step(self, step):
        self.value["step"] = step
        self.value["operation"] = "none"

    def operation(self, operation):
        self.value["operation"] = operation

    def count(self, key):
        # Bounded like selector_matches: the fact that it happened, and whether
        # it happened more than once, is all the public record needs.
        self.value[key] = min(self.value[key] + 1, 2)

    def failed(self, error):
        if self.value["failure"] != "none":
            return
        if isinstance(error, DiagnosticFailure):
            code = error.code
        elif isinstance(error, AttributeError):
            code = "attribute_error"
        elif isinstance(error, TypeError):
            code = "type_error"
        elif isinstance(error, NotImplementedError):
            code = "not_implemented"
        elif isinstance(error, LookupError):
            code = "lookup_error"
        elif isinstance(error, ValueError):
            code = "value_error"
        elif isinstance(error, OverflowError):
            code = "overflow_error"
        elif isinstance(error, RuntimeError):
            code = "runtime_error"
        elif isinstance(error, OSError):
            code = "os_error"
        else:
            code = "unclassified"
        # Defensive fixed fallback if a future assertion supplies a bad token.
        self.value["failure"] = code if code in smoke.KEYBOARD_FAILURES else "unclassified"


class ChildCountUnavailable(LookupError):
    """One node's ChildCount call failed without a GError.

    at-spi2-core's atspi_accessible_get_child_count returns -1, and sets no
    error, when the D-Bus property call gets no reply (the configured 1000 ms
    timeout) or a malformed reply; an unreachable or hung application raises
    GLib.Error instead. pyatspi 2.46.1 binds Accessible.__len__ directly to
    get_child_count, so that -1 reaches Python's len(), which raises ValueError
    about the protocol rather than about the node. This is the same per-call
    IPC failure the driver already retries as GLib.Error, not a value bug.
    """


class IncompleteTraversal(LookupError):
    """A selector pass skipped an owned node; its result is not evidence."""


class ChildUnavailable(LookupError):
    """get_child_at_index returned no object for an index inside the count.

    libatspi returns NULL, with no GError, when the GetChildAtIndex call fails;
    pyatspi hands that back as None. Treating it as an empty position would
    present a subtree the driver never saw as complete.
    """


class ObservedSequence:
    """Drive pyatspi's child protocol with libatspi's sentinels recognized.

    pyatspi 2.46.1 binds Accessible.__len__ to get_child_count and defines
    Accessible_getitem as: call get_child_count again, adjust a negative index,
    raise IndexError when the index is outside that count, else return
    get_child_at_index. A -1 at the second count therefore ends iteration early
    as if the sequence were complete. This class issues the same native calls
    in the same order, so list() still reads the length hint, repeats the count
    for every index and stops at the same IndexError, but a -1 at any count or
    a missing child raises instead of truncating. No node/exception text is read.
    """
    def __init__(self, accessible, diagnostics, kind):
        self.accessible, self.diagnostics, self.kind = accessible, diagnostics, kind

    def count(self):
        self.diagnostics.operation(self.kind + "_child_count")
        count = self.accessible.get_child_count()
        # Exactly -1 is libatspi's documented sentinel. Any other negative or
        # non-integer length is unexpected and fails closed.
        if isinstance(count, int) and count == -1:
            self.diagnostics.count("child_count_unavailable")
            raise ChildCountUnavailable()
        return count

    def __len__(self):
        # An unexpected length still reaches len(), which raises ValueError.
        return self.count()

    def __getitem__(self, index):
        count = self.count()
        if type(count) is not int:
            raise TypeError()
        if count < 0:
            raise ValueError()
        if index < 0:
            index = count + index
        if index < 0 or index >= count:
            raise IndexError
        self.diagnostics.operation(self.kind + "_child_index")
        child = self.accessible.get_child_at_index(index)
        if child is None:
            self.diagnostics.count("child_unavailable")
            raise ChildUnavailable()
        return child


def command(arguments):
    result = subprocess.run(arguments, stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
    smoke.require(result.returncode == 0, "Installed UI helper command failed")
    return result.stdout


def seed_profile(root):
    env = os.environ.copy()
    env.pop("MORPHEUS_QA_PROFILE_DIR", None)
    for suffix in ("CONFIG", "DATA", "CACHE", "STATE", "RUNTIME"):
        path = root / suffix.lower()
        path.mkdir(mode=0o700)
        env[f"XDG_{suffix}_HOME" if suffix != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
    # directories::ProjectDirs normalizes the Linux app name to lower case.
    config = root / "config/morpheus/config.toml"
    config.parent.mkdir(mode=0o700)
    config.write_text("schema_version = 1\n[appearance]\nscale_percent = 110\n")
    config.chmod(0o600)
    env.update(SLINT_BACKEND="winit-software", WINIT_UNIX_BACKEND="x11", APPIMAGE_EXTRACT_AND_RUN="1")
    return env, config


class AccessibleUI:
    def __init__(self, process, nonce, diagnostics=None):
        self.diagnostics = diagnostics or Diagnostics({})
        self.diagnostics.step("atspi_setup")
        import pyatspi
        from gi.repository import GLib
        self.api, self.transient_error = pyatspi, GLib.Error
        self.process, self.nonce = process, nonce
        pyatspi.setTimeout(1000, 1000)

    def owned(self, pid):
        return smoke.owned_linux_pid(pid, self.process.pid, self.nonce)

    def nodes(self):
        count = 0
        # A pass that skipped any node, for an IPC failure or a vanished node,
        # has not seen the complete owned tree. find() refuses its result and
        # wait_for() runs another pass instead.
        self.pass_incomplete = False
        self.diagnostics.operation("desktop")
        desktop = self.api.Registry.getDesktop(0)
        for application in ObservedSequence(desktop, self.diagnostics, "desktop"):
            self.diagnostics.value["desktop_app_seen"] = True
            try:
                self.diagnostics.operation("application_pid")
                pid = application.get_process_id()
                self.diagnostics.operation("ownership")
                if not self.owned(pid):
                    continue
                self.diagnostics.value["owned_app_seen"] = True
                pending = [application]
                while pending:
                    node = pending.pop()
                    count += 1
                    require_ui(count <= 5000, "tree_bound")
                    try:
                        self.diagnostics.operation("node_cache")
                        node.clearCache()
                        self.diagnostics.operation("ownership")
                        if self.owned(pid):
                            self.diagnostics.value["owned_node_seen"] = True
                            self.diagnostics.value["traversed_node_matched"] = False
                            yield node
                            self.diagnostics.operation("node_children")
                            pending.extend(list(ObservedSequence(node, self.diagnostics, "node")))
                        else:
                            # The application was owned when this pass began;
                            # losing that proof mid-tree leaves its subtree unseen.
                            self.pass_incomplete = True
                    except (LookupError, self.transient_error):
                        self.pass_incomplete = True
                        continue
            except (LookupError, self.transient_error):
                self.pass_incomplete = True
                continue

    def find(self, label, *, prefix=False):
        matches = []
        self.diagnostics.value["selector_matches"] = 0
        for node in self.nodes():
            try:
                self.diagnostics.operation("node_name")
                value = node.name or ""
                matched = value.startswith(label) if prefix else value == label
                self.diagnostics.value["traversed_node_matched"] = matched
                if matched:
                    matches.append(node)
                    self.diagnostics.value["selector_matches"] = min(len(matches), 2)
            except self.transient_error:
                self.pass_incomplete = True
                continue
        # Two matches are ambiguous whatever else the pass missed.
        require_ui(len(matches) <= 1, "selector_ambiguous")
        if getattr(self, "pass_incomplete", False):
            self.diagnostics.count("incomplete_passes")
            raise IncompleteTraversal()
        return matches[0] if matches else None

    def wait_for(self, predicate, seconds=12):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.diagnostics.operation("process_status")
            require_ui(smoke.process_status(self.process) is None, "app_exited")
            try:
                value = predicate()
                if value:
                    return value
            except (LookupError, self.transient_error):
                pass
            time.sleep(0.1)
        raise DiagnosticFailure("wait_timeout")

    def wait(self, label, *, prefix=False):
        return self.wait_for(lambda: self.find(label, prefix=prefix))

    def focused_name(self):
        self.diagnostics.value["focus_kind"] = "unavailable"
        self.diagnostics.value["focused_nodes"] = 0
        found = []
        for node in self.nodes():
            try:
                if node.getState().contains(self.api.STATE_FOCUSED):
                    found.append(node.name or "")
            except self.transient_error:
                self.pass_incomplete = True
                continue
        # Two focused nodes are ambiguous whatever else the pass missed; a pass
        # that skipped any node could hide a second one, so it is not evidence.
        if len(found) > 1:
            self.diagnostics.value["focus_kind"] = "ambiguous"
            self.diagnostics.value["focused_nodes"] = 2
        smoke.require(len(found) <= 1, "Installed accessibility focus is ambiguous")
        if getattr(self, "pass_incomplete", False):
            self.diagnostics.count("incomplete_passes")
            raise IncompleteTraversal()
        self.diagnostics.value["focused_nodes"] = len(found)
        self.diagnostics.value["focus_kind"] = focus_kind(found[0] if found else None)
        return found[0] if found else None

    def expect_focus(self, name, *, prefix=False):
        return self.wait_for(lambda: (focused := self.focused_name()) is not None
                             and (focused.startswith(name) if prefix else focused == name))

    def window(self):
        windows = smoke.owned_x11_windows(self.process.pid, self.nonce)
        smoke.require(len(windows) == 1, "Installed UI must have one owned visible X11 window")
        return windows[0][0]

    def focus(self, label, *, prefix=False):
        # The initial anchor is explicitly focused through AT-SPI; subsequent
        # Tab/Shift+Tab/Escape/activation steps are real XTEST key events.
        command(["xdotool", "windowfocus", "--sync", self.window()])
        node = self.wait(label, prefix=prefix)
        smoke.require(node.queryComponent().grabFocus(), "Installed control could not receive focus")
        self.expect_focus(label, prefix=prefix)

    def key(self, key):
        smoke.require(key in {"Return", "space", "Tab", "shift+Tab", "Escape"}, "Unsupported acceptance key")
        window = self.window()
        command(["xdotool", "windowfocus", "--sync", window])
        # No --window: xdotool uses XTEST for the focused window, rather than
        # synthetic XSendEvent messages that bypass the normal input path.
        actual = command(["xdotool", "getwindowfocus"]).strip()
        smoke.require(actual.isdigit() and int(actual) == int(window, 16), "Keyboard target changed")
        command(["xdotool", "key", "--clearmodifiers", key])

    def action(self, label):
        node = self.wait(label)
        smoke.require(node.getState().contains(self.api.STATE_ENABLED), "Installed control is disabled")
        action = node.queryAction()
        smoke.require(action.nActions > 0 and action.doAction(0), "Installed accessible action failed")

    def tab_to(self, label, limit=160):
        """Reveal an offscreen action through the actual bounded Tab order.

        AccessKit excludes clipped nodes from AT-SPI. Waiting for a distant
        Settings label before focusing/scrolling its row cannot establish that
        it is absent. No intervening control is activated; every key still
        goes through the owned-window and foreground checks in key().
        """
        require_ui(type(limit) is int and 1 <= limit <= 160, "tree_bound")
        for attempts in range(limit + 1):
            previous = self.focused_name()
            if previous == label:
                # Preserve unique-selector proof, including the complete-pass
                # guard, before the caller can activate this focused action.
                self.wait(label)
                return
            if attempts == limit:
                break
            require_ui(previous is not None, "lookup_error")
            self.key("Tab")
            self.wait_for(lambda: (current := self.focused_name()) is not None
                          and current != previous)
        raise DiagnosticFailure("wait_timeout")


def inspect_sign_in(ui, report, diagnostics):
    report["phase"] = "sign_in_semantics"
    diagnostics.step("homeserver")
    ui.wait("Homeserver URL")
    diagnostics.value["homeserver_seen"] = True
    diagnostics.step("username")
    ui.wait("Matrix ID or username")
    diagnostics.value["username_seen"] = True
    diagnostics.step("password")
    password = ui.wait("Password")
    diagnostics.value["password_seen"] = True
    diagnostics.step("password_role")
    diagnostics.operation("node_role")
    role = password.getRole()
    diagnostics.value["password_role"] = (
        "password_text" if role == ui.api.ROLE_PASSWORD_TEXT else
        "entry" if role == ui.api.ROLE_ENTRY else
        "text" if role == ui.api.ROLE_TEXT else "other")
    require_ui(role == ui.api.ROLE_PASSWORD_TEXT, "password_role")
    report["sign_in_semantics"] = True


def focus_kind(name):
    """A fixed category only; never expose an account label or input value."""
    if name is None:
        return "none"
    if name.startswith("Account menu for "):
        return "account_trigger"
    if name.startswith("Switch to "):
        return "account_choice"
    return {"Manage accounts": "manage_choice", "Add another Matrix account": "add_choice",
            "Done": "done", "Add account": "add_account"}.get(name, "other")


def menu_keyboard(ui, diagnostics=None):
    def checkpoint(step):
        if diagnostics is not None:
            diagnostics.step(step)

    trigger = "Account menu for "
    checkpoint("menu_trigger_focus")
    ui.focus(trigger, prefix=True)
    checkpoint("menu_open_return")
    ui.key("Return")
    checkpoint("menu_return_focus")
    ui.expect_focus("Switch to ", prefix=True)
    checkpoint("menu_return_visible")
    ui.wait("Manage accounts")
    checkpoint("menu_escape")
    ui.key("Escape")
    checkpoint("menu_escape_hidden")
    ui.wait_for(lambda: ui.find("Manage accounts") is None)
    checkpoint("menu_escape_focus")
    ui.expect_focus(trigger, prefix=True)
    checkpoint("menu_open_space")
    ui.key("space")
    checkpoint("menu_space_focus")
    ui.expect_focus("Switch to ", prefix=True)
    # All transitions to Manage accounts must stay among menu choices. Avoid
    # clicking/focusing that target directly, which would hide broken tab order.
    for _ in range(16):
        checkpoint("menu_tab_manage")
        focused = ui.focused_name()
        if focused == "Manage accounts":
            break
        smoke.require(focused is not None and (focused.startswith("Switch to ")
                      or focused == "Add another Matrix account"), "Account menu tab order escaped")
        ui.key("Tab")
        ui.wait_for(lambda: ui.focused_name() != focused)
    else:
        raise RuntimeError("Account menu target was not keyboard reachable")
    checkpoint("menu_manage_activate")
    ui.key("Return")
    checkpoint("menu_manage_visible")
    ui.wait("Manage saved Matrix accounts")
    checkpoint("menu_done_focus")
    ui.expect_focus("Done")
    checkpoint("menu_add_tab")
    ui.key("Tab")
    ui.expect_focus("Add account")
    checkpoint("menu_done_backtab")
    ui.key("shift+Tab")
    ui.expect_focus("Done")
    checkpoint("menu_manage_escape")
    ui.key("Escape")
    ui.wait_for(lambda: ui.find("Manage saved Matrix accounts") is None)
    checkpoint("menu_final_focus")
    ui.expect_focus(trigger, prefix=True)


def appearance_keyboard(ui, config, diagnostics=None):
    def checkpoint(step):
        if diagnostics is not None:
            diagnostics.step(step)

    checkpoint("appearance")
    ui.focus("Open settings")
    ui.key("Return")
    checkpoint("appearance_reach_scale")
    ui.wait("Appearance")
    ui.tab_to("Increase font scale")
    checkpoint("appearance_initial_scale")
    ui.wait("110% · text throughout the app")
    ui.expect_focus("Increase font scale")
    ui.key("space")
    checkpoint("appearance_saved_scale")
    ui.wait("120% · text throughout the app")
    ui.wait_for(lambda: tomllib.loads(config.read_text())["appearance"]["scale_percent"] == 120)
    valid = config.read_bytes()
    try:
        # A real external TOML edit, picked up by the installed app's watcher.
        config.write_text("schema_version = 1\n[appearance]\nscale_percent = 9999\n")
        checkpoint("appearance_invalid_notice")
        ui.wait("Settings file: ", prefix=True)
        checkpoint("appearance_invalid_retained")
        ui.wait("120% · text throughout the app")
        smoke.require(config.read_text().endswith("scale_percent = 9999\n"), "Invalid edit was overwritten")
    finally:
        config.write_bytes(valid)
    checkpoint("appearance_restored")
    ui.wait_for(lambda: ui.find("Settings file: ", prefix=True) is None)
    ui.wait("120% · text throughout the app")


def request_close(window):
    """Send WM_DELETE_WINDOW, not XDestroyWindow or a process signal."""
    class Data(ctypes.Union):
        _fields_ = [("longs", ctypes.c_long * 5)]

    class Message(ctypes.Structure):
        _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong),
                    ("send_event", ctypes.c_int), ("display", ctypes.c_void_p),
                    ("window", ctypes.c_ulong), ("message_type", ctypes.c_ulong),
                    ("format", ctypes.c_int), ("data", Data)]

    class Event(ctypes.Union):
        _fields_ = [("message", Message), ("padding", ctypes.c_long * 24)]

    xlib = ctypes.CDLL("libX11.so.6")
    xlib.XOpenDisplay.argtypes, xlib.XOpenDisplay.restype = [ctypes.c_char_p], ctypes.c_void_p
    xlib.XInternAtom.argtypes, xlib.XInternAtom.restype = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int], ctypes.c_ulong
    xlib.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_long, ctypes.POINTER(Event)]
    xlib.XFlush.argtypes = xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
    display = xlib.XOpenDisplay(None)
    smoke.require(bool(display), "Could not open the acceptance X11 display")
    try:
        event = Event()
        event.message.type, event.message.display, event.message.window = 33, display, int(window, 16)
        event.message.message_type = xlib.XInternAtom(display, b"WM_PROTOCOLS", 0)
        event.message.format = 32
        event.message.data.longs[0] = xlib.XInternAtom(display, b"WM_DELETE_WINDOW", 0)
        smoke.require(xlib.XSendEvent(display, int(window, 16), 0, 0, ctypes.byref(event)) != 0,
                      "Could not send installed window close request")
        xlib.XFlush(display)
    finally:
        xlib.XCloseDisplay(display)


def acceptance(executable, report, diagnostics=None):
    diagnostics = diagnostics or Diagnostics(report)
    smoke.require(os.environ.get("GITHUB_ACTIONS") == "true" and sys.platform == "linux"
                  and Path("/.dockerenv").exists() and os.geteuid() != 0,
                  "Installed keyboard acceptance requires the unprivileged CI container")
    smoke.require_nonreaping_wait()
    for setting in ("IsEnabled", "ScreenReaderEnabled"):
        command(["gdbus", "call", "--session", "--dest", "org.a11y.Bus", "--object-path", "/org/a11y/bus",
                 "--method", "org.freedesktop.DBus.Properties.Set", "org.a11y.Status", setting, "<true>"])
    with tempfile.TemporaryDirectory(prefix="morpheus-installed-ui-") as folder, tempfile.TemporaryFile() as log:
        env, config = seed_profile(Path(folder))
        nonce = secrets.token_hex(16)
        env[smoke.NONCE_ENV] = nonce
        process = subprocess.Popen([str(executable)], env=env, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            ui = AccessibleUI(process, nonce, diagnostics)
            inspect_sign_in(ui, report, diagnostics)
            # Enter the account-free preview through its accessible action.
            # This flow starts keyboard checks in the account menu; it does not
            # claim keyboard-only startup or submit a login.
            diagnostics.step("preview")
            ui.action("Open interactive preview without signing in")
            report["phase"] = "account_menu_keyboard"
            diagnostics.step("account_menu")
            menu_keyboard(ui, diagnostics)
            report["account_menu_keyboard"] = True
            report["phase"] = "appearance_config"
            diagnostics.step("appearance")
            appearance_keyboard(ui, config, diagnostics)
            report["appearance_config"] = True
            report["phase"] = "normal_window_close"
            diagnostics.step("normal_close")
            request_close(ui.window())
            deadline = time.monotonic() + 15
            while smoke.process_status(process) is None and time.monotonic() < deadline:
                time.sleep(0.1)
            smoke.require(smoke.process_status(process) == 0, "Installed app did not exit normally")
            # The nonce also detects descendants that left the original group.
            marker = (smoke.NONCE_ENV + "=" + nonce).encode()
            for candidate in Path("/proc").iterdir():
                if not candidate.name.isdigit():
                    continue
                with contextlib.suppress(FileNotFoundError, ProcessLookupError, PermissionError):
                    data = (candidate / "environ").read_bytes()
                    smoke.require(marker not in data.split(b"\0"), "Installed app left a running process")
            report["normal_window_close"] = True
        except Exception as error:
            diagnostics.failed(error)
            raise
        finally:
            # Preserve the first failure position if bounded cleanup also fails.
            signal.alarm(0)
            try:
                smoke.stop_process(process)
                diagnostics.value["cleanup"] = "passed"
            except Exception as error:
                diagnostics.value["cleanup"] = "failed"
                if diagnostics.value["failure"] == "none":
                    diagnostics.step("cleanup")
                    diagnostics.operation("cleanup")
                    diagnostics.failed(error)
                raise


def main():
    report = {"check": "installed_linux_keyboard_atspi", "status": "failed", "phase": "setup",
              "sign_in_semantics": False, "account_menu_keyboard": False, "appearance_config": False,
              "normal_window_close": False, "app_log_public": False, "screen_reader_acceptance": False}

    diagnostics = Diagnostics(report)

    def deadline(_signal, _frame):
        raise DiagnosticFailure("deadline")

    try:
        smoke.require(len(sys.argv) == 2, "Expected installed package path")
        signal.signal(signal.SIGALRM, deadline)
        signal.alarm(180)
        acceptance(Path(sys.argv[1]).resolve(), report, diagnostics)
        report.update(status="passed", phase="complete")
        diagnostics.step("complete")
    except Exception as error:
        diagnostics.failed(error)
        # Library errors, tree contents, config diagnostics and application logs
        # are never printed. Fixed phases/booleans are enough to target a rerun.
        pass
    finally:
        signal.alarm(0)
        print(json.dumps(report, sort_keys=True), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
