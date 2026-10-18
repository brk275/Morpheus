import contextlib
import ctypes
import io
import json
import os
from pathlib import Path
import re
import tempfile
import unittest
from unittest.mock import Mock, patch

from scripts import ci_scope, package_smoke as smoke, run_windows_capture_acceptance as capture
from scripts import windows_package_ui as ui

HOSTED = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Windows",
          "ImageOS": "win25"}
SCRIPT = Path(__file__).resolve().parents[1] / "windows_package_ui.ps1"
SOURCE = Path(__file__).resolve().parents[1] / "windows_package_ui.py"
NONCE = "0123456789abcdef0123456789abcdef"


def fact(**changes):
    """One published inventory entry: the synthetic main window by default."""
    return {"family": "winit_window", "style": 0x16CF0000, "exstyle": 0x00040100, "visible": True,
            "owner": "none", "cloaked": False, "width": 480, "height": 320, **changes}


def event_target_fact(**changes):
    return fact(**{"family": "winit_event_target", "style": 0x90000000, "exstyle": 0x080800A0,
                   "width": 0, "height": 0, **changes})


def inventory(*windows, **overrides):
    return {"status": "collected", "windows": sorted(windows, key=lambda item: json.dumps(item, sort_keys=True)),
            "truncated": False, "discarded": 0, **overrides}


def diagnostic(**overrides):
    return {"step": "complete", "operation": "none", "failure": "none", "bridge_error": "none",
            "owned_window_seen": True, "uia_root_seen": True, "homeserver_seen": True,
            "username_seen": True, "password_seen": True,
            "selector_matches": 1, "incomplete_passes": 0, "foreground_recoveries": 0,
            "bounded_snapshots": 0, "password_semantics": "protected",
            "window_candidates": 1, "event_targets_ignored": 0,
            "window_inventory": inventory(fact(), event_target_fact()),
            "traversed_node_matched": False, "cleanup": "passed", "profile_owned": True, **overrides}


def report(**overrides):
    return {"check": "installed_windows_keyboard_uia", "status": "passed", "phase": "complete",
            "sign_in_semantics": True, "account_menu_keyboard": True, "appearance_config": True,
            "normal_window_close": True, "app_log_public": False, "screen_reader_acceptance": False,
            "diagnostic": diagnostic(), **overrides}


def window_record(**changes):
    return {"pid": 77, "class": "Window Class", "style": 0x16CF0000, "exstyle": 0x00040100,
            "owner": None, "rect": (10, 20, 490, 340), "visible": True, "cloaked": False, **changes}


class WindowQuery:
    """Read-only provider double: no Win32 call, process, focus or keyboard.
    A handle that is not (or no longer) present fails like a destroyed window."""
    def __init__(self):
        self.windows = {
            65538: window_record(),
            2: window_record(**{"class": "Winit Thread Event Target", "style": 0x90000000,
                                "exstyle": 0x080800A0, "rect": (0, 0, 0, 0)}),
        }

    def record(self, window):
        if window not in self.windows:
            raise ui.window_probe.ApiError()
        return self.windows[window]

    def process_of(self, window):
        return self.record(window)["pid"]

    def class_name(self, window):
        return self.record(window)["class"]

    def long(self, window, index):
        return self.record(window)["style" if index == ui.window_probe.GWL_STYLE else "exstyle"]

    def owner(self, window):
        return self.record(window)["owner"]

    def rect(self, window):
        return self.record(window)["rect"]

    def is_visible(self, window):
        return self.record(window)["visible"]

    def cloaked(self, window):
        return self.windows[window]["cloaked"] if window in self.windows else None

    def is_window(self, window):
        return window in self.windows


def node(id, name="", *, parent=0, pid=77, type="custom", password=False, focused=False, error=False,
         truncated=False):
    return {"id": id, "parent": parent, "pid": pid, "name": name, "type": type,
            "password": password, "focused": focused, "error": error, "truncated": truncated}


def snapshot(*nodes, bounded=False):
    return {"ok": True, "nodes": [node(0, pid=77, type="window", parent=-1), *nodes], "bounded": bounded}


def focused_reply(name, owned=True, present=True, truncated=False):
    return {"ok": True, "present": present, "owned": owned, "name": name, "truncated": truncated}


def fake_bridge(diagnostics, snapshots=(), focused=(), key_replies=()):
    """The real Bridge with only its transport replaced: call() keeps the
    production reply validation and error handling, replies are scripted."""
    bridge = object.__new__(ui.Bridge)
    bridge.job, bridge.handle, bridge.reader, bridge.diagnostics = None, None, None, diagnostics
    bridge.commands = []
    bridge.scripted = {"snapshot": list(snapshots), "key": list(key_replies)}
    bridge.focused_script = focused
    bridge.focused_calls = 0
    bridge.pending = None

    def transmit(command):
        bridge.commands.append(command)
        bridge.pending = command

    def reply():
        op = bridge.pending["op"]
        if op == "focused":
            if callable(bridge.focused_script):
                value = bridge.focused_script(bridge.focused_calls)
            else:
                value = bridge.focused_script[bridge.focused_calls]
            bridge.focused_calls += 1
            return value
        if op == "focus":
            return {"ok": True}
        if op == "key" and not bridge.scripted["key"]:
            return {"ok": True, "recovered": False}
        return bridge.scripted[op].pop(0)

    bridge.alive = lambda: True
    bridge.transmit = transmit
    bridge.reply = reply
    return bridge


def driver(snapshots=(), focused=(), key_replies=(), pid=77, alive=True):
    diagnostics = ui.Diagnostics({})
    bridge = fake_bridge(diagnostics, snapshots, focused, key_replies)
    job = Mock()
    job.kernel.WaitForSingleObject.return_value = 258 if alive else 0
    return ui.WindowsUI(bridge, pid, job, 20, diagnostics), bridge, diagnostics


def temp_roots(base, name=""):
    """Mocked RoamingAppData/LocalAppData bases exist, as on a real runner; the
    brk275 organization chain below them never does unless a test arranges it."""
    roots = []
    for folder in ("roaming", "local"):
        known = Path(base) / name / folder
        known.mkdir(parents=True, exist_ok=True)
        roots.append(known / "brk275" / "Morpheus")
    return roots


