import contextlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from scripts import ci_scope, linux_package_ui as ui, package_smoke as smoke


def report(**overrides):
    return {"check": "installed_linux_keyboard_atspi", "status": "passed", "phase": "complete",
            "sign_in_semantics": True, "account_menu_keyboard": True, "appearance_config": True,
            "normal_window_close": True, "app_log_public": False, "screen_reader_acceptance": False,
            **overrides}


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
        with patch.object(smoke.subprocess, "Popen", side_effect=launch), \
                patch.object(smoke, "process_status", return_value=7), \
                patch.object(smoke, "require_nonreaping_wait"), patch.object(smoke, "stop_process"), \
                contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
            with self.assertRaises(RuntimeError):
                smoke.launch_smoke(Path("/synthetic/app"))
        self.assertNotIn("PRIVATE_APPLICATION_DIAGNOSTIC", captured.getvalue())
        self.assertIn('"app_log_public":false', captured.getvalue())

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


if __name__ == "__main__":
    unittest.main()
