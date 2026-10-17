import contextlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, call, patch

from scripts import ci_scope, linux_package_ui as ui, package_smoke as smoke


def diagnostic(**overrides):
    return {"step": "complete", "operation": "none", "failure": "none",
            "desktop_app_seen": True, "owned_app_seen": True, "owned_node_seen": True,
            "homeserver_seen": True, "username_seen": True, "password_seen": True,
            "selector_matches": 1, "password_role": "password_text", "cleanup": "passed",
            "traversed_node_matched": False,
            "child_count_unavailable": 0, "child_unavailable": 0, "incomplete_passes": 0,
            "focus_kind": "other", "focused_nodes": 1,
            **overrides}


def report(**overrides):
    return {"check": "installed_linux_keyboard_atspi", "status": "passed", "phase": "complete",
            "sign_in_semantics": True, "account_menu_keyboard": True, "appearance_config": True,
            "normal_window_close": True, "app_log_public": False, "screen_reader_acceptance": False,
            "diagnostic": diagnostic(), **overrides}


class PackageKeyboardMockTests(unittest.TestCase):
    def test_ownership_accepts_appimage_child_but_not_other_group_or_nonce(self):
        with tempfile.TemporaryDirectory() as folder:
            proc = Path(folder)
            (proc / "202").mkdir()
            (proc / "202/environ").write_bytes(b"MORPHEUS_PACKAGE_SMOKE_NONCE=owned\0")
            with patch.object(smoke, "linux_process_identity", side_effect=[("S", 101, 101, 7), ("R", 101, 101, 7)]):
                self.assertTrue(smoke.owned_linux_pid(202, 101, "owned", proc))
            for first, second, nonce in [(("S", 999, 999, 7), ("S", 999, 999, 7), "owned"),
                                         (("S", 101, 101, 7), ("S", 101, 101, 8), "owned"),
                                         (("S", 101, 101, 7), ("Z", 101, 101, 7), "owned"),
                                         (("S", 101, 101, 7), ("S", 101, 101, 7), "other")]:
                with self.subTest(first=first, second=second, nonce=nonce), \
                        patch.object(smoke, "linux_process_identity", side_effect=[first, second]):
                    self.assertFalse(smoke.owned_linux_pid(202, 101, nonce, proc))

    def test_linux_stat_parser_handles_spaces_and_parentheses(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "123"
            path.mkdir()
            fields = ["S", "1", "100", "100"] + ["0"] * 15 + ["901"]
            (path / "stat").write_text("123 (app (runtime)) " + " ".join(fields))
            self.assertEqual(smoke.linux_process_identity(123, Path(folder)), ("S", 100, 100, 901))

    def test_same_title_from_unowned_process_is_not_window_evidence(self):
        outputs = ['     0x123 "Morpheus": ("Morpheus" "Morpheus")\n', "_NET_WM_PID(CARDINAL) = 42\n"]
        with patch.object(smoke, "x11_output", side_effect=outputs), \
                patch.object(smoke, "owned_linux_pid", return_value=False):
            self.assertEqual(smoke.owned_x11_windows(101, "nonce"), [])

    def test_owned_hidden_window_is_not_visibility_evidence(self):
        outputs = ['     0x123 "Morpheus": app\n', "_NET_WM_PID(CARDINAL) = 202\n", "  Map State: IsUnMapped\n"]
        with patch.object(smoke, "x11_output", side_effect=outputs), \
                patch.object(smoke, "owned_linux_pid", return_value=True):
            self.assertEqual(smoke.owned_x11_windows(101, "nonce"), [])

    def test_window_rechecks_owner_after_reading_visibility(self):
        outputs = ['     0x123 "Morpheus": app\n', "_NET_WM_PID(CARDINAL) = 202\n", "  Map State: IsViewable\n"]
        with patch.object(smoke, "x11_output", side_effect=outputs), \
                patch.object(smoke, "owned_linux_pid", side_effect=[True, False]):
            self.assertEqual(smoke.owned_x11_windows(101, "nonce"), [])

    def test_failure_does_not_publish_application_log(self):
        process = Mock(pid=123, returncode=None)

        def launch(_args, **kwargs):
            kwargs["stdout"].write(b"PRIVATE_APPLICATION_DIAGNOSTIC")
            return process

        captured = io.StringIO()
        with patch.object(smoke.sys, "platform", "linux"), \
                patch.object(smoke.subprocess, "Popen", side_effect=launch), \
                patch.object(smoke, "process_status", return_value=7), \
                patch.object(smoke, "require_nonreaping_wait"), patch.object(smoke, "stop_process"), \
                contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
            with self.assertRaises(RuntimeError):
                smoke.launch_smoke(Path("/synthetic/app"))
        self.assertNotIn("PRIVATE_APPLICATION_DIAGNOSTIC", captured.getvalue())
        record = json.loads(captured.getvalue())
        self.assertIs(record["app_log_public"], False)
        self.assertEqual(record, {
            "check": "installed_startup", "status": "failed", "app_log_public": False,
            "diagnostic": {"category": "unclassified", "exit_status": 7, "missing_library": None},
        })

    def test_minimal_launch_failure_never_installs_accessibility_dependencies(self):
        with patch.object(smoke, "linux_display_launch", side_effect=RuntimeError("failed")), \
                patch.object(smoke, "apt_install") as install:
            with self.assertRaises(RuntimeError):
                smoke.linux_appimage_launch(Path("/synthetic/app"))
        install.assert_not_called()

    def test_same_package_is_reused_after_runtime_proof(self):
        events = []
        executable = Path("/synthetic/app")
        with patch.object(smoke, "linux_display_launch", side_effect=lambda *a, **k: events.append((a, k))), \
                patch.object(smoke, "apt_install", side_effect=lambda *a: events.append("deps")), \
                patch.object(smoke, "record"):
            smoke.linux_appimage_launch(executable)
        self.assertEqual(events, [((executable,), {}), "deps", ((executable,), {"keyboard": True})])

    def test_driver_output_is_allowlisted_and_cannot_claim_incomplete_success(self):
        raw = b"private native library error\n" + json.dumps(report()).encode()
        self.assertEqual(smoke.keyboard_report(raw), report())
        for value in [report(normal_window_close=False), report(phase="setup"),
                      report(private_path="secret"), report(screen_reader_acceptance=True),
                      report(appearance_config="true")]:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                smoke.keyboard_report(json.dumps(value).encode())
        for raw in [b"zero results", json.dumps(report()).encode() * 2, b"a" * 65537]:
            with self.assertRaises(RuntimeError):
                smoke.keyboard_report(raw)

    def test_appearance_checkpoints_are_closed_failure_facts_not_success(self):
        for step in ("appearance_reach_scale", "appearance_initial_scale", "appearance_saved_scale",
                     "appearance_invalid_notice", "appearance_invalid_retained", "appearance_restored"):
            value = report(status="failed", phase="appearance_config", appearance_config=False,
                           normal_window_close=False, diagnostic=diagnostic(step=step, failure="wait_timeout"))
            self.assertEqual(smoke.keyboard_report(json.dumps(value).encode()), value)
            with self.assertRaises(RuntimeError):
                smoke.keyboard_report(json.dumps(report(diagnostic=diagnostic(step=step))).encode())
        with self.assertRaises(RuntimeError):
            smoke.keyboard_report(json.dumps(report(diagnostic=diagnostic(step="private arbitrary label"))).encode())

    def test_profile_is_private_and_uses_release_xdg_paths(self):
        with tempfile.TemporaryDirectory() as folder, \
                patch.dict(os.environ, {"MORPHEUS_QA_PROFILE_DIR": "/should/not/be/used"}):
            env, config = ui.seed_profile(Path(folder))
            self.assertNotIn("MORPHEUS_QA_PROFILE_DIR", env)
            self.assertEqual(config, Path(env["XDG_CONFIG_HOME"]) / "morpheus/config.toml")
            self.assertEqual(config.stat().st_mode & 0o777, 0o600)
            self.assertEqual(Path(env["XDG_RUNTIME_DIR"]).stat().st_mode & 0o777, 0o700)
            self.assertIn("scale_percent = 110", config.read_text())

    def test_manual_only_helpers_do_not_trigger_unrelated_native_matrix(self):
        result = ci_scope.classify(["scripts/package_smoke.py", "scripts/linux_package_ui.py",
                                    "scripts/tests/test_linux_package_ui.py", "docs/PACKAGING.md"])
        self.assertFalse(result["native"])

    def test_missing_container_guard_cannot_launch_ui(self):
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}), patch.object(ui.subprocess, "Popen") as launch:
            with self.assertRaises(RuntimeError):
                ui.acceptance(Path("/synthetic/app"), {})
        launch.assert_not_called()

    def test_main_publishes_fixed_phase_not_exception_details(self):
        captured = io.StringIO()
        with patch.object(ui.sys, "argv", ["driver", "/synthetic/app"]), \
                patch.object(ui.signal, "signal"), patch.object(ui.signal, "alarm"), \
                patch.object(ui, "acceptance", side_effect=RuntimeError("PRIVATE_TREE_OR_PATH")), \
                contextlib.redirect_stdout(captured):
            self.assertEqual(ui.main(), 1)
        self.assertNotIn("PRIVATE_TREE_OR_PATH", captured.getvalue())
        self.assertEqual(smoke.keyboard_report(captured.getvalue().encode())["phase"], "setup")