class GuardTests(unittest.TestCase):
    def test_guard_requires_fresh_hosted_windows_2025(self):
        ui.guard(HOSTED, "win32")
        for missing in HOSTED:
            with self.subTest(missing=missing), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.guard({key: value for key, value in HOSTED.items() if key != missing}, "win32")
            self.assertEqual(error.exception.code, "guard")
        with self.assertRaises(ui.DiagnosticFailure):
            ui.guard(dict(HOSTED, ImageOS="win22"), "win32")
        with self.assertRaises(ui.DiagnosticFailure):
            ui.guard(HOSTED, "linux")

    def test_only_the_two_reviewed_windows_2025_images_are_accepted(self):
        self.assertEqual(set(ui.HOSTED_WINDOWS_IMAGES), {"win25", "win25-vs2026"})
        for image in ui.HOSTED_WINDOWS_IMAGES:
            with self.subTest(image=image):
                ui.guard(dict(HOSTED, ImageOS=image), "win32")
        for image in ("win22", "win26", "win25-unknown", "", None):
            with self.subTest(image=image), self.assertRaises(ui.DiagnosticFailure):
                ui.guard(dict(HOSTED, ImageOS=image), "win32")

    def test_metadata_preflight_has_no_application_or_profile_side_effects(self):
        for image in ui.HOSTED_WINDOWS_IMAGES:
            visible = io.StringIO()
            with patch.dict(os.environ, dict(HOSTED, ImageOS=image), clear=True), \
                    patch.object(ui.sys, "platform", "win32"), \
                    patch.object(ui.sys, "argv", ["driver", "--check-host"]), \
                    patch.object(ui, "acceptance") as acceptance, \
                    patch.object(ui.PackageProfile, "open") as profile, \
                    patch.object(capture, "WindowsJob") as job, contextlib.redirect_stdout(visible):
                self.assertEqual(ui.main(), 0)
            self.assertEqual(json.loads(visible.getvalue()), {
                "check": "windows_keyboard_host_preflight", "status": "passed", "failure": "none",
            })
            acceptance.assert_not_called()
            profile.assert_not_called()
            job.assert_not_called()

    def test_preflight_still_rejects_nonhosted_and_unknown_images_without_mutation(self):
        for changed in ({"GITHUB_ACTIONS": "false"}, {"RUNNER_ENVIRONMENT": "self-hosted"},
                        {"RUNNER_OS": "Linux"}, {"ImageOS": "win22"}, {"ImageOS": "win25-other"}):
            visible = io.StringIO()
            with patch.dict(os.environ, HOSTED | changed, clear=True), \
                    patch.object(ui.sys, "platform", "win32"), \
                    patch.object(ui.sys, "argv", ["driver", "--check-host"]), \
                    patch.object(ui, "acceptance") as acceptance, \
                    patch.object(ui, "profile_roots") as roots, contextlib.redirect_stdout(visible):
                self.assertEqual(ui.main(), 1)
            self.assertEqual(json.loads(visible.getvalue()), {
                "check": "windows_keyboard_host_preflight", "status": "failed", "failure": "guard",
            })
            acceptance.assert_not_called()
            roots.assert_not_called()

    def test_invalid_preflight_arguments_cannot_enter_the_installed_driver(self):
        visible = io.StringIO()
        with patch.object(ui.sys, "argv", ["driver", "--check-host", "private-value"]), \
                patch.object(ui, "guard") as guard, patch.object(ui, "acceptance") as acceptance, \
                contextlib.redirect_stdout(visible):
            self.assertEqual(ui.main(), 1)
        self.assertEqual(json.loads(visible.getvalue()), {
            "check": "windows_keyboard_host_preflight", "status": "failed", "failure": "arguments",
        })
        guard.assert_not_called()
        acceptance.assert_not_called()
        self.assertNotIn("private-value", visible.getvalue())

    def test_host_preflight_precedes_compilers_and_keeps_interactive_desktop_guard(self):
        workflow = (SOURCE.parents[1] / ".github/workflows/package-qa.yml").read_text()
        source_check = workflow.index("Verify exact reviewed source checkout")
        preflight = workflow.index("Check Windows keyboard host before building")
        compiler = workflow.index("Install pinned Rust toolchain")
        self.assertLess(source_check, preflight)
        self.assertLess(preflight, compiler)
        block = workflow[preflight:workflow.index("Require reviewed installed capture adapter", preflight)]
        self.assertIn("if: runner.os == 'Windows'", block)
        self.assertIn("shell: powershell", block)
        self.assertIn("[Environment]::UserInteractive", block)
        self.assertIn("python3 scripts/windows_package_ui.py --check-host", block)

    def test_guard_failure_cannot_create_a_job_or_touch_a_profile(self):
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}, clear=False), \
                patch.object(ui.sys, "platform", "win32"), \
                patch.object(capture, "WindowsJob") as job, patch.object(ui, "profile_roots") as roots:
            with self.assertRaises(ui.DiagnosticFailure):
                ui.acceptance(Path("/synthetic/morpheus-app.exe"), NONCE, {})
        job.assert_not_called()
        roots.assert_not_called()

    def test_installed_executable_must_match_the_single_registration(self):
        with tempfile.TemporaryDirectory() as folder:
            installed = Path(folder) / "Installed Morpheus"
            installed.mkdir()
            executable = installed / "morpheus-app.exe"
            entry = {"DisplayName": "Morpheus", "DisplayVersion": "0.1.0", "InstallLocation": f'"{installed}"'}
            with patch.object(smoke, "uninstall_registry", return_value=[entry]):
                ui.installed_executable(executable)
            for entries, path in [([], executable), ([entry, entry], executable),
                                  ([dict(entry, DisplayName="Other")], executable),
                                  ([entry], installed / "other.exe"),
                                  ([entry], Path(folder) / "elsewhere" / "morpheus-app.exe")]:
                with self.subTest(entries=entries, path=path), \
                        patch.object(smoke, "uninstall_registry", return_value=entries), \
                        self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.installed_executable(path)
                self.assertEqual(error.exception.code, "install_state")

    def test_bridge_command_is_a_native_argument_list_with_identifiers_only(self):
        with patch.dict(os.environ, {"SystemRoot": r"C:\Windows"}):
            command = ui.bridge_command(Path("driver.ps1"), 4242, 65538)
        self.assertEqual(command[0], str(Path(r"C:\Windows") / "System32" / "WindowsPowerShell" / "v1.0" / "powershell.exe"))
        self.assertEqual(command[1:6], ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        self.assertEqual(command[7:], ["-ProcessId", "4242", "-WindowHandle", "65538"])
        for pid, window in [(0, 1), (1, 0), (-1, 5), ("7", 5), (7, 5.0)]:
            with self.subTest(pid=pid, window=window), self.assertRaises(ui.DiagnosticFailure):
                ui.bridge_command(Path("driver.ps1"), pid, window)

    def test_manual_only_helpers_do_not_trigger_the_native_matrix(self):
        result = ci_scope.classify(["scripts/windows_package_ui.py", "scripts/windows_package_ui.ps1",
                                    "scripts/tests/test_windows_package_ui.py", "scripts/package_smoke.py",
                                    "docs/PACKAGING.md"])
        self.assertFalse(result["native"])


class ShortAllocation:
    """A fake CoTaskMem block holding exactly the characters the shell wrote.

    Any read past its last character models an access beyond the allocation
    and fails the test immediately instead of returning stale memory.
    """
    def __init__(self, text, base=0x1000):
        self.text, self.base = text, base
        self.reads = []
        self.stride = ctypes.sizeof(ctypes.c_wchar)

    def read_char(self, address):
        offset = address - self.base
        if offset < 0 or offset % self.stride:
            raise AssertionError("misaligned wide read")
        index = offset // self.stride
        if index >= len(self.text):
            raise AssertionError("read past the shell allocation")
        self.reads.append(index)
        return self.text[index]


class KnownFolderTests(unittest.TestCase):
    """Native shell calls are mocked at the ctypes boundary; nothing runs on Windows here."""
    class Shell:
        def __init__(self, status=0, pointer=0x1000, clsid_status=0):
            self.status, self.pointer, self.clsid_status = status, pointer, clsid_status
            self.CLSIDFromString = Mock(side_effect=self.clsid)
            self.SHGetKnownFolderPath = Mock(side_effect=self.get)
            self.CoTaskMemFree = Mock()

        def clsid(self, text, guid):
            self.guid_text = text
            return self.clsid_status

        def get(self, guid, flags, token, out):
            self.flags, self.token = flags, token
            out._obj.value = self.pointer
            return self.status

    def resolve(self, shell, text):
        allocation = ShortAllocation(text)
        path = ui.known_folder("roaming", libraries=(shell, shell), read_char=allocation.read_char)
        return path, allocation

    def test_success_reads_exactly_up_to_the_terminator_and_frees_the_allocation(self):
        shell = self.Shell()
        path, allocation = self.resolve(shell, "C:\\Users\\qa\\AppData\\Roaming\0")
        self.assertEqual(path, Path("C:\\Users\\qa\\AppData\\Roaming"))
        self.assertEqual(shell.guid_text, ui.KNOWN_FOLDERS["roaming"])
        self.assertEqual((shell.flags, shell.token), (0, None))
        # One read per character plus the terminator, in order, and nothing past it.
        self.assertEqual(allocation.reads, list(range(len("C:\\Users\\qa\\AppData\\Roaming") + 1)))
        shell.CoTaskMemFree.assert_called_once()
        self.assertEqual(shell.CoTaskMemFree.call_args.args[0].value, 0x1000)

    def test_reads_advance_by_the_wide_character_stride(self):
        allocation = ShortAllocation("ab\0", base=0x2000)
        self.assertEqual(ui.read_terminated(0x2000, allocation.read_char), "ab")
        self.assertEqual(allocation.reads, [0, 1, 2])
        with patch.object(ui, "KNOWN_FOLDER_CHARS", 3):
            self.assertEqual(ui.read_terminated(0x2000, allocation.read_char), "ab")
        source = SOURCE.read_text(encoding="utf-8")
        # The only wide read in the driver is one character at the given address.
        self.assertEqual(source.count("wstring_at("), 1)
        self.assertIn("ctypes.wstring_at(address, 1)", source)
        self.assertIn("ctypes.sizeof(ctypes.c_wchar)", source)

    def test_unterminated_allocation_stops_at_the_cap_and_frees(self):
        shell = self.Shell()
        allocation = ShortAllocation("ABCDEFGHIJ")  # No terminator; longer than the cap below.
        with patch.object(ui, "KNOWN_FOLDER_CHARS", 8), self.assertRaises(RuntimeError):
            ui.known_folder("roaming", libraries=(shell, shell), read_char=allocation.read_char)
        self.assertEqual(allocation.reads, list(range(8)))
        shell.CoTaskMemFree.assert_called_once()

    def test_failure_status_frees_nothing_and_empty_or_relative_paths_still_free(self):
        shell = self.Shell(status=-2147024894, pointer=0)
        with self.assertRaises(RuntimeError):
            self.resolve(shell, "")
        shell.CoTaskMemFree.assert_not_called()
        # Even an error status may return an allocation. Free it without
        # attempting a read; a null-only error fake cannot prove this path.
        shell = self.Shell(status=-2147024894, pointer=0x1000)
        read_char = Mock(side_effect=AssertionError("Failed shell call must not be read"))
        with self.assertRaises(RuntimeError):
            ui.known_folder("roaming", libraries=(shell, shell), read_char=read_char)
        read_char.assert_not_called()
        shell.CoTaskMemFree.assert_called_once()
        self.assertEqual(shell.CoTaskMemFree.call_args.args[0].value, 0x1000)
        for text in ["\0", "relative\\path\0"]:
            shell = self.Shell()
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                self.resolve(shell, text)
            shell.CoTaskMemFree.assert_called_once()
        shell = self.Shell(clsid_status=-2147221005)
        with self.assertRaises(RuntimeError):
            self.resolve(shell, "")
        shell.SHGetKnownFolderPath.assert_not_called()
        shell.CoTaskMemFree.assert_not_called()

    def test_native_signatures_are_declared_before_use(self):
        shell32, ole32 = Mock(), Mock()
        ui.declare_shell_abi(shell32, ole32)
        self.assertEqual(ole32.CLSIDFromString.argtypes, [ctypes.c_wchar_p, ctypes.c_void_p])
        self.assertIs(ole32.CLSIDFromString.restype, ctypes.c_long)
        self.assertEqual(shell32.SHGetKnownFolderPath.argtypes[:3], [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_void_p])
        self.assertIs(shell32.SHGetKnownFolderPath.restype, ctypes.c_long)
        self.assertEqual(ole32.CoTaskMemFree.argtypes, [ctypes.c_void_p])
        self.assertIsNone(ole32.CoTaskMemFree.restype)
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn("declare_shell_abi(shell32, ole32)", source.split("def shell_libraries")[1])

    def test_profile_roots_come_from_known_folders_not_environment_variables(self):
        with patch.dict(os.environ, {"APPDATA": "/env/roaming", "LOCALAPPDATA": "/env/local"}), \
                patch.object(ui, "known_folder", side_effect=lambda name: Path("/known") / name) as resolve:
            roots = ui.profile_roots()
        self.assertEqual(roots, [Path("/known/roaming/brk275/Morpheus"), Path("/known/local/brk275/Morpheus")])
        self.assertEqual([call.args[0] for call in resolve.call_args_list], ["roaming", "local"])
        source = SOURCE.read_text(encoding="utf-8")
        for forbidden in ['"APPDATA"', '"LOCALAPPDATA"', "USERPROFILE", "winreg", "SetKnownFolderPath",
                          "MORPHEUS_QA_PROFILE_DIR\"]", "subprocess"]:
            self.assertNotIn(forbidden, source.replace('env.pop("MORPHEUS_QA_PROFILE_DIR", None)', ""))
        self.assertEqual(ui.KNOWN_FOLDERS["roaming"], "{3EB685DB-65F9-4CF6-A03A-E3EF65729F3D}")
        self.assertEqual(ui.KNOWN_FOLDERS["local"], "{F1B32785-6FBA-4FCF-9D55-7B8E7F157091}")


class ProfileLifecycleTests(unittest.TestCase):
    def test_release_profile_override_is_never_forwarded_and_no_folder_is_redirected(self):
        with patch.dict(os.environ, {"MORPHEUS_QA_PROFILE_DIR": "/should/not/be/used", "APPDATA": "/runner/roaming"}):
            env = ui.environment_for(NONCE)
            self.assertEqual(set(env) - set(os.environ), {smoke.NONCE_ENV})
            self.assertEqual(env["APPDATA"], "/runner/roaming")
        self.assertNotIn("MORPHEUS_QA_PROFILE_DIR", env)
        self.assertEqual(env[smoke.NONCE_ENV], NONCE)

    def test_create_refuses_any_existing_organization_folder_root_or_reparse_point(self):
        with tempfile.TemporaryDirectory() as folder:
            base = Path(folder)
            elsewhere = base / "elsewhere"
            elsewhere.mkdir()
            (elsewhere / "vault").write_bytes(b"not ours")
            cases = {
                "organization_folder": lambda roots: roots[0].parent.mkdir(parents=True),
                "existing_profile_with_data": lambda roots: ((roots[0] / "data").mkdir(parents=True),
                                                             (roots[0] / "data" / "store").write_bytes(b"account")),
                "root_file": lambda roots: (roots[1].parent.mkdir(parents=True), roots[1].write_text("x")),
                "root_symlink": lambda roots: (roots[0].parent.mkdir(parents=True),
                                               os.symlink(elsewhere, roots[0], target_is_directory=True)),
                "dangling_symlink": lambda roots: (roots[1].parent.mkdir(parents=True),
                                                   os.symlink(base / "missing", roots[1])),
                "organization_symlink": lambda roots: os.symlink(elsewhere, roots[0].parent, target_is_directory=True),
            }
            for name, arrange in cases.items():
                with self.subTest(case=name):
                    roots = temp_roots(base, name)
                    arrange(roots)
                    before = sorted(str(path) for path in (base / name).rglob("*"))
                    with self.assertRaises(RuntimeError):
                        ui.PackageProfile.create(NONCE, roots)
                    self.assertEqual(sorted(str(path) for path in (base / name).rglob("*")), before)
            self.assertEqual((elsewhere / "vault").read_bytes(), b"not ours")

    def test_create_seeds_markers_and_config_and_a_failed_create_removes_only_its_own_directories(self):
        with tempfile.TemporaryDirectory() as folder:
            roots = temp_roots(folder)
            profile = ui.PackageProfile.create(NONCE, roots)
            self.assertEqual(profile.config, roots[0] / "config" / "config.toml")
            self.assertEqual(profile.config.read_text(encoding="utf-8"), ui.SEED_CONFIG)
            for root in roots:
                self.assertEqual((root / ui.MARKER).read_text(encoding="ascii"), NONCE)
            self.assertTrue(profile.remove())
            original = Path.write_text

            def failing(self, data, *args, **kwargs):
                if data == ui.SEED_CONFIG:
                    raise OSError("disk")
                return original(self, data, *args, **kwargs)

            with patch.object(Path, "write_text", failing), self.assertRaises(OSError):
                ui.PackageProfile.create(NONCE, roots)
            for root in roots:
                self.assertFalse(root.exists())
                self.assertFalse(root.parent.exists())
        for nonce in ["", "short", "G" * 32, NONCE.upper(), 12]:
            with self.subTest(nonce=nonce), self.assertRaises(RuntimeError):
                ui.PackageProfile.create(nonce, roots)

    def test_open_accepts_only_the_marked_profile_even_after_the_app_created_its_store(self):
        with tempfile.TemporaryDirectory() as folder:
            roots = temp_roots(folder)
            profile = ui.PackageProfile.create(NONCE, roots)
            # The release executable's first launch creates these inside the profile.
            (roots[0] / "data").mkdir()
            (roots[0] / "data" / "accounts.db").write_bytes(b"synthetic store")
            (roots[1] / "cache").mkdir()
            (roots[1] / "cache" / "fonts").write_bytes(b"cache")
            opened = ui.PackageProfile.open(NONCE, roots)
            self.assertEqual(opened.config, profile.config)
            for nonce in ["f" * 32, "not-a-nonce"]:
                with self.subTest(nonce=nonce), self.assertRaises(ui.DiagnosticFailure) as error:
                    ui.PackageProfile.open(nonce, roots)
                self.assertEqual(error.exception.code, "profile_invalid")
            (roots[1] / ui.MARKER).unlink()
            with self.assertRaises(ui.DiagnosticFailure) as error:
                ui.PackageProfile.open(NONCE, roots)
            self.assertEqual(error.exception.code, "profile_invalid")
            (roots[1] / ui.MARKER).write_text(NONCE, encoding="ascii")
            profile.config.unlink()
            with self.assertRaises(ui.DiagnosticFailure):
                ui.PackageProfile.open(NONCE, roots)
            profile.config.write_text(ui.SEED_CONFIG, encoding="utf-8")
            self.assertTrue(ui.PackageProfile.open(NONCE, roots).remove())
            self.assertFalse(roots[0].parent.exists())
            self.assertFalse(roots[1].parent.exists())
        with tempfile.TemporaryDirectory() as folder:
            roots = temp_roots(folder)
            with self.assertRaises(ui.DiagnosticFailure) as error:
                ui.PackageProfile.open(NONCE, roots)  # Never created: nothing to own.
            self.assertEqual(error.exception.code, "profile_invalid")

    def test_remove_refuses_redirected_or_foreign_roots_and_removes_app_data_inside_its_own(self):
        with tempfile.TemporaryDirectory() as folder:
            base = Path(folder)
            elsewhere = base / "elsewhere"
            elsewhere.mkdir()
            (elsewhere / "vault").write_bytes(b"not ours")
            roots = temp_roots(base, "redirected")
            roots[0].parent.mkdir(parents=True)
            os.symlink(elsewhere, roots[0], target_is_directory=True)
            with self.assertRaises(RuntimeError):
                ui.PackageProfile(roots, NONCE).remove()
            self.assertEqual((elsewhere / "vault").read_bytes(), b"not ours")
            self.assertTrue(roots[0].is_symlink())
            roots = temp_roots(base, "foreign")
            (roots[0] / "data").mkdir(parents=True)
            (roots[0] / "data" / "store").write_bytes(b"account")
            with self.assertRaises(RuntimeError):
                ui.PackageProfile(roots, NONCE).remove()
            self.assertEqual((roots[0] / "data" / "store").read_bytes(), b"account")
            roots = temp_roots(base, "owned")
            profile = ui.PackageProfile.create(NONCE, roots)
            (roots[0] / "data").mkdir()
            (roots[0] / "data" / "store").write_bytes(b"synthetic")
            self.assertTrue(profile.remove())
            self.assertFalse(roots[0].parent.exists())

    def test_smoke_creates_the_profile_before_the_plain_launch_and_removes_it_after_verified_stops(self):
        events = []
        with tempfile.TemporaryDirectory() as folder:
            roots = temp_roots(folder)

            def create():
                events.append("profile")
                return ui.PackageProfile.create(NONCE, roots)

            def plain_launch(executable, check):
                # The release executable's first start writes its account store
                # into the already existing synthetic profile.
                self.assertTrue(roots[0].exists())
                (roots[0] / "data").mkdir()
                (roots[0] / "data" / "accounts.db").write_bytes(b"synthetic store")
                events.append(("launch", executable, check))

            def keyboard(executable, nonce):
                ui.PackageProfile.open(nonce, roots)  # The driver's ownership proof succeeds.
                events.append(("keyboard", executable, nonce))
                return True, True

            with patch.object(smoke, "windows_profile", side_effect=create), \
                    patch.object(smoke, "launch_smoke", side_effect=plain_launch), \
                    patch.object(smoke, "windows_keyboard_launch", side_effect=keyboard), \
                    patch.object(smoke, "record") as record:
                smoke.windows_installed_launches(Path("/installed/morpheus-app.exe"))
            self.assertEqual(events, ["profile", ("launch", Path("/installed/morpheus-app.exe"), "win32"),
                                      ("keyboard", Path("/installed/morpheus-app.exe"), NONCE)])
            self.assertFalse(roots[0].parent.exists())
            self.assertFalse(roots[1].parent.exists())
            record.assert_called_once_with("windows_profile_removed")

    def test_smoke_retains_the_profile_unless_both_stops_are_verified(self):
        for case in ("plain_launch_failed", "driver_unverified_stop", "driver_report_invalid"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as folder:
                roots = temp_roots(folder)
                captured = io.StringIO()
                launch = Mock(side_effect=RuntimeError("startup") if case == "plain_launch_failed" else None)
                if case == "driver_report_invalid":
                    keyboard = Mock(side_effect=RuntimeError("Missing or ambiguous installed keyboard evidence"))
                else:
                    keyboard = Mock(return_value=(False, False))
                with patch.object(smoke, "windows_profile", side_effect=lambda: ui.PackageProfile.create(NONCE, roots)), \
                        patch.object(smoke, "launch_smoke", launch), \
                        patch.object(smoke, "windows_keyboard_launch", keyboard), \
                        patch.object(smoke, "record") as record, contextlib.redirect_stdout(captured):
                    with self.assertRaises(RuntimeError):
                        smoke.windows_installed_launches(Path("/installed/morpheus-app.exe"))
                self.assertTrue((roots[0] / ui.MARKER).exists())
                self.assertEqual(json.loads(captured.getvalue()), {"check": "windows_profile_retained", "status": "failed"})
                record.assert_not_called()
                if case == "plain_launch_failed":
                    keyboard.assert_not_called()

    def test_existing_profile_is_refused_before_any_launch(self):
        with tempfile.TemporaryDirectory() as folder:
            roots = temp_roots(folder)
            (roots[0] / "data").mkdir(parents=True)
            (roots[0] / "data" / "store").write_bytes(b"account")
            with patch.object(ui, "profile_roots", return_value=roots), \
                    patch.object(smoke, "launch_smoke") as launch, patch.object(smoke, "windows_keyboard_launch") as keyboard:
                with self.assertRaises(RuntimeError):
                    smoke.windows_installed_launches(Path("/installed/morpheus-app.exe"))
            launch.assert_not_called()
            keyboard.assert_not_called()
            self.assertEqual((roots[0] / "data" / "store").read_bytes(), b"account")

    def test_full_smoke_runs_both_launches_between_registration_and_uninstall(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as work:
            dist = Path(folder)
            (dist / "Morpheus_0.1.0_x64-setup.exe").write_bytes(b"synthetic")
            installed = Path(work) / "Installed Morpheus"
            entry = {"DisplayName": "Morpheus", "DisplayVersion": smoke.package_version(), "InstallLocation": str(installed)}
            events = []
            with patch.object(smoke, "record"), \
                    patch.object(smoke.tempfile, "TemporaryDirectory", return_value=contextlib.nullcontext(work)), \
                    patch.object(smoke, "uninstall_registry", side_effect=[[], [entry], []]), \
                    patch.object(smoke, "run", side_effect=lambda *a, **k: events.append("nsis")), \
                    patch.object(smoke, "check_windows_binary"), patch.object(smoke, "check_resources"), \
                    patch.object(smoke, "windows_installed_launches", side_effect=lambda e: events.append(("launches", e))):
                smoke.windows_smoke(dist)
            self.assertEqual(events, ["nsis", ("launches", installed / "morpheus-app.exe")])


class TraversalTests(unittest.TestCase):
    def test_unowned_root_or_errored_root_is_not_a_tree(self):
        for root in (node(0, pid=1, type="window", parent=-1), node(0, type="window", parent=-1, error=True)):
            instance, _, diagnostics = driver([{"ok": True, "nodes": [root], "bounded": False}])
            with self.subTest(root=root), self.assertRaises(ui.DiagnosticFailure) as error:
                list(instance.nodes())
            self.assertEqual(error.exception.code, "bridge_invalid")
            self.assertFalse(diagnostics.value["uia_root_seen"])

    def test_unowned_node_is_skipped_and_marks_the_pass_incomplete(self):
        instance, _, diagnostics = driver([snapshot(node(1, "Homeserver URL", type="edit"), node(2, "Other app", pid=5))])
        with self.assertRaises(ui.IncompleteTraversal):
            instance.find("Homeserver URL")
        self.assertEqual(diagnostics.value["incomplete_passes"], 1)
        self.assertTrue(diagnostics.value["uia_root_seen"])

    def test_node_read_error_marks_the_pass_incomplete_but_ambiguity_still_fails(self):
        instance, _, _ = driver([snapshot(node(1, "Password", type="edit", password=True), node(2, error=True))])
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
        oversized = {"ok": True, "nodes": [node(0, pid=77, type="window", parent=-1)]
                     + [node(index) for index in range(1, ui.TREE_BOUND + 1)], "bounded": False}
        instance, _, _ = driver([oversized])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.find("Done")
        self.assertEqual(error.exception.code, "tree_bound")

    def test_prefix_and_exact_selectors_publish_only_booleans(self):
        instance, _, diagnostics = driver([snapshot(node(1, "Account menu for Casey, @casey:example.org"))] * 2)
        self.assertIsNotNone(instance.find("Account menu for ", prefix=True))
        self.assertTrue(diagnostics.value["traversed_node_matched"])
        self.assertIsNone(instance.find("Account menu for "))
        self.assertNotIn("casey", json.dumps(diagnostics.value))

    def test_malformed_or_overlong_snapshot_entries_are_rejected(self):
        for raw in [{"ok": True, "nodes": "nodes", "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, type="window"), {"id": 1}], "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, type="window"), node(5)], "bounded": False},
                    {"ok": True, "nodes": [], "bounded": False},
                    {"ok": True, "nodes": [dict(node(0, parent=-1, type="window"), name=5)], "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, type="window"), node(1, "n" * (ui.NAME_BOUND + 1))], "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, type="window")], "bounded": "no"}]:
            instance, _, _ = driver([raw])
            with self.subTest(raw=raw), self.assertRaises(ui.DiagnosticFailure) as error:
                list(instance.nodes())
            self.assertEqual(error.exception.code, "bridge_invalid")
        instance, _, _ = driver([snapshot(node(1, "x" * ui.NAME_BOUND, truncated=True))])
        self.assertIsNone(instance.find("Done"))

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

    def test_lost_foreground_or_partial_chord_fails_through_the_real_transport_path(self):
        for token, expected in [("foreground_unowned", "foreground_unowned"), ("window_gone", "foreground_unowned"),
                                ("input_rejected", "input_rejected"), ("invalid_key", "bridge_invalid"),
                                ("C:\\private\\path", "bridge_invalid")]:
            instance, bridge, diagnostics = driver(focused=[focused_reply("Done")],
                                                   key_replies=[{"ok": False, "error": token}])
            with self.subTest(token=token), self.assertRaises(ui.DiagnosticFailure) as error:
                instance.key("Tab")
            self.assertEqual(error.exception.code, expected)
            self.assertEqual(diagnostics.value["bridge_error"],
                             token if token in smoke.WINDOWS_BRIDGE_ERRORS else "bridge_error")
            self.assertEqual(bridge.commands[-1], {"op": "key", "key": "Tab"})
            self.assertNotIn("private", json.dumps(diagnostics.value))
        instance, bridge, diagnostics = driver(focused=[focused_reply("Done")], key_replies=[{"ok": True, "recovered": True}])
        instance.key("ShiftTab")
        self.assertEqual(diagnostics.value["foreground_recoveries"], 1)
        self.assertEqual(bridge.commands[-1], {"op": "key", "key": "ShiftTab"})

    def test_only_reviewed_keys_are_sent(self):
        instance, bridge, _ = driver(focused=[focused_reply("Done")])
        with self.assertRaises(RuntimeError):
            instance.key("Alt+F4")
        self.assertEqual(bridge.commands, [])

    def test_truncated_focus_or_node_name_matches_a_prefix_but_never_an_exact_label(self):
        label = "L" * (ui.NAME_BOUND - 1)
        instance, _, _ = driver(focused=[focused_reply(label, truncated=True)] * 50)
        with patch.object(ui, "WAIT_SECONDS", 0.3), self.assertRaises(ui.DiagnosticFailure) as error:
            instance.expect_focus(label)
        self.assertEqual(error.exception.code, "wait_timeout")
        instance, _, _ = driver(focused=[focused_reply(label, truncated=True)])
        instance.expect_focus("L" * 8, prefix=True)
        instance, _, _ = driver(focused=[focused_reply(label)])
        instance.expect_focus(label)
        instance, _, _ = driver([snapshot(node(1, label, truncated=True))] * 2)
        self.assertIsNone(instance.find(label))
        self.assertEqual(instance.find("L" * 8, prefix=True).id, 1)
        for reply in (focused_reply("Done"), {"ok": True, "present": True, "owned": True, "name": "Done"}):
            reply = dict(reply)
            reply.pop("truncated", None)
            instance, _, _ = driver(focused=[reply])
            with self.subTest(reply=reply), self.assertRaises(ui.DiagnosticFailure) as error:
                instance.focused_name()
            self.assertEqual(error.exception.code, "bridge_invalid")
        for label in ("", "x" * ui.NAME_BOUND):
            instance, _, _ = driver(focused=[focused_reply("Done")])
            with self.subTest(label=label), self.assertRaises(RuntimeError):
                instance.expect_focus(label)

    def test_focus_command_targets_the_found_snapshot_element(self):
        instance, bridge, _ = driver([snapshot(node(1, "Open settings", type="button"))],
                                     focused=[focused_reply("Open settings")])
        instance.focus("Open settings")
        self.assertIn({"op": "focus", "id": 1}, bridge.commands)

    def test_bridge_error_replies_raise_even_where_the_caller_cannot_tolerate_them(self):
        instance, _, diagnostics = driver([{"ok": False, "error": "unowned_root"}])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            list(instance.nodes())
        self.assertEqual(error.exception.code, "bridge_invalid")
        self.assertEqual(diagnostics.value["bridge_error"], "unowned_root")

    def test_settings_scale_control_is_reached_by_bounded_tabs_before_its_label_is_queried(self):
        focused = [focused_reply(name) for name in ["Open settings", "Open settings", "Open settings", "Dark theme",
                                                    "Dark theme", "Decrease font scale", "Decrease font scale",
                                                    "Increase font scale", "Increase font scale", "Increase font scale"]]
        instance, bridge, _ = driver([snapshot(node(1, "Open settings", type="button")),
                                      snapshot(node(1, "110% · text throughout the app", type="text")),
                                      snapshot(node(1, "120% · text throughout the app", type="text"))], focused=focused)
        ui.appearance_keyboard(instance)
        ops = [command for command in bridge.commands if command["op"] in ("key", "snapshot")]
        keys = [command["key"] for command in ops if command["op"] == "key"]
        self.assertEqual(keys, ["Return", "Tab", "Tab", "Space"])
        first_tab = ops.index({"op": "key", "key": "Tab"})
        last_tab = len(ops) - 1 - ops[::-1].index({"op": "key", "key": "Tab"})
        self.assertFalse(any(command["op"] == "snapshot" for command in ops[first_tab:last_tab + 1]))

    def test_unreachable_scale_control_fails_within_the_tab_bound(self):
        def focused(call):
            return focused_reply("Open settings" if call == 0 else f"Control {call}")

        instance, bridge, _ = driver([snapshot(node(1, "Open settings", type="button"))], focused=focused)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            ui.appearance_keyboard(instance)
        self.assertEqual(error.exception.code, "scale_unreachable")
        self.assertLessEqual(sum(1 for command in bridge.commands if command["op"] == "key"), ui.SETTINGS_TAB_BOUND + 1)


