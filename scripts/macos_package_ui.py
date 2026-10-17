#!/usr/bin/env python3
"""Bounded, account-free installed macOS keyboard/accessibility check on CI only.

package_smoke compiles the Swift accessibility bridge, proves the bridge's own
no-prompt permission state, creates one synthetic release profile, proves the
plain installed launch, then runs this driver as a second launch of the same
installed bundle executable. The driver owns the application process as an
unreaped child in its own process group, checks the real window through the
macOS Accessibility API and keyboard events posted directly to that process,
then presses the window's close button and requires exit status zero. It is
accessibility API/keyboard evidence on a fresh hosted runner profile, not
VoiceOver, live-account, signing or visual acceptance.

Containment primitives: macOS has no job object. The application starts with
start_new_session, so it leads its own process group and session; the leader
stays an unreaped child (waitid with WNOWAIT) until every cleanup signal and
the final accessibility action have happened, so its PID and group ID cannot
be recycled under us; group members are read from the kernel's process table
and signalled with killpg; the executable behind the PID is read with
proc_pidpath and compared with the installed bundle executable.
"""

import contextlib
import ctypes
import json
import os
import plistlib
import pwd
import queue
import re
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
else:
    import package_smoke as smoke

CHECK = "installed_macos_keyboard_ax"
# directories 6 joins qualifier, organization and application into the project
# path "io.github.brk275.Morpheus" and places config, data and the account
# store in ~/Library/Application Support/<project> (config.toml at its root)
# and the cache in ~/Library/Caches/<project>. dirs-sys reads HOME first, so the
# driver requires HOME to be the account's real home and never redirects it.
PROJECT_DIR = "io.github.brk275.Morpheus"
LIBRARY_BASES = (Path("Library") / "Application Support", Path("Library") / "Caches")
MARKER = ".morpheus-package-qa"
SEED_CONFIG = "schema_version = 1\n[appearance]\nscale_percent = 110\n"
ACCEPTANCE_SECONDS = 180
BRIDGE_REPLY_SECONDS = 20
BRIDGE_EXIT_SECONDS = 5
OWNERSHIP_SECONDS = 5
WAIT_SECONDS = 12
BRIDGE_LINE_BOUND = 4 * 1024 * 1024
BRIDGE_QUEUE_BOUND = 4
TREE_BOUND = 5000
NAME_BOUND = 256
WINDOW_BOUND = 16
KEYS = frozenset({"Return", "Space", "Tab", "ShiftTab", "Escape"})
SETTINGS_TAB_BOUND = 160
PROC_PIDPATHINFO_MAXSIZE = 4096
OVERLONG = object()
TEXT_FIELD_ROLE = "AXTextField"
SECURE_SUBROLE = "AXSecureTextField"


class DiagnosticFailure(RuntimeError):
    def __init__(self, code):
        self.code = code
        super().__init__("Installed macOS UI assertion failed")


def require_ui(condition, code):
    if not condition:
        raise DiagnosticFailure(code)


