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
    def __init__(self, process, nonce):
        import pyatspi
        from gi.repository import GLib
        self.api, self.transient_error = pyatspi, GLib.Error
        self.process, self.nonce = process, nonce
        pyatspi.setTimeout(1000, 1000)

    def owned(self, pid):
        return smoke.owned_linux_pid(pid, self.process.pid, self.nonce)

    def nodes(self):
        count = 0
        for application in self.api.Registry.getDesktop(0):
            try:
                pid = application.get_process_id()
                if not self.owned(pid):
                    continue
                pending = [application]
                while pending:
                    node = pending.pop()
                    count += 1
                    smoke.require(count <= 5000, "Installed accessibility tree exceeds bound")
                    try:
                        node.clearCache()
                        if self.owned(pid):
                            yield node
                            pending.extend(list(node))
                    except (LookupError, self.transient_error):
                        continue
            except (LookupError, self.transient_error):
                continue

    def find(self, label, *, prefix=False):
        matches = []
        for node in self.nodes():
            try:
                value = node.name or ""
                if value.startswith(label) if prefix else value == label:
                    matches.append(node)
            except self.transient_error:
                continue
        smoke.require(len(matches) <= 1, "Installed accessibility selector is ambiguous")
        return matches[0] if matches else None

    def wait_for(self, predicate, seconds=12):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            smoke.require(smoke.process_status(self.process) is None, "Installed UI exited early")
            try:
                value = predicate()
                if value:
                    return value
            except (LookupError, self.transient_error):
                pass
            time.sleep(0.1)
        raise RuntimeError("Installed accessibility state did not arrive")

    def wait(self, label, *, prefix=False):
        return self.wait_for(lambda: self.find(label, prefix=prefix))

    def focused_name(self):
        found = []
        for node in self.nodes():
            try:
                if node.getState().contains(self.api.STATE_FOCUSED):
                    found.append(node.name or "")
            except self.transient_error:
                continue
        smoke.require(len(found) <= 1, "Installed accessibility focus is ambiguous")
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


def menu_keyboard(ui):
    trigger = "Account menu for "
    ui.focus(trigger, prefix=True)
    ui.key("Return")
    ui.expect_focus("Switch to ", prefix=True)
    ui.wait("Manage accounts")
    ui.key("Escape")
    ui.wait_for(lambda: ui.find("Manage accounts") is None)
    ui.expect_focus(trigger, prefix=True)
    ui.key("space")
    ui.expect_focus("Switch to ", prefix=True)
    # All transitions to Manage accounts must stay among menu choices. Avoid
    # clicking/focusing that target directly, which would hide broken tab order.
    for _ in range(16):
        focused = ui.focused_name()
        if focused == "Manage accounts":
            break
        smoke.require(focused is not None and (focused.startswith("Switch to ")
                      or focused == "Add another Matrix account"), "Account menu tab order escaped")
        ui.key("Tab")
        ui.wait_for(lambda: ui.focused_name() != focused)
    else:
        raise RuntimeError("Account menu target was not keyboard reachable")
    ui.key("Return")
    ui.wait("Manage saved Matrix accounts")
    ui.expect_focus("Done")
    ui.key("Tab")
    ui.expect_focus("Add account")
    ui.key("shift+Tab")
    ui.expect_focus("Done")
    ui.key("Escape")
    ui.wait_for(lambda: ui.find("Manage saved Matrix accounts") is None)
    ui.expect_focus(trigger, prefix=True)


def appearance_keyboard(ui, config):
    ui.focus("Open settings")
    ui.key("Return")
    ui.wait("110% · text throughout the app")
    ui.focus("Increase font scale")
    ui.key("space")
    ui.wait("120% · text throughout the app")
    ui.wait_for(lambda: tomllib.loads(config.read_text())["appearance"]["scale_percent"] == 120)
    valid = config.read_bytes()
    try:
        # A real external TOML edit, picked up by the installed app's watcher.
        config.write_text("schema_version = 1\n[appearance]\nscale_percent = 9999\n")
        ui.wait("Settings file: ", prefix=True)
        ui.wait("120% · text throughout the app")
        smoke.require(config.read_text().endswith("scale_percent = 9999\n"), "Invalid edit was overwritten")
    finally:
        config.write_bytes(valid)
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


def acceptance(executable, report):
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
            ui = AccessibleUI(process, nonce)
            report["phase"] = "sign_in_semantics"
            ui.wait("Homeserver URL")
            ui.wait("Matrix ID or username")
            password = ui.wait("Password")
            smoke.require(password.getRole() == ui.api.ROLE_PASSWORD_TEXT, "Password field lacks protected text semantics")
            report["sign_in_semantics"] = True
            # Enter the account-free preview through its accessible action.
            # This flow starts keyboard checks in the account menu; it does not
            # claim keyboard-only startup or submit a login.
            ui.action("Open interactive preview without signing in")
            report["phase"] = "account_menu_keyboard"
            menu_keyboard(ui)
            report["account_menu_keyboard"] = True
            report["phase"] = "appearance_config"
            appearance_keyboard(ui, config)
            report["appearance_config"] = True
            report["phase"] = "normal_window_close"
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
        finally:
            # Cancel the acceptance alarm before bounded TERM/KILL/reap cleanup.
            signal.alarm(0)
            smoke.stop_process(process)


def main():
    report = {"check": "installed_linux_keyboard_atspi", "status": "failed", "phase": "setup",
              "sign_in_semantics": False, "account_menu_keyboard": False, "appearance_config": False,
              "normal_window_close": False, "app_log_public": False, "screen_reader_acceptance": False}

    def deadline(_signal, _frame):
        raise RuntimeError("Installed UI acceptance deadline")

    try:
        smoke.require(len(sys.argv) == 2, "Expected installed package path")
        signal.signal(signal.SIGALRM, deadline)
        signal.alarm(180)
        acceptance(Path(sys.argv[1]).resolve(), report)
        report.update(status="passed", phase="complete")
    except Exception:
        # Library errors, tree contents, config diagnostics and application logs
        # are never printed. Fixed phases/booleans are enough to target a rerun.
        pass
    finally:
        signal.alarm(0)
        print(json.dumps(report, sort_keys=True), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