class SignInSemanticsTests(unittest.TestCase):
    def fields(self, password_type="edit", protected=True):
        return snapshot(node(1, "Homeserver URL", type="edit"), node(2, "Matrix ID or username", type="edit"),
                        node(3, "Password", type=password_type, password=protected))

    def test_protected_password_field_passes_without_reading_values(self):
        instance, bridge, diagnostics = driver([self.fields()] * 3)
        record = {}
        ui.inspect_sign_in(instance, record, diagnostics)
        self.assertTrue(record["sign_in_semantics"])
        self.assertEqual(diagnostics.value["password_semantics"], "protected")
        self.assertTrue(all(diagnostics.value[key] for key in ("homeserver_seen", "username_seen", "password_seen")))
        self.assertTrue(all(command["op"] == "snapshot" for command in bridge.commands))

    def test_unprotected_or_non_edit_password_cannot_pass(self):
        for kind, expected in [(("edit", False), "unprotected"), (("text", False), "not_edit"), (("text", True), "not_edit")]:
            instance, _, diagnostics = driver([self.fields(*kind)] * 3)
            record = {}
            with self.subTest(kind=kind), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.inspect_sign_in(instance, record, diagnostics)
            self.assertEqual(error.exception.code, "password_semantics")
            self.assertEqual(diagnostics.value["password_semantics"], expected)
            self.assertNotIn("sign_in_semantics", record)

    def test_missing_second_field_retains_the_first_observation(self):
        instance, _, diagnostics = driver([snapshot(node(1, "Homeserver URL", type="edit"))] * 200)
        with patch.object(ui, "WAIT_SECONDS", 0.3), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.inspect_sign_in(instance, {}, diagnostics)
        self.assertEqual(error.exception.code, "wait_timeout")
        self.assertTrue(diagnostics.value["homeserver_seen"])
        self.assertFalse(diagnostics.value["username_seen"])
        self.assertEqual(diagnostics.value["step"], "username")