class Diagnostics:
    """Only fixed tokens/booleans/bounded counts; never names, values, paths, PIDs or errors."""
    def __init__(self, report):
        self.value = report["diagnostic"] = {
            "step": "setup", "operation": "none", "failure": "none", "bridge_error": "none",
            "permission_checked": False, "owned_window_seen": False, "ax_root_seen": False,
            "homeserver_seen": False, "username_seen": False, "password_seen": False,
            "selector_matches": 0, "incomplete_passes": 0, "activation_recoveries": 0,
            "bounded_snapshots": 0, "password_semantics": "unchecked",
            "traversed_node_matched": False, "cleanup": "not_started", "profile_owned": False,
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
            self.value["bridge_error"] = code if code in smoke.MACOS_BRIDGE_ERRORS else "bridge_error"

    def check_deadline(self):
        require_ui(time.monotonic() < self.deadline, "deadline")

    def failed(self, error):
        if self.value["failure"] != "none":
            return
        if isinstance(error, DiagnosticFailure):
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
        self.value["failure"] = code if code in smoke.MACOS_KEYBOARD_FAILURES else "unclassified"


class IncompleteTraversal(LookupError):
    """A snapshot skipped or lost an owned node; its result is not evidence."""


class BridgeFailure(RuntimeError):
    """The bridge answered with a fixed error token (kept in diagnostics only)."""
    def __init__(self, code):
        self.code = code
        super().__init__("Accessibility bridge command failed")


def guard(environment=None, platform=None):
    environment = os.environ if environment is None else environment
    require_ui((sys.platform if platform is None else platform) == "darwin"
               and environment.get("GITHUB_ACTIONS") == "true"
               and environment.get("RUNNER_ENVIRONMENT") == "github-hosted"
               and environment.get("RUNNER_OS") == "macOS"
               and environment.get("ImageOS") == "macos15", "guard")


def home_directory(environment=None, account=None):
    """The account's real home: HOME must name it, because the app reads HOME first."""
    environment = os.environ if environment is None else environment
    account = pwd.getpwuid(os.getuid()) if account is None else account
    home = Path(account.pw_dir)
    smoke.require(home.is_absolute() and environment.get("HOME") == str(home),
                  "HOME must be the runner account's real home directory")
    smoke.require(plain_directory(home), "Runner home directory is not a plain directory")
    return home


def symlink(path):
    try:
        os.lstat(path)
    except FileNotFoundError:
        return False
    return path.is_symlink()


def plain_directory(path):
    return path.is_dir() and not symlink(path)


def profile_roots(home=None):
    home = home_directory() if home is None else home
    return [home / base / PROJECT_DIR for base in LIBRARY_BASES]


class PackageProfile:
    """The one synthetic release profile of a package smoke run.

    The smoke creates it before the first installed launch, both launches share
    it, and the smoke removes it only after both launches verifiably stopped. A
    marker file carrying the run nonce in each root proves the roots are this
    run's; any other existing profile material is refused and never read,
    inspected or deleted. The Library bases are the account's real folders and
    are never created or removed here.
    """
    def __init__(self, roots, nonce):
        self.roots, self.nonce = list(roots), nonce
        self.config = self.roots[0] / "config.toml"

    @staticmethod
    def valid_nonce(nonce):
        return type(nonce) is str and re.fullmatch(r"[0-9a-f]{32}", nonce) is not None

    @classmethod
    def create(cls, nonce=None, roots=None):
        nonce = secrets.token_hex(16) if nonce is None else nonce
        smoke.require(cls.valid_nonce(nonce), "Invalid package smoke profile nonce")
        roots = profile_roots() if roots is None else list(roots)
        for root in roots:
            smoke.require(plain_directory(root.parent), "Runner Library folder is missing or redirected")
            # Anything at the project path, including a symbolic link, may hold
            # or redirect to real profile or vault material: refuse, never inspect.
            smoke.require(not symlink(root) and not root.exists(),
                          "Refusing an existing application profile on this runner")
        profile = cls(roots, nonce)
        created = []
        try:
            for root in roots:
                root.mkdir()
                created.append(root)
                smoke.require(plain_directory(root), "Profile root was redirected")
                (root / MARKER).write_text(nonce, encoding="ascii")
            profile.config.write_text(SEED_CONFIG, encoding="utf-8")
        except BaseException:
            # Only what this call created, verified absent moments ago.
            for path in reversed(created):
                if path.exists() and not symlink(path):
                    shutil.rmtree(path, ignore_errors=True)
            raise
        return profile

    @classmethod
    def open(cls, nonce, roots=None):
        require_ui(cls.valid_nonce(nonce), "profile_invalid")
        roots = profile_roots() if roots is None else list(roots)
        profile = cls(roots, nonce)
        for root in roots:
            require_ui(plain_directory(root.parent) and plain_directory(root), "profile_invalid")
            try:
                marker = (root / MARKER).read_text(encoding="ascii")
            except (OSError, UnicodeError):
                raise DiagnosticFailure("profile_invalid") from None
            require_ui(marker == nonce, "profile_invalid")
        require_ui(profile.config.is_file() and not symlink(profile.config), "profile_invalid")
        return profile

    def remove(self):
        for root in self.roots:
            smoke.require(not symlink(root) and plain_directory(root.parent),
                          "Refusing to remove a redirected profile root")
            if root.exists():
                try:
                    marker = (root / MARKER).read_text(encoding="ascii")
                except (OSError, UnicodeError):
                    marker = None
                smoke.require(marker == self.nonce, "Refusing to remove a profile this run did not create")
                shutil.rmtree(root)
        return all(not root.exists() and not symlink(root) for root in self.roots)


def environment_for(nonce):
    env = os.environ.copy()
    env.pop("MORPHEUS_QA_PROFILE_DIR", None)
    env[smoke.NONCE_ENV] = nonce
    return env


def installed_bundle(executable):
    """The executable must be the bundle executable of the installed Morpheus app copy."""
    contents = executable.parent.parent
    require_ui(executable.name == smoke.BINARY and executable.parent.name == "MacOS"
               and contents.name == "Contents" and contents.parent.suffix == ".app"
               and executable.is_file() and not symlink(executable), "install_state")
    try:
        with (contents / "Info.plist").open("rb") as file:
            info = plistlib.load(file)
    except (OSError, ValueError, plistlib.InvalidFileException):
        raise DiagnosticFailure("install_state") from None
    require_ui(info.get("CFBundleIdentifier") == smoke.IDENTIFIER and info.get("CFBundleExecutable") == smoke.BINARY,
               "install_state")
    return smoke.IDENTIFIER


def libproc():
    library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    # int proc_pidpath(int pid, void *buffer, uint32_t buffersize); returns bytes written or 0.
    library.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    library.proc_pidpath.restype = ctypes.c_int
    return library


def executable_of(pid, library=None):
    library = libproc() if library is None else library
    buffer = ctypes.create_string_buffer(PROC_PIDPATHINFO_MAXSIZE)
    length = library.proc_pidpath(pid, buffer, PROC_PIDPATHINFO_MAXSIZE)
    if length <= 0 or length >= PROC_PIDPATHINFO_MAXSIZE:
        return None
    try:
        return Path(buffer.raw[:length].decode("utf-8"))
    except UnicodeError:
        return None


def process_table(snapshot=None):
    """(pid, pgid, sess, state) rows of the live process table."""
    if snapshot is None:
        snapshot = subprocess.run(["ps", "-axo", "pid=,pgid=,sess=,stat="], check=True, capture_output=True,
                                  text=True, timeout=5).stdout
    rows = []
    for line in snapshot.splitlines():
        parts = line.split()
        if len(parts) < 4 or not all(part.isdigit() for part in parts[:3]):
            continue
        rows.append((int(parts[0]), int(parts[1]), int(parts[2]), parts[3]))
    return rows


def group_members(leader, table=None):
    """Live (non-zombie) members of the leader's process group, including the leader."""
    table = process_table() if table is None else table
    return [row for row in table if row[1] == leader and not row[3].startswith("Z")]


def owned_mac_pid(pid, leader, executable, table=None, library=None):
    """pid leads its own group and session as our unreaped child and runs the installed executable."""
    rows = [row for row in (process_table() if table is None else table) if row[0] == pid]
    if len(rows) != 1:
        return False
    _, pgid, sess, state = rows[0]
    if pgid != leader or sess != leader or state.startswith("Z"):
        return False
    path = executable_of(pid, library)
    return path is not None and path == Path(executable)


def bridge_command(binary):
    binary = Path(binary)
    require_ui(binary.is_absolute() and binary.is_file() and not symlink(binary)
               and os.access(binary, os.X_OK), "bridge_invalid")
    return [str(binary)]


class Bridge:
    """One Swift accessibility bridge, an unreaped child in its own process group.

    Its whole stream stays private; the driver reads bounded lines through a
    bounded queue and accepts only JSON objects. close() asks it to exit and
    otherwise signals its whole group; a stop that cannot be verified raises.
    """
    def __init__(self, command, diagnostics, environment=None, log=None):
        self.diagnostics = diagnostics
        self.process = None
        self.reader = None
        self.replies = queue.Queue(maxsize=BRIDGE_QUEUE_BOUND)
        self.stdin = self.stdout = None
        try:
            self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                            stderr=subprocess.DEVNULL if log is None else log,
                                            env=os.environ.copy() if environment is None else environment,
                                            start_new_session=True)
            self.stdin, self.stdout = self.process.stdin, self.process.stdout
            self.reader = threading.Thread(target=self.pump, daemon=True)
            self.reader.start()
            ready = self.reply()
            require_ui(ready.get("ready") is True, "bridge_invalid")
        except BaseException:
            self.close()
            raise

    def alive(self):
        return self.process is not None and smoke.process_status(self.process) is None

    def pump(self):
        try:
            while True:
                # Bounded before allocation: at most one byte past the bound is
                # read, and an unterminated or overlong line ends the stream.
                line = self.stdout.readline(BRIDGE_LINE_BOUND + 1)
                if not line:
                    break
                if len(line) > BRIDGE_LINE_BOUND or not line.endswith(b"\n"):
                    self.replies.put(OVERLONG)
                    break
                self.replies.put(line)
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
        require_ui(line is not OVERLONG, "bridge_overflow")
        try:
            value = json.loads(line.decode("utf-8"))
        except (ValueError, UnicodeError):
            raise DiagnosticFailure("bridge_invalid") from None
        require_ui(type(value) is dict and type(value.get("ok")) is bool, "bridge_invalid")
        return value

    def transmit(self, command):
        require_ui(self.alive(), "bridge_exited")
        try:
            self.stdin.write((json.dumps(command, separators=(",", ":")) + "\n").encode("utf-8"))
            self.stdin.flush()
        except (OSError, ValueError):
            raise DiagnosticFailure("bridge_exited") from None

    def call(self, command, allow_error=False):
        self.diagnostics.check_deadline()
        self.transmit(command)
        value = self.reply()
        if not value["ok"]:
            code = value.get("error")
            code = code if code in smoke.MACOS_BRIDGE_ERRORS else "bridge_error"
            self.diagnostics.bridge_error(code)
            if allow_error:
                raise BridgeFailure(code)
            raise DiagnosticFailure("bridge_invalid")
        return value

    def close(self):
        """Ask the bridge to exit, then stop and reap its whole group; an unverified stop raises."""
        try:
            if self.process is not None:
                if self.alive():
                    with contextlib.suppress(OSError, ValueError):
                        self.stdin.write(b'{"op":"exit"}\n')
                        self.stdin.flush()
                    deadline = time.monotonic() + BRIDGE_EXIT_SECONDS
                    while smoke.process_status(self.process) is None and time.monotonic() < deadline:
                        time.sleep(0.05)
                # Signals the group, waits, then reaps the leader last.
                smoke.stop_process(self.process)
        finally:
            with contextlib.suppress(OSError, ValueError):
                if self.stdin is not None:
                    self.stdin.close()
            # The reader owns the read pipe while it blocks in readline; the
            # child's exit ends that read, so join before closing.
            if self.reader is not None:
                self.reader.join(timeout=1)
            if (self.reader is None or not self.reader.is_alive()) and self.stdout is not None:
                with contextlib.suppress(OSError, ValueError):
                    self.stdout.close()


