import contextlib
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


def diagnostic(**overrides):
    return {"step": "complete", "operation": "none", "failure": "none", "bridge_error": "none",
            "owned_window_seen": True, "uia_root_seen": True, "homeserver_seen": True,
            "username_seen": True, "password_seen": True,
            "selector_matches": 1, "incomplete_passes": 0, "foreground_recoveries": 0,
            "bounded_snapshots": 0, "password_semantics": "protected",
            "traversed_node_matched": False, "cleanup": "passed", "profile_removed": True, **overrides}


def report(**overrides):
    return {"check": "installed_windows_keyboard_uia", "status": "passed", "phase": "complete",
            "sign_in_semantics": True, "account_menu_keyboard": True, "appearance_config": True,
            "normal_window_close": True, "app_log_public": False, "screen_reader_acceptance": False,
            "diagnostic": diagnostic(), **overrides}


def node(id, name="", *, parent=0, pid=77, type="custom", password=False, focused=False, error=False):
    return {"id": id, "parent": parent, "pid": pid, "name": name, "type": type,
            "password": password, "focused": focused, "error": error}


class FakeBridge:
    """Replays snapshot/focus/key replies; records every command it received."""
    def __init__(self, snapshots, focused=None, key_replies=None):
        self.snapshots = list(snapshots)
        self.focused = focused if callable(focused) else list(focused or [])
        self.focused_calls = 0
        self.key_replies = list(key_replies or [])
        self.commands = []

    def call(self, command, allow_error=False):
        self.commands.append(command)
        if command["op"] == "snapshot":
            value = self.snapshots.pop(0)
        elif command["op"] == "focused":
            if callable(self.focused):
                value = self.focused(self.focused_calls)
                self.focused_calls += 1
            else:
                value = self.focused.pop(0)
        elif command["op"] == "key":
            value = self.key_replies.pop(0) if self.key_replies else {"ok": True, "recovered": False}
        else:
            value = {"ok": True}
        if isinstance(value, Exception):
            raise value
        return value


def driver(bridge, pid=77, alive=True):
    diagnostics = ui.Diagnostics({})
    job = Mock()
    job.kernel.WaitForSingleObject.return_value = 258 if alive else 0
    instance = ui.WindowsUI(bridge, pid, job, 20, diagnostics)
    return instance, diagnostics


def snapshot(*nodes, bounded=False):
    return {"ok": True, "nodes": [node(0, pid=77, type="window", parent=-1), *nodes], "bounded": bounded}