class KeyboardFlowMockTests(unittest.TestCase):
    def fake_menu(self, escape_returns=True):
        class Menu:
            focus_name = ""
            modal = False
            menu = False
            keys = []

            def focus(self, name, **_):
                self.focus_name = name

            def focused_name(self):
                return self.focus_name

            def expect_focus(self, name, **_):
                if not self.focus_name.startswith(name):
                    raise RuntimeError("focus mismatch")

            def find(self, name):
                present = self.modal if name == "Manage saved Matrix accounts" else self.menu
                return True if present else None

            def wait(self, name):
                if not self.find(name):
                    raise RuntimeError("missing region")

            def wait_for(self, predicate, **_):
                if not predicate():
                    raise RuntimeError("state did not arrive")

            def key(self, key):
                self.keys.append(key)
                if key == "Escape":
                    self.menu = self.modal = False
                    self.focus_name = "Account menu for fixture" if escape_returns else "unrelated"
                elif self.modal:
                    self.focus_name = "Add account" if key == "Tab" else "Done"
                elif key in {"Return", "space"}:
                    if self.focus_name == "Manage accounts":
                        self.menu, self.modal, self.focus_name = False, True, "Done"
                    else:
                        self.menu, self.focus_name = True, "Switch to fixture"
                elif key == "Tab":
                    self.focus_name = "Manage accounts"
        return Menu()

    def test_menu_is_opened_dismissed_and_reached_by_actual_keys(self):
        driver = self.fake_menu()
        ui.menu_keyboard(driver)
        self.assertEqual(driver.keys, ["Return", "Escape", "space", "Tab", "Return", "Tab", "shift+Tab", "Escape"])

    def test_menu_diagnostics_preserve_the_exact_keyboard_sequence(self):
        driver = self.fake_menu()
        diagnostics = ui.Diagnostics({})
        ui.menu_keyboard(driver, diagnostics)
        self.assertEqual(driver.keys, ["Return", "Escape", "space", "Tab", "Return", "Tab", "shift+Tab", "Escape"])
        self.assertEqual(diagnostics.value["step"], "menu_final_focus")
        smoke.validate_keyboard_diagnostic(diagnostics.value, False)

    def test_menu_diagnostic_pins_initial_focus_failure_without_private_label(self):
        driver = self.fake_menu()
        driver.focus = Mock(side_effect=RuntimeError("PRIVATE_ACCOUNT_LABEL"))
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(RuntimeError) as error:
            ui.menu_keyboard(driver, diagnostics)
        diagnostics.failed(error.exception)
        self.assertEqual(driver.keys, [])
        self.assertEqual(diagnostics.value["step"], "menu_trigger_focus")
        self.assertNotIn("PRIVATE", json.dumps(diagnostics.value))

    def test_menu_escape_without_focus_return_fails(self):
        with self.assertRaisesRegex(RuntimeError, "focus mismatch"):
            ui.menu_keyboard(self.fake_menu(False))

    def test_keyboard_command_rejects_unowned_focus_before_key_injection(self):
        driver = object.__new__(ui.AccessibleUI)
        driver.window = lambda: "0x123"
        with patch.object(ui, "command", side_effect=[b"", b"999"]) as command:
            with self.assertRaises(RuntimeError):
                driver.key("Return")
        self.assertEqual(command.call_count, 2)

    def test_keyboard_uses_focused_xtest_path_not_send_event(self):
        driver = object.__new__(ui.AccessibleUI)
        driver.window = lambda: "0x123"
        with patch.object(ui, "command", side_effect=[b"", b"291", b""]) as command:
            driver.key("Tab")
        self.assertEqual(command.call_args.args[0], ["xdotool", "key", "--clearmodifiers", "Tab"])

    def test_invalid_config_is_restored_even_if_error_is_not_accessible(self):
        with tempfile.TemporaryDirectory() as folder:
            config = Path(folder) / "config.toml"
            valid = b"[appearance]\nscale_percent = 120\n"
            config.write_bytes(valid)
            driver = Mock()
            driver.wait_for.side_effect = lambda predicate, **_: predicate()
            driver.wait.side_effect = [True, True, True, RuntimeError("error not accessible")]
            with self.assertRaises(RuntimeError):
                ui.appearance_keyboard(driver, config)
            self.assertEqual(config.read_bytes(), valid)

    def navigation_driver(self, names):
        class Navigation:
            index = 0
            keys = []
            def focused_name(self):
                return names[min(self.index, len(names) - 1)]
            def key(self, key):
                self.keys.append(key)
                self.index += 1
            def wait_for(self, predicate):
                if not predicate():
                    raise ui.DiagnosticFailure("wait_timeout")
            def wait(self, label):
                if self.focused_name() != label:
                    raise RuntimeError("offscreen selector queried before focus")
        return Navigation()

    def test_tab_reveals_target_before_querying_offscreen_selector(self):
        driver = self.navigation_driver(["Open settings", "Apply Nocturne appearance preset", "Increase font scale"])
        ui.AccessibleUI.tab_to(driver, "Increase font scale", limit=2)
        self.assertEqual(driver.keys, ["Tab", "Tab"])

    def test_tab_budget_never_activates_an_unrelated_control(self):
        driver = self.navigation_driver(["Open settings", "first", "second", "third"])
        with self.assertRaises(ui.DiagnosticFailure):
            ui.AccessibleUI.tab_to(driver, "Increase font scale", limit=2)
        self.assertEqual(driver.keys, ["Tab", "Tab"])

    def test_tab_target_still_requires_unambiguous_complete_selector(self):
        driver = self.navigation_driver(["Increase font scale"])
        driver.wait = Mock(side_effect=ui.DiagnosticFailure("selector_ambiguous"))
        with self.assertRaises(ui.DiagnosticFailure):
            ui.AccessibleUI.tab_to(driver, "Increase font scale")
        self.assertEqual(driver.keys, [])

    def test_tab_loss_of_window_ownership_stops_before_more_keys(self):
        driver = self.navigation_driver(["Open settings", "Increase font scale"])
        driver.key = Mock(side_effect=RuntimeError("owned window lost"))
        with self.assertRaises(RuntimeError):
            ui.AccessibleUI.tab_to(driver, "Increase font scale")
        driver.key.assert_called_once_with("Tab")

    def test_tab_never_sends_keys_without_an_observed_focused_control(self):
        driver = self.navigation_driver([None])
        with self.assertRaises(ui.DiagnosticFailure):
            ui.AccessibleUI.tab_to(driver, "Increase font scale")
        self.assertEqual(driver.keys, [])

    def test_tab_rejects_unbounded_or_noninteger_budget(self):
        for limit in (0, -1, 161, True, 2.0):
            driver = self.navigation_driver(["Increase font scale"])
            with self.assertRaises(ui.DiagnosticFailure):
                ui.AccessibleUI.tab_to(driver, "Increase font scale", limit=limit)
            self.assertEqual(driver.keys, [])

    def test_appearance_reports_fixed_checkpoints_without_direct_offscreen_focus(self):
        with tempfile.TemporaryDirectory() as folder:
            config = Path(folder) / "config.toml"
            config.write_text("[appearance]\nscale_percent = 110\n")
            driver = Mock()
            def key(value):
                if value == "space":
                    config.write_text("[appearance]\nscale_percent = 120\n")
            driver.key.side_effect = key
            driver.wait_for.side_effect = lambda predicate, **_: predicate()
            driver.find.return_value = None
            diagnostics = Mock()
            ui.appearance_keyboard(driver, config, diagnostics)
            driver.focus.assert_called_once_with("Open settings")
            driver.tab_to.assert_called_once_with("Increase font scale")
            driver.expect_focus.assert_called_once_with("Increase font scale")
            self.assertEqual(driver.key.call_args_list, [call("Return"), call("space")])
            self.assertEqual([c.args[0] for c in diagnostics.step.call_args_list], [
                "appearance", "appearance_reach_scale", "appearance_initial_scale", "appearance_saved_scale",
                "appearance_invalid_notice", "appearance_invalid_retained", "appearance_restored",
            ])
            self.assertEqual(config.read_text(), "[appearance]\nscale_percent = 120\n")