def check_permissions(bridge, diagnostics):
    """The bridge's own read-only permission state; no prompt is ever requested."""
    diagnostics.operation("permissions")
    value = bridge.call({"op": "permissions"})
    require_ui(type(value.get("accessibility")) is bool and type(value.get("post_event")) is bool, "bridge_invalid")
    diagnostics.value["permission_checked"] = True
    require_ui(value["accessibility"] and value["post_event"], "permission_unavailable")


class Node:
    __slots__ = ("id", "parent", "pid", "name", "role", "subrole", "focused", "error", "truncated", "name_source")

    def __init__(self, raw):
        require_ui(type(raw) is dict and type(raw.get("id")) is int and type(raw.get("parent")) is int
                   and type(raw.get("pid")) is int and type(raw.get("name")) is str
                   and len(raw["name"]) <= NAME_BOUND
                   and all(type(raw.get(key)) is str for key in ("role", "subrole", "name_source"))
                   and raw["name_source"] in {"title", "value", "none"}
                   and all(type(raw.get(key)) is bool for key in ("focused", "error", "truncated")),
                   "bridge_invalid")
        # A value may name only a static text; any other role with a value-sourced
        # name means the bridge read something it must never read.
        require_ui(raw["name_source"] != "value" or raw["role"] == "AXStaticText", "bridge_invalid")
        for key in self.__slots__:
            setattr(self, key, raw[key])

    @property
    def text_field(self):
        return self.role == TEXT_FIELD_ROLE

    @property
    def protected(self):
        return self.text_field and self.subrole == SECURE_SUBROLE


