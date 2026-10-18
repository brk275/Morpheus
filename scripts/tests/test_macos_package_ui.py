import contextlib
import io
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import tempfile
import types
import unittest
from unittest.mock import Mock, patch

from scripts import ci_scope, package_smoke as smoke
from scripts import macos_package_ui as ui

HOSTED = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "macOS",
          "ImageOS": "macos15"}
SWIFT = Path(__file__).resolve().parents[1] / "macos_package_ui.swift"
SOURCE = Path(__file__).resolve().parents[1] / "macos_package_ui.py"
NONCE = "0123456789abcdef0123456789abcdef"
PID = 4242


def diagnostic(**overrides):
    return {"step": "complete", "operation": "none", "failure": "none", "bridge_error": "none",
            "appearance_stage": "complete",
            "permission_checked": True, "owned_window_seen": True, "ax_root_seen": True,
            "homeserver_seen": True, "username_seen": True, "password_seen": True,
            "selector_matches": 1, "incomplete_passes": 0, "activation_recoveries": 0,
            "bounded_snapshots": 0, "password_semantics": "protected",
            "traversed_node_matched": False, "cleanup": "passed", "profile_owned": True, **overrides}


def report(**overrides):
    return {"check": "installed_macos_keyboard_ax", "status": "passed", "phase": "complete",
            "sign_in_semantics": True, "account_menu_keyboard": True, "appearance_config": True,
            "normal_window_close": True, "app_log_public": False, "screen_reader_acceptance": False,
            "diagnostic": diagnostic(), **overrides}


def node(id, name="", *, parent=0, pid=PID, role="AXGroup", subrole="", focused=False, error=False,
         truncated=False, name_source="title"):
    return {"id": id, "parent": parent, "pid": pid, "name": name, "role": role, "subrole": subrole,
            "focused": focused, "error": error, "truncated": truncated,
            "name_source": name_source if name else "none"}


def snapshot(*nodes, bounded=False):
    return {"ok": True, "nodes": [node(0, pid=PID, role="AXApplication", parent=-1), *nodes], "bounded": bounded}


def focused_reply(name, owned=True, present=True, truncated=False):
    return {"ok": True, "present": present, "owned": owned, "name": name, "truncated": truncated}


def fake_bridge(diagnostics, snapshots=(), focused=(), key_replies=(), windows=(), permissions=None, close=None,
                attach=()):
    """The real Bridge with only its transport replaced: call() keeps the
    production reply validation and error handling, replies are scripted."""
    bridge = object.__new__(ui.Bridge)
    bridge.process, bridge.reader, bridge.diagnostics = None, None, diagnostics
    bridge.stdin = bridge.stdout = None
    bridge.commands = []
    bridge.scripted = {"snapshot": list(snapshots), "key": list(key_replies), "windows": list(windows),
                       "attach": list(attach)}
    bridge.focused_script = focused
    bridge.focused_calls = 0
    bridge.permissions_reply = permissions or {"ok": True, "accessibility": True, "post_event": True}
    bridge.close_reply = close or {"ok": True}
    bridge.pending = None

    def transmit(command):
        bridge.commands.append(command)
        bridge.pending = command

    def reply(deadline=None):
        op = bridge.pending["op"]
        if op == "focused":
            if callable(bridge.focused_script):
                value = bridge.focused_script(bridge.focused_calls)
            else:
                value = bridge.focused_script[bridge.focused_calls]
            bridge.focused_calls += 1
            return value
        if op == "attach" and bridge.scripted["attach"]:
            return bridge.scripted["attach"].pop(0)
        if op in ("focus", "attach"):
            return {"ok": True}
        if op == "permissions":
            return bridge.permissions_reply
        if op == "close":
            return bridge.close_reply
        if op == "key" and not bridge.scripted["key"]:
            return {"ok": True, "recovered": False, "posted": 4 if bridge.pending["key"] == "ShiftTab" else 2}
        return bridge.scripted[op].pop(0)

    bridge.alive = lambda: True
    bridge.transmit = transmit
    bridge.reply = reply
    return bridge


def driver(snapshots=(), focused=(), key_replies=(), alive=True, **extra):
    diagnostics = ui.Diagnostics({})
    bridge = fake_bridge(diagnostics, snapshots, focused, key_replies, **extra)
    instance = ui.MacUI(bridge, PID, Mock(pid=PID, returncode=None), diagnostics)
    instance.alive = lambda: alive
    return instance, bridge, diagnostics


def temp_home(base):
    """A mocked account home with its real Library bases, but no project folders."""
    home = Path(base) / "Users" / "runner"
    for library in ui.LIBRARY_BASES:
        (home / library).mkdir(parents=True, exist_ok=True)
    return home


def bundle(base, name="Morpheus.app"):
    """A synthetic installed bundle with the executable path ancestry the driver requires."""
    app = Path(base) / "Applications" / name
    macos = app / "Contents" / "MacOS"
    macos.mkdir(parents=True)
    executable = macos / smoke.BINARY
    executable.write_bytes(b"synthetic")
    with (app / "Contents" / "Info.plist").open("wb") as file:
        plistlib.dump({"CFBundleIdentifier": smoke.IDENTIFIER, "CFBundleExecutable": smoke.BINARY}, file)
    return executable


def table(*rows):
    return [tuple(row) for row in rows]