class FakeJob:
    """Records spawn/stop calls; the fake bridge child is the bytes written to its stdout pipe."""
    def __init__(self, output=b'{"ok":true,"ready":true}\n', hold_open=False, spawn_error=None, alive=True,
                 stop_result=0, terminate_error=None):
        self.output, self.hold_open, self.spawn_error = output, hold_open, spawn_error
        self.alive_result = 258 if alive else 0
        self.stop_result, self.terminate_error = stop_result, terminate_error
        self.spawned, self.terminated, self.held = [], [], []
        self.kernel = Mock()
        self.kernel.WaitForSingleObject.side_effect = self.wait

    def wait(self, handle, milliseconds):
        return self.alive_result if milliseconds == 0 else self.stop_result

    def spawn(self, command, environment, log, *, stdin_stream=None):
        self.spawned.append((command, environment, stdin_stream is not None))
        if self.spawn_error:
            raise self.spawn_error
        log.write(self.output)
        log.flush()
        if self.hold_open:
            self.held.append(os.dup(log.fileno()))  # The child keeps its end open.
        return 20

    def terminate_handle(self, handle):
        self.terminated.append(handle)
        self.release()  # A terminated child's pipe ends close with it.
        if self.terminate_error:
            raise self.terminate_error

    def release(self):
        for fd in self.held:
            os.close(fd)
        self.held.clear()