class MacUI:
    """Selectors over bridge snapshots with the Linux driver's evidence rules."""
    def __init__(self, bridge, pid, process, diagnostics):
        self.bridge, self.pid, self.process, self.diagnostics = bridge, pid, process, diagnostics
        self.pass_incomplete = False
        self.focus_truncated = False

    def alive(self):
        self.diagnostics.operation("process_status")
        return smoke.process_status(self.process) is None

    def nodes(self):
        self.pass_incomplete = False
        self.diagnostics.operation("snapshot")
        value = self.bridge.call({"op": "snapshot"})
        raw = value.get("nodes")
        require_ui(type(raw) is list and type(value.get("bounded")) is bool, "bridge_invalid")
        if value["bounded"]:
            self.diagnostics.count("bounded_snapshots")
        # A bounded walk saw an unknown remainder: it is not a tree.
        require_ui(len(raw) <= TREE_BOUND and not value["bounded"], "tree_bound")
        nodes = [Node(item) for item in raw]
        require_ui(all(node.id == index for index, node in enumerate(nodes)), "bridge_invalid")
        require_ui(bool(nodes) and nodes[0].parent == -1, "bridge_invalid")
        self.diagnostics.operation("node_owner")
        require_ui(nodes[0].pid == self.pid and not nodes[0].error, "bridge_invalid")
        self.diagnostics.value["ax_root_seen"] = True
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
        smoke.require(0 < len(label) < NAME_BOUND, "Acceptance label exceeds the name bound")
        matches = []
        self.diagnostics.value["selector_matches"] = 0
        for node in self.nodes():
            self.diagnostics.operation("node_name")
            # A cut name can still carry a prefix and cannot hide a second match
            # because the cut keeps the first 256 characters, but it never proves
            # a complete exact label.
            matched = node.name.startswith(label) if prefix else (node.name == label and not node.truncated)
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
        require_ui(all(type(value.get(key)) is bool for key in ("present", "owned", "truncated"))
                   and type(value.get("name")) is str and len(value["name"]) <= NAME_BOUND, "bridge_invalid")
        if not value["present"]:
            self.focus_truncated = False
            return None
        # Focus elsewhere is never treated as our focus state and no key is sent toward it.
        require_ui(value["owned"], "focus_unowned")
        self.focus_truncated = value["truncated"]
        return value["name"]

    def expect_focus(self, name, *, prefix=False):
        smoke.require(0 < len(name) < NAME_BOUND, "Acceptance label exceeds the name bound")
        return self.wait_for(lambda: (focused := self.focused_name()) is not None
                             and (focused.startswith(name) if prefix else (focused == name and not self.focus_truncated)))

    def focus(self, label, *, prefix=False):
        node = self.wait(label, prefix=prefix)
        self.diagnostics.operation("focus")
        # The bridge rechecks ownership of the live element before setting focus.
        self.bridge.call({"op": "focus", "id": node.id})
        self.expect_focus(label, prefix=prefix)

    def key(self, key):
        smoke.require(key in KEYS, "Unsupported acceptance key")
        # Only an owned focused element may receive input, and the bridge
        # requires the owned process to be frontmost immediately before every
        # native post, including releases, each posted to that process only.
        self.diagnostics.operation("focused_element")
        require_ui(self.focused_name() is not None, "focus_unowned")
        self.diagnostics.operation("key_input")
        try:
            value = self.bridge.call({"op": "key", "key": key}, allow_error=True)
        except BridgeFailure as error:
            if error.code in {"foreground_unowned", "window_gone"}:
                raise DiagnosticFailure("foreground_unowned") from None
            if error.code == "input_rejected":
                raise DiagnosticFailure("input_rejected") from None
            raise DiagnosticFailure("bridge_invalid") from None
        expected = 4 if key == "ShiftTab" else 2
        require_ui(value.get("posted") == expected, "input_rejected")
        if value.get("recovered") is True:
            self.diagnostics.count("activation_recoveries")


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
    # Only role and subrole are read; no field value is ever requested.
    semantics = ("protected" if password.protected else "unprotected") if password.text_field else "not_text_field"
    diagnostics.value["password_semantics"] = semantics
    require_ui(semantics == "protected", "password_semantics")
    require_ui(homeserver.text_field and username.text_field
               and not homeserver.protected and not username.protected, "password_semantics")
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