class GuardAndProfileTests(unittest.TestCase):
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

    def test_guard_failure_cannot_create_a_job_or_process(self):
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}, clear=False), \
                patch.object(ui.sys, "platform", "win32"), \
                patch.object(capture, "WindowsJob") as job, patch.object(ui, "profile_roots") as roots:
            with self.assertRaises(ui.DiagnosticFailure):
                ui.acceptance(Path("/synthetic/morpheus-app.exe"), {})
        job.assert_not_called()
        roots.assert_not_called()

    def test_preexisting_profile_state_is_refused_and_untouched(self):
        with tempfile.TemporaryDirectory() as folder:
            roots = [Path(folder) / "roaming/brk275/Morpheus", Path(folder) / "local/brk275/Morpheus"]
            roots[1].mkdir(parents=True)
            (roots[1] / "existing").write_text("keep")
            with self.assertRaises(ui.DiagnosticFailure) as error:
                ui.seed_profile(roots)
            self.assertEqual(error.exception.code, "profile_exists")
            self.assertFalse(roots[0].exists())
            self.assertEqual((roots[1] / "existing").read_text(), "keep")

    def test_fresh_profile_is_seeded_then_removed_completely(self):
        with tempfile.TemporaryDirectory() as folder:
            roots = [Path(folder) / "roaming/brk275/Morpheus", Path(folder) / "local/brk275/Morpheus"]
            config = ui.seed_profile(roots)
            self.assertEqual(config, roots[0] / "config/config.toml")
            self.assertEqual(config.read_text(encoding="utf-8"), ui.SEED_CONFIG)
            roots[1].mkdir(parents=True)
            (roots[1] / "data").write_bytes(b"synthetic")
            self.assertTrue(ui.remove_profile(roots))
            self.assertFalse(roots[0].exists())
            self.assertFalse(roots[1].exists())
            # The organization folders this run created are gone as well.
            self.assertFalse(roots[0].parent.exists())
            self.assertFalse(roots[1].parent.exists())

    def test_existing_organization_folder_or_reparse_point_is_refused(self):
        with tempfile.TemporaryDirectory() as folder:
            base = Path(folder)
            elsewhere = base / "elsewhere"
            elsewhere.mkdir()
            (elsewhere / "vault").write_bytes(b"not ours")
            cases = {
                "organization_folder": lambda roots: roots[0].parent.mkdir(parents=True),
                "root_file": lambda roots: (roots[1].parent.mkdir(parents=True), roots[1].write_text("x")),
                "root_symlink": lambda roots: (roots[0].parent.mkdir(parents=True), os.symlink(elsewhere, roots[0], target_is_directory=True)),
                "dangling_symlink": lambda roots: (roots[1].parent.mkdir(parents=True), os.symlink(base / "missing", roots[1])),
                "organization_symlink": lambda roots: (roots[0].parent.parent.mkdir(parents=True), os.symlink(elsewhere, roots[0].parent, target_is_directory=True)),
            }
            for name, arrange in cases.items():
                with self.subTest(case=name):
                    roots = [base / name / "roaming/brk275/Morpheus", base / name / "local/brk275/Morpheus"]
                    arrange(roots)
                    with self.assertRaises(ui.DiagnosticFailure) as error:
                        ui.seed_profile(roots)
                    self.assertEqual(error.exception.code, "profile_exists")
                    self.assertFalse((roots[0] / "config").exists())
            self.assertEqual((elsewhere / "vault").read_bytes(), b"not ours")

    def test_removal_never_follows_a_redirected_root(self):
        with tempfile.TemporaryDirectory() as folder:
            base = Path(folder)
            elsewhere = base / "elsewhere"
            elsewhere.mkdir()
            (elsewhere / "vault").write_bytes(b"not ours")
            roots = [base / "roaming/brk275/Morpheus", base / "local/brk275/Morpheus"]
            roots[0].parent.mkdir(parents=True)
            os.symlink(elsewhere, roots[0], target_is_directory=True)
            with self.assertRaises(RuntimeError):
                ui.remove_profile(roots)
            self.assertEqual((elsewhere / "vault").read_bytes(), b"not ours")
            self.assertTrue(roots[0].is_symlink())

    def test_profile_roots_come_from_known_folders_not_environment_variables(self):
        with patch.dict(os.environ, {"APPDATA": "/env/roaming", "LOCALAPPDATA": "/env/local"}), \
                patch.object(ui, "known_folder", side_effect=lambda name: Path("/known") / name) as resolve:
            roots = ui.profile_roots()
        self.assertEqual(roots, [Path("/known/roaming/brk275/Morpheus"), Path("/known/local/brk275/Morpheus")])
        self.assertEqual([call.args[0] for call in resolve.call_args_list], ["roaming", "local"])
        source = (Path(__file__).resolve().parents[1] / "windows_package_ui.py").read_text(encoding="utf-8")
        for forbidden in ['"APPDATA"', '"LOCALAPPDATA"', "USERPROFILE", "winreg", "SetKnownFolderPath",
                          "SHSetKnownFolderPath", "MORPHEUS_QA_PROFILE_DIR\"]"]:
            self.assertNotIn(forbidden, source.replace('env.pop("MORPHEUS_QA_PROFILE_DIR", None)', ""))
        self.assertIn("SHGetKnownFolderPath", source)
        self.assertEqual(ui.KNOWN_FOLDERS["roaming"], "{3EB685DB-65F9-4CF6-A03A-E3EF65729F3D}")
        self.assertEqual(ui.KNOWN_FOLDERS["local"], "{F1B32785-6FBA-4FCF-9D55-7B8E7F157091}")

    def test_release_profile_override_is_never_forwarded_and_no_folder_is_redirected(self):
        with patch.dict(os.environ, {"MORPHEUS_QA_PROFILE_DIR": "/should/not/be/used", "APPDATA": "/runner/roaming"}):
            env = ui.environment_for("a" * 32)
            # The launcher adds only the nonce; it cannot and does not try to
            # relocate the release profile through the environment.
            self.assertEqual(set(env) - set(os.environ), {smoke.NONCE_ENV})
            self.assertEqual(env["APPDATA"], "/runner/roaming")
        self.assertNotIn("MORPHEUS_QA_PROFILE_DIR", env)
        self.assertEqual(env[smoke.NONCE_ENV], "a" * 32)

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


