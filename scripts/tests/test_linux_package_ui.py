import contextlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from scripts import ci_scope, linux_package_ui as ui, package_smoke as smoke


def diagnostic(**overrides):
    return {"step": "complete", "operation": "none", "failure": "none",
            "desktop_app_seen": True, "owned_app_seen": True, "owned_node_seen": True,
            "homeserver_seen": True, "username_seen": True, "password_seen": True,
            "selector_matches": 1, "password_role": "password_text", "cleanup": "passed",
            "traversed_node_matched": False,
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
            driver.wait.side_effect = [True, True, RuntimeError("error not accessible")]
            with self.assertRaises(RuntimeError):
                ui.appearance_keyboard(driver, config)
            self.assertEqual(config.read_bytes(), valid)



class BoundedDiagnosticTests(unittest.TestCase):
    def driver(self, applications=()):
        driver = object.__new__(ui.AccessibleUI)
        driver.diagnostics = ui.Diagnostics({})
        driver.api = Mock()
        driver.api.Registry.getDesktop.return_value = applications
        driver.transient_error = type("Transient", (Exception,), {})
        driver.process = Mock()
        driver.owned = lambda _: True
        return driver

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
               diagnostic(owned_app_seen=1), diagnostic(cleanup="failed"),
               diagnostic(homeserver_seen=False), diagnostic(private_text="SECRET")]
        for value in bad:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                smoke.keyboard_report(json.dumps(report(diagnostic=value)).encode())

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
        def __init__(self, values, error_at=None, error=None, count=None):
            self.values, self.error_at, self.error, self.count = values, error_at, error, count
            self.calls = []

        def __len__(self):
            self.calls.append("count")
            return len(self.values) if self.count is None else self.count

        def __getitem__(self, index):
            self.calls.append(index)
            if index == self.error_at:
                raise self.error
            return self.values[index]

    def test_list_preserves_exact_length_and_index_operations(self):
        original, observed = self.Sequence([1, 2, 3]), self.Sequence([1, 2, 3])
        diagnostics = ui.Diagnostics({})
        self.assertEqual(list(original), list(ui.ObservedSequence(observed, diagnostics, "node")))
        self.assertEqual(original.calls, observed.calls)
        self.assertEqual(diagnostics.value["operation"], "node_child_index")

    def test_desktop_for_loop_preserves_index_only_protocol(self):
        original, observed = self.Sequence([1, 2]), self.Sequence([1, 2])
        self.assertEqual([x for x in original],
                         [x for x in ui.ObservedSequence(observed, ui.Diagnostics({}), "desktop")])
        self.assertEqual(original.calls, observed.calls)
        self.assertNotIn("count", observed.calls)

    def test_negative_child_count_reports_value_error_at_count_without_skipping(self):
        sequence = self.Sequence([], count=-1)
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(ValueError) as error:
            list(ui.ObservedSequence(sequence, diagnostics, "node"))
        diagnostics.failed(error.exception)
        self.assertEqual(sequence.calls, ["count"])
        self.assertEqual(diagnostics.value["operation"], "node_child_count")
        self.assertEqual(diagnostics.value["failure"], "value_error")

    def test_child_index_runtime_failure_is_not_an_empty_subtree(self):
        sequence = self.Sequence([1, 2], error_at=1, error=RuntimeError("PRIVATE_NODE_DATA"))
        diagnostics = ui.Diagnostics({})
        with self.assertRaises(RuntimeError) as error:
            list(ui.ObservedSequence(sequence, diagnostics, "node"))
        diagnostics.failed(error.exception)
        self.assertEqual(sequence.calls, ["count", 0, 1])
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