def owned_window(bridge, process, diagnostics, seconds=30):
    deadline = time.monotonic() + seconds
    while True:
        diagnostics.check_deadline()
        diagnostics.operation("process_status")
        require_ui(smoke.process_status(process) is None, "app_exited")
        diagnostics.operation("window_enumeration")
        value = bridge.call({"op": "windows"})
        count = value.get("count")
        require_ui(type(count) is int and 0 <= count <= WINDOW_BOUND, "bridge_invalid")
        if count:
            require_ui(count == 1, "window_count")
            diagnostics.value["owned_window_seen"] = True
            return
        require_ui(time.monotonic() < deadline, "wait_timeout")
        time.sleep(0.25)


def normal_close(bridge, process, diagnostics, executable, table=None, after_table=None, library=None, seconds=15):
    """Press the owned window's close button, then require exit status zero and an empty live group.

    The leader stays unreaped throughout, so the status is read with WNOWAIT and
    the group ID still names this launch when its members are counted. `table`
    is the process table before the close, `after_table` the one after exit;
    both default to live reads.
    """
    diagnostics.operation("ownership")
    require_ui(owned_mac_pid(process.pid, process.pid, executable, table, library), "ownership_lost")
    diagnostics.operation("close_request")
    try:
        bridge.call({"op": "close"}, allow_error=True)
    except BridgeFailure as error:
        raise DiagnosticFailure("window_count" if error.code == "window_count" else "bridge_invalid") from None
    diagnostics.operation("process_wait")
    deadline = time.monotonic() + seconds
    while (status := smoke.process_status(process)) is None and time.monotonic() < deadline:
        time.sleep(0.1)
    require_ui(status == 0, "exit_status")
    diagnostics.operation("group_query")
    require_ui(not group_members(process.pid, after_table), "process_remaining")