class TraversalTests(unittest.TestCase):
    def test_unowned_root_or_errored_root_is_not_a_tree(self):
        for root in (node(0, pid=1, type="window", parent=-1), node(0, type="window", parent=-1, error=True)):
            bridge = FakeBridge([{"ok": True, "nodes": [root], "bounded": False}])
            instance, diagnostics = driver(bridge)
            with self.subTest(root=root), self.assertRaises(ui.DiagnosticFailure) as error:
                list(instance.nodes())
            self.assertEqual(error.exception.code, "bridge_invalid")
            self.assertFalse(diagnostics.value["uia_root_seen"])

    def test_unowned_node_is_skipped_and_marks_the_pass_incomplete(self):
        bridge = FakeBridge([snapshot(node(1, "Homeserver URL", type="edit"), node(2, "Other app", pid=5))])
        instance, diagnostics = driver(bridge)
        with self.assertRaises(ui.IncompleteTraversal):
            instance.find("Homeserver URL")
        self.assertEqual(diagnostics.value["incomplete_passes"], 1)
        self.assertTrue(diagnostics.value["uia_root_seen"])

    def test_node_read_error_marks_the_pass_incomplete_but_ambiguity_still_fails(self):
        bridge = FakeBridge([snapshot(node(1, "Password", type="edit", password=True), node(2, error=True))])
        instance, diagnostics = driver(bridge)
        with self.assertRaises(ui.IncompleteTraversal):
            instance.find("Password")
        bridge = FakeBridge([snapshot(node(1, "Password"), node(2, error=True), node(3, "Password"))])
        instance, diagnostics = driver(bridge)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.find("Password")
        self.assertEqual(error.exception.code, "selector_ambiguous")
        self.assertEqual(diagnostics.value["selector_matches"], 2)

    def test_incomplete_pass_is_retried_until_a_complete_pass_decides(self):
        bridge = FakeBridge([snapshot(node(1, "Done"), node(2, error=True)), snapshot(node(1, "Done"))])
        instance, diagnostics = driver(bridge)
        found = instance.wait("Done")
        self.assertEqual(found.id, 1)
        self.assertEqual(diagnostics.value["incomplete_passes"], 1)
        self.assertEqual([command["op"] for command in bridge.commands], ["snapshot", "snapshot"])

    def test_bounded_snapshot_is_not_evidence(self):
        bridge = FakeBridge([snapshot(node(1, "Done"), bounded=True)])
        instance, diagnostics = driver(bridge)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.find("Done")
        self.assertEqual(error.exception.code, "tree_bound")
        self.assertEqual(diagnostics.value["bounded_snapshots"], 1)

    def test_prefix_and_exact_selectors_publish_only_booleans(self):
        bridge = FakeBridge([snapshot(node(1, "Account menu for Casey, @casey:example.org"))] * 2)
        instance, diagnostics = driver(bridge)
        self.assertIsNotNone(instance.find("Account menu for ", prefix=True))
        self.assertTrue(diagnostics.value["traversed_node_matched"])
        self.assertIsNone(instance.find("Account menu for "))
        self.assertNotIn("casey", json.dumps(diagnostics.value))

    def test_malformed_snapshot_entries_are_rejected(self):
        for raw in [{"ok": True, "nodes": "nodes", "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, type="window"), {"id": 1}], "bounded": False},
                    {"ok": True, "nodes": [node(0, parent=-1, type="window"), node(5)], "bounded": False},
                    {"ok": True, "nodes": [], "bounded": False},
                    {"ok": True, "nodes": [dict(node(0, parent=-1, type="window"), name=5)], "bounded": "no"}]:
            bridge = FakeBridge([raw])
            instance, _ = driver(bridge)
            with self.subTest(raw=raw), self.assertRaises(ui.DiagnosticFailure) as error:
                list(instance.nodes())
            self.assertEqual(error.exception.code, "bridge_invalid")

    def test_early_process_exit_is_not_a_wait_timeout(self):
        bridge = FakeBridge([snapshot()] * 50)
        instance, diagnostics = driver(bridge, alive=False)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.wait("Password")
        self.assertEqual(error.exception.code, "app_exited")


class FocusAndKeyTests(unittest.TestCase):
    def test_focus_outside_the_owned_process_fails_closed(self):
        bridge = FakeBridge([], focused=[{"ok": True, "present": True, "owned": False, "name": ""}])
        instance, _ = driver(bridge)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.focused_name()
        self.assertEqual(error.exception.code, "focus_unowned")

    def test_no_key_is_sent_without_an_owned_focused_element(self):
        for focused in ({"ok": True, "present": False, "owned": False, "name": ""},
                        {"ok": True, "present": True, "owned": False, "name": ""}):
            bridge = FakeBridge([], focused=[focused])
            instance, _ = driver(bridge)
            with self.subTest(focused=focused), self.assertRaises(ui.DiagnosticFailure) as error:
                instance.key("Return")
            self.assertEqual(error.exception.code, "focus_unowned")
            self.assertNotIn("key", [command["op"] for command in bridge.commands])

    def test_lost_foreground_is_a_failure_and_recovery_is_counted(self):
        focused = {"ok": True, "present": True, "owned": True, "name": "Done"}
        bridge = FakeBridge([], focused=[focused], key_replies=[{"ok": False, "error": "foreground_unowned"}])
        instance, diagnostics = driver(bridge)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            instance.key("Tab")
        self.assertEqual(error.exception.code, "foreground_unowned")
        self.assertEqual(diagnostics.value["bridge_error"], "foreground_unowned")
        bridge = FakeBridge([], focused=[focused], key_replies=[{"ok": True, "recovered": True}])
        instance, diagnostics = driver(bridge)
        instance.key("ShiftTab")
        self.assertEqual(diagnostics.value["foreground_recoveries"], 1)
        self.assertEqual(bridge.commands[-1], {"op": "key", "key": "ShiftTab"})

    def test_only_reviewed_keys_are_sent(self):
        bridge = FakeBridge([], focused=[{"ok": True, "present": True, "owned": True, "name": "Done"}])
        instance, _ = driver(bridge)
        with self.assertRaises(RuntimeError):
            instance.key("Alt+F4")
        self.assertEqual(bridge.commands, [])

    def test_focus_command_targets_the_found_snapshot_element(self):
        bridge = FakeBridge([snapshot(node(3, "Open settings", type="button"))],
                            focused=[{"ok": True, "present": True, "owned": True, "name": "Open settings"}])
        instance, _ = driver(bridge)
        instance.focus("Open settings")
        self.assertIn({"op": "focus", "id": 3}, bridge.commands)

    def test_settings_scale_control_is_reached_by_bounded_tabs_before_its_label_is_queried(self):
        focus_sequence = ["Open settings", "Open settings", "Open settings", "Dark theme", "Dark theme",
                          "Decrease font scale", "Decrease font scale", "Increase font scale",
                          "Increase font scale", "Increase font scale"]
        focused = [{"ok": True, "present": True, "owned": True, "name": name} for name in focus_sequence]
        bridge = FakeBridge([snapshot(node(1, "Open settings", type="button")),
                             snapshot(node(1, "110% · text throughout the app", type="text")),
                             snapshot(node(1, "120% · text throughout the app", type="text"))], focused=focused)
        instance, _ = driver(bridge)
        ui.appearance_keyboard(instance)
        ops = [command for command in bridge.commands if command["op"] in ("key", "snapshot")]
        keys = [command["key"] for command in ops if command["op"] == "key"]
        self.assertEqual(keys, ["Return", "Tab", "Tab", "Space"])
        # No snapshot (label query) happens between opening Settings and reaching the control.
        first_tab = ops.index({"op": "key", "key": "Tab"})
        last_tab = len(ops) - 1 - ops[::-1].index({"op": "key", "key": "Tab"})
        self.assertFalse(any(command["op"] == "snapshot" for command in ops[first_tab:last_tab + 1]))

    def test_unreachable_scale_control_fails_within_the_tab_bound(self):
        # Every focus query sees a new owned control, so each Tab appears to
        # move focus without ever reaching the scale control.
        def focused(call):
            return {"ok": True, "present": True, "owned": True,
                    "name": "Open settings" if call == 0 else f"Control {call}"}

        bridge = FakeBridge([snapshot(node(1, "Open settings", type="button"))], focused=focused)
        instance, _ = driver(bridge)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            ui.appearance_keyboard(instance)
        self.assertEqual(error.exception.code, "scale_unreachable")
        self.assertLessEqual(sum(1 for command in bridge.commands if command["op"] == "key"), ui.SETTINGS_TAB_BOUND + 1)


class SignInSemanticsTests(unittest.TestCase):
    def fields(self, password_type="edit", protected=True):
        return snapshot(node(1, "Homeserver URL", type="edit"), node(2, "Matrix ID or username", type="edit"),
                        node(3, "Password", type=password_type, password=protected))

    def test_protected_password_field_passes_without_reading_values(self):
        bridge = FakeBridge([self.fields()] * 3)
        instance, diagnostics = driver(bridge)
        record = {}
        ui.inspect_sign_in(instance, record, diagnostics)
        self.assertTrue(record["sign_in_semantics"])
        self.assertEqual(diagnostics.value["password_semantics"], "protected")
        self.assertTrue(all(diagnostics.value[key] for key in ("homeserver_seen", "username_seen", "password_seen")))
        self.assertTrue(all(command["op"] == "snapshot" for command in bridge.commands))

    def test_unprotected_or_non_edit_password_cannot_pass(self):
        for kind, expected in [(("edit", False), "unprotected"), (("text", False), "not_edit"), (("text", True), "not_edit")]:
            bridge = FakeBridge([self.fields(*kind)] * 3)
            instance, diagnostics = driver(bridge)
            record = {}
            with self.subTest(kind=kind), self.assertRaises(ui.DiagnosticFailure) as error:
                ui.inspect_sign_in(instance, record, diagnostics)
            self.assertEqual(error.exception.code, "password_semantics")
            self.assertEqual(diagnostics.value["password_semantics"], expected)
            self.assertNotIn("sign_in_semantics", record)

    def test_missing_second_field_retains_the_first_observation(self):
        bridge = FakeBridge([snapshot(node(1, "Homeserver URL", type="edit"))] * 200)
        instance, diagnostics = driver(bridge)
        with patch.object(ui, "WAIT_SECONDS", 0.3), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.inspect_sign_in(instance, {}, diagnostics)
        self.assertEqual(error.exception.code, "wait_timeout")
        self.assertTrue(diagnostics.value["homeserver_seen"])
        self.assertFalse(diagnostics.value["username_seen"])
        self.assertEqual(diagnostics.value["step"], "username")


class BridgeProtocolTests(unittest.TestCase):
    def bridge(self, lines):
        instance = object.__new__(ui.Bridge)
        instance.diagnostics = ui.Diagnostics({})
        instance.replies = ui.queue.Queue()
        for line in lines:
            instance.replies.put(line)
        instance.process = Mock()
        instance.process.poll.return_value = None
        instance.process.stdin = io.StringIO()
        return instance

    def test_replies_must_be_json_objects_with_a_boolean_status(self):
        for line in ["not json\n", "[1]\n", '{"ok": "yes"}\n', ""]:
            bridge = self.bridge([line])
            with self.subTest(line=line), self.assertRaises(ui.DiagnosticFailure) as error:
                bridge.reply()
            self.assertEqual(error.exception.code, "bridge_invalid")
        bridge = self.bridge([None])
        with self.assertRaises(ui.DiagnosticFailure) as error:
            bridge.reply()
        self.assertEqual(error.exception.code, "bridge_exited")

    def test_bridge_errors_are_fixed_tokens_and_unknown_tokens_collapse(self):
        bridge = self.bridge(['{"ok": false, "error": "unowned"}\n', '{"ok": false, "error": "C:\\\\private\\\\path"}\n'])
        with self.assertRaises(ui.DiagnosticFailure):
            bridge.call({"op": "focus", "id": 1})
        self.assertEqual(bridge.diagnostics.value["bridge_error"], "unowned")
        bridge.diagnostics.value["bridge_error"] = "none"
        with self.assertRaises(ui.BridgeFailure) as error:
            bridge.call({"op": "key", "key": "Tab"}, allow_error=True)
        self.assertEqual(error.exception.code, "bridge_error")
        self.assertNotIn("private", json.dumps(bridge.diagnostics.value))
        self.assertEqual(bridge.process.stdin.getvalue(), '{"op":"focus","id":1}\n{"op":"key","key":"Tab"}\n')

    def test_silent_bridge_is_a_timeout_not_a_hang(self):
        bridge = self.bridge([])
        with patch.object(ui, "BRIDGE_REPLY_SECONDS", 0.01):
            with self.assertRaises(ui.DiagnosticFailure) as error:
                bridge.reply()
        self.assertEqual(error.exception.code, "bridge_timeout")

    def test_powershell_bridge_source_keeps_its_guards_and_never_reads_values(self):
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn("ValuePattern", source)
        self.assertNotIn("TextPattern", source)
        self.assertNotIn("Exception.Message", source)
        self.assertNotIn("$_", source)
        for required in ["$env:GITHUB_ACTIONS -ne 'true'", "$env:RUNNER_ENVIRONMENT -ne 'github-hosted'",
                         "$env:ImageOS -ne 'win25'", "[Environment]::UserInteractive",
                         "IsPassword", "HasKeyboardFocus", "GetForegroundWindow", "SendInput",
                         "RawViewWalker", "$MaxNodes = 5000"]:
            self.assertIn(required, source)
        # Every error token the bridge can emit is in the reviewed allowlist.
        for token in re.findall(r"Fail '([a-z_]+)'", source):
            self.assertIn(token, smoke.WINDOWS_BRIDGE_ERRORS)
        # The foreground revalidation precedes the only SendInput call path.
        self.assertLess(source.index("if (-not (ForegroundOwned))"), source.index("[MorpheusInput]::Key($Keys[$name], $false)"))
        self.assertNotIn("PostMessage", source)
        self.assertNotIn("SendMessage", source)


class CleanupAndReportTests(unittest.TestCase):
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

    def test_failure_still_terminates_the_job_removes_the_profile_and_keeps_the_first_failure(self):
        record = {"sign_in_semantics": False}
        trace = ui.Diagnostics(record)
        job = Mock()
        job.spawn.return_value = 20
        bridge = Mock()

        def fail_signin(_ui, _report, diagnostics):
            diagnostics.step("password_semantics")
            raise ui.DiagnosticFailure("password_semantics")

        with tempfile.TemporaryDirectory() as folder:
            roots = [Path(folder) / "roaming/brk275/Morpheus", Path(folder) / "local/brk275/Morpheus"]
            with patch.object(ui, "guard"), patch.object(ui, "installed_executable"), \
                    patch.object(ui, "profile_roots", return_value=roots), \
                    patch.object(capture, "WindowsJob", return_value=job), \
                    patch.object(ui, "process_id", return_value=77), \
                    patch.object(ui, "owned_window", return_value=65538), \
                    patch.object(ui, "Bridge", return_value=bridge), \
                    patch.object(ui, "inspect_sign_in", side_effect=fail_signin), \
                    patch.object(ui, "normal_close") as close:
                with self.assertRaises(ui.DiagnosticFailure):
                    ui.acceptance(Path("/synthetic/morpheus-app.exe"), record, trace)
            self.assertFalse(roots[0].exists())
        job.close.assert_called_once()
        bridge.close.assert_called_once()
        close.assert_not_called()
        self.assertEqual(trace.value["failure"], "password_semantics")
        self.assertEqual(trace.value["step"], "password_semantics")
        self.assertEqual(trace.value["cleanup"], "passed")
        self.assertTrue(trace.value["profile_removed"])
        self.assertFalse(record["sign_in_semantics"])

    def test_cleanup_failure_is_reported_without_replacing_the_original_failure(self):
        record = {}
        trace = ui.Diagnostics(record)
        job = Mock()
        job.spawn.return_value = 20
        job.close.side_effect = capture.Failure("cleanup")
        with tempfile.TemporaryDirectory() as folder:
            roots = [Path(folder) / "roaming/brk275/Morpheus", Path(folder) / "local/brk275/Morpheus"]
            with patch.object(ui, "guard"), patch.object(ui, "installed_executable"), \
                    patch.object(ui, "profile_roots", return_value=roots), \
                    patch.object(capture, "WindowsJob", return_value=job), \
                    patch.object(ui, "process_id", return_value=77), \
                    patch.object(ui, "owned_window", side_effect=ui.DiagnosticFailure("window_count")):
                with self.assertRaises(capture.Failure):
                    ui.acceptance(Path("/synthetic/morpheus-app.exe"), record, trace)
            self.assertFalse(roots[0].exists())
        self.assertEqual(trace.value["failure"], "window_count")
        self.assertEqual(trace.value["cleanup"], "failed")
        self.assertTrue(trace.value["profile_removed"])

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
            self.assertEqual(ui.owned_window(job, 20, 77, trace), 65538)
        self.assertTrue(trace.value["owned_window_seen"])
        with patch.object(smoke, "visible_windows", return_value=[1, 2]), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.owned_window(job, 20, 77, ui.Diagnostics({}))
        self.assertEqual(error.exception.code, "window_count")
        job.kernel.WaitForSingleObject.return_value = 0
        with patch.object(smoke, "visible_windows", return_value=[]), self.assertRaises(ui.DiagnosticFailure) as error:
            ui.owned_window(job, 20, 77, ui.Diagnostics({}))
        self.assertEqual(error.exception.code, "app_exited")

    def test_main_publishes_only_the_fixed_report(self):
        captured = io.StringIO()
        with patch.object(ui.sys, "argv", ["driver", "/synthetic/morpheus-app.exe"]), \
                patch.object(ui, "acceptance", side_effect=RuntimeError("PRIVATE_TREE_OR_PATH")), \
                contextlib.redirect_stdout(captured):
            self.assertEqual(ui.main(), 1)
        self.assertNotIn("PRIVATE_TREE_OR_PATH", captured.getvalue())
        parsed = smoke.keyboard_report(captured.getvalue().encode(), check="installed_windows_keyboard_uia",
                                       validate=smoke.validate_windows_keyboard_diagnostic)
        self.assertEqual(parsed["phase"], "setup")
        self.assertEqual(parsed["diagnostic"]["failure"], "runtime_error")

    def test_smoke_parser_rejects_arbitrary_values_and_incomplete_success(self):
        parse = lambda value: smoke.keyboard_report(json.dumps(value).encode(), check="installed_windows_keyboard_uia",
                                                    validate=smoke.validate_windows_keyboard_diagnostic)
        self.assertEqual(parse(report()), report())
        bad = [diagnostic(step="PRIVATE"), diagnostic(operation="PRIVATE"), diagnostic(failure="PRIVATE"),
               diagnostic(bridge_error="PRIVATE"), diagnostic(password_semantics="PRIVATE"),
               diagnostic(selector_matches=3), diagnostic(foreground_recoveries=-1), diagnostic(bounded_snapshots="0"),
               diagnostic(owned_window_seen=1), diagnostic(profile_removed="yes"), diagnostic(private_text="SECRET")]
        for value in bad:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(report(diagnostic=value))
        incomplete = [diagnostic(password_semantics="unchecked"), diagnostic(cleanup="failed"),
                      diagnostic(profile_removed=False), diagnostic(uia_root_seen=False),
                      diagnostic(bridge_error="unowned"), diagnostic(step="appearance")]
        for value in incomplete:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(report(diagnostic=value))
        for value in [report(normal_window_close=False), report(screen_reader_acceptance=True),
                      report(check="installed_linux_keyboard_atspi")]:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                parse(value)
        failed = report(status="failed", phase="appearance_config", appearance_config=False, normal_window_close=False,
                        diagnostic=diagnostic(step="appearance", failure="scale_unreachable", cleanup="passed"))
        self.assertEqual(parse(failed)["status"], "failed")
        with self.assertRaises(RuntimeError):
            smoke.keyboard_report(json.dumps(report()).encode())  # The Linux parser does not accept it.

    def test_smoke_runs_the_keyboard_driver_after_the_plain_launch_and_before_uninstall(self):
        events = []
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as work:
            dist = Path(folder)
            (dist / "Morpheus_0.1.0_x64-setup.exe").write_bytes(b"synthetic")
            installed = Path(work) / "Installed Morpheus"
            entry = {"DisplayName": "Morpheus", "DisplayVersion": smoke.package_version(),
                     "InstallLocation": str(installed)}
            with patch.object(smoke, "record"), \
                    patch.object(smoke.tempfile, "TemporaryDirectory", return_value=contextlib.nullcontext(work)), \
                    patch.object(smoke, "uninstall_registry", side_effect=[[], [entry], []]), \
                    patch.object(smoke, "run", side_effect=lambda *a, **k: events.append("nsis")), \
                    patch.object(smoke, "check_windows_binary"), patch.object(smoke, "check_resources"), \
                    patch.object(smoke, "launch_smoke", side_effect=lambda *a, **k: events.append(("launch", a))), \
                    patch.object(smoke, "windows_keyboard_launch", side_effect=lambda *a: events.append(("keyboard", a))):
                smoke.windows_smoke(dist)
        self.assertEqual(events[0], "nsis")
        self.assertEqual(events[1], ("launch", (installed / "morpheus-app.exe", "win32")))
        self.assertEqual(events[2], ("keyboard", (installed / "morpheus-app.exe",)))

    def test_plain_launch_failure_never_starts_the_keyboard_driver(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as work:
            dist = Path(folder)
            (dist / "Morpheus_0.1.0_x64-setup.exe").write_bytes(b"synthetic")
            entry = {"DisplayName": "Morpheus", "DisplayVersion": smoke.package_version(),
                     "InstallLocation": str(Path(work) / "Installed Morpheus")}
            with patch.object(smoke, "record"), \
                    patch.object(smoke.tempfile, "TemporaryDirectory", return_value=contextlib.nullcontext(work)), \
                    patch.object(smoke, "uninstall_registry", side_effect=[[], [entry], []]), \
                    patch.object(smoke, "run"), patch.object(smoke, "check_windows_binary"), \
                    patch.object(smoke, "check_resources"), \
                    patch.object(smoke, "launch_smoke", side_effect=RuntimeError("startup")), \
                    patch.object(smoke, "windows_keyboard_launch") as keyboard:
                with self.assertRaises(RuntimeError):
                    smoke.windows_smoke(dist)
        keyboard.assert_not_called()

    def test_driver_launch_keeps_its_stream_private_and_requires_a_passing_report(self):
        captured = io.StringIO()

        def run(command, **kwargs):
            kwargs["stdout"].write(b"PRIVATE powershell text\n" + json.dumps(report()).encode() + b"\n")
            return Mock(returncode=0)

        with patch.object(smoke.subprocess, "run", side_effect=run) as launch, contextlib.redirect_stdout(captured):
            smoke.windows_keyboard_launch(Path("/synthetic/morpheus-app.exe"))
        self.assertNotIn("PRIVATE", captured.getvalue())
        self.assertEqual(json.loads(captured.getvalue()), report())
        command = launch.call_args.args[0]
        self.assertEqual(Path(command[1]).name, "windows_package_ui.py")
        self.assertEqual(launch.call_args.kwargs["stdin"], smoke.subprocess.DEVNULL)

        def failing(command, **kwargs):
            kwargs["stdout"].write(json.dumps(report(status="failed", phase="setup", sign_in_semantics=False,
                                                     account_menu_keyboard=False, appearance_config=False,
                                                     normal_window_close=False,
                                                     diagnostic=diagnostic(step="setup", failure="guard",
                                                                           owned_window_seen=False, uia_root_seen=False,
                                                                           homeserver_seen=False, username_seen=False,
                                                                           password_seen=False,
                                                                           password_semantics="unchecked",
                                                                           selector_matches=0))).encode())
            return Mock(returncode=1)

        with patch.object(smoke.subprocess, "run", side_effect=failing), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(RuntimeError):
                smoke.windows_keyboard_launch(Path("/synthetic/morpheus-app.exe"))


if __name__ == "__main__":
    unittest.main()