def pyatspi_getitem(self, i):
    # Verbatim Accessible_getitem from pyatspi 2.46.1 pyatspi/Accessibility.py.
    len=self.get_child_count()
    if i < 0:
            i = len + i
    if i < 0 or i >= len:
            raise IndexError
    return self.get_child_at_index(i)


class FakeDesktop:
    """The desktop root, exposing pyatspi 2.46.1's native calls and protocol."""
    def __init__(self, applications):
        self.applications = list(applications)

    def get_child_count(self):
        return len(self.applications)

    def get_child_at_index(self, index):
        return self.applications[index]

    __len__ = get_child_count
    __getitem__ = pyatspi_getitem


def fake_driver(applications=()):
    driver = object.__new__(ui.AccessibleUI)
    driver.diagnostics = ui.Diagnostics({})
    driver.api = Mock()
    driver.api.Registry.getDesktop.return_value = FakeDesktop(applications)
    driver.transient_error = type("Transient", (Exception,), {})
    driver.process = Mock()
    driver.owned = lambda _: True
    return driver


class BoundedDiagnosticTests(unittest.TestCase):
    def driver(self, applications=()):
        return fake_driver(applications)

    def test_duplicate_exact_labels_remain_failure_with_bounded_match_count(self):
        driver = self.driver()
        nodes = [Mock(), Mock(), Mock()]
        for node in nodes:
            node.name = "Homeserver URL"
        driver.nodes = lambda: iter(nodes)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            driver.find("Homeserver URL")
        driver.diagnostics.failed(error.exception)
        self.assertEqual(driver.diagnostics.value["failure"], "selector_ambiguous")
        self.assertEqual(driver.diagnostics.value["selector_matches"], 2)

    def test_pid_api_error_is_distinguished_from_missing_owned_tree(self):
        class MissingPid:
            pass
        driver = self.driver([MissingPid()])
        with self.assertRaises(AttributeError) as error:
            list(driver.nodes())
        driver.diagnostics.failed(error.exception)
        value = driver.diagnostics.value
        self.assertTrue(value["desktop_app_seen"])
        self.assertFalse(value["owned_app_seen"])
        self.assertEqual(value["operation"], "application_pid")
        self.assertEqual(value["failure"], "attribute_error")

    def test_unowned_application_is_never_traversed(self):
        app = Mock()
        app.get_process_id.return_value = 123
        driver = self.driver([app])
        driver.owned = lambda _: False
        self.assertEqual(list(driver.nodes()), [])
        self.assertTrue(driver.diagnostics.value["desktop_app_seen"])
        self.assertFalse(driver.diagnostics.value["owned_app_seen"])
        self.assertFalse(driver.diagnostics.value["owned_node_seen"])
        app.clearCache.assert_not_called()

    def test_owned_tree_and_name_api_failure_are_separate_facts(self):
        class Node:
            @property
            def name(self):
                raise TypeError("PRIVATE_VALUE")
        driver = self.driver()
        driver.nodes = lambda: iter([Node()])
        with self.assertRaises(TypeError) as error:
            driver.find("Password")
        driver.diagnostics.failed(error.exception)
        self.assertEqual(driver.diagnostics.value["operation"], "node_name")
        self.assertEqual(driver.diagnostics.value["failure"], "type_error")
        self.assertNotIn("PRIVATE_VALUE", json.dumps(driver.diagnostics.value))

    def test_sign_in_fields_and_wrong_password_role_cannot_pass(self):
        fake = Mock()
        fake.api.ROLE_PASSWORD_TEXT, fake.api.ROLE_ENTRY, fake.api.ROLE_TEXT = 1, 2, 3
        fake.wait.return_value.getRole.return_value = 2
        record = {"sign_in_semantics": False}
        trace = ui.Diagnostics(record)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            ui.inspect_sign_in(fake, record, trace)
        trace.failed(error.exception)
        self.assertFalse(record["sign_in_semantics"])
        self.assertEqual(trace.value["step"], "password_role")
        self.assertEqual(trace.value["operation"], "node_role")
        self.assertEqual(trace.value["password_role"], "entry")
        self.assertTrue(all(trace.value[key] for key in ("homeserver_seen", "username_seen", "password_seen")))
        self.assertEqual([call.args[0] for call in fake.wait.call_args_list],
                         ["Homeserver URL", "Matrix ID or username", "Password"])

    def test_missing_second_label_retains_first_observation(self):
        fake = Mock()
        fake.wait.side_effect = [True, ui.DiagnosticFailure("wait_timeout")]
        record = {"sign_in_semantics": False}
        trace = ui.Diagnostics(record)
        with self.assertRaises(ui.DiagnosticFailure) as error:
            ui.inspect_sign_in(fake, record, trace)
        trace.failed(error.exception)
        self.assertEqual(trace.value["step"], "username")
        self.assertTrue(trace.value["homeserver_seen"])
        self.assertFalse(trace.value["username_seen"])
        self.assertFalse(record["sign_in_semantics"])

    def test_early_process_exit_is_not_a_label_timeout(self):
        driver = self.driver()
        with patch.object(smoke, "process_status", return_value=7):
            with self.assertRaises(ui.DiagnosticFailure) as error:
                driver.wait_for(lambda: False)
        driver.diagnostics.failed(error.exception)
        self.assertEqual(driver.diagnostics.value["failure"], "app_exited")
        self.assertEqual(driver.diagnostics.value["operation"], "process_status")

    def test_first_failure_survives_cleanup_error(self):
        trace = ui.Diagnostics({})
        trace.step("password_role")
        trace.failed(ui.DiagnosticFailure("password_role"))
        trace.value["cleanup"] = "failed"
        trace.failed(OSError("PRIVATE_PATH"))
        self.assertEqual(trace.value["failure"], "password_role")
        self.assertEqual(trace.value["step"], "password_role")
        self.assertEqual(trace.value["cleanup"], "failed")

    def test_real_acceptance_finally_preserves_failure_and_reports_cleanup_failure(self):
        record = {"sign_in_semantics": False}
        trace = ui.Diagnostics(record)
        original_path = Path

        def fail_signin(_ui, _report, diagnostics):
            diagnostics.step("password_role")
            raise ui.DiagnosticFailure("password_role")

        with patch.dict(os.environ, {"GITHUB_ACTIONS": "true"}), \
                patch.object(ui.sys, "platform", "linux"), patch.object(ui.os, "geteuid", return_value=1000), \
                patch.object(ui, "Path", side_effect=lambda value: Mock(exists=lambda: True)
                             if value == "/.dockerenv" else original_path(value)), \
                patch.object(smoke, "require_nonreaping_wait"), patch.object(ui, "command"), \
                patch.object(ui.subprocess, "Popen", return_value=Mock()), \
                patch.object(ui, "AccessibleUI"), patch.object(ui, "inspect_sign_in", side_effect=fail_signin), \
                patch.object(ui.signal, "alarm"), \
                patch.object(smoke, "stop_process", side_effect=OSError("PRIVATE_CLEANUP_PATH")):
            with self.assertRaises(OSError):
                ui.acceptance(Path("/synthetic/app"), record, trace)
        self.assertEqual(trace.value["failure"], "password_role")
        self.assertEqual(trace.value["step"], "password_role")
        self.assertEqual(trace.value["cleanup"], "failed")
        self.assertNotIn("PRIVATE", json.dumps(trace.value))

    def test_parser_rejects_arbitrary_diagnostic_values_and_incomplete_success(self):
        bad = [diagnostic(step="PRIVATE_PATH"), diagnostic(step=[]), diagnostic(operation="PRIVATE_LABEL"),
               diagnostic(failure="PRIVATE_ERROR"), diagnostic(password_role="PRIVATE_ROLE"),
               diagnostic(selector_matches=3), diagnostic(selector_matches=True),
               diagnostic(child_count_unavailable=3), diagnostic(child_count_unavailable=-1),
               diagnostic(child_unavailable=3), diagnostic(child_unavailable=False),
               diagnostic(incomplete_passes=True), diagnostic(incomplete_passes="1"),
               diagnostic(owned_app_seen=1), diagnostic(cleanup="failed"),
               diagnostic(homeserver_seen=False), diagnostic(private_text="SECRET")]
        for value in bad:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                smoke.keyboard_report(json.dumps(report(diagnostic=value)).encode())
        missing = diagnostic()
        del missing["incomplete_passes"]
        with self.assertRaises(RuntimeError):
            smoke.keyboard_report(json.dumps(report(diagnostic=missing)).encode())
        # Retried IPC failures are facts, not blockers: a complete final pass passes.
        for value in (diagnostic(child_count_unavailable=2, incomplete_passes=2),
                      diagnostic(child_count_unavailable=1)):
            self.assertEqual(smoke.keyboard_report(json.dumps(report(diagnostic=value)).encode())["status"], "passed")

    def test_exception_text_never_reaches_final_record(self):
        for error in (AttributeError("PRIVATE_PATH"), TypeError("PRIVATE_SECRET"),
                      NotImplementedError("PRIVATE_ROLE"), OSError("PRIVATE_TOKEN")):
            with self.subTest(error=type(error).__name__):
                trace = ui.Diagnostics({})
                trace.failed(error)
                raw = json.dumps(report(status="failed", phase="sign_in_semantics", diagnostic=trace.value))
                self.assertNotIn("PRIVATE_", raw)
                self.assertEqual(smoke.keyboard_report(raw.encode())["status"], "failed")