def acceptance(executable, nonce, bridge_binary, report, diagnostics=None):
    diagnostics = diagnostics or Diagnostics(report)
    guard()
    smoke.require_nonreaping_wait()
    diagnostics.operation("bundle")
    bundle = installed_bundle(executable)
    diagnostics.step("profile")
    diagnostics.operation("profile_open")
    # The smoke created this profile before the plain launch and removes it
    # after both launches verifiably stopped; the driver only proves ownership.
    profile = PackageProfile.open(nonce)
    diagnostics.value["profile_owned"] = True
    bridge = None
    process = None
    try:
        with tempfile.TemporaryFile() as bridge_log, tempfile.TemporaryFile() as app_log:
            diagnostics.step("bridge")
            diagnostics.operation("bridge_start")
            bridge = Bridge(bridge_command(bridge_binary), diagnostics, log=bridge_log)
            diagnostics.step("permission")
            # Permission state is proven before the application starts.
            check_permissions(bridge, diagnostics)
            diagnostics.step("launch")
            diagnostics.operation("process_create")
            process = subprocess.Popen([str(executable)], env=environment_for(nonce), stdin=subprocess.DEVNULL,
                                       stdout=app_log, stderr=subprocess.STDOUT, start_new_session=True)
            diagnostics.operation("ownership")
            deadline = time.monotonic() + OWNERSHIP_SECONDS
            while not owned_mac_pid(process.pid, process.pid, executable) and time.monotonic() < deadline:
                require_ui(smoke.process_status(process) is None, "app_exited")
                time.sleep(0.1)
            require_ui(owned_mac_pid(process.pid, process.pid, executable), "install_state")
            diagnostics.operation("attach")
            bridge.call({"op": "attach", "pid": process.pid, "executable": str(executable), "bundle": bundle})
            diagnostics.step("window")
            owned_window(bridge, process, diagnostics)
            ui = MacUI(bridge, process.pid, process, diagnostics)
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
            config_persisted(profile.config, diagnostics)
            report["appearance_config"] = True
            report["phase"] = "normal_window_close"
            diagnostics.step("normal_close")
            normal_close(bridge, process, diagnostics, executable)
            report["normal_window_close"] = True
    except Exception as error:
        diagnostics.failed(error)
        raise
    finally:
        # Preserve the first failure position if bounded cleanup also fails.
        # The profile is never touched here: the smoke removes it only after
        # this process reported a verified stop (cleanup passed). The app's
        # leader is signalled with its group and reaped last.
        failure = None
        try:
            if bridge is not None:
                bridge.close()
        except Exception as error:  # noqa: BLE001 - every cleanup step must run
            failure = error
        try:
            if process is not None:
                smoke.stop_process(process)
        except Exception as error:  # noqa: BLE001
            failure = failure or error
        if failure is None:
            diagnostics.value["cleanup"] = "passed"
        else:
            diagnostics.value["cleanup"] = "failed"
            if diagnostics.value["failure"] == "none":
                diagnostics.step("cleanup")
                diagnostics.operation("cleanup")
                diagnostics.failed(failure)
            raise failure


def probe_permissions(bridge_binary, report=None):
    """Smoke-side check of the bridge's permission state before any profile or launch."""
    diagnostics = Diagnostics(report if report is not None else {})
    bridge = Bridge(bridge_command(bridge_binary), diagnostics)
    try:
        check_permissions(bridge, diagnostics)
    finally:
        bridge.close()


def main():
    report = {"check": CHECK, "status": "failed", "phase": "setup",
              "sign_in_semantics": False, "account_menu_keyboard": False, "appearance_config": False,
              "normal_window_close": False, "app_log_public": False, "screen_reader_acceptance": False}
    diagnostics = Diagnostics(report)
    try:
        smoke.require(len(sys.argv) == 4, "Expected installed executable path, profile nonce and bridge path")
        acceptance(Path(sys.argv[1]).resolve(), sys.argv[2], Path(sys.argv[3]).resolve(), report, diagnostics)
        report.update(status="passed", phase="complete")
        diagnostics.step("complete")
    except Exception as error:  # noqa: BLE001 - only fixed tokens leave this process
        diagnostics.failed(error)
    finally:
        print(json.dumps(report, sort_keys=True), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