class BridgeContainmentTests(unittest.TestCase):
    def test_bridge_is_spawned_inside_the_job_with_private_pipes_and_no_subprocess(self):
        job = FakeJob()
        with patch.dict(os.environ, HOSTED):
            bridge = ui.Bridge(["powershell.exe", "-File", "bridge.ps1"], job, ui.Diagnostics({}))
        self.assertEqual(job.spawned[0][0], ["powershell.exe", "-File", "bridge.ps1"])
        self.assertEqual(job.spawned[0][1]["ImageOS"], "win25")
        self.assertTrue(job.spawned[0][2])
        self.assertEqual(bridge.handle, 20)
        bridge.close()
        self.assertEqual(job.terminated, [])
        self.assertNotIn("Popen", SOURCE.read_text(encoding="utf-8"))

    def test_handshake_without_ready_terminates_the_contained_child(self):
        for output in (b"", b'{"ok":true}\n', b"not json\n"):
            job = FakeJob(output=output, stop_result=258)
            with self.subTest(output=output), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.Bridge(["bridge"], job, ui.Diagnostics({}), environment={})
            self.assertIn(error.exception.code, {"bridge_exited", "bridge_invalid"})
            # The child's handle stays in the job; close() ran and, with the
            # child still alive after the exit request, terminated it.
            self.assertEqual(job.terminated, [20])

    def test_handshake_timeout_is_bounded_and_terminates_the_child(self):
        job = FakeJob(output=b"", hold_open=True, stop_result=258)
        try:
            with patch.object(ui, "BRIDGE_REPLY_SECONDS", 0.05), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.Bridge(["bridge"], job, ui.Diagnostics({}), environment={})
        finally:
            job.release()
        self.assertEqual(error.exception.code, "bridge_timeout")
        self.assertEqual(job.terminated, [20])

    def test_spawn_failure_propagates_without_a_handle_to_terminate(self):
        job = FakeJob(spawn_error=capture.Failure("process_start"))
        with self.assertRaises(capture.Failure):
            ui.Bridge(["bridge"], job, ui.Diagnostics({}), environment={})
        self.assertEqual(job.terminated, [])

    def test_stop_that_cannot_be_verified_raises_instead_of_pretending(self):
        job = FakeJob(stop_result=258, terminate_error=capture.Failure("cleanup", operation="process_terminate"))
        bridge = ui.Bridge(["bridge"], job, ui.Diagnostics({}), environment={})
        with self.assertRaises(capture.Failure):
            bridge.close()
        self.assertEqual(job.terminated, [20])
        self.assertTrue(bridge.stdin.closed)
        bridge.reader.join(timeout=2)
        self.assertFalse(bridge.reader.is_alive())

    def test_overlong_reply_is_rejected_before_it_is_buffered(self):
        line = b'{"ok":true,"nodes":[' + b"x" * 200 + b"]}\n"
        with patch.object(ui, "BRIDGE_LINE_BOUND", 64):
            job = FakeJob(output=b'{"ok":true,"ready":true}\n' + line)
            bridge = ui.Bridge(["bridge"], job, ui.Diagnostics({}), environment={})
            bridge.reader.join(timeout=2)
            with self.assertRaises(ui.DiagnosticFailure) as error:
                bridge.reply()
            self.assertEqual(error.exception.code, "bridge_overflow")
            # The reader stopped at the overlong line: nothing after it was read.
            self.assertEqual(bridge.replies.qsize(), 1)
            self.assertIsNone(bridge.replies.get_nowait())
            bridge.close()

    def test_flooded_replies_stay_within_the_bounded_queue(self):
        # Far more lines than the queue holds, yet under 1 KiB in total so the
        # synchronous fake producer never depends on any OS pipe capacity.
        flood = 40
        output = b'{"ok":true,"ready":true}\n' + b'{"ok":true,"flood":1}\n' * flood
        self.assertLess(len(output), 1024)
        self.assertGreater(flood, 2 * ui.BRIDGE_QUEUE_BOUND)
        job = FakeJob(output=output)
        bridge = ui.Bridge(["bridge"], job, ui.Diagnostics({}), environment={})
        try:
            for _ in range(10):
                self.assertLessEqual(bridge.replies.qsize(), ui.BRIDGE_QUEUE_BOUND)
                self.assertEqual(bridge.reply(), {"ok": True, "flood": 1})
            self.assertLessEqual(bridge.replies.qsize(), ui.BRIDGE_QUEUE_BOUND)
            # Drain the rest so the bounded reader can finish, and verify it did.
            for _ in range(flood - 10):
                self.assertEqual(bridge.reply(), {"ok": True, "flood": 1})
            with self.assertRaises(ui.DiagnosticFailure) as error:
                bridge.reply()
            self.assertEqual(error.exception.code, "bridge_exited")
            bridge.reader.join(timeout=2)
            self.assertFalse(bridge.reader.is_alive())
        finally:
            bridge.close()
        self.assertTrue(bridge.stdin.closed and bridge.stdout.closed)

    def test_acceptance_cleanup_never_removes_the_profile_and_reports_an_unverified_stop(self):
        with tempfile.TemporaryDirectory() as folder:
            roots = temp_roots(folder)
            ui.PackageProfile.create(NONCE, roots)
            for job_close_error, expected_cleanup in [(None, "passed"), (capture.Failure("cleanup"), "failed")]:
                record = {}
                trace = ui.Diagnostics(record)
                job = Mock()
                job.spawn.return_value = 20
                job.close.side_effect = job_close_error
                with patch.object(ui, "guard"), patch.object(ui, "installed_executable"), \
                        patch.object(ui, "profile_roots", return_value=roots), \
                        patch.object(capture, "WindowsJob", return_value=job), \
                        patch.object(ui, "process_id", return_value=77), \
                        patch.object(ui, "owned_window", side_effect=ui.DiagnosticFailure("window_count")):
                    with self.subTest(cleanup=expected_cleanup), \
                            self.assertRaises(capture.Failure if job_close_error else ui.DiagnosticFailure):
                        ui.acceptance(Path("/synthetic/morpheus-app.exe"), NONCE, record, trace)
                self.assertEqual(trace.value["failure"], "window_count")
                self.assertEqual(trace.value["cleanup"], expected_cleanup)
                self.assertTrue(trace.value["profile_owned"])
                self.assertTrue((roots[0] / ui.MARKER).exists())
                job.close.assert_called_once()

    def test_bridge_failure_mid_flow_keeps_the_first_failure_and_still_closes_bridge_and_job(self):
        with tempfile.TemporaryDirectory() as folder:
            roots = temp_roots(folder)
            ui.PackageProfile.create(NONCE, roots)
            record = {"sign_in_semantics": False}
            trace = ui.Diagnostics(record)
            job = Mock()
            job.spawn.return_value = 20
            bridge = Mock()

            def fail_signin(_ui, _report, diagnostics):
                diagnostics.step("password_semantics")
                raise ui.DiagnosticFailure("password_semantics")

            with patch.object(ui, "guard"), patch.object(ui, "installed_executable"), \
                    patch.object(ui, "profile_roots", return_value=roots), \
                    patch.object(capture, "WindowsJob", return_value=job), \
                    patch.object(ui, "process_id", return_value=77), \
                    patch.object(ui, "owned_window", return_value=65538), \
                    patch.object(ui, "Bridge", return_value=bridge) as bridge_type, \
                    patch.object(ui, "inspect_sign_in", side_effect=fail_signin), \
                    patch.object(ui, "normal_close") as close:
                with self.assertRaises(ui.DiagnosticFailure):
                    ui.acceptance(Path("/synthetic/morpheus-app.exe"), NONCE, record, trace)
            self.assertIs(bridge_type.call_args.args[1], job)  # The bridge joins the app's job.
            bridge.close.assert_called_once()
            job.close.assert_called_once()
            close.assert_not_called()
            self.assertEqual((trace.value["failure"], trace.value["step"], trace.value["cleanup"]),
                             ("password_semantics", "password_semantics", "passed"))
            self.assertFalse(record["sign_in_semantics"])
            self.assertTrue((roots[0] / ui.MARKER).exists())