class SequenceDiagnosticTests(unittest.TestCase):
    class Sequence:
        """pyatspi 2.46.1's Accessible protocol over fake native calls.

        __len__ is get_child_count and __getitem__ repeats get_child_count
        before get_child_at_index. `count` fixes every count answer; `counts`
        lists successive answers, the last one repeating; `missing` indexes
        return None like libatspi's NULL child.
        """
        def __init__(self, values, error_at=None, error=None, count=None, counts=None, missing=()):
            self.values, self.error_at, self.error = values, error_at, error
            self.count, self.counts, self.missing = count, list(counts or ()), set(missing)
            self.calls = []

        def get_child_count(self):
            self.calls.append("count")
            if self.counts:
                return self.counts.pop(0) if len(self.counts) > 1 else self.counts[0]
            return len(self.values) if self.count is None else self.count

        def get_child_at_index(self, index):
            self.calls.append(index)
            if index == self.error_at:
                raise self.error
            if index in self.missing:
                return None
            return self.values[index]

        __len__ = get_child_count
        __getitem__ = pyatspi_getitem

    def test_list_preserves_exact_length_and_index_operations(self):
        original, observed = self.Sequence([1, 2, 3]), self.Sequence([1, 2, 3])
        diagnostics = ui.Diagnostics({})
        self.assertEqual(list(original), list(ui.ObservedSequence(observed, diagnostics, "node")))
        self.assertEqual(original.calls, observed.calls)
        self.assertEqual(observed.calls, ["count", "count", 0, "count", 1, "count", 2, "count"])
        # The terminating IndexError comes from the last repeated count.
        self.assertEqual(diagnostics.value["operation"], "node_child_count")

    def test_desktop_for_loop_preserves_index_only_protocol(self):
        original, observed = self.Sequence([1, 2]), self.Sequence([1, 2])
        self.assertEqual([x for x in original],
                         [x for x in ui.ObservedSequence(observed, ui.Diagnostics({}), "desktop")])
        # No length hint: pyatspi counts once per index, then stops at IndexError.
        self.assertEqual(original.calls, observed.calls)
        self.assertEqual(observed.calls, ["count", 0, "count", 1, "count"])

    def test_negative_child_count_is_libatspi_ipc_failure_not_python_value_error(self):
        # at-spi2-core returns -1 without a GError when the ChildCount call got
        # no reply; pyatspi 2.46.1 binds __len__ to get_child_count, so len()
        # would report ValueError. The sentinel is recognized before len() and
        # surfaces as the retryable LookupError the traversal already handles.
        sequence = self.Sequence([], count=-1)
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(ui.ChildCountUnavailable) as error:
            list(ui.ObservedSequence(sequence, diagnostics, "node"))
        self.assertIsInstance(error.exception, LookupError)
        self.assertEqual(sequence.calls, ["count"])
        self.assertEqual(diagnostics.value["operation"], "node_child_count")
        self.assertEqual(diagnostics.value["child_count_unavailable"], 1)
        diagnostics.failed(error.exception)
        self.assertEqual(diagnostics.value["failure"], "lookup_error")
        # Repeats stay bounded; a non-negative count is forwarded unchanged.
        with self.assertRaises(ui.ChildCountUnavailable):
            list(ui.ObservedSequence(sequence, diagnostics, "node"))
        with self.assertRaises(ui.ChildCountUnavailable):
            list(ui.ObservedSequence(sequence, diagnostics, "node"))
        self.assertEqual(diagnostics.value["child_count_unavailable"], 2)
        self.assertEqual(list(ui.ObservedSequence(self.Sequence([5]), diagnostics, "node")), [5])

    class Node:
        """A fake accessible exposing pyatspi 2.46.1's native calls and protocol."""
        def __init__(self, name, children=(), count=None, counts=None, missing=(), focused=False):
            self.name, self.children, self.count = name, list(children), count
            self.counts, self.missing, self.focused = list(counts or ()), set(missing), focused
            self.cleared = 0

        def clearCache(self):
            self.cleared += 1

        def get_process_id(self):
            return 4242

        def getState(self):
            return Mock(contains=Mock(return_value=self.focused))

        def get_child_count(self):
            if self.counts:
                return self.counts.pop(0) if len(self.counts) > 1 else self.counts[0]
            return len(self.children) if self.count is None else self.count

        def get_child_at_index(self, index):
            return None if index in self.missing else self.children[index]

        __len__ = get_child_count
        __getitem__ = pyatspi_getitem

    def test_indexed_count_failure_cannot_hide_a_second_match_or_focus(self):
        # Hosted shape: the length hint succeeds, then -1 arrives while reading
        # index 1. pyatspi alone would yield only the first child, so a duplicate
        # label or a second focused node behind it would stay unseen and the
        # pass would wrongly look unique.
        def tree():
            first = self.Node("Homeserver URL", focused=True)
            second = self.Node("Homeserver URL", focused=True)
            return self.Node("PRIVATE_APP", [first, second], counts=[2, 2, -1, 2])

        # list() reads the length hint first, exactly as the driver does; pyatspi
        # then returns only the first child.
        self.assertEqual([child.name for child in list(tree())], ["Homeserver URL"])

        driver = fake_driver([tree()])
        with self.assertRaises(ui.IncompleteTraversal):
            driver.find("Homeserver URL")
        value = driver.diagnostics.value
        self.assertEqual(value["child_count_unavailable"], 1)
        self.assertEqual(value["incomplete_passes"], 1)
        self.assertTrue(driver.pass_incomplete)
        # The retry sees the complete tree and reports the real ambiguity.
        with patch.object(smoke, "process_status", return_value=None):
            with self.assertRaises(ui.DiagnosticFailure) as error:
                driver.wait("Homeserver URL")
        self.assertEqual(error.exception.code, "selector_ambiguous")
        self.assertEqual(value["selector_matches"], 2)

        driver = fake_driver([tree()])
        driver.api.STATE_FOCUSED = object()
        with self.assertRaises(ui.IncompleteTraversal):
            driver.focused_name()
        with patch.object(smoke, "process_status", return_value=None):
            with self.assertRaises(RuntimeError) as error:
                driver.expect_focus("Homeserver URL")
        self.assertNotIsInstance(error.exception, LookupError)
        self.assertNotIn("PRIVATE", json.dumps(driver.diagnostics.value))

    def test_missing_child_during_traversal_marks_the_pass_incomplete(self):
        hidden = self.Node("Homeserver URL")
        app = self.Node("PRIVATE_APP", [self.Node("PRIVATE_LEAF"), hidden], missing={1})
        driver = fake_driver([app])
        self.assertEqual(list(driver.nodes()), [app])
        self.assertTrue(driver.pass_incomplete)
        self.assertEqual(driver.diagnostics.value["child_unavailable"], 1)
        self.assertEqual(hidden.cleared, 0)

    def test_only_the_exact_minus_one_sentinel_is_retryable(self):
        # at-spi2-core documents -1 alone; anything else is not a known signal
        # and keeps failing closed through len().
        for count in (-2, -2147483648):
            sequence = self.Sequence([], count=count)
            diagnostics = ui.Diagnostics({})
            with self.assertRaises(ValueError) as error:
                list(ui.ObservedSequence(sequence, diagnostics, "node"))
            diagnostics.failed(error.exception)
            self.assertEqual(sequence.calls, ["count"])
            self.assertEqual(diagnostics.value["operation"], "node_child_count")
            self.assertEqual(diagnostics.value["failure"], "value_error")
            self.assertEqual(diagnostics.value["child_count_unavailable"], 0)
        with self.assertRaises(TypeError):
            list(ui.ObservedSequence(self.Sequence([], count="PRIVATE_LENGTH"), ui.Diagnostics({}), "node"))
        # The same closed failure applies to the repeated count inside an index
        # read, where pyatspi itself would have ended the list quietly.
        truncating = self.Sequence([1, 2], counts=[2, -2])
        self.assertEqual(list(truncating), [])
        checked = self.Sequence([1, 2], counts=[2, -2])
        with self.assertRaises(ValueError):
            list(ui.ObservedSequence(checked, ui.Diagnostics({}), "node"))
        self.assertEqual(checked.calls, ["count", "count"])

    def test_minus_one_at_an_indexed_count_is_not_end_of_sequence(self):
        # pyatspi's own protocol: a good length hint, then -1 while reading
        # index 1 raises IndexError and silently drops the second child.
        native = self.Sequence(["first", "second"], counts=[2, 2, -1])
        self.assertEqual(list(native), ["first"])
        observed = self.Sequence(["first", "second"], counts=[2, 2, -1])
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(ui.ChildCountUnavailable):
            list(ui.ObservedSequence(observed, diagnostics, "node"))
        self.assertEqual(observed.calls, ["count", "count", 0, "count"])
        self.assertEqual(diagnostics.value["child_count_unavailable"], 1)
        self.assertEqual(diagnostics.value["operation"], "node_child_count")

    def test_missing_child_object_is_not_an_empty_position(self):
        native = self.Sequence(["first", "second"], missing={1})
        self.assertEqual(list(native), ["first", None])
        observed = self.Sequence(["first", "second"], missing={1})
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(ui.ChildUnavailable) as error:
            list(ui.ObservedSequence(observed, diagnostics, "node"))
        self.assertIsInstance(error.exception, LookupError)
        self.assertEqual(observed.calls, ["count", "count", 0, "count", 1])
        self.assertEqual(diagnostics.value["child_unavailable"], 1)
        self.assertEqual(diagnostics.value["operation"], "node_child_index")

    def test_ownership_lost_mid_tree_marks_the_pass_incomplete(self):
        hidden = self.Node("PRIVATE_HIDDEN")
        child = self.Node("PRIVATE_CHILD", [hidden])
        app = self.Node("PRIVATE_APP", [child])
        driver = fake_driver([app])
        answers = iter([True, True, False])
        driver.owned = lambda _: next(answers)
        self.assertEqual(list(driver.nodes()), [app])
        self.assertTrue(driver.pass_incomplete)
        self.assertEqual(child.cleared, 1)
        self.assertEqual(hidden.cleared, 0)
        self.assertTrue(driver.diagnostics.value["owned_node_seen"])

    def focused(self, name, state_error=None):
        node = Mock()
        node.name = name
        if state_error is None:
            node.getState.return_value.contains.return_value = True
        else:
            node.getState.side_effect = state_error
        return node

    def test_focus_from_an_incomplete_pass_is_refused_then_retried(self):
        driver = fake_driver([])
        driver.api.STATE_FOCUSED = object()
        focused = self.focused("Account menu")
        passes = []

        def nodes():
            passes.append(len(passes))
            driver.pass_incomplete = len(passes) == 1
            yield focused

        driver.nodes = nodes
        with self.assertRaises(ui.IncompleteTraversal) as error:
            driver.focused_name()
        self.assertIsInstance(error.exception, LookupError)
        self.assertEqual(driver.diagnostics.value["incomplete_passes"], 1)
        self.assertEqual(driver.diagnostics.value["focus_kind"], "unavailable")
        self.assertEqual(driver.diagnostics.value["focused_nodes"], 0)
        with patch.object(smoke, "process_status", return_value=None):
            self.assertTrue(driver.expect_focus("Account menu"))
        self.assertEqual(passes, [0, 1])
        self.assertEqual(driver.diagnostics.value["incomplete_passes"], 1)

    def test_state_read_failure_marks_focus_pass_incomplete(self):
        driver = fake_driver([])
        driver.api.STATE_FOCUSED = object()
        unreadable = self.focused("PRIVATE_UNREADABLE", state_error=driver.transient_error())
        driver.nodes = lambda: iter([unreadable, self.focused("Account menu")])
        with self.assertRaises(ui.IncompleteTraversal):
            driver.focused_name()
        self.assertNotIn("PRIVATE", json.dumps(driver.diagnostics.value))

    def test_two_focused_nodes_fail_even_when_the_pass_is_incomplete(self):
        driver = fake_driver([])
        driver.api.STATE_FOCUSED = object()

        def nodes():
            driver.pass_incomplete = True
            yield self.focused("Account menu")
            yield self.focused("Settings")

        driver.nodes = nodes
        with self.assertRaises(RuntimeError) as error:
            driver.focused_name()
        self.assertNotIsInstance(error.exception, LookupError)
        self.assertEqual(driver.diagnostics.value["incomplete_passes"], 0)
        self.assertEqual(driver.diagnostics.value["focus_kind"], "ambiguous")
        self.assertEqual(driver.diagnostics.value["focused_nodes"], 2)

    def test_focus_categories_never_return_an_account_label_or_unknown_text(self):
        for name, expected in [(None, "none"), ("Account menu for PRIVATE_ACCOUNT", "account_trigger"),
                               ("Switch to PRIVATE_ACCOUNT", "account_choice"),
                               ("Manage accounts", "manage_choice"), ("Add another Matrix account", "add_choice"),
                               ("Done", "done"), ("Add account", "add_account"),
                               ("PRIVATE_MESSAGE_OR_VALUE", "other")]:
            self.assertEqual(ui.focus_kind(name), expected)
            self.assertIn(expected, smoke.KEYBOARD_FOCUS_KINDS)
            self.assertNotIn("PRIVATE", expected)

    def test_complete_focus_observation_retains_only_count_and_fixed_category(self):
        driver = fake_driver([])
        driver.api.STATE_FOCUSED = object()
        driver.nodes = lambda: iter([self.focused("PRIVATE_FOCUSED_LABEL")])
        self.assertEqual(driver.focused_name(), "PRIVATE_FOCUSED_LABEL")
        self.assertEqual(driver.diagnostics.value["focus_kind"], "other")
        self.assertEqual(driver.diagnostics.value["focused_nodes"], 1)
        self.assertNotIn("PRIVATE", json.dumps(driver.diagnostics.value))

    def test_report_rejects_raw_focus_text_and_inconsistent_or_ambiguous_success(self):
        for value in [diagnostic(focus_kind="PRIVATE_ACCOUNT"), diagnostic(focused_nodes=True),
                      diagnostic(focus_kind="none", focused_nodes=1),
                      diagnostic(focus_kind="unavailable", focused_nodes=0),
                      diagnostic(focus_kind="ambiguous", focused_nodes=2)]:
            with self.assertRaises(RuntimeError):
                smoke.validate_keyboard_diagnostic(value, True)

    def test_persistent_focus_gap_ends_as_wait_timeout(self):
        driver = fake_driver([])
        driver.api.STATE_FOCUSED = object()

        def stalled():
            driver.pass_incomplete = True
            yield self.focused("Account menu")

        driver.nodes = stalled
        with patch.object(smoke, "process_status", return_value=None), \
                patch.object(ui.time, "monotonic", side_effect=[0.0, 0.0, 0.1, 0.2, 100.0]), \
                patch.object(ui.time, "sleep"):
            with self.assertRaises(ui.DiagnosticFailure) as error:
                driver.expect_focus("Account menu")
        self.assertEqual(error.exception.code, "wait_timeout")
        self.assertEqual(driver.diagnostics.value["incomplete_passes"], 2)

    def test_unavailable_child_count_skips_only_that_subtree_and_marks_pass_incomplete(self):
        hidden = self.Node("PRIVATE_HIDDEN")
        stalled = self.Node("PRIVATE_STALLED", [hidden], count=-1)
        sibling = self.Node("PRIVATE_SIBLING")
        app = self.Node("PRIVATE_APP", [stalled, sibling])
        driver = fake_driver([app])
        seen = list(driver.nodes())
        # Every reachable node is still visited and cache-cleared; only the
        # children behind the failed count are missing, and the pass says so.
        self.assertEqual(seen, [app, sibling, stalled])
        self.assertTrue(all(node.cleared == 1 for node in seen))
        self.assertTrue(driver.pass_incomplete)
        value = driver.diagnostics.value
        self.assertEqual(value["child_count_unavailable"], 1)
        self.assertTrue(value["owned_node_seen"])
        self.assertNotIn("PRIVATE", json.dumps(value))
        # A healthy tree is a complete pass.
        healthy = fake_driver([self.Node("PRIVATE_APP", [self.Node("PRIVATE_LEAF")])])
        self.assertEqual(len(list(healthy.nodes())), 2)
        self.assertFalse(healthy.pass_incomplete)
        self.assertEqual(healthy.diagnostics.value["child_count_unavailable"], 0)

    def test_find_refuses_a_match_from_an_incomplete_pass_and_retries(self):
        driver = fake_driver([])
        match = Mock()
        match.name = "Matrix ID or username"
        passes = []

        def nodes():
            passes.append(len(passes))
            driver.pass_incomplete = len(passes) == 1
            yield match

        driver.nodes = nodes
        # The hosted failure shape: the label is present, but the owned tree
        # was not completely traversed. That pass is not evidence.
        with self.assertRaises(ui.IncompleteTraversal) as error:
            driver.find("Matrix ID or username")
        self.assertIsInstance(error.exception, LookupError)
        value = driver.diagnostics.value
        self.assertEqual(value["selector_matches"], 1)
        self.assertTrue(value["traversed_node_matched"])
        self.assertEqual(value["incomplete_passes"], 1)
        # wait_for treats it like any transient lookup failure and runs the
        # next pass, which is complete and returns the same match.
        with patch.object(smoke, "process_status", return_value=None):
            self.assertIs(driver.wait("Matrix ID or username"), match)
        self.assertEqual(passes, [0, 1])
        self.assertEqual(value["incomplete_passes"], 1)

    def test_incomplete_pass_never_hides_ambiguity_or_becomes_a_pass(self):
        driver = fake_driver([])
        first, second = Mock(), Mock()
        first.name = second.name = "Homeserver URL"

        def nodes():
            driver.pass_incomplete = True
            yield first
            yield second

        driver.nodes = nodes
        with self.assertRaises(ui.DiagnosticFailure) as error:
            driver.find("Homeserver URL")
        self.assertEqual(error.exception.code, "selector_ambiguous")
        # A pass that keeps failing to complete ends as the existing timeout,
        # with the counters bounded, never as a silent success.
        single = Mock()
        single.name = "Homeserver URL"

        def stalled():
            driver.pass_incomplete = True
            yield single

        driver.nodes = stalled
        with patch.object(smoke, "process_status", return_value=None), \
                patch.object(ui.time, "monotonic", side_effect=[0.0, 0.0, 0.1, 0.2, 100.0]), \
                patch.object(ui.time, "sleep"):
            with self.assertRaises(ui.DiagnosticFailure) as error:
                driver.wait("Homeserver URL")
        self.assertEqual(error.exception.code, "wait_timeout")
        self.assertEqual(driver.diagnostics.value["incomplete_passes"], 2)

    def test_child_index_runtime_failure_is_not_an_empty_subtree(self):
        sequence = self.Sequence([1, 2], error_at=1, error=RuntimeError("PRIVATE_NODE_DATA"))
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(RuntimeError) as error:
            list(ui.ObservedSequence(sequence, diagnostics, "node"))
        diagnostics.failed(error.exception)
        self.assertEqual(sequence.calls, ["count", "count", 0, "count", 1])
        self.assertEqual(diagnostics.value["operation"], "node_child_index")
        self.assertEqual(diagnostics.value["failure"], "runtime_error")
        self.assertNotIn("PRIVATE", json.dumps(diagnostics.value))

    def test_sequence_overflow_is_closed_and_failure_remains_sticky(self):
        diagnostics = ui.Diagnostics({})
        diagnostics.failed(OverflowError("PRIVATE_COUNT"))
        diagnostics.failed(RuntimeError("PRIVATE_OTHER"))
        self.assertEqual(diagnostics.value["failure"], "overflow_error")
        self.assertNotIn("PRIVATE", json.dumps(diagnostics.value))

    def test_exact_match_boolean_does_not_publish_the_label(self):
        driver = object.__new__(ui.AccessibleUI)
        driver.diagnostics = ui.Diagnostics({})
        driver.transient_error = type("Transient", (Exception,), {})
        node = Mock()
        node.name = "PRIVATE_MATCHED_NAME"
        driver.nodes = lambda: iter([node])
        self.assertIs(driver.find("PRIVATE_MATCHED_NAME"), node)
        self.assertTrue(driver.diagnostics.value["traversed_node_matched"])
        self.assertNotIn("PRIVATE", json.dumps(driver.diagnostics.value))
        node.name = "PRIVATE_OTHER_NAME"
        self.assertIsNone(driver.find("PRIVATE_MATCHED_NAME"))
        self.assertFalse(driver.diagnostics.value["traversed_node_matched"])

    def test_traversal_match_is_boolean_but_not_required_true_for_pass(self):
        for value in (True, False):
            smoke.keyboard_report(json.dumps(report(diagnostic=diagnostic(traversed_node_matched=value))).encode())
        for value in (1, None, "PRIVATE_NAME"):
            with self.assertRaises(RuntimeError):
                smoke.keyboard_report(json.dumps(report(diagnostic=diagnostic(traversed_node_matched=value))).encode())


if __name__ == "__main__":
    unittest.main()