class GuardAndIdentityTests(unittest.TestCase):
    def test_guard_requires_fresh_hosted_macos_15(self):
        ui.guard(HOSTED, "darwin")
        for missing in HOSTED:
            with self.subTest(missing=missing), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.guard({key: value for key, value in HOSTED.items() if key != missing}, "darwin")
            self.assertEqual(error.exception.code, "guard")
        with self.assertRaises(ui.DiagnosticFailure):
            ui.guard(dict(HOSTED, ImageOS="macos14"), "darwin")
        with self.assertRaises(ui.DiagnosticFailure):
            ui.guard(HOSTED, "linux")

    def test_guard_failure_happens_before_profile_bridge_or_process_work(self):
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}, clear=False), \
                patch.object(ui.sys, "platform", "darwin"), \
                patch.object(ui, "profile_roots") as roots, patch.object(ui, "Bridge") as bridge, \
                patch.object(ui.subprocess, "Popen") as popen:
            with self.assertRaises(ui.DiagnosticFailure):
                ui.acceptance(Path("/synthetic/Morpheus.app/Contents/MacOS/morpheus-app"), NONCE, Path("/b"), {})
        roots.assert_not_called()
        bridge.assert_not_called()
        popen.assert_not_called()

    def test_home_must_be_the_accounts_real_home_and_never_an_override(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            account = types.SimpleNamespace(pw_dir=str(home))
            self.assertEqual(ui.home_directory({"HOME": str(home)}, account), home)
            for environment in ({}, {"HOME": str(Path(folder) / "elsewhere")}, {"HOME": ""}):
                with self.subTest(environment=environment), self.assertRaises(RuntimeError):
                    ui.home_directory(environment, account)
            link = Path(folder) / "linked-home"
            os.symlink(home, link, target_is_directory=True)
            with self.assertRaises(RuntimeError):
                ui.home_directory({"HOME": str(link)}, types.SimpleNamespace(pw_dir=str(link)))
            roots = ui.profile_roots(home)
            self.assertEqual(roots, [home / "Library" / "Application Support" / "io.github.brk275.Morpheus",
                                     home / "Library" / "Caches" / "io.github.brk275.Morpheus"])
        source = SOURCE.read_text(encoding="utf-8")
        for forbidden in ["MORPHEUS_QA_PROFILE_DIR\"]", "XDG_", "expanduser", "os.environ[\"HOME\"] ="]:
            self.assertNotIn(forbidden, source)

    def test_installed_bundle_requires_the_real_path_ancestry_and_identity(self):
        with tempfile.TemporaryDirectory() as folder:
            executable = bundle(folder)
            self.assertEqual(ui.installed_bundle(executable), smoke.IDENTIFIER)
            loose = Path(folder) / "loose" / smoke.BINARY
            loose.parent.mkdir()
            loose.write_bytes(b"x")
            with self.assertRaises(ui.DiagnosticFailure) as error:
                ui.installed_bundle(loose)
            self.assertEqual(error.exception.code, "install_state")
            other = bundle(Path(folder) / "other", "Other.app")
            with (other.parent.parent / "Info.plist").open("wb") as file:
                plistlib.dump({"CFBundleIdentifier": "org.example.other", "CFBundleExecutable": smoke.BINARY}, file)
            with self.assertRaises(ui.DiagnosticFailure):
                ui.installed_bundle(other)
            link = Path(folder) / "Applications" / "Link.app"
            os.symlink(executable.parent.parent.parent, link, target_is_directory=True)
            linked = link / "Contents" / "MacOS" / smoke.BINARY
            self.assertEqual(ui.installed_bundle(linked), smoke.IDENTIFIER)  # The executable itself is a file.
            os.unlink(executable)
            os.symlink(Path(folder) / "loose" / smoke.BINARY, executable)
            with self.assertRaises(ui.DiagnosticFailure):
                ui.installed_bundle(executable)

    def test_manual_only_helpers_do_not_trigger_the_native_matrix(self):
        result = ci_scope.classify(["scripts/macos_package_ui.py", "scripts/macos_package_ui.swift",
                                    "scripts/tests/test_macos_package_ui.py", "scripts/package_smoke.py",
                                    "docs/PACKAGING.md"])
        self.assertFalse(result["native"])


class OwnershipTests(unittest.TestCase):
    class Proc:
        """libproc fake: proc_pidpath copies a known path or reports nothing."""
        def __init__(self, paths):
            self.paths = paths
            self.calls = []

        def proc_pidpath(self, pid, buffer, size):
            self.calls.append((pid, size))
            path = self.paths.get(pid)
            if path is None:
                return 0
            data = path.encode("utf-8")
            if len(data) >= size:
                return 0
            buffer.raw = data + b"\0" * (len(buffer.raw) - len(data))
            return len(data)

    def test_executable_of_reads_libproc_with_a_bounded_buffer(self):
        proc = self.Proc({PID: "/Applications/Morpheus.app/Contents/MacOS/morpheus-app"})
        self.assertEqual(ui.executable_of(PID, proc), Path("/Applications/Morpheus.app/Contents/MacOS/morpheus-app"))
        self.assertEqual(proc.calls, [(PID, ui.PROC_PIDPATHINFO_MAXSIZE)])
        self.assertIsNone(ui.executable_of(99, proc))
        self.assertIsNone(ui.executable_of(PID, self.Proc({PID: "x" * ui.PROC_PIDPATHINFO_MAXSIZE})))

    def test_owned_pid_requires_own_group_and_session_live_state_and_the_installed_executable(self):
        executable = "/Applications/Morpheus.app/Contents/MacOS/morpheus-app"
        proc = self.Proc({PID: executable, 5000: executable, 6000: "/usr/bin/true"})
        own_session = lambda pid: PID
        owned = table((PID, PID, "S+"))
        self.assertTrue(ui.owned_mac_pid(PID, PID, executable, owned, proc, own_session))
        cases = {
            "other_group": (table((PID, 1, "S")), own_session),
            "other_session": (owned, lambda pid: 1),
            "session_unreadable": (owned, lambda pid: None),
            "zombie": (table((PID, PID, "Z")), own_session),
            "missing": (table((5000, 5000, "S")), own_session),
            "duplicate_rows": (table((PID, PID, "S"), (PID, PID, "S")), own_session),
        }
        for name, (rows, session) in cases.items():
            with self.subTest(case=name):
                self.assertFalse(ui.owned_mac_pid(PID, PID, executable, rows, proc, session))
        # Same group and session, but a different executable behind the PID.
        self.assertFalse(ui.owned_mac_pid(6000, 6000, executable, table((6000, 6000, "S")), proc, lambda pid: 6000))
        self.assertFalse(ui.owned_mac_pid(7000, 7000, executable, table((7000, 7000, "S")), proc, lambda pid: 7000))

    def test_session_comes_from_getsid_and_never_from_the_darwin_process_table(self):
        # Darwin ps prints the session structure's kernel address under "sess"
        # (adv_cmds ps/keyword.c), which never equals the leader's PID. Such a
        # snapshot must neither parse as a session nor pass as a malformed row.
        darwin = " 4242 4242 ffffff801a2b3c4d S+\n 4243 4242 ffffff801a2b3c4d Z\n"
        with self.assertRaises(ValueError):
            ui.process_table(darwin)
        with self.assertRaises(ValueError):
            ui.process_table(" 4242 4242 4242 S+\n")  # No fourth column is ever accepted.
        executable = "/Applications/Morpheus.app/Contents/MacOS/morpheus-app"
        proc = self.Proc({PID: executable})
        with patch.object(ui.os, "getsid", return_value=PID) as getsid:
            self.assertEqual(ui.session_of(PID), PID)
            self.assertTrue(ui.owned_mac_pid(PID, PID, executable, table((PID, PID, "S")), proc))
        self.assertEqual([entry.args for entry in getsid.call_args_list], [(PID,), (PID,)])
        for error in (ProcessLookupError(), PermissionError()):
            with self.subTest(error=type(error).__name__), patch.object(ui.os, "getsid", side_effect=error):
                self.assertIsNone(ui.session_of(PID))
                self.assertFalse(ui.owned_mac_pid(PID, PID, executable, table((PID, PID, "S")), proc))
        with patch.object(ui.os, "getsid", return_value=PID + 1):
            self.assertFalse(ui.owned_mac_pid(PID, PID, executable, table((PID, PID, "S")), proc))

    def test_process_table_rejects_incomplete_snapshots_and_group_members_exclude_zombies(self):
        rows = ui.process_table(" 4242 4242 S+\n\n 4243 4242 Z\n   \n 1 1 Ss\n 4244 4242 R\n")
        self.assertEqual(rows, [(4242, 4242, "S+"), (4243, 4242, "Z"), (1, 1, "Ss"), (4244, 4242, "R")])
        self.assertEqual(ui.group_members(4242, rows), [(4242, 4242, "S+"), (4244, 4242, "R")])
        self.assertEqual(ui.group_members(4242, table((4242, 4242, "Z"))), [])
        # A malformed non-blank row anywhere, an old four-column sess row, or no
        # row at all means an unparsed process may be alive: never an empty group.
        for snapshot in [" 4242 4242 S+\n bad line\n 4244 4242 R\n", " x 4242 S\n", " 4242 x S\n",
                         " 4242 4242 4242 S+\n 4243 4242 4242 R\n", "", "\n\n", "ps: cannot read\n"]:
            with self.subTest(snapshot=snapshot), self.assertRaises(ValueError):
                ui.process_table(snapshot)
        executable = "/Applications/Morpheus.app/Contents/MacOS/morpheus-app"
        proc = self.Proc({PID: executable})
        with patch.object(ui, "process_table", side_effect=ValueError("incomplete")):
            self.assertFalse(ui.owned_mac_pid(PID, PID, executable, None, proc, lambda pid: PID))
        with patch.object(ui.subprocess, "run", return_value=Mock(stdout=" 4242 4242 4242 S+\n")):
            self.assertFalse(ui.owned_mac_pid(PID, PID, executable, None, proc, lambda pid: PID))


class ProfileLifecycleTests(unittest.TestCase):
    def test_release_profile_override_is_never_forwarded_and_home_is_untouched(self):
        with patch.dict(os.environ, {"MORPHEUS_QA_PROFILE_DIR": "/should/not/be/used", "HOME": "/Users/runner"}):
            env = ui.environment_for(NONCE)
            self.assertEqual(set(env) - set(os.environ), {smoke.NONCE_ENV})
            self.assertEqual(env["HOME"], "/Users/runner")
        self.assertNotIn("MORPHEUS_QA_PROFILE_DIR", env)

    def test_create_refuses_existing_project_folders_symlinks_or_missing_library_bases(self):
        with tempfile.TemporaryDirectory() as folder:
            elsewhere = Path(folder) / "elsewhere"
            elsewhere.mkdir()
            (elsewhere / "vault").write_bytes(b"not ours")
            cases = {
                "existing_profile_with_store": lambda roots: ((roots[0] / "accounts").mkdir(parents=True),
                                                              (roots[0] / "accounts" / "db").write_bytes(b"account")),
                "existing_cache": lambda roots: roots[1].mkdir(),
                "root_file": lambda roots: roots[0].write_text("x"),
                "root_symlink": lambda roots: os.symlink(elsewhere, roots[0], target_is_directory=True),
                "dangling_symlink": lambda roots: os.symlink(Path(folder) / "missing", roots[1]),
                "library_base_symlink": lambda roots: (shutil.rmtree(roots[1].parent),
                                                       os.symlink(elsewhere, roots[1].parent, target_is_directory=True)),
                "library_base_missing": lambda roots: shutil.rmtree(roots[0].parent),
            }
            for name, arrange in cases.items():
                with self.subTest(case=name):
                    home = temp_home(Path(folder) / name)
                    roots = ui.profile_roots(home)
                    arrange(roots)
                    before = sorted(str(path) for path in home.rglob("*"))
                    with self.assertRaises(RuntimeError):
                        ui.PackageProfile.create(NONCE, roots, home)
                    self.assertEqual(sorted(str(path) for path in home.rglob("*")), before)
            self.assertEqual((elsewhere / "vault").read_bytes(), b"not ours")

    def test_redirected_library_ancestor_is_refused_before_create_open_or_remove(self):
        # A symbolic link at ~/Library (above the bases the old check looked at)
        # redirects every project path into foreign material. Create, open and
        # remove must all refuse it, and the foreign material must survive.
        with tempfile.TemporaryDirectory() as folder:
            foreign = Path(folder) / "foreign"
            for base in ui.LIBRARY_BASES:
                (foreign / base.name / ui.PROJECT_DIR).mkdir(parents=True)
                (foreign / base.name / ui.PROJECT_DIR / ui.MARKER).write_text(NONCE, encoding="ascii")
            (foreign / "Application Support" / ui.PROJECT_DIR / "config.toml").write_text(ui.SEED_CONFIG, encoding="utf-8")
            (foreign / "Application Support" / ui.PROJECT_DIR / "vault").write_bytes(b"not ours")
            before = sorted(str(path) for path in foreign.rglob("*"))
            home = Path(folder) / "Users" / "runner"
            home.mkdir(parents=True)
            os.symlink(foreign, home / "Library", target_is_directory=True)
            roots = ui.profile_roots(home)
            self.assertFalse(ui.plain_ancestors(home, roots[0]))
            with self.assertRaises(RuntimeError):
                ui.PackageProfile.create(NONCE, roots, home)
            with self.assertRaises(ui.DiagnosticFailure) as error:
                ui.PackageProfile.open(NONCE, roots, home)
            self.assertEqual(error.exception.code, "profile_invalid")
            with self.assertRaises(RuntimeError):
                ui.PackageProfile(roots, NONCE, home).remove()
            self.assertEqual(sorted(str(path) for path in foreign.rglob("*")), before)
            self.assertEqual((foreign / "Application Support" / ui.PROJECT_DIR / "vault").read_bytes(), b"not ours")
            # A symbolic link at the home itself, or roots outside the home, fail the same proof.
            link = Path(folder) / "home-link"
            os.symlink(temp_home(Path(folder) / "real"), link, target_is_directory=True)
            self.assertFalse(ui.plain_ancestors(link, ui.profile_roots(link)[0]))
            self.assertFalse(ui.plain_ancestors(temp_home(Path(folder) / "other"), roots[0]))
        with tempfile.TemporaryDirectory() as folder:
            # relative_to is lexical, so a ".." component inside the home prefix is
            # refused before anything is resolved, created or removed.
            home = temp_home(folder)
            elsewhere = Path(folder) / "elsewhere" / "Application Support" / ui.PROJECT_DIR
            elsewhere.mkdir(parents=True)
            (elsewhere / ui.MARKER).write_text(NONCE, encoding="ascii")
            (elsewhere / "vault").write_bytes(b"not ours")
            escaping = home / "Library" / ".." / ".." / ".." / "elsewhere" / "Application Support" / ui.PROJECT_DIR
            inside = home / "Library" / ".." / "Library" / "Application Support" / ui.PROJECT_DIR
            self.assertEqual(escaping.resolve(), elsewhere.resolve())
            for root in (escaping, inside):
                with self.subTest(root=root.relative_to(home)):
                    self.assertFalse(ui.plain_ancestors(home, root))
                    with self.assertRaises(RuntimeError):
                        ui.PackageProfile.create(NONCE, [root, ui.profile_roots(home)[1]], home)
                    with self.assertRaises(RuntimeError):
                        ui.PackageProfile([root, ui.profile_roots(home)[1]], NONCE, home).remove()
            self.assertEqual((elsewhere / "vault").read_bytes(), b"not ours")
            self.assertTrue((elsewhere / ui.MARKER).exists())
            self.assertFalse(ui.profile_roots(home)[0].exists())
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            self.assertTrue(all(ui.plain_ancestors(home, root) for root in roots))
            profile = ui.PackageProfile.create(NONCE, roots, home)
            # Redirecting Library after creation must stop the removal too.
            real = home / "Library"
            moved = Path(folder) / "moved-library"
            real.rename(moved)
            os.symlink(moved, real, target_is_directory=True)
            with self.assertRaises(RuntimeError):
                profile.remove()
            self.assertEqual((moved / "Application Support" / ui.PROJECT_DIR / ui.MARKER).read_text(encoding="ascii"), NONCE)

    def test_create_seeds_markers_and_root_config_and_a_failed_create_removes_only_its_roots(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            profile = ui.PackageProfile.create(NONCE, roots, home)
            self.assertEqual(profile.config, roots[0] / "config.toml")  # macOS config_dir is the project folder.
            self.assertEqual(profile.config.read_text(encoding="utf-8"), ui.SEED_CONFIG)
            for root in roots:
                self.assertEqual((root / ui.MARKER).read_text(encoding="ascii"), NONCE)
            self.assertTrue(profile.remove())
            for root in roots:
                self.assertFalse(root.exists())
                self.assertTrue(root.parent.is_dir())  # Library bases are never removed.
            original = Path.write_text

            def failing(self, data, *args, **kwargs):
                if data == ui.SEED_CONFIG:
                    raise OSError("disk")
                return original(self, data, *args, **kwargs)

            with patch.object(Path, "write_text", failing), self.assertRaises(OSError):
                ui.PackageProfile.create(NONCE, roots, home)
            for root in roots:
                self.assertFalse(root.exists())
                self.assertTrue(root.parent.is_dir())
            for nonce in ["", "short", "G" * 32, NONCE.upper(), 12]:
                with self.subTest(nonce=nonce), self.assertRaises(RuntimeError):
                    ui.PackageProfile.create(nonce, roots, home)

    def test_open_accepts_only_the_marked_profile_even_after_the_app_created_its_store(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            profile = ui.PackageProfile.create(NONCE, roots, home)
            (roots[0] / "accounts").mkdir()
            (roots[0] / "accounts" / "db").write_bytes(b"synthetic store")
            (roots[1] / "fonts").write_bytes(b"cache")
            self.assertEqual(ui.PackageProfile.open(NONCE, roots, home).config, profile.config)
            for nonce in ["f" * 32, "not-a-nonce"]:
                with self.subTest(nonce=nonce), self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.PackageProfile.open(nonce, roots, home)
                self.assertEqual(error.exception.code, "profile_invalid")
            (roots[1] / ui.MARKER).unlink()
            with self.assertRaises(ui.DiagnosticFailure):
                ui.PackageProfile.open(NONCE, roots, home)
            (roots[1] / ui.MARKER).write_text(NONCE, encoding="ascii")
            profile.config.unlink()
            with self.assertRaises(ui.DiagnosticFailure):
                ui.PackageProfile.open(NONCE, roots, home)
            profile.config.write_text(ui.SEED_CONFIG, encoding="utf-8")
            self.assertTrue(ui.PackageProfile.open(NONCE, roots, home).remove())
            self.assertFalse(roots[0].exists())
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            with self.assertRaises(ui.DiagnosticFailure):
                ui.PackageProfile.open(NONCE, roots, home)  # Never created: nothing to own.

    def test_remove_refuses_redirected_or_foreign_roots(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            elsewhere = Path(folder) / "elsewhere"
            elsewhere.mkdir()
            (elsewhere / "vault").write_bytes(b"not ours")
            roots = ui.profile_roots(home)
            os.symlink(elsewhere, roots[0], target_is_directory=True)
            with self.assertRaises(RuntimeError):
                ui.PackageProfile(roots, NONCE, home).remove()
            self.assertEqual((elsewhere / "vault").read_bytes(), b"not ours")
            os.unlink(roots[0])
            (roots[0] / "accounts").mkdir(parents=True)
            (roots[0] / "accounts" / "db").write_bytes(b"account")
            with self.assertRaises(RuntimeError):
                ui.PackageProfile(roots, NONCE, home).remove()
            self.assertEqual((roots[0] / "accounts" / "db").read_bytes(), b"account")

    def test_smoke_compiles_probes_permissions_creates_the_profile_then_launches_and_removes_after_verified_stops(self):
        events = []
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            executable = bundle(folder)

            def build(work):
                events.append(("build", Path(work)))
                return Path(work) / "morpheus-ax-bridge"

            def probe(binary):
                events.append(("probe", binary))

            def plain_launch(path, stop):
                self.assertTrue(roots[0].exists())  # The profile exists before the first launch.
                self.assertIs(stop, ui.stop_group)  # The plain launch stops through the group proof.
                (roots[0] / "accounts").mkdir()
                (roots[0] / "accounts" / "db").write_bytes(b"synthetic store")
                events.append(("launch", path))

            def keyboard(path, nonce, binary):
                ui.PackageProfile.open(nonce, roots, home)
                events.append(("keyboard", path, nonce, binary))
                return True, True

            with patch.object(ui, "home_directory", return_value=home), patch.object(ui, "probe_permissions", side_effect=probe), \
                    patch.object(smoke, "macos_bridge_build", side_effect=build), \
                    patch.object(smoke, "launch_smoke", side_effect=plain_launch), \
                    patch.object(smoke, "macos_keyboard_launch", side_effect=keyboard), \
                    patch.object(smoke, "record") as record:
                smoke.macos_installed_launches(executable, Path(folder) / "work")
            nonce = events[3][2]
            self.assertEqual(events, [("build", Path(folder) / "work"), ("probe", Path(folder) / "work" / "morpheus-ax-bridge"),
                                      ("launch", executable), ("keyboard", executable, nonce, Path(folder) / "work" / "morpheus-ax-bridge")])
            self.assertTrue(ui.PackageProfile.valid_nonce(nonce))
            self.assertFalse(roots[0].exists())
            self.assertFalse(roots[1].exists())
            self.assertEqual([call.args[0] for call in record.call_args_list],
                             ["macos_accessibility_permission_present", "macos_profile_removed"])

    def test_permission_failure_stops_before_any_profile_or_launch(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            with patch.object(ui, "home_directory", return_value=home), \
                    patch.object(smoke, "macos_bridge_build", return_value=Path(folder) / "bridge"), \
                    patch.object(ui, "probe_permissions", side_effect=ui.DiagnosticFailure("permission_unavailable")), \
                    patch.object(smoke, "launch_smoke") as launch, patch.object(smoke, "macos_keyboard_launch") as keyboard:
                with self.assertRaises(ui.DiagnosticFailure):
                    smoke.macos_installed_launches(Path("/installed/Morpheus.app/Contents/MacOS/morpheus-app"), folder)
            launch.assert_not_called()
            keyboard.assert_not_called()
            self.assertFalse(roots[0].exists())

    def test_smoke_retains_the_profile_unless_both_stops_are_verified(self):
        first_launch = {"plain_launch_failed": RuntimeError("startup"),
                        "plain_launch_survivor": ui.DiagnosticFailure("process_remaining")}
        for case in ("plain_launch_failed", "plain_launch_survivor", "driver_unverified_stop", "driver_report_invalid"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as folder:
                home = temp_home(folder)
                roots = ui.profile_roots(home)
                captured = io.StringIO()
                launch = Mock(side_effect=first_launch.get(case))
                if case == "driver_report_invalid":
                    keyboard = Mock(side_effect=RuntimeError("Missing or ambiguous installed keyboard evidence"))
                else:
                    keyboard = Mock(return_value=(False, False))
                with patch.object(ui, "home_directory", return_value=home), patch.object(ui, "probe_permissions"), \
                        patch.object(smoke, "macos_bridge_build", return_value=Path(folder) / "bridge"), \
                        patch.object(smoke, "launch_smoke", launch), patch.object(smoke, "macos_keyboard_launch", keyboard), \
                        patch.object(smoke, "record"), contextlib.redirect_stdout(captured):
                    with self.assertRaises(RuntimeError):
                        smoke.macos_installed_launches(Path("/installed/Morpheus.app/Contents/MacOS/morpheus-app"), folder)
                self.assertTrue((roots[0] / ui.MARKER).exists())
                self.assertEqual(json.loads(captured.getvalue()), {"check": "macos_profile_retained", "status": "failed"})
                if case in first_launch:
                    keyboard.assert_not_called()  # No second launch without a verified first stop.
                else:
                    self.assertIs(launch.call_args.kwargs["stop"], ui.stop_group)

    def test_plain_launch_stops_through_the_given_callback_and_keeps_the_default_elsewhere(self):
        process = Mock(pid=PID, returncode=None)
        for stop_error in (None, ui.DiagnosticFailure("process_remaining")):
            stops = []

            def stop(target):
                stops.append(target)
                if stop_error:
                    raise stop_error

            with patch.object(smoke.sys, "platform", "darwin"), patch.object(smoke, "require_nonreaping_wait"), \
                    patch.object(smoke.subprocess, "Popen", return_value=process), \
                    patch.object(smoke, "process_status", return_value=None), patch.object(smoke, "stop_process") as shared, \
                    patch.object(smoke, "record") as record, patch.object(smoke.time, "sleep"):
                if stop_error:
                    with self.subTest(stop="raises"), self.assertRaises(ui.DiagnosticFailure) as error:
                        smoke.launch_smoke(Path("/i/Morpheus.app/Contents/MacOS/morpheus-app"), seconds=0.05, stop=stop)
                    self.assertEqual(error.exception.code, "process_remaining")
                    record.assert_not_called()  # No startup record without a verified stop.
                else:
                    smoke.launch_smoke(Path("/i/Morpheus.app/Contents/MacOS/morpheus-app"), seconds=0.05, stop=stop)
                    record.assert_called_once()
                shared.assert_not_called()
            self.assertEqual(stops, [process])
        with patch.object(smoke.sys, "platform", "darwin"), patch.object(smoke, "require_nonreaping_wait"), \
                patch.object(smoke.subprocess, "Popen", return_value=process), \
                patch.object(smoke, "process_status", return_value=None), patch.object(smoke, "stop_process") as shared, \
                patch.object(smoke, "record"), patch.object(smoke.time, "sleep"):
            smoke.launch_smoke(Path("/i/Morpheus.app/Contents/MacOS/morpheus-app"), seconds=0.05)
        shared.assert_called_once_with(process)  # Linux and Windows keep the shared stop.

    def test_existing_profile_is_refused_before_any_launch(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            (roots[0] / "accounts").mkdir(parents=True)
            (roots[0] / "accounts" / "db").write_bytes(b"account")
            with patch.object(ui, "home_directory", return_value=home), patch.object(ui, "probe_permissions"), \
                    patch.object(smoke, "macos_bridge_build", return_value=Path(folder) / "bridge"), \
                    patch.object(smoke, "launch_smoke") as launch, patch.object(smoke, "macos_keyboard_launch") as keyboard:
                with self.assertRaises(RuntimeError):
                    smoke.macos_installed_launches(Path("/installed/Morpheus.app/Contents/MacOS/morpheus-app"), folder)
            launch.assert_not_called()
            keyboard.assert_not_called()
            self.assertEqual((roots[0] / "accounts" / "db").read_bytes(), b"account")

    def test_bridge_build_uses_the_system_toolchain_privately_and_requires_a_binary(self):
        with tempfile.TemporaryDirectory() as folder:
            def compile_ok(command, **kwargs):
                kwargs["stdout"].write(b"PRIVATE compiler text with /Users/runner paths\n")
                Path(command[command.index("-o") + 1]).write_bytes(b"binary")
                return Mock(returncode=0)

            captured = io.StringIO()
            with patch.object(smoke.subprocess, "run", side_effect=compile_ok) as run, patch.object(smoke, "record"), \
                    contextlib.redirect_stdout(captured):
                binary = smoke.macos_bridge_build(folder)
            command = run.call_args.args[0]
            self.assertEqual(command[:3], ["xcrun", "swiftc", "-O"])
            self.assertEqual(Path(command[-1]).name, "macos_package_ui.swift")
            self.assertEqual(binary, Path(folder) / "morpheus-ax-bridge")
            self.assertEqual(run.call_args.kwargs["stdin"], smoke.subprocess.DEVNULL)
            self.assertNotIn("PRIVATE", captured.getvalue())
            with patch.object(smoke.subprocess, "run", return_value=Mock(returncode=1)), patch.object(smoke, "record"):
                with self.assertRaises(RuntimeError):
                    smoke.macos_bridge_build(Path(folder) / "other")


class TraversalTests(unittest.TestCase):
    def test_unowned_root_or_errored_root_is_not_a_tree(self):
        for root in (node(0, pid=1, role="AXApplication", parent=-1), node(0, role="AXApplication", parent=-1, error=True)):
            instance, _, diagnostics = driver([{"ok": True, "nodes": [root], "bounded": False}])
            with self.subTest(root=root), self.assertRaises(ui.DiagnosticFailure) as error:
                list(instance.nodes())
            self.assertEqual(error.exception.code, "bridge_invalid")
            self.assertFalse(diagnostics.value["ax_root_seen"])

    def test_unowned_node_is_skipped_and_marks_the_pass_incomplete(self):
        instance, _, diagnostics = driver([snapshot(node(1, "Homeserver URL", role="AXTextField"), node(2, "Other app", pid=5))])
        with self.assertRaises(ui.IncompleteTraversal):
            instance.find("Homeserver URL")
        self.assertEqual(diagnostics.value["incomplete_passes"], 1)
        self.assertTrue(diagnostics.value["ax_root_seen"])

    def test_node_read_error_marks_the_pass_incomplete_but_ambiguity_still_fails(self):
        instance, _, _ = driver([snapshot(node(1, "Password", role="AXTextField", subrole="AXSecureTextField"), node(2, error=True))])
        with self.assertRaises(ui.IncompleteTraversal):
            instance.find("Password")
        instance, _, diagnostics = driver([snapshot(node(1, "Password"), node(2, error=True), node(3, "Password"))])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.find("Password")
        self.assertEqual(error.exception.code, "selector_ambiguous")
        self.assertEqual(diagnostics.value["selector_matches"], 2)

    def test_incomplete_pass_is_retried_until_a_complete_pass_decides(self):
        instance, bridge, diagnostics = driver([snapshot(node(1, "Done"), node(2, error=True)), snapshot(node(1, "Done"))])
        self.assertEqual(instance.wait("Done").id, 1)
        self.assertEqual(diagnostics.value["incomplete_passes"], 1)
        self.assertEqual([command["op"] for command in bridge.commands], ["snapshot", "snapshot"])

    def test_bounded_snapshot_is_not_evidence(self):
        instance, _, diagnostics = driver([snapshot(node(1, "Done"), bounded=True)])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.find("Done")
        self.assertEqual(error.exception.code, "tree_bound")
        self.assertEqual(diagnostics.value["bounded_snapshots"], 1)
        oversized = {"ok": True, "nodes": [node(0, role="AXApplication", parent=-1)]
                     + [node(index) for index in range(1, ui.TREE_BOUND + 1)], "bounded": False}
        instance, _, _ = driver([oversized])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.find("Done")
        self.assertEqual(error.exception.code, "tree_bound")

    def test_value_sourced_names_are_accepted_only_for_static_text(self):
        instance, _, _ = driver([snapshot(node(1, "110% · text throughout the app", role="AXStaticText", name_source="value"))])
        self.assertEqual(instance.find("110% · text throughout the app").id, 1)
        for role in ("AXTextField", "AXButton", "AXGroup"):
            leaked = snapshot(node(1, "secret", role=role, name_source="value"))
            instance, _, _ = driver([leaked])
            with self.subTest(role=role), self.assertRaises(ui.DiagnosticFailure) as error:
                list(instance.nodes())
            self.assertEqual(error.exception.code, "bridge_invalid")

    def test_malformed_or_overlong_snapshot_entries_are_rejected(self):
        for raw in [{"ok": True, "nodes": "nodes", "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, role="AXApplication"), {"id": 1}], "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, role="AXApplication"), node(5)], "bounded": False},
                    {"ok": True, "nodes": [], "bounded": False},
                    {"ok": True, "nodes": [dict(node(0, parent=-1, role="AXApplication"), name=5)], "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, role="AXApplication"), node(1, "n" * (ui.NAME_BOUND + 1))], "bounded": False},
                    {"ok": True, "nodes": [dict(node(0, parent=-1, role="AXApplication"), name_source="other")], "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, role="AXApplication")], "bounded": "no"}]:
            instance, _, _ = driver([raw])
            with self.subTest(raw=raw), self.assertRaises(ui.DiagnosticFailure) as error:
                list(instance.nodes())
            self.assertEqual(error.exception.code, "bridge_invalid")

    def test_early_process_exit_is_not_a_wait_timeout(self):
        instance, _, _ = driver([snapshot()] * 50, alive=False)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.wait("Password")
        self.assertEqual(error.exception.code, "app_exited")


class FocusAndKeyTests(unittest.TestCase):
    def test_focus_outside_the_owned_process_fails_closed(self):
        instance, _, _ = driver(focused=[focused_reply("", owned=False)])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.focused_name()
        self.assertEqual(error.exception.code, "focus_unowned")

    def test_no_key_is_sent_without_an_owned_focused_element(self):
        for focused in (focused_reply("", owned=False, present=False), focused_reply("", owned=False)):
            instance, bridge, _ = driver(focused=[focused])
            with self.subTest(focused=focused), self.assertRaises(ui.DiagnosticFailure) as error:
                instance.key("Return")
            self.assertEqual(error.exception.code, "focus_unowned")
            self.assertNotIn("key", [command["op"] for command in bridge.commands])

    def test_lost_frontmost_or_partial_chord_fails_through_the_real_transport_path(self):
        for token, expected in [("foreground_unowned", "foreground_unowned"), ("window_gone", "foreground_unowned"),
                                ("input_rejected", "input_rejected"), ("invalid_key", "bridge_invalid"),
                                ("/Users/private/path", "bridge_invalid")]:
            instance, bridge, diagnostics = driver(focused=[focused_reply("Done")], key_replies=[{"ok": False, "error": token}])
            with self.subTest(token=token), self.assertRaises(ui.DiagnosticFailure) as error:
                instance.key("Tab")
            self.assertEqual(error.exception.code, expected)
            self.assertEqual(diagnostics.value["bridge_error"], token if token in smoke.MACOS_BRIDGE_ERRORS else "bridge_error")
            self.assertEqual(bridge.commands[-1], {"op": "key", "key": "Tab"})
            self.assertNotIn("private", json.dumps(diagnostics.value))
        # A chord the bridge reports as shorter than its transitions is rejected.
        instance, _, _ = driver(focused=[focused_reply("Done")], key_replies=[{"ok": True, "recovered": False, "posted": 3}])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.key("ShiftTab")
        self.assertEqual(error.exception.code, "input_rejected")
        instance, bridge, diagnostics = driver(focused=[focused_reply("Done")],
                                               key_replies=[{"ok": True, "recovered": True, "posted": 4}])
        instance.key("ShiftTab")
        self.assertEqual(diagnostics.value["activation_recoveries"], 1)
        self.assertEqual(bridge.commands[-1], {"op": "key", "key": "ShiftTab"})

    def test_only_reviewed_keys_are_sent(self):
        instance, bridge, _ = driver(focused=[focused_reply("Done")])
        with self.assertRaises(RuntimeError):
            instance.key("Cmd+Q")
        self.assertEqual(bridge.commands, [])

    def test_truncated_focus_or_node_name_matches_a_prefix_but_never_an_exact_label(self):
        label = "L" * (ui.NAME_BOUND - 1)
        instance, _, _ = driver(focused=[focused_reply(label, truncated=True)] * 50)
        with patch.object(ui, "WAIT_SECONDS", 0.3), self.assertRaises(ui.DiagnosticFailure) as error:
            instance.expect_focus(label)
        self.assertEqual(error.exception.code, "wait_timeout")
        instance, _, _ = driver(focused=[focused_reply(label, truncated=True)])
        instance.expect_focus("L" * 8, prefix=True)
        instance, _, _ = driver([snapshot(node(1, label, truncated=True))] * 2)
        self.assertIsNone(instance.find(label))
        self.assertEqual(instance.find("L" * 8, prefix=True).id, 1)
        reply = focused_reply("Done")
        del reply["truncated"]
        instance, _, _ = driver(focused=[reply])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.focused_name()
        self.assertEqual(error.exception.code, "bridge_invalid")

    def test_focus_command_targets_the_found_snapshot_element(self):
        instance, bridge, _ = driver([snapshot(node(1, "Open settings", role="AXButton"))], focused=[focused_reply("Open settings")])
        instance.focus("Open settings")
        self.assertIn({"op": "focus", "id": 1}, bridge.commands)

    def test_settings_scale_control_is_reached_by_bounded_tabs_before_its_label_is_queried(self):
        focused = [focused_reply(name) for name in ["Open settings", "Open settings", "Open settings", "Dark theme",
                                                    "Dark theme", "Decrease font scale", "Decrease font scale",
                                                    "Increase font scale", "Increase font scale", "Increase font scale"]]
        instance, bridge, _ = driver([snapshot(node(1, "Open settings", role="AXButton")),
                                      snapshot(node(1, "110% · text throughout the app", role="AXStaticText", name_source="value")),
                                      snapshot(node(1, "120% · text throughout the app", role="AXStaticText", name_source="value"))],
                                     focused=focused)
        ui.appearance_keyboard(instance)
        ops = [command for command in bridge.commands if command["op"] in ("key", "snapshot")]
        self.assertEqual([command["key"] for command in ops if command["op"] == "key"], ["Return", "Tab", "Tab", "Space"])
        first_tab = ops.index({"op": "key", "key": "Tab"})
        last_tab = len(ops) - 1 - ops[::-1].index({"op": "key", "key": "Tab"})
        self.assertFalse(any(command["op"] == "snapshot" for command in ops[first_tab:last_tab + 1]))

    def test_unreachable_scale_control_fails_within_the_tab_bound(self):
        def focused(call):
            return focused_reply("Open settings" if call == 0 else f"Control {call}")

        instance, bridge, _ = driver([snapshot(node(1, "Open settings", role="AXButton"))], focused=focused)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            ui.appearance_keyboard(instance)
        self.assertEqual(error.exception.code, "scale_unreachable")
        self.assertLessEqual(sum(1 for command in bridge.commands if command["op"] == "key"), ui.SETTINGS_TAB_BOUND + 1)


class AccountMenuStageTests(unittest.TestCase):
    def test_each_native_menu_boundary_keeps_a_closed_failure_position(self):
        stages = ("menu_focus_trigger", "menu_open_return", "menu_close_escape",
                  "menu_reopen_space", "menu_tab_manage", "menu_open_manage",
                  "menu_dialog_tabs", "menu_close_dialog")
        for expected in stages:
            with self.subTest(stage=expected):
                diagnostics = ui.Diagnostics({})
                instance = Mock()
                instance.diagnostics = diagnostics

                def operation(*arguments):
                    diagnostics.operation("focused_element")
                    if diagnostics.value["step"] == expected:
                        raise ui.DiagnosticFailure("wait_timeout")
                    return "Manage accounts"

                for method in ("focus", "key", "expect_focus", "wait", "wait_for", "focused_name"):
                    getattr(instance, method).side_effect = operation
                with self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.menu_keyboard(instance)
                diagnostics.failed(error.exception)
                diagnostics.value["cleanup"] = "passed"
                self.assertEqual(diagnostics.value["step"], expected)
                self.assertEqual(diagnostics.value["failure"], "wait_timeout")
                self.assertNotIn("Manage accounts", repr(diagnostics.value))
                smoke.validate_macos_keyboard_diagnostic(diagnostic(step=expected), False)

    def test_completed_menu_keeps_the_final_boundary_until_the_next_check(self):
        instance = Mock()
        diagnostics = ui.Diagnostics({})
        instance.diagnostics = diagnostics
        instance.focused_name.return_value = "Manage accounts"
        ui.menu_keyboard(instance)
        self.assertEqual(diagnostics.value["step"], "menu_close_dialog")
        instance.expect_focus.assert_called_with("Account menu for ", prefix=True)
        # A completed menu cannot stand in for completion of all acceptance.
        with self.assertRaises(RuntimeError):
            smoke.validate_macos_keyboard_diagnostic(diagnostic(step="menu_close_dialog"), True)


class AppearanceStageTests(unittest.TestCase):
    def test_failure_stage_survives_lower_level_operations_and_cleanup(self):
        cases = [("focus", "Open settings", "open_settings"),
                 ("focused_name", None, "reach_scale"),
                 ("wait", "110% · text throughout the app", "read_seed_scale"),
                 ("key", "Space", "increase_scale"),
                 ("wait", "120% · text throughout the app", "read_changed_scale")]
        for method, argument, expected in cases:
            with self.subTest(stage=expected):
                diagnostics = ui.Diagnostics({})
                instance = Mock()
                instance.diagnostics = diagnostics
                instance.focused_name.return_value = "Increase font scale"

                def fail(*arguments):
                    diagnostics.operation("node_name")
                    if argument is None or arguments == (argument,):
                        raise ui.DiagnosticFailure("wait_timeout")

                getattr(instance, method).side_effect = fail
                with self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.appearance_keyboard(instance)
                diagnostics.failed(error.exception)
                diagnostics.step("cleanup")
                self.assertEqual(diagnostics.value["appearance_stage"], expected)
                self.assertEqual(diagnostics.value["failure"], "wait_timeout")

    def test_complete_stage_requires_successful_persistence(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.toml"
            diagnostics = ui.Diagnostics({})
            config.write_text("[appearance]\nscale_percent = 110\n")
            with self.assertRaises(ui.DiagnosticFailure):
                ui.config_persisted(config, diagnostics, seconds=0)
            self.assertEqual(diagnostics.value["appearance_stage"], "persist_scale")
            config.write_text("[appearance]\nscale_percent = 120\n")
            ui.config_persisted(config, diagnostics, seconds=0)
            self.assertEqual(diagnostics.value["appearance_stage"], "complete")

    def test_unknown_stages_never_enter_a_report(self):
        diagnostics = ui.Diagnostics({})
        for value in ("PRIVATE_TEXT", "", None, 1, []):
            with self.subTest(value=value), self.assertRaises(ui.DiagnosticFailure):
                diagnostics.appearance_stage(value)
            self.assertEqual(diagnostics.value["appearance_stage"], "not_started")

    def test_report_accepts_only_closed_stages_and_requires_complete_on_success(self):
        validator = smoke.validate_macos_keyboard_diagnostic
        for stage in smoke.APPEARANCE_STAGES:
            validator(diagnostic(appearance_stage=stage), False)
            if stage != "complete":
                with self.assertRaises(RuntimeError):
                    validator(diagnostic(appearance_stage=stage), True)
        for stage in ("PRIVATE_TEXT", None, [], True):
            with self.subTest(stage=stage), self.assertRaises(RuntimeError):
                validator(diagnostic(appearance_stage=stage), False)


class SignInSemanticsTests(unittest.TestCase):
    def fields(self, password_role="AXTextField", subrole="AXSecureTextField"):
        return snapshot(node(1, "Homeserver URL", role="AXTextField"), node(2, "Matrix ID or username", role="AXTextField"),
                        node(3, "Password", role=password_role, subrole=subrole))

    def test_protected_password_field_passes_without_reading_values(self):
        instance, bridge, diagnostics = driver([self.fields()] * 3)
        record = {}
        ui.inspect_sign_in(instance, record, diagnostics)
        self.assertTrue(record["sign_in_semantics"])
        self.assertEqual(diagnostics.value["password_semantics"], "protected")
        self.assertTrue(all(command["op"] == "snapshot" for command in bridge.commands))

    def test_unprotected_or_non_text_field_password_cannot_pass(self):
        for kind, expected in [(("AXTextField", ""), "unprotected"), (("AXStaticText", "AXSecureTextField"), "not_text_field"),
                               (("AXGroup", ""), "not_text_field")]:
            instance, _, diagnostics = driver([self.fields(*kind)] * 3)
            record = {}
            with self.subTest(kind=kind), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.inspect_sign_in(instance, record, diagnostics)
            self.assertEqual(error.exception.code, "password_semantics")
            self.assertEqual(diagnostics.value["password_semantics"], expected)
            self.assertNotIn("sign_in_semantics", record)
        secure_username = snapshot(node(1, "Homeserver URL", role="AXTextField"),
                                   node(2, "Matrix ID or username", role="AXTextField", subrole="AXSecureTextField"),
                                   node(3, "Password", role="AXTextField", subrole="AXSecureTextField"))
        instance, _, _ = driver([secure_username] * 3)
        with self.assertRaises(ui.DiagnosticFailure):
            ui.inspect_sign_in(instance, {}, ui.Diagnostics({}))

    def test_missing_second_field_retains_the_first_observation(self):
        instance, _, diagnostics = driver([snapshot(node(1, "Homeserver URL", role="AXTextField"))] * 200)
        with patch.object(ui, "WAIT_SECONDS", 0.3), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.inspect_sign_in(instance, {}, diagnostics)
        self.assertEqual(error.exception.code, "wait_timeout")
        self.assertTrue(diagnostics.value["homeserver_seen"])
        self.assertFalse(diagnostics.value["username_seen"])
        self.assertEqual(diagnostics.value["step"], "username")


class FakeChild:
    """A Popen stand-in whose pipes are real; the scripted bytes are the bridge's output."""
    def __init__(self, output, hold_open=False):
        in_read, in_write = os.pipe()
        out_read, out_write = os.pipe()
        self.stdin = os.fdopen(in_write, "wb")
        self.stdout = os.fdopen(out_read, "rb")
        self.pid = 5151
        self.returncode = None
        self.sink = os.fdopen(in_read, "rb")
        writer = os.fdopen(out_write, "wb")
        writer.write(output)
        writer.flush()
        self.held = writer if hold_open else None
        if not hold_open:
            writer.close()

    def release(self):
        if self.held is not None:
            self.held.close()
            self.held = None
        # A constructor timeout can leave the reader alive until held closes.
        # Release every fake pipe end even when Bridge.close could not join it.
        self.stdin.close()
        self.sink.close()
        self.stdout.close()


class BridgeProtocolTests(unittest.TestCase):
    def setUp(self):
        patcher = patch.object(ui, "BRIDGE_EXIT_SECONDS", 0.05)
        patcher.start()
        self.addCleanup(patcher.stop)

    def start(self, output, hold_open=False, status=None, stop=None):
        child = FakeChild(output, hold_open)
        diagnostics = ui.Diagnostics({})
        statuses = {"value": status}
        with patch.object(ui.subprocess, "Popen", return_value=child) as popen, \
                patch.object(smoke, "process_status", side_effect=lambda process: statuses["value"]), \
                patch.object(ui, "stop_group", side_effect=stop or (lambda process: None)) as stopper:
            try:
                bridge = ui.Bridge(["/work/morpheus-ax-bridge"], diagnostics, environment=dict(HOSTED))
            except BaseException:
                child.release()
                raise
        return bridge, child, popen, stopper, statuses

    def test_bridge_starts_in_its_own_session_with_private_streams(self):
        bridge, child, popen, stopper, statuses = self.start(b'{"ok":true,"ready":true}\n')
        kwargs = popen.call_args.kwargs
        self.assertTrue(kwargs["start_new_session"])
        self.assertEqual(kwargs["stderr"], ui.subprocess.DEVNULL)
        self.assertEqual(kwargs["env"]["ImageOS"], "macos15")
        self.assertEqual(popen.call_args.args[0], ["/work/morpheus-ax-bridge"])
        with patch.object(smoke, "process_status", return_value=None), patch.object(ui, "stop_group") as stop:
            bridge.close()
        stop.assert_called_once_with(child)
        child.release()

    def test_handshake_without_ready_or_timeout_stops_the_child(self):
        for output, hold, expected in [(b"", False, "bridge_exited"), (b'{"ok":true}\n', False, "bridge_invalid"),
                                       (b"not json\n", False, "bridge_invalid"), (b"", True, "bridge_timeout")]:
            stops = []
            with self.subTest(output=output, hold=hold), patch.object(ui, "BRIDGE_REPLY_SECONDS", 0.05):
                with self.assertRaises(ui.DiagnosticFailure) as error:
                    self.start(output, hold_open=hold, stop=lambda process: stops.append(process.pid))
            self.assertEqual(error.exception.code, expected)
            self.assertEqual(stops, [5151])

    def test_spawn_failure_leaves_nothing_to_stop(self):
        with patch.object(ui.subprocess, "Popen", side_effect=OSError("missing")), patch.object(ui, "stop_group") as stop:
            with self.assertRaises(OSError):
                ui.Bridge(["/work/morpheus-ax-bridge"], ui.Diagnostics({}), environment={})
        stop.assert_not_called()

    def test_stop_failure_propagates_instead_of_pretending(self):
        bridge, child, _, _, _ = self.start(b'{"ok":true,"ready":true}\n')
        with patch.object(smoke, "process_status", return_value=None), \
                patch.object(ui, "stop_group", side_effect=RuntimeError("Smoke app did not stop")):
            with self.assertRaises(RuntimeError):
                bridge.close()
        child.release()

    def test_overlong_reply_is_rejected_before_it_is_buffered(self):
        line = b'{"ok":true,"nodes":[' + b"x" * 200 + b"]}\n"
        with patch.object(ui, "BRIDGE_LINE_BOUND", 64):
            bridge, child, _, _, _ = self.start(b'{"ok":true,"ready":true}\n' + line)
            bridge.reader.join(timeout=2)
            with self.assertRaises(ui.DiagnosticFailure) as error:
                bridge.reply()
            self.assertEqual(error.exception.code, "bridge_overflow")
            self.assertEqual(bridge.replies.qsize(), 1)
            self.assertIsNone(bridge.replies.get_nowait())
        with patch.object(smoke, "process_status", return_value=0), patch.object(ui, "stop_group"):
            bridge.close()
        child.release()

    def test_flooded_replies_stay_within_the_bounded_queue_without_pipe_capacity_assumptions(self):
        flood = 40
        output = b'{"ok":true,"ready":true}\n' + b'{"ok":true,"flood":1}\n' * flood
        self.assertLess(len(output), 1024)
        self.assertGreater(flood, 2 * ui.BRIDGE_QUEUE_BOUND)
        bridge, child, _, _, _ = self.start(output)
        try:
            for _ in range(10):
                self.assertLessEqual(bridge.replies.qsize(), ui.BRIDGE_QUEUE_BOUND)
                self.assertEqual(bridge.reply(), {"ok": True, "flood": 1})
            for _ in range(flood - 10):
                self.assertEqual(bridge.reply(), {"ok": True, "flood": 1})
            with self.assertRaises(ui.DiagnosticFailure) as error:
                bridge.reply()
            self.assertEqual(error.exception.code, "bridge_exited")
            bridge.reader.join(timeout=2)
            self.assertFalse(bridge.reader.is_alive())
        finally:
            with patch.object(smoke, "process_status", return_value=0), patch.object(ui, "stop_group"):
                bridge.close()
            child.release()

    def test_bridge_error_tokens_are_fixed_and_unknown_tokens_collapse(self):
        instance, _, diagnostics = driver([{"ok": False, "error": "unowned_root"}])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            list(instance.nodes())
        self.assertEqual(error.exception.code, "bridge_invalid")
        self.assertEqual(diagnostics.value["bridge_error"], "unowned_root")
        instance, _, diagnostics = driver([{"ok": False, "error": "/Users/runner/private"}])
        with self.assertRaises(ui.DiagnosticFailure):
            list(instance.nodes())
        self.assertEqual(diagnostics.value["bridge_error"], "bridge_error")

    def test_permission_check_requires_both_booleans_and_never_prompts(self):
        diagnostics = ui.Diagnostics({})
        bridge = fake_bridge(diagnostics)
        ui.check_permissions(bridge, diagnostics)
        self.assertTrue(diagnostics.value["permission_checked"])
        for reply in ({"ok": True, "accessibility": True, "post_event": False},
                      {"ok": True, "accessibility": False, "post_event": True}):
            diagnostics = ui.Diagnostics({})
            with self.subTest(reply=reply), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.check_permissions(fake_bridge(diagnostics, permissions=reply), diagnostics)
            self.assertEqual(error.exception.code, "permission_unavailable")
            self.assertTrue(diagnostics.value["permission_checked"])
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(ui.DiagnosticFailure) as error:
            ui.check_permissions(fake_bridge(diagnostics, permissions={"ok": True, "accessibility": "yes", "post_event": True}),
                                 diagnostics)
        self.assertEqual(error.exception.code, "bridge_invalid")
        self.assertFalse(diagnostics.value["permission_checked"])

    def test_owned_window_requires_exactly_one_window_within_the_bound(self):
        process = Mock(pid=PID)
        diagnostics = ui.Diagnostics({})
        with patch.object(smoke, "process_status", return_value=None):
            ui.owned_window(fake_bridge(diagnostics, windows=[{"ok": True, "count": 0}, {"ok": True, "count": 1}]), process, diagnostics)
            self.assertTrue(diagnostics.value["owned_window_seen"])
            for reply, expected in [({"ok": True, "count": 2}, "window_count"), ({"ok": True, "count": 99}, "bridge_invalid"),
                                    ({"ok": True, "count": "1"}, "bridge_invalid")]:
                diagnostics = ui.Diagnostics({})
                with self.subTest(reply=reply), self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.owned_window(fake_bridge(diagnostics, windows=[reply]), process, diagnostics)
                self.assertEqual(error.exception.code, expected)
        with patch.object(smoke, "process_status", return_value=1), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.owned_window(fake_bridge(ui.Diagnostics({}), windows=[{"ok": True, "count": 0}]), process, ui.Diagnostics({}))
        self.assertEqual(error.exception.code, "app_exited")


class Clock:
    """A monotonic clock double that advances only when told to."""
    def __init__(self, now=64.0):
        self.now = now

    def __call__(self):
        return self.now


def transport_bridge(diagnostics, clock, answers):
    """The real Bridge.call and Bridge.reply over a scripted reply queue.

    An answer is a reply object, (seconds, reply) for one that arrives after
    that long, or "stall" for none. A wait that ends first advances the clock
    by its whole timeout and raises queue.Empty, as Queue.get does.
    """
    bridge = object.__new__(ui.Bridge)
    bridge.process, bridge.reader, bridge.diagnostics = None, None, diagnostics
    bridge.stdin = bridge.stdout = None
    bridge.commands, bridge.waits = [], []
    script = list(answers)

    def get(timeout):
        bridge.waits.append(timeout)
        answer = script.pop(0) if script else "stall"
        delay, answer = answer if isinstance(answer, tuple) else (0.0, answer)
        if answer == "stall" or delay > timeout:
            clock.now += timeout
            raise ui.queue.Empty
        clock.now += delay
        return (json.dumps(answer) + "\n").encode("utf-8")

    bridge.replies = types.SimpleNamespace(get=get)
    bridge.alive = lambda: True
    bridge.transmit = bridge.commands.append
    return bridge


class AttachRegistrationTests(unittest.TestCase):
    """The bounded wait for AppKit to register the already-owned process.

    Every case runs the real Bridge.call and Bridge.reply against a scripted
    reply queue and one controllable monotonic clock.
    """
    EXECUTABLE = Path("/private/var/folders/synthetic/T/Applications/Morpheus.app/Contents/MacOS/morpheus-app")
    COMMAND = {"op": "attach", "pid": PID, "executable": str(EXECUTABLE), "bundle": smoke.IDENTIFIER}

    def run_attach(self, answers, *, owned=True, status=None, seconds=ui.REGISTRATION_SECONDS, clock=None,
                   poll=None, acceptance_left=ui.ACCEPTANCE_SECONDS):
        clock = clock or Clock()
        diagnostics = ui.Diagnostics({})
        diagnostics.deadline = clock() + acceptance_left
        bridge = transport_bridge(diagnostics, clock, answers)
        process = Mock(pid=PID, returncode=None)
        ownership = Mock(return_value=owned) if isinstance(owned, bool) else Mock(side_effect=owned)
        statuses = Mock(side_effect=status) if isinstance(status, list) else Mock(return_value=status)
        sleep = Mock(side_effect=lambda seconds: setattr(clock, "now", clock.now + seconds))
        error = None
        with contextlib.ExitStack() as stack:
            stack.enter_context(patch.object(ui, "owned_mac_pid", ownership))
            stack.enter_context(patch.object(smoke, "process_status", statuses))
            stack.enter_context(patch.object(ui.time, "sleep", sleep))
            stack.enter_context(patch.object(ui.time, "monotonic", clock))
            if poll is not None:
                stack.enter_context(patch.object(ui, "REGISTRATION_POLL_SECONDS", poll))
            try:
                ui.attach_owned(bridge, process, self.EXECUTABLE, smoke.IDENTIFIER, diagnostics, seconds=seconds)
            except ui.DiagnosticFailure as failure:
                error = failure.code
        return error, bridge, diagnostics, ownership, sleep

    @staticmethod
    def slept(sleep):
        return [entry.args[0] for entry in sleep.call_args_list]

    def test_normal_pending_then_complete_identity_passes(self):
        pending = [{"ok": False, "error": code} for code in sorted(ui.REGISTRATION_PENDING)]
        error, bridge, diagnostics, ownership, sleep = self.run_attach(pending + [{"ok": True}])
        self.assertIsNone(error)
        self.assertEqual(bridge.commands, [self.COMMAND] * 4)
        # The anchored process is rechecked before and after every attempt.
        self.assertEqual(ownership.call_count, 8)
        ownership.assert_called_with(PID, PID, self.EXECUTABLE)
        self.assertEqual(self.slept(sleep), [ui.REGISTRATION_POLL_SECONDS] * 3)
        # Every reply wait stayed inside the registration budget.
        self.assertEqual(len(bridge.waits), 4)
        self.assertTrue(all(0 < wait <= ui.REGISTRATION_SECONDS for wait in bridge.waits))
        # Transient answers are not recorded: a passing report is unchanged.
        self.assertEqual((diagnostics.value["bridge_error"], diagnostics.value["failure"]), ("none", "none"))

    def test_positive_mismatches_fail_at_once(self):
        for code in ("attach_bundle_mismatch", "attach_executable_mismatch", "attach_executable_unresolved",
                     "attach_mismatch", "invalid_command", "not_attached"):
            error, bridge, diagnostics, ownership, sleep = self.run_attach([{"ok": False, "error": code},
                                                                            {"ok": True}])
            with self.subTest(code=code):
                self.assertEqual(error, "bridge_invalid")
                self.assertEqual(diagnostics.value["bridge_error"], code)
                self.assertEqual(bridge.commands, [self.COMMAND])
                self.assertEqual(ownership.call_count, 2)  # The failed attempt is rechecked too.
                sleep.assert_not_called()
        error, _, diagnostics, _, _ = self.run_attach([{"ok": False, "error": "/Users/runner/private"}, {"ok": True}])
        self.assertEqual((error, diagnostics.value["bridge_error"]), ("bridge_invalid", "bridge_error"))
        # A mismatch after earlier absent answers is still never accepted.
        error, bridge, diagnostics, _, _ = self.run_attach([{"ok": False, "error": "attach_application_missing"},
                                                            {"ok": False, "error": "attach_executable_mismatch"},
                                                            {"ok": True}])
        self.assertEqual((error, diagnostics.value["bridge_error"], len(bridge.commands)),
                         ("bridge_invalid", "attach_executable_mismatch", 2))

    def test_stalled_reply_waits_only_for_the_remaining_registration_budget(self):
        answers = [{"ok": False, "error": "attach_application_missing"}, "stall", {"ok": True}]
        error, bridge, diagnostics, ownership, sleep = self.run_attach(answers, seconds=1.0, poll=0.25)
        self.assertEqual(error, "wait_timeout")
        # The second wait got only what remained (0.75 s), never the 20 s reply bound.
        self.assertEqual(bridge.waits, [1.0, 0.75])
        self.assertEqual(self.slept(sleep), [0.25])
        self.assertEqual(len(bridge.commands), 2)
        # The timed-out attempt was rechecked as well; a stall records no absent token.
        self.assertEqual(ownership.call_count, 4)
        self.assertEqual((diagnostics.value["bridge_error"], diagnostics.value["operation"]), ("none", "attach"))
        # A lost anchor after a timed-out attempt takes precedence.
        error, _, _, _, _ = self.run_attach(["stall"], owned=[True, False], seconds=1.0)
        self.assertEqual(error, "ownership_lost")

    def test_reply_deadline_is_optional_and_other_operations_keep_their_bounds(self):
        for deadline, expected_wait, code in ((None, ui.BRIDGE_REPLY_SECONDS, "bridge_timeout"),
                                              (64.0 + 3.0, 3.0, "wait_timeout"),
                                              (64.0 + 60.0, ui.BRIDGE_REPLY_SECONDS, "bridge_timeout")):
            clock = Clock()
            diagnostics = ui.Diagnostics({})
            diagnostics.deadline = clock() + ui.ACCEPTANCE_SECONDS
            bridge = transport_bridge(diagnostics, clock, ["stall"])
            with self.subTest(deadline=deadline), patch.object(ui.time, "monotonic", clock), \
                    self.assertRaises(ui.DiagnosticFailure) as error:
                bridge.reply(deadline)
            self.assertEqual((bridge.waits, error.exception.code), ([expected_wait], code))
        # Without a narrower deadline a success is accepted whenever it arrives within the bounds.
        clock = Clock()
        diagnostics = ui.Diagnostics({})
        diagnostics.deadline = clock() + ui.ACCEPTANCE_SECONDS
        bridge = transport_bridge(diagnostics, clock, [(15.0, {"ok": True, "count": 1})])
        with patch.object(ui.time, "monotonic", clock):
            self.assertEqual(bridge.call({"op": "windows"}), {"ok": True, "count": 1})
        # The acceptance deadline still bounds a narrower-deadline wait and keeps its own token.
        error, bridge, _, _, _ = self.run_attach(["stall"], seconds=10.0, acceptance_left=0.5)
        self.assertEqual((error, bridge.waits), ("deadline", [0.5]))

    def test_late_success_is_refused(self):
        # A success that arrives exactly at the end of the budget is too late.
        error, bridge, diagnostics, ownership, _ = self.run_attach([(1.0, {"ok": True})], seconds=1.0)
        self.assertEqual((error, bridge.waits, len(bridge.commands)), ("wait_timeout", [1.0], 1))
        self.assertEqual(ownership.call_count, 2)
        self.assertEqual(diagnostics.value["bridge_error"], "none")
        # A timely reply whose following anchor recheck ends past the budget is refused too.
        clock = Clock()
        checks = []

        def slow_recheck(*arguments):
            checks.append(arguments)
            if len(checks) == 2:
                clock.now += 1.0
            return True

        error, bridge, _, _, _ = self.run_attach([(0.5, {"ok": True})], owned=slow_recheck, seconds=1.0, clock=clock)
        self.assertEqual((error, len(bridge.commands), len(checks)), ("wait_timeout", 1, 2))

    def test_expired_budget_starts_no_further_attempt(self):
        pending = [{"ok": False, "error": "attach_bundle_missing"}] * 5
        error, bridge, diagnostics, _, sleep = self.run_attach(pending, seconds=0.375, poll=0.25)
        self.assertEqual(error, "wait_timeout")
        # The pause is clipped to what remains, and the expired clock stops a third attempt.
        self.assertEqual(self.slept(sleep), [0.25, 0.125])
        self.assertEqual(bridge.waits, [0.375, 0.125])
        self.assertEqual(len(bridge.commands), 2)
        self.assertEqual((diagnostics.value["bridge_error"], diagnostics.value["operation"]),
                         ("attach_bundle_missing", "attach"))
        # A pre-attempt recheck that outlasts the budget also prevents the attempt.
        clock = Clock()
        checks = []

        def slow_before(*arguments):
            checks.append(arguments)
            if len(checks) == 3:
                clock.now += 5.0
            return True

        error, bridge, diagnostics, _, _ = self.run_attach(pending, owned=slow_before, seconds=1.0, clock=clock)
        self.assertEqual((error, len(bridge.commands), len(checks)), ("wait_timeout", 1, 3))
        self.assertEqual(diagnostics.value["bridge_error"], "attach_bundle_missing")

    def test_zero_budget_makes_no_attempt(self):
        error, bridge, diagnostics, ownership, sleep = self.run_attach([{"ok": True}], seconds=0)
        self.assertEqual(error, "wait_timeout")
        self.assertEqual((bridge.commands, bridge.waits), ([], []))
        ownership.assert_not_called()
        sleep.assert_not_called()
        self.assertEqual((diagnostics.value["bridge_error"], diagnostics.value["operation"]), ("none", "attach"))

    def test_acceptance_deadline_ends_the_wait_before_another_attempt(self):
        error, bridge, _, ownership, _ = self.run_attach([{"ok": True}], acceptance_left=0)
        self.assertEqual(error, "deadline")
        self.assertEqual(bridge.commands, [])
        ownership.assert_not_called()

    def test_process_death_stops_the_wait(self):
        error, bridge, _, _, _ = self.run_attach([{"ok": True}], status=[0])
        self.assertEqual((error, bridge.commands), ("app_exited", []))
        pending = [{"ok": False, "error": "attach_application_missing"}] * 3
        error, bridge, _, _, _ = self.run_attach(pending, status=[None, None, None, 0])
        self.assertEqual((error, len(bridge.commands)), ("app_exited", 2))
        # Exit during a successful attempt is still a failure, not an attachment.
        error, bridge, _, _, _ = self.run_attach([{"ok": True}], status=[None, 0])
        self.assertEqual((error, len(bridge.commands)), ("app_exited", 1))

    def test_identity_change_around_an_attempt_fails_closed(self):
        error, bridge, _, _, _ = self.run_attach([{"ok": True}], owned=[False])
        self.assertEqual((error, bridge.commands), ("ownership_lost", []))
        for reply in ({"ok": True}, {"ok": False, "error": "attach_bundle_missing"},
                      {"ok": False, "error": "attach_bundle_mismatch"}):
            error, bridge, diagnostics, _, sleep = self.run_attach([reply, {"ok": True}], owned=[True, False])
            with self.subTest(reply=reply):
                self.assertEqual((error, len(bridge.commands)), ("ownership_lost", 1))
                self.assertEqual(diagnostics.value["operation"], "ownership")
                self.assertEqual(diagnostics.value["bridge_error"], "none")
                sleep.assert_not_called()
        pending = [{"ok": False, "error": "attach_executable_missing"}, {"ok": True}]
        error, bridge, _, _, _ = self.run_attach(pending, owned=[True, True, False])
        self.assertEqual((error, len(bridge.commands)), ("ownership_lost", 1))

    def test_only_absent_metadata_tokens_are_retryable(self):
        attach = SWIFT.read_text(encoding="utf-8").split("func attach(")[1].split("func windows()")[0]
        tokens = set(re.findall(r'fail\("([a-z_]+)"\)', attach))
        self.assertEqual({token for token in tokens if token.endswith("_missing")}, set(ui.REGISTRATION_PENDING))
        self.assertFalse({token for token in tokens if token.endswith(("_mismatch", "_unresolved"))}
                         & ui.REGISTRATION_PENDING)
        self.assertNotIn("invalid_command", ui.REGISTRATION_PENDING)
        self.assertLessEqual(ui.REGISTRATION_PENDING, smoke.MACOS_BRIDGE_ERRORS)
        self.assertTrue(0 < ui.REGISTRATION_POLL_SECONDS < ui.REGISTRATION_SECONDS <= 30)

    def test_acceptance_attaches_only_after_the_ownership_proof(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            ui.PackageProfile.create(NONCE, roots, home)
            executable = bundle(folder)
            bridge_binary = Path(folder) / "bridge"
            bridge_binary.write_bytes(b"binary")
            bridge_binary.chmod(0o700)
            process = Mock(pid=PID, returncode=None)
            bridge = Mock()
            trace = ui.Diagnostics({})
            with patch.object(ui, "guard"), patch.object(smoke, "require_nonreaping_wait"), \
                    patch.object(ui, "home_directory", return_value=home), \
                    patch.object(ui, "Bridge", return_value=bridge), patch.object(ui, "check_permissions"), \
                    patch.object(ui.subprocess, "Popen", return_value=process), \
                    patch.object(ui, "owned_mac_pid", return_value=True), \
                    patch.object(ui, "attach_owned", side_effect=ui.DiagnosticFailure("wait_timeout")) as attach, \
                    patch.object(ui, "owned_window") as window, patch.object(ui, "stop_group"), \
                    patch.object(smoke, "process_status", return_value=None):
                with self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.acceptance(executable, NONCE, bridge_binary, {}, trace)
            self.assertEqual(error.exception.code, "wait_timeout")
            attach.assert_called_once_with(bridge, process, executable, smoke.IDENTIFIER, trace)
            window.assert_not_called()
            bridge.call.assert_not_called()  # Attachment goes only through the bounded wait.
            self.assertEqual((trace.value["step"], trace.value["failure"]), ("launch", "wait_timeout"))


class CloseAndCleanupTests(unittest.TestCase):
    executable = "/Applications/Morpheus.app/Contents/MacOS/morpheus-app"

    live = table((PID, PID, "S+"))

    def close(self, statuses, after, close_reply=None, owned=True, before=None, session=None):
        diagnostics = ui.Diagnostics({})
        bridge = fake_bridge(diagnostics, close=close_reply)
        process = Mock(pid=PID, returncode=None)
        proc = OwnershipTests.Proc({PID: self.executable if owned else "/usr/bin/true"})
        with patch.object(smoke, "process_status", side_effect=statuses):
            ui.normal_close(bridge, process, diagnostics, self.executable, self.live if before is None else before,
                            after, proc, (lambda pid: PID) if session is None else session, seconds=0.5)
        return bridge, diagnostics

    def test_normal_close_requires_zero_exit_and_an_empty_live_group(self):
        # Before: the live leader; after: only its unreaped zombie remains in the group.
        bridge, diagnostics = self.close([None, 0], table((PID, PID, "Z")))
        self.assertEqual([command["op"] for command in bridge.commands], ["close"])
        for statuses, after, expected in [([None, 3], table((PID, PID, "Z")), "exit_status"),
                                          ([None, -15], table((PID, PID, "Z")), "exit_status"),
                                          ([None] * 100, self.live, "exit_status"),
                                          ([None, 0], table((PID, PID, "Z"), (PID + 1, PID, "S")), "process_remaining")]:
            with self.subTest(statuses=statuses, after=after), self.assertRaises(ui.DiagnosticFailure) as error:
                self.close(statuses, after)
            self.assertEqual(error.exception.code, expected)

    def test_close_is_refused_when_the_pid_no_longer_runs_the_installed_executable(self):
        for arrangement in ({"owned": False}, {"before": table((PID, 1, "S"))}, {"before": table((PID, PID, "Z"))},
                            {"session": lambda pid: 1}, {"session": lambda pid: None}):
            with self.subTest(arrangement=list(arrangement)), self.assertRaises(ui.DiagnosticFailure) as error:
                self.close([None, 0], table((PID, PID, "Z")), **arrangement)
            self.assertEqual(error.exception.code, "ownership_lost")
        diagnostics = ui.Diagnostics({})
        bridge = fake_bridge(diagnostics)
        with patch.object(smoke, "process_status", return_value=None):
            with self.assertRaises(ui.DiagnosticFailure):
                ui.normal_close(bridge, Mock(pid=PID), diagnostics, self.executable, self.live, self.live,
                                OwnershipTests.Proc({PID: "/usr/bin/true"}), lambda pid: PID)
        self.assertEqual(bridge.commands, [])  # No close button press toward an unowned process.

    def test_close_button_failures_map_to_fixed_tokens(self):
        for reply, expected in [({"ok": False, "error": "window_count"}, "window_count"),
                                ({"ok": False, "error": "close_unavailable"}, "bridge_invalid")]:
            with self.subTest(reply=reply), self.assertRaises(ui.DiagnosticFailure) as error:
                self.close([None, 0], table((PID, PID, "Z")), close_reply=reply)
            self.assertEqual(error.exception.code, expected)

    def test_untrusted_table_after_exit_counts_as_a_remaining_member(self):
        for failure in (ValueError("incomplete"), OSError("ps"), smoke.subprocess.CalledProcessError(1, "ps")):
            with self.subTest(failure=type(failure).__name__), patch.object(ui, "process_table", side_effect=failure), \
                    self.assertRaises(ui.DiagnosticFailure) as error:
                self.close([None, 0], None)
            self.assertEqual(error.exception.code, "process_remaining")
        with patch.object(ui.subprocess, "run", return_value=Mock(stdout=" 4242 4242 4242 Z\n")), \
                self.assertRaises(ui.DiagnosticFailure) as error:
            self.close([None, 0], None)
        self.assertEqual(error.exception.code, "process_remaining")

    def stop(self, statuses, tables, process=None):
        """stop_group with scripted leader statuses, scripted table reads and recorded group signals."""
        process = Mock(pid=PID, returncode=None) if process is None else process
        signals = []
        values = list(tables)

        def read_table():
            # Scripted reads; the last value repeats for as long as the bound allows.
            value = values.pop(0) if len(values) > 1 else values[0]
            if isinstance(value, BaseException):
                raise value
            return value

        with patch.object(smoke, "process_status", side_effect=statuses), \
                patch.object(smoke, "signal_process_group", side_effect=lambda target, sig: signals.append((target.pid, sig))), \
                patch.object(ui.time, "sleep"):
            ui.stop_group(process, read_table, seconds=0.5)
        return process, signals

    def test_stop_group_proves_the_group_empty_before_the_anchored_leader_is_reaped(self):
        # The leader exits on TERM; one member outlives it for one table read, then
        # only the leader's zombie remains, and only then is the leader reaped.
        process = Mock(pid=PID, returncode=None)
        process.wait.side_effect = lambda timeout: setattr(process, "returncode", 0)
        process, signals = self.stop([None, None, 0], [table((PID, PID, "Z"), (PID + 1, PID, "S")), table((PID, PID, "Z"))],
                                     process)
        self.assertEqual(signals, [(PID, ui.signal.SIGTERM), (PID, ui.signal.SIGKILL)])
        process.wait.assert_called_once_with(timeout=0.5)
        process.poll.assert_not_called()

    def test_stop_group_leaves_the_leader_anchored_when_a_member_remains_or_the_table_is_unknown(self):
        survivor = table((PID, PID, "Z"), (PID + 1, PID, "S"))
        for tables in ([survivor], [smoke.subprocess.CalledProcessError(1, "ps")], [OSError("ps")],
                       [ValueError("incomplete")]):
            with self.subTest(tables=type(tables[0]).__name__), self.assertRaises(ui.DiagnosticFailure) as error:
                self.stop([None, 0], tables)
            self.assertEqual(error.exception.code, "process_remaining")
        process = Mock(pid=PID, returncode=None)
        with self.assertRaises(ui.DiagnosticFailure):
            self.stop([None, 0], [survivor], process)
        process.wait.assert_not_called()  # The group ID stays anchored for the retained-profile report.
        # The real table reader fed an old four-column, a truncated or an empty
        # ps snapshot fails closed the same way, with the leader still unreaped.
        for stdout in (" 4242 4242 ffffff801a2b3c4d Z\n", " 4242 4242 Z\n 4243 4242\n", ""):
            process = Mock(pid=PID, returncode=None)
            with self.subTest(stdout=stdout), patch.object(smoke, "process_status", side_effect=[None, 0]), \
                    patch.object(smoke, "signal_process_group"), patch.object(ui.time, "sleep"), \
                    patch.object(ui.subprocess, "run", return_value=Mock(stdout=stdout)), \
                    self.assertRaises(ui.DiagnosticFailure) as error:
                ui.stop_group(process, seconds=0.5)
            self.assertEqual(error.exception.code, "process_remaining")
            process.wait.assert_not_called()

    def test_stop_group_rejects_a_reaped_leader_and_a_leader_that_never_stops(self):
        # smoke.process_status itself refuses a reaped leader before any signal.
        process = Mock(pid=PID, returncode=0)
        with patch.object(smoke, "signal_process_group") as signalled:
            with self.assertRaises(RuntimeError):
                ui.stop_group(process, lambda: [], seconds=0.5)
        signalled.assert_not_called()
        process = Mock(pid=PID, returncode=None)
        process.wait.side_effect = lambda timeout: None  # Reaping recorded no status.
        with self.assertRaises(RuntimeError):
            self.stop(lambda target: None, [table((PID, PID, "Z"))], process)

    def test_bridge_close_and_acceptance_cleanup_stop_through_the_group_proof(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertNotIn("smoke.stop_process(", source)
        self.assertEqual(source.count("stop_group(self.process)"), 1)
        self.assertEqual(source.count("stop_group(process)"), 1)

    def test_acceptance_keeps_the_leader_unreaped_until_cleanup_and_never_touches_the_profile(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            ui.PackageProfile.create(NONCE, roots, home)
            executable = bundle(folder)
            bridge_binary = Path(folder) / "bridge"
            bridge_binary.write_bytes(b"binary")
            bridge_binary.chmod(0o700)
            for stop_error, expected_cleanup in [(None, "passed"), (ui.DiagnosticFailure("process_remaining"), "failed")]:
                record = {}
                trace = ui.Diagnostics(record)
                process = Mock(pid=PID, returncode=None)
                bridge = Mock()
                order = []
                bridge.close.side_effect = lambda: order.append("bridge")

                def stop(target):
                    order.append("app")
                    if stop_error:
                        raise stop_error

                with patch.object(ui, "guard"), patch.object(smoke, "require_nonreaping_wait"), \
                        patch.object(ui, "home_directory", return_value=home), \
                        patch.object(ui, "Bridge", return_value=bridge), patch.object(ui, "check_permissions"), \
                        patch.object(ui.subprocess, "Popen", return_value=process), \
                        patch.object(ui, "owned_mac_pid", return_value=True), \
                        patch.object(ui, "owned_window", side_effect=ui.DiagnosticFailure("window_count")), \
                        patch.object(ui, "stop_group", side_effect=stop), \
                        patch.object(smoke, "process_status", return_value=None):
                    with self.subTest(cleanup=expected_cleanup), self.assertRaises(ui.DiagnosticFailure) as error:
                        ui.acceptance(executable, NONCE, bridge_binary, record, trace)
                self.assertEqual(order, ["bridge", "app"])  # The app anchor is released last.
                self.assertEqual(error.exception.code, "process_remaining" if stop_error else "window_count")
                self.assertEqual(trace.value["failure"], "window_count")  # The first failure position is kept.
                self.assertEqual(trace.value["cleanup"], expected_cleanup)
                self.assertTrue(trace.value["profile_owned"])
                self.assertTrue((roots[0] / ui.MARKER).exists())
                process.poll.assert_not_called()
                process.wait.assert_not_called()

    def test_launch_uses_a_new_session_and_the_nonce_environment(self):
        with tempfile.TemporaryDirectory() as folder:
            home = temp_home(folder)
            roots = ui.profile_roots(home)
            ui.PackageProfile.create(NONCE, roots, home)
            executable = bundle(folder)
            bridge_binary = Path(folder) / "bridge"
            bridge_binary.write_bytes(b"binary")
            bridge_binary.chmod(0o700)
            process = Mock(pid=PID, returncode=None)
            with patch.object(ui, "guard"), patch.object(smoke, "require_nonreaping_wait"), \
                    patch.object(ui, "home_directory", return_value=home), patch.object(ui, "Bridge"), \
                    patch.object(ui, "check_permissions"), patch.object(ui, "OWNERSHIP_SECONDS", 0.2), \
                    patch.object(ui.subprocess, "Popen", return_value=process) as popen, \
                    patch.object(ui, "owned_mac_pid", return_value=False), \
                    patch.object(smoke, "process_status", return_value=None), patch.object(ui, "stop_group"):
                with self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.acceptance(executable, NONCE, bridge_binary, {}, ui.Diagnostics({}))
            self.assertEqual(error.exception.code, "install_state")
            kwargs = popen.call_args.kwargs
            self.assertTrue(kwargs["start_new_session"])
            self.assertEqual(kwargs["stdin"], ui.subprocess.DEVNULL)
            self.assertEqual(kwargs["env"][smoke.NONCE_ENV], NONCE)
            self.assertNotIn("MORPHEUS_QA_PROFILE_DIR", kwargs["env"])
            self.assertEqual(popen.call_args.args[0], [str(executable)])

    def test_bridge_command_requires_an_absolute_executable_regular_file(self):
        with tempfile.TemporaryDirectory() as folder:
            binary = Path(folder) / "bridge"
            binary.write_bytes(b"binary")
            binary.chmod(0o700)
            self.assertEqual(ui.bridge_command(binary), [str(binary)])
            binary.chmod(0o600)
            with self.assertRaises(ui.DiagnosticFailure):
                ui.bridge_command(binary)
            link = Path(folder) / "link"
            os.symlink(binary, link)
            binary.chmod(0o700)
            with self.assertRaises(ui.DiagnosticFailure):
                ui.bridge_command(link)
            with self.assertRaises(ui.DiagnosticFailure):
                ui.bridge_command(Path("relative/bridge"))


class SwiftSourceTests(unittest.TestCase):
    def setUp(self):
        self.source = SWIFT.read_text(encoding="utf-8")

    def test_guards_privacy_and_no_permission_requests(self):
        for forbidden in ["AXIsProcessTrustedWithOptions", "CGRequestPostEventAccess", "kAXTrustedCheckOptionPrompt",
                          "print(", "NSLog", "localizedDescription", "tccutil", "NSWorkspace.shared.open", "NSTask",
                          "Process()", "kAXValueAttribute as String, &"]:
            self.assertNotIn(forbidden, self.source)
        for required in ['environment["GITHUB_ACTIONS"] != "true"', 'environment["RUNNER_ENVIRONMENT"] != "github-hosted"',
                         'environment["ImageOS"] != "macos15"', "AXIsProcessTrusted()", "CGPreflightPostEventAccess()",
                         "AXUIElementGetPid", "kAXSubroleAttribute", "kAXFocusedUIElementAttribute", "kAXCloseButtonAttribute",
                         "kAXPressAction"]:
            self.assertIn(required, self.source)
        for token in re.findall(r'fail\("([a-z_]+)"\)', self.source):
            self.assertIn(token, smoke.MACOS_BRIDGE_ERRORS)

    def test_values_are_read_only_for_static_text_and_never_for_fields(self):
        value_reads = [match.start() for match in re.finditer(r"kAXValueAttribute", self.source)]
        self.assertEqual(len(value_reads), 1)
        name_function = self.source.split("func accessibleName")[1].split("func frontmostOwned")[0]
        self.assertIn("role == staticTextRole, let value = string(element, kAXValueAttribute as String)", name_function)
        self.assertLess(name_function.index("kAXTitleAttribute"), name_function.index("kAXValueAttribute"))
        self.assertIn('let staticTextRole = "AXStaticText"', self.source)

    def test_every_key_transition_is_posted_to_the_owned_pid_after_its_own_frontmost_proof(self):
        send_key = self.source.split("func sendKey")[1].split("func close")[0]
        loop = send_key.split("for transition in transitions {")[1]
        self.assertLess(loop.index("guard frontmostOwned(target.pid) else"), loop.index("event.postToPid(target.pid)"))
        self.assertEqual(send_key.count("postToPid("), 1)
        self.assertNotIn("CGEventTapLocation", self.source)
        self.assertNotIn(".post(tap:", self.source)
        self.assertNotIn("CGEventPost(", self.source)
        # A failed proof mid-chord ends the chord with a distinct token and nothing further is posted.
        self.assertIn('fail(posted == 0 ? "foreground_unowned" : "input_rejected")', loop)
        self.assertIn("return", loop.split('fail(posted == 0 ? "foreground_unowned" : "input_rejected")')[1].split("}")[0])
        self.assertIn('"posted": posted', send_key)

    def test_snapshot_bounds_are_enforced_before_enumeration_and_serialization(self):
        snapshot_source = self.source.split("func snapshot()")[1].split("func focus(")[0]
        children_loop = snapshot_source.split("for child in children {")[1].split("queue.append")[0]
        self.assertIn("elements.count + (queue.count - index) >= maxNodes", children_loop)
        self.assertIn("let maxNodes = 5000", self.source)
        self.assertIn("let maxName = 256", self.source)
        self.assertIn("let maxLine = 256", self.source)
        self.assertLess(snapshot_source.index("accessibleName(element, role: role)"), snapshot_source.index('node["name"] = name'))
        focused_source = self.source.split("func focused()")[1].split("struct Transition")[0]
        self.assertIn("accessibleName(element, role: role)", focused_source)
        self.assertEqual(ui.TREE_BOUND, 5000)
        self.assertEqual(ui.NAME_BOUND, 256)

    def test_attach_and_close_verify_the_owned_process(self):
        attach = self.source.split("func attach(")[1].split("func windows()")[0]
        self.assertIn("guard let identifier = running.bundleIdentifier else", attach)
        self.assertIn("guard identifier == bundle else", attach)
        self.assertIn("guard let path = canonicalPath(url) else", attach)
        self.assertIn("guard path == executable else", attach)
        self.assertIn("pidValue <= Int(Int32.max)", attach)
        self.assertLess(attach.index("target = nil"), attach.index('command["pid"]'))
        self.assertLess(attach.index("elements = []"), attach.index('command["pid"]'))
        # The target is set only after every identity check, and once.
        self.assertEqual(attach.count("target = Target("), 1)
        self.assertLess(attach.index('fail("attach_executable_mismatch")'), attach.index("target = Target("))
        for token in ("attach_application_missing", "attach_bundle_missing", "attach_bundle_mismatch",
                      "attach_executable_missing", "attach_executable_unresolved", "attach_executable_mismatch"):
            self.assertIn('fail("' + token + '")', attach)
            self.assertIn(token, smoke.MACOS_BRIDGE_ERRORS)
        # The bridge never waits or retries an identity itself; the driver owns the bound.
        for forbidden in ("while", "repeat", "sleep", "usleep"):
            self.assertNotIn(forbidden, attach)
        # Foundation's standardization strips a leading /private, so identity
        # compares realpath results with the driver's resolved executable.
        self.assertNotIn("standardizedFileURL", self.source)
        self.assertNotIn("resolvingSymlinksInPath", self.source)
        canonical = self.source.split("func canonicalPath(")[1].split("func refreshApplicationState()")[0]
        self.assertIn("\nimport Darwin\n", self.source)  # realpath and free come from Darwin explicitly.
        self.assertIn("realpath(representation, nil)", canonical)
        self.assertIn("defer { free(resolved) }", canonical)
        self.assertIn("guard url.isFileURL else { return nil }", canonical)
        # One bounded turn of the bridge's main run loop in a common mode,
        # before the lookup, and nowhere else.
        refresh = self.source.split("func refreshApplicationState()")[1].split("func attach(")[0]
        self.assertIn("RunLoop.main.run(mode: .default, before: Date(timeIntervalSinceNow: runLoopTurn))", refresh)
        self.assertIn("let runLoopTurn: TimeInterval = 0.05", self.source)
        self.assertEqual(self.source.count("RunLoop"), 1)
        self.assertEqual(self.source.count("refreshApplicationState()"), 2)
        self.assertLess(attach.index("refreshApplicationState()"),
                        attach.index("NSRunningApplication(processIdentifier: pid)"))
        self.assertLess(attach.index('fail("invalid_command")'), attach.index("refreshApplicationState()"))
        close = self.source.split("func close()")[1].split('reply(["ready": true])')[0]
        self.assertIn("list.count == 1, owner(list[0]) == target.pid", close)
        self.assertIn("owner(button) == target.pid", close)
        self.assertNotIn("kill", close)
        self.assertNotIn("terminate", close)


class DiagnosticsAndReportTests(unittest.TestCase):
    def test_diagnostics_map_exceptions_to_fixed_tokens_and_keep_the_first_failure(self):
        trace = ui.Diagnostics({})
        trace.step("password_semantics")
        trace.failed(ui.DiagnosticFailure("password_semantics"))
        trace.failed(OSError("/Users/private/path"))
        self.assertEqual(trace.value["failure"], "password_semantics")
        for error, code in [(AttributeError("x"), "attribute_error"), (TypeError("x"), "type_error"),
                            (KeyError("x"), "lookup_error"), (ValueError("x"), "value_error"),
                            (RuntimeError("x"), "runtime_error"), (OSError("x"), "os_error"),
                            (ui.DiagnosticFailure("PRIVATE_TOKEN"), "unclassified"), (Exception("x"), "unclassified")]:
            trace = ui.Diagnostics({})
            trace.failed(error)
            self.assertEqual(trace.value["failure"], code)
            self.assertNotIn("PRIVATE", json.dumps(trace.value))

    def test_config_persistence_requires_the_exact_saved_scale(self):
        with tempfile.TemporaryDirectory() as folder:
            config = Path(folder) / "config.toml"
            config.write_text("schema_version = 1\n[appearance]\nscale_percent = 120\n")
            ui.config_persisted(config, ui.Diagnostics({}), seconds=1)
            for text in ["schema_version = 1\n[appearance]\nscale_percent = 110\n", "not toml ["]:
                config.write_text(text)
                with self.subTest(text=text), self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.config_persisted(config, ui.Diagnostics({}), seconds=0.2)
                self.assertEqual(error.exception.code, "config_mismatch")

    def test_main_publishes_only_the_fixed_report_and_requires_three_arguments(self):
        captured = io.StringIO()
        with patch.object(ui.sys, "argv", ["driver", "/synthetic/Morpheus.app/Contents/MacOS/morpheus-app", NONCE, "/work/bridge"]), \
                patch.object(ui, "acceptance", side_effect=RuntimeError("PRIVATE_TREE_OR_PATH")), \
                contextlib.redirect_stdout(captured):
            self.assertEqual(ui.main(), 1)
        self.assertNotIn("PRIVATE_TREE_OR_PATH", captured.getvalue())
        parsed = smoke.keyboard_report(captured.getvalue().encode(), check="installed_macos_keyboard_ax",
                                       validate=smoke.validate_macos_keyboard_diagnostic)
        self.assertEqual((parsed["phase"], parsed["diagnostic"]["failure"]), ("setup", "runtime_error"))
        with patch.object(ui.sys, "argv", ["driver", "/synthetic/app", NONCE]), patch.object(ui, "acceptance") as acceptance, \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(ui.main(), 1)
        acceptance.assert_not_called()

    def test_smoke_parser_rejects_arbitrary_values_and_incomplete_success(self):
        parse = lambda value: smoke.keyboard_report(json.dumps(value).encode(), check="installed_macos_keyboard_ax",
                                                    validate=smoke.validate_macos_keyboard_diagnostic)
        self.assertEqual(parse(report()), report())
        bad = [diagnostic(step="PRIVATE"), diagnostic(operation="PRIVATE"), diagnostic(failure="PRIVATE"),
               diagnostic(bridge_error="PRIVATE"), diagnostic(password_semantics="PRIVATE"),
               diagnostic(selector_matches=3), diagnostic(activation_recoveries=-1), diagnostic(bounded_snapshots="0"),
               diagnostic(permission_checked=1), diagnostic(profile_owned="yes"), diagnostic(private_text="SECRET"),
               diagnostic(foreground_recoveries=0)]
        for value in bad:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(report(diagnostic=value))
        incomplete = [diagnostic(password_semantics="unchecked"), diagnostic(cleanup="failed"),
                      diagnostic(profile_owned=False), diagnostic(ax_root_seen=False), diagnostic(permission_checked=False),
                      diagnostic(bridge_error="unowned"), diagnostic(step="appearance")]
        for value in incomplete:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(report(diagnostic=value))
        for value in [report(normal_window_close=False), report(screen_reader_acceptance=True),
                      report(check="installed_windows_keyboard_uia")]:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(value)
        for failure in ["permission_unavailable", "ownership_lost", "input_rejected", "bridge_overflow", "profile_invalid"]:
            failed = report(status="failed", phase="setup", sign_in_semantics=False, account_menu_keyboard=False,
                            appearance_config=False, normal_window_close=False,
                            diagnostic=diagnostic(step="permission", failure=failure, cleanup="passed",
                                                  owned_window_seen=False, ax_root_seen=False, homeserver_seen=False,
                                                  username_seen=False, password_seen=False, password_semantics="unchecked",
                                                  selector_matches=0, profile_owned=False))
            self.assertEqual(parse(failed)["status"], "failed")
        with self.assertRaises(RuntimeError):
            smoke.keyboard_report(json.dumps(report()).encode())  # The Linux parser does not accept it.
        with self.assertRaises(RuntimeError):
            smoke.keyboard_report(json.dumps(report()).encode(), check="installed_macos_keyboard_ax",
                                  validate=smoke.validate_windows_keyboard_diagnostic)  # Nor the Windows validator.

    def test_driver_launch_keeps_its_stream_private_and_reports_pass_and_verified_stop_separately(self):
        captured = io.StringIO()

        def run(command, **kwargs):
            kwargs["stdout"].write(b"PRIVATE accessibility text\n" + json.dumps(report()).encode() + b"\n")
            return Mock(returncode=0)

        with patch.object(smoke.subprocess, "run", side_effect=run) as launch, contextlib.redirect_stdout(captured):
            self.assertEqual(smoke.macos_keyboard_launch(Path("/i/Morpheus.app/Contents/MacOS/morpheus-app"), NONCE,
                                                         Path("/work/bridge")), (True, True))
        self.assertNotIn("PRIVATE", captured.getvalue())
        self.assertEqual(json.loads(captured.getvalue()), report())
        command = launch.call_args.args[0]
        self.assertEqual(Path(command[1]).name, "macos_package_ui.py")
        self.assertEqual(command[3:], [NONCE, "/work/bridge"])
        self.assertEqual(launch.call_args.kwargs["stdin"], smoke.subprocess.DEVNULL)

        def failing(stopped):
            def run(command, **kwargs):
                kwargs["stdout"].write(json.dumps(report(
                    status="failed", phase="appearance_config", appearance_config=False, normal_window_close=False,
                    diagnostic=diagnostic(step="appearance", failure="wait_timeout",
                                          cleanup="passed" if stopped else "failed"))).encode())
                return Mock(returncode=1)
            return run

        for stopped in (True, False):
            with patch.object(smoke.subprocess, "run", side_effect=failing(stopped)), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(smoke.macos_keyboard_launch(Path("/i/Morpheus.app/Contents/MacOS/morpheus-app"), NONCE,
                                                             Path("/work/bridge")), (False, stopped))


if __name__ == "__main__":
    unittest.main()