class PowerShellSourceTests(unittest.TestCase):
    def setUp(self):
        self.source = SCRIPT.read_text(encoding="utf-8")

    def test_guards_and_privacy_invariants(self):
        for forbidden in ["ValuePattern", "TextPattern", "Exception.Message", "$_", "PostMessage", "SendMessage",
                          "Write-Host", "Write-Error", "Write-Output"]:
            self.assertNotIn(forbidden, self.source)
        for required in ["$env:GITHUB_ACTIONS -ne 'true'", "$env:RUNNER_ENVIRONMENT -ne 'github-hosted'",
                         "@('win25', 'win25-vs2026') -notcontains $env:ImageOS", "[Environment]::UserInteractive", "IsPassword",
                         "HasKeyboardFocus", "GetForegroundWindow", "RawViewWalker"]:
            self.assertIn(required, self.source)
        for token in re.findall(r"Fail '([a-z_]+)'", self.source):
            self.assertIn(token, smoke.WINDOWS_BRIDGE_ERRORS)

    def test_each_key_is_one_native_sendinput_call_immediately_after_the_foreground_proof(self):
        send_key = self.source.split("function SendKey")[1].split("\ntry {")[0]
        self.assertEqual(send_key.count("[MorpheusInput]::Chord("), 1)
        self.assertNotIn("[MorpheusInput]::Key(", self.source)
        proof = send_key.index("if (-not (ForegroundOwned))")
        chord = send_key.index("[MorpheusInput]::Chord(")
        self.assertLess(proof, chord)
        between = send_key[proof:chord]
        self.assertNotIn("SendInput", between)
        self.assertNotIn("Release", between)
        # A partially accepted chord is repaired by exactly one native call that
        # releases every held key, issued in the same statement as a fresh
        # ownership proof, so no release can follow a lost foreground.
        failure_branch = send_key.split("if ($sent -ne $expected)")[1]
        self.assertEqual(failure_branch.count("[MorpheusInput]::"), 1)
        self.assertIn("if ($held.Count -gt 0 -and (ForegroundOwned)) { [void][MorpheusInput]::ReleaseAll($held.ToArray()) }",
                      failure_branch)
        self.assertNotIn("::Release(", self.source)
        self.assertIn("Fail 'input_rejected'", failure_branch)
        self.assertLess(failure_branch.index("ReleaseAll"), failure_branch.index("Fail 'input_rejected'"))
        # The C# chord and release builders each issue a single SendInput over a complete array.
        chord_source = self.source.split("public static uint Chord(")[1].split("public static uint ReleaseAll(")[0]
        self.assertEqual(chord_source.count("SendInput("), 1)
        self.assertIn("SendInput((uint)count, inputs", chord_source)
        release_source = self.source.split("public static uint ReleaseAll(")[1].split("public static uint WindowProcess(")[0]
        self.assertEqual(release_source.count("SendInput("), 1)
        self.assertIn("SendInput((uint)virtualKeys.Length, inputs", release_source)

    def test_snapshot_bounds_are_enforced_before_enumeration_and_serialization(self):
        snapshot_source = self.source.split("function Snapshot")[1].split("function FocusElement")[0]
        sibling_loop = snapshot_source.split("while ($child -ne $null) {")[1].split("$walker.GetNextSibling")[0]
        self.assertIn("$script:elements.Count + $queue.Count -ge $MaxNodes", sibling_loop)
        self.assertLess(sibling_loop.index("$MaxNodes"), sibling_loop.index("$queue.Enqueue"))
        self.assertIn("$MaxNodes = 5000", self.source)
        self.assertIn("$MaxName = 256", self.source)
        self.assertLess(snapshot_source.index("$name.Substring(0, $MaxName)"), snapshot_source.index("$node.name = $name"))
        self.assertIn("-and -not $bounded", snapshot_source)
        self.assertEqual(ui.TREE_BOUND, 5000)
        self.assertEqual(ui.NAME_BOUND, 256)
        self.assertIn("if ($line.Length -gt 256) { Fail 'invalid_command'", self.source)
        focused_source = self.source.split("function Focused")[1].split("function ForegroundOwned")[0]
        self.assertLess(focused_source.index("$name.Substring(0, $MaxName); $truncated = $true"),
                        focused_source.index("Reply @{ ok = $true; present = $true"))
        self.assertIn("truncated = $truncated", focused_source)
        self.assertIn("present = $false; owned = $false; name = ''; truncated = $false", focused_source)


class DiagnosticsAndReportTests(unittest.TestCase):
    def test_diagnostics_map_exceptions_to_fixed_tokens_and_keep_the_first_failure(self):
        trace = ui.Diagnostics({})
        trace.step("password_semantics")
        trace.failed(ui.DiagnosticFailure("password_semantics"))
        trace.failed(OSError("C:\\private\\path"))
        trace.failed(capture.Failure("cleanup"))
        self.assertEqual(trace.value["failure"], "password_semantics")
        for error, code in [(capture.Failure("job_assignment"), "job_assignment"), (AttributeError("x"), "attribute_error"),
                            (TypeError("x"), "type_error"), (KeyError("x"), "lookup_error"), (ValueError("x"), "value_error"),
                            (RuntimeError("x"), "runtime_error"), (OSError("x"), "os_error"),
                            (ui.DiagnosticFailure("PRIVATE_TOKEN"), "unclassified"), (Exception("x"), "unclassified")]:
            trace = ui.Diagnostics({})
            trace.failed(error)
            self.assertEqual(trace.value["failure"], code)
            self.assertNotIn("PRIVATE", json.dumps(trace.value))

    def test_normal_close_requires_zero_exit_and_an_empty_job(self):
        job = Mock()
        job.wait.return_value = 0
        job.active.return_value = 0
        with patch.object(ui, "request_close") as request:
            ui.normal_close(job, 20, 77, 65538, ui.Diagnostics({}))
        request.assert_called_once_with(77, 65538)
        job.active.return_value = 1
        with patch.object(ui, "request_close"), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.normal_close(job, 20, 77, 65538, ui.Diagnostics({}))
        self.assertEqual(error.exception.code, "process_remaining")
        job.active.return_value = 0
        job.wait.return_value = 3
        with patch.object(ui, "request_close"), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.normal_close(job, 20, 77, 65538, ui.Diagnostics({}))
        self.assertEqual(error.exception.code, "exit_status")
        job.wait.side_effect = capture.Failure("deadline", operation="process_wait")
        with patch.object(ui, "request_close"), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.normal_close(job, 20, 77, 65538, ui.Diagnostics({}))
        self.assertEqual(error.exception.code, "exit_status")

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

    def test_window_wait_requires_exactly_one_owned_visible_window(self):
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        trace = ui.Diagnostics({})
        with patch.object(smoke, "visible_windows", return_value=[65538]):
            self.assertEqual(ui.owned_window(job, 20, 77, trace, api=WindowQuery()), 65538)
        self.assertTrue(trace.value["owned_window_seen"])
        api = WindowQuery()
        api.windows[1] = window_record()
        with patch.object(smoke, "visible_windows", return_value=[1, 2]), self.assertRaises(ui.DiagnosticFailure) as error:
            # A real second window must remain ambiguous, even if encountered first.
            api.windows[2] = window_record()
            ui.owned_window(job, 20, 77, ui.Diagnostics({}), api=api)
        self.assertEqual(error.exception.code, "window_count")
        job.kernel.WaitForSingleObject.return_value = 0
        with patch.object(smoke, "visible_windows", return_value=[]), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.owned_window(job, 20, 77, ui.Diagnostics({}), api=WindowQuery())
        self.assertEqual(error.exception.code, "app_exited")

    def test_exact_event_target_is_ignored_in_either_enumeration_order(self):
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        for windows in ([65538, 2], [2, 65538]):
            trace = ui.Diagnostics({})
            with self.subTest(windows=windows), patch.object(smoke, "visible_windows", return_value=windows):
                self.assertEqual(ui.owned_window(job, 20, 77, trace, api=WindowQuery()), 65538)
            self.assertEqual((trace.value["window_candidates"], trace.value["event_targets_ignored"]), (1, 1))

    def test_near_match_helpers_are_not_discarded(self):
        changes = ({"class": "unknown private class"}, {"style": 0x92000000},
                   {"exstyle": 0x080801A0}, {"owner": 65538})
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        for change in changes:
            api = WindowQuery()
            api.windows[2].update(change)
            trace = ui.Diagnostics({})
            with (self.subTest(change=change), patch.object(smoke, "visible_windows", return_value=[2, 65538]),
                  self.assertRaises(ui.DiagnosticFailure) as error):
                ui.owned_window(job, 20, 77, trace, api=api)
            self.assertEqual(error.exception.code, "window_count")
            self.assertEqual(trace.value["window_candidates"], 2)
            self.assertNotIn("private", json.dumps(trace.value))

    def test_installed_helper_signature_keeps_the_real_window_in_either_order(self):
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        for style in (0x90000000, 0x94000000):
            for order in ([2, 65538], [65538, 2]):
                api = WindowQuery()
                api.windows[2].update(style=style, rect=(0, 0, 16, 16))
                trace = ui.Diagnostics({})
                with (self.subTest(style=style, order=order),
                      patch.object(smoke, "visible_windows", return_value=order)):
                    self.assertEqual(ui.owned_window(job, 20, 77, trace, api=api), 65538)
                self.assertEqual(trace.value["event_targets_ignored"], 1)
                self.assertEqual(trace.value["window_candidates"], 1)

    def test_every_other_single_style_bit_change_remains_a_candidate(self):
        accepted = (0x90000000, 0x94000000)
        for original in accepted:
            for field, expected in (("style", original), ("exstyle", 0x080800A0)):
                for bit in range(32):
                    changed = expected ^ (1 << bit)
                    if field == "style" and changed in accepted:
                        continue
                    api = WindowQuery()
                    api.windows[2].update(style=original)
                    api.windows[2][field] = changed
                    with self.subTest(original=original, field=field, bit=bit):
                        self.assertEqual(ui.keyboard_window_kind(api, 2, 77), "candidate")

    def test_exact_event_target_identity_does_not_depend_on_geometry(self):
        # The actual hosted probe found a nonzero-area event target. A zero
        # CreateWindowEx size is not proof of the eventual GetWindowRect size.
        for rectangle in ((0, 0, 0, 0), (0, 0, 1, 0), (0, 0, 0, 1),
                          (10, 20, 330, 220), (-600, -400, -100, -100)):
            api = WindowQuery()
            api.windows[2]["rect"] = rectangle
            trace = ui.Diagnostics({})
            with (self.subTest(rectangle=rectangle),
                  patch.object(smoke, "visible_windows", return_value=[2, 65538]),
                  patch.object(api, "rect", side_effect=AssertionError("Geometry is not identity"))):
                self.assertEqual(ui.keyboard_window_candidates(77, trace, api), [65538])
            self.assertEqual(trace.value["event_targets_ignored"], 1)

    def test_event_target_alone_waits_for_the_actual_window(self):
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        trace = ui.Diagnostics({})
        with (patch.object(smoke, "visible_windows", side_effect=[[2], [2, 65538]]),
              patch.object(ui.time, "sleep")):
            self.assertEqual(ui.owned_window(job, 20, 77, trace, api=WindowQuery()), 65538)
        self.assertEqual((trace.value["window_candidates"], trace.value["event_targets_ignored"]), (1, 1))
        with (patch.object(smoke, "visible_windows", return_value=[2]),
              self.assertRaises(ui.DiagnosticFailure) as error):
            ui.owned_window(job, 20, 77, ui.Diagnostics({}), seconds=0, api=WindowQuery())
        self.assertEqual(error.exception.code, "wait_timeout")

    def test_foreign_and_reused_handles_fail_before_selection(self):
        for owners in ([99], [77, 99]):
            api = WindowQuery()
            with (self.subTest(owners=owners), patch.object(api, "process_of", side_effect=owners),
                  self.assertRaises(ui.DiagnosticFailure) as error):
                ui.keyboard_window_kind(api, 65538, 77)
            self.assertEqual(error.exception.code, "window_unowned")

    def test_vanished_windows_are_ignored_but_live_query_failures_reject(self):
        for exists in (False, True):
            api = WindowQuery()
            with (self.subTest(exists=exists), patch.object(api, "class_name", side_effect=ui.window_probe.ApiError),
                  patch.object(api, "is_window", return_value=exists)):
                if exists:
                    with self.assertRaises(ui.DiagnosticFailure) as error:
                        ui.keyboard_window_kind(api, 65538, 77)
                    self.assertEqual(error.exception.code, "window_facts")
                else:
                    self.assertEqual(ui.keyboard_window_kind(api, 65538, 77), "vanished")

    def test_hidden_windows_and_overflow_do_not_produce_a_candidate(self):
        api = WindowQuery()
        api.windows[65538]["visible"] = False
        with patch.object(smoke, "visible_windows", return_value=[65538]):
            self.assertEqual(ui.keyboard_window_candidates(77, ui.Diagnostics({}), api), [])
        with (patch.object(smoke, "visible_windows", return_value=[65538] * 65),
              self.assertRaises(ui.DiagnosticFailure) as error):
            ui.keyboard_window_candidates(77, ui.Diagnostics({}), api)
        self.assertEqual(error.exception.code, "window_bound")

    def parse_failed_window_report(self, record):
        """Run a driver record through the smoke's validating parser."""
        record.update(check=ui.CHECK, status="failed", phase="setup", sign_in_semantics=False,
                      account_menu_keyboard=False, appearance_config=False, normal_window_close=False,
                      app_log_public=False, screen_reader_acceptance=False)
        return smoke.keyboard_report(json.dumps(record).encode(), check=ui.CHECK,
                                     validate=smoke.validate_windows_keyboard_diagnostic)

    def test_window_count_inventory_publishes_complete_words_for_the_deciding_poll(self):
        # A synthetic event target whose style word carries one extra bit
        # (WS_CLIPCHILDREN) remains an unknown signature; the inventory shows
        # which complete word differs without relaxing the selector.
        api = WindowQuery()
        api.windows[2].update(style=0x92000000, rect=(0, 0, 136, 39))
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        record = {}
        trace = ui.Diagnostics(record)
        with (patch.object(smoke, "visible_windows", return_value=[2, 65538]),
              self.assertRaises(ui.DiagnosticFailure) as error):
            ui.owned_window(job, 20, 77, trace, api=api)
        self.assertEqual(error.exception.code, "window_count")
        self.assertEqual((trace.value["window_candidates"], trace.value["event_targets_ignored"],
                          trace.value["owned_window_seen"]), (2, 0, False))
        self.assertEqual(trace.value["window_inventory"],
                         inventory(fact(), event_target_fact(style=0x92000000, width=136, height=39)))
        trace.step("window")
        trace.failed(error.exception)
        parsed = self.parse_failed_window_report(record)
        self.assertEqual(parsed["diagnostic"]["window_inventory"], trace.value["window_inventory"])
        published = json.dumps(parsed)
        for private in ("Window Class", "Winit Thread Event Target", "65538", '"pid"', '"rect"', '"class"'):
            self.assertNotIn(private, published)

    def test_inventory_is_recorded_on_success_and_does_not_depend_on_enumeration_order(self):
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        inventories = []
        for windows in ([65538, 2], [2, 65538]):
            trace = ui.Diagnostics({})
            with self.subTest(windows=windows), patch.object(smoke, "visible_windows", return_value=windows):
                self.assertEqual(ui.owned_window(job, 20, 77, trace, api=WindowQuery()), 65538)
            inventories.append(trace.value["window_inventory"])
        self.assertEqual(inventories, [inventory(fact(), event_target_fact())] * 2)

    def test_wait_timeout_records_the_last_poll(self):
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        trace = ui.Diagnostics({})
        with (patch.object(smoke, "visible_windows", return_value=[2]),
              self.assertRaises(ui.DiagnosticFailure) as error):
            ui.owned_window(job, 20, 77, trace, seconds=0, api=WindowQuery())
        self.assertEqual(error.exception.code, "wait_timeout")
        self.assertEqual(trace.value["window_inventory"], inventory(event_target_fact()))
        trace = ui.Diagnostics({})
        with (patch.object(smoke, "visible_windows", return_value=[]),
              self.assertRaises(ui.DiagnosticFailure)):
            ui.owned_window(job, 20, 77, trace, seconds=0, api=WindowQuery())
        self.assertEqual(trace.value["window_inventory"], inventory())

    def test_inventory_reduces_unknown_classes_to_a_closed_family(self):
        api = WindowQuery()
        api.windows[3] = window_record(**{"class": "PRIVATE native helper", "style": 0x94000000})
        trace = ui.Diagnostics({})
        ui.record_window_inventory(api, [3, 65538], 77, trace)
        self.assertEqual(trace.value["window_inventory"], inventory(fact(), fact(family="other", style=0x94000000)))
        self.assertNotIn("PRIVATE", json.dumps(trace.value))

    def test_inventory_keeps_no_facts_for_vanished_or_reused_handles(self):
        api = WindowQuery()
        trace = ui.Diagnostics({})
        ui.record_window_inventory(api, [2, 65538, 3], 77, trace)  # Handle 3 was destroyed.
        self.assertEqual(trace.value["window_inventory"], inventory(fact(), event_target_fact(), discarded=1))
        for owners in ([99], [77, 99]):
            # Foreign before the queries, or reused by another process before the recheck.
            api = WindowQuery()
            sequence = iter(owners)
            original = api.process_of
            trace = ui.Diagnostics({})
            with (self.subTest(owners=owners),
                  patch.object(api, "process_of",
                               side_effect=lambda window: next(sequence, None) if window == 2 else original(window))):
                ui.record_window_inventory(api, [2, 65538], 77, trace)
            self.assertEqual(trace.value["window_inventory"], inventory(fact(), discarded=1))

    def test_failed_queries_on_live_owned_windows_publish_only_unavailable(self):
        api = WindowQuery()
        api.windows[65538].update(owner=4, cloaked=None)
        trace = ui.Diagnostics({})
        with (patch.object(api, "rect", side_effect=ui.window_probe.ApiError),
              patch.object(api, "class_name", side_effect=ui.window_probe.ApiError),
              patch.object(api, "long", side_effect=ui.window_probe.ApiError),
              patch.object(api, "is_visible", side_effect=ui.window_probe.ApiError)):
            ui.record_window_inventory(api, [65538], 77, trace)
        self.assertEqual(trace.value["window_inventory"],
                         inventory(fact(family=None, style=None, exstyle=None, visible=None, owner=None,
                                        cloaked=None, width=None, height=None)))
        api = WindowQuery()
        api.windows[65538]["owner"] = 2
        api.windows[2]["pid"] = 99
        api.windows[65538]["rect"] = (-40000, 0, 40000, 10)
        trace = ui.Diagnostics({})
        ui.record_window_inventory(api, [65538], 77, trace)
        self.assertEqual(trace.value["window_inventory"],
                         inventory(fact(owner="other_process", width=smoke.WINDOWS_WINDOW_EXTENT, height=10)))
        self.assertTrue(smoke.valid_windows_inventory(trace.value["window_inventory"]))

    def test_inventory_errors_never_change_the_selection_outcome(self):
        job = Mock()
        job.kernel.WaitForSingleObject.return_value = 258
        for windows, outcome in (([65538], 65538), ([65538, 1], "window_count")):
            api = WindowQuery()
            api.windows[1] = window_record()
            trace = ui.Diagnostics({})
            with (self.subTest(windows=windows), patch.object(smoke, "visible_windows", return_value=windows),
                  patch.object(ui, "window_facts", side_effect=RuntimeError("PRIVATE"))):
                if outcome == "window_count":
                    with self.assertRaises(ui.DiagnosticFailure) as error:
                        ui.owned_window(job, 20, 77, trace, api=api)
                    self.assertEqual(error.exception.code, outcome)
                else:
                    self.assertEqual(ui.owned_window(job, 20, 77, trace, api=api), outcome)
            self.assertEqual(trace.value["window_inventory"], inventory(status="failed"))
            self.assertNotIn("PRIVATE", json.dumps(trace.value))
        trace = ui.Diagnostics({})
        trace.deadline = 0
        ui.record_window_inventory(WindowQuery(), [2, 65538], 77, trace)
        self.assertEqual(trace.value["window_inventory"], inventory(status="incomplete"))

    def test_inventory_is_bounded_sorted_and_flags_truncation(self):
        api = WindowQuery()
        handles = list(range(100, 112))
        for index, window in enumerate(handles):
            api.windows[window] = window_record(style=0x10000000 + index)
        published = []
        for order in (handles, list(reversed(handles))):
            trace = ui.Diagnostics({})
            ui.record_window_inventory(api, order, 77, trace)
            published.append(trace.value["window_inventory"])
        self.assertEqual(published[0], published[1])
        self.assertEqual(len(published[0]["windows"]), smoke.WINDOWS_INVENTORY_BOUND)
        self.assertTrue(published[0]["truncated"])
        self.assertTrue(smoke.valid_windows_inventory(published[0]))

    def test_inventory_vocabulary_matches_the_probe_and_the_initial_record_validates(self):
        self.assertEqual(smoke.WINDOWS_WINDOW_FAMILIES, frozenset(ui.window_probe.FAMILIES.values()) | {"other"})
        initial = ui.Diagnostics({}).value["window_inventory"]
        self.assertEqual(initial, {"status": "not_collected", "windows": [], "truncated": False, "discarded": 0})
        self.assertTrue(smoke.valid_windows_inventory(initial))

    def test_main_publishes_only_the_fixed_report_and_requires_the_nonce_argument(self):
        captured = io.StringIO()
        with patch.object(ui.sys, "argv", ["driver", "/synthetic/morpheus-app.exe", NONCE]), \
                patch.object(ui, "acceptance", side_effect=RuntimeError("PRIVATE_TREE_OR_PATH")), \
                contextlib.redirect_stdout(captured):
            self.assertEqual(ui.main(), 1)
        self.assertNotIn("PRIVATE_TREE_OR_PATH", captured.getvalue())
        parsed = smoke.keyboard_report(captured.getvalue().encode(), check="installed_windows_keyboard_uia",
                                       validate=smoke.validate_windows_keyboard_diagnostic)
        self.assertEqual((parsed["phase"], parsed["diagnostic"]["failure"]), ("setup", "runtime_error"))
        captured = io.StringIO()
        with patch.object(ui.sys, "argv", ["driver", "/synthetic/morpheus-app.exe"]), \
                patch.object(ui, "acceptance") as acceptance, contextlib.redirect_stdout(captured):
            self.assertEqual(ui.main(), 1)
        acceptance.assert_not_called()

    def test_smoke_parser_rejects_arbitrary_values_and_incomplete_success(self):
        parse = lambda value: smoke.keyboard_report(json.dumps(value).encode(), check="installed_windows_keyboard_uia",
                                                    validate=smoke.validate_windows_keyboard_diagnostic)
        self.assertEqual(parse(report()), report())
        bad = [diagnostic(step="PRIVATE"), diagnostic(operation="PRIVATE"), diagnostic(failure="PRIVATE"),
               diagnostic(bridge_error="PRIVATE"), diagnostic(password_semantics="PRIVATE"),
               diagnostic(selector_matches=3), diagnostic(foreground_recoveries=-1), diagnostic(bounded_snapshots="0"),
               diagnostic(owned_window_seen=1), diagnostic(profile_owned="yes"), diagnostic(private_text="SECRET"),
               diagnostic(window_candidates=3), diagnostic(event_targets_ignored=True),
               diagnostic(event_targets_ignored=-1),
               diagnostic(profile_removed=True)]
        bad_windows = [fact(family="Window Class"), fact(family="PRIVATE"), fact(style=-1), fact(style=0x100000000),
                       fact(style=True), fact(exstyle="0x080800a0"), fact(style=1.5), fact(visible=1),
                       fact(owner="PRIVATE"), fact(cloaked="no"), fact(width=smoke.WINDOWS_WINDOW_EXTENT + 1),
                       fact(height=-1), fact(class_name="Window Class"), fact(title="PRIVATE"), fact(handle=65538),
                       fact(pid=77), fact(left=10), {**fact(), "rect": [10, 20, 490, 340]}, "PRIVATE", None]
        bad_inventories = [inventory(window) for window in bad_windows] + [
            inventory(status="PRIVATE"), inventory(truncated="no"), inventory(discarded=3),
            inventory(discarded=True), inventory(*[fact(style=index) for index in range(9)]),
            inventory(fact(), status="not_collected"), inventory(fact(), status="failed"),
            inventory(status="failed", discarded=1), inventory(status="not_collected", truncated=True),
            {**inventory(), "private": "PRIVATE"}, {"status": "collected", "windows": []},
            [fact()], None]
        bad += [diagnostic(window_inventory=value) for value in bad_inventories]
        for value in bad:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(report(diagnostic=value))
        good_inventories = [inventory(), inventory(status="not_collected"), inventory(status="failed"),
                            inventory(status="incomplete"),
                            inventory(fact(family=None, style=None, exstyle=None, visible=None, owner=None,
                                           cloaked=None, width=None, height=None)),
                            inventory(*[fact(style=index) for index in range(8)], truncated=True, discarded=2),
                            inventory(fact(owner="other_process", style=0xFFFFFFFF, exstyle=0, width=0,
                                           height=smoke.WINDOWS_WINDOW_EXTENT))]
        for value in good_inventories:
            with self.subTest(value=value):
                accepted = report(diagnostic=diagnostic(window_inventory=value))
                self.assertEqual(parse(accepted), accepted)
        incomplete = [diagnostic(password_semantics="unchecked"), diagnostic(cleanup="failed"),
                      diagnostic(profile_owned=False), diagnostic(uia_root_seen=False),
                      diagnostic(bridge_error="unowned"), diagnostic(step="appearance"),
                      diagnostic(window_candidates=0), diagnostic(window_candidates=2)]
        for value in incomplete:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(report(diagnostic=value))
        for value in [report(normal_window_close=False), report(screen_reader_acceptance=True),
                      report(check="installed_linux_keyboard_atspi")]:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(value)
        for failure in ["scale_unreachable", "input_rejected", "bridge_overflow", "profile_invalid"]:
            failed = report(status="failed", phase="appearance_config", appearance_config=False, normal_window_close=False,
                            diagnostic=diagnostic(step="appearance", failure=failure, cleanup="passed"))
            self.assertEqual(parse(failed)["status"], "failed")
        with self.assertRaises(RuntimeError):
            smoke.keyboard_report(json.dumps(report()).encode())  # The Linux parser does not accept it.

    def test_driver_launch_keeps_its_stream_private_and_reports_pass_and_verified_stop_separately(self):
        captured = io.StringIO()

        def run(command, **kwargs):
            kwargs["stdout"].write(b"PRIVATE powershell text\n" + json.dumps(report()).encode() + b"\n")
            return Mock(returncode=0)

        with patch.object(smoke.subprocess, "run", side_effect=run) as launch, contextlib.redirect_stdout(captured):
            self.assertEqual(smoke.windows_keyboard_launch(Path("/synthetic/morpheus-app.exe"), NONCE), (True, True))
        self.assertNotIn("PRIVATE", captured.getvalue())
        self.assertEqual(json.loads(captured.getvalue()), report())
        command = launch.call_args.args[0]
        self.assertEqual(Path(command[1]).name, "windows_package_ui.py")
        self.assertEqual(command[3], NONCE)
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
                self.assertEqual(smoke.windows_keyboard_launch(Path("/synthetic/morpheus-app.exe"), NONCE), (False, stopped))


if __name__ == "__main__":
    unittest.main()
