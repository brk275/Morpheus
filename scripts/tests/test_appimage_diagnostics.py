"""Fixed public reduction only: no native commands, subprocesses or services."""
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "packaging/linux"))
import appimage_diagnostics as D
import appimage_process as C
import package_appimage as P


class ProgressTests(unittest.TestCase):
    def test_first_failure_position_survives_later_commands_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            progress = D.Progress(root)
            progress.wrapper("capture")
            progress.capture("ordinary", "static_frame")
            progress.command("capture-green")
            progress.failed(C.CommandFailure("deadline"))
            progress.command("capture-rtc-down")
            progress.failed(RuntimeError("private token and body"))
            progress.cleanup(False)
            progress.cleanup(True)
            result = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertEqual(result["failure"]["capture_phase"], "static_frame")
            self.assertEqual(result["failure"]["command_stage"], "capture-green")
            self.assertEqual(result["failure"]["reason"], "command_deadline")
            self.assertEqual(result["inner_cleanup"], "failed")
            self.assertNotIn("private", json.dumps(result))
            self.assertEqual((root / "fixed-progress.json").stat().st_mode & 0o777, 0o600)

    def test_only_exact_true_peer_facts_are_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            control = root / "peer"
            control.mkdir()
            (control / "static_frame.json").write_bytes(b'{"passed":true}\n')
            (control / "peer_ready.json").write_bytes(b'{"passed":true,"secret":"private"}')
            (control / "app_left.json").write_bytes(b'{"passed":false}\n')
            progress = D.Progress(root)
            progress.capture("ordinary", "static_frame", control)
            progress.failed(RuntimeError("private"), control)
            result = D.public_failure(root, scan_passed=True, outer_cleanup="failed", outer_reason="unknown")
            self.assertEqual(result["peer_facts"], ["static_frame"])
            self.assertEqual(result["failure"]["reason"], "unknown")

    def test_missing_scan_withholds_without_reading_private_record(self):
        with mock.patch.object(D.Path, "lstat", side_effect=AssertionError("must not read")):
            self.assertEqual(D.public_failure(Path("/private"), scan_passed=False, outer_cleanup="failed", outer_reason="unknown"),
                             {"diagnostics": "withheld", "passed": False})

    def test_malformed_nested_or_secret_values_are_wholly_withheld(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            p = D.Progress(root)
            good = dict(p.value)
            invalid = [dict(good, extra="private"), dict(good, command_stage="private"),
                       dict(good, capture_phase=["private"]), dict(good, peer_facts=["private"]),
                       dict(good, failure={"reason": "private"}), dict(good, schema=True)]
            for value in invalid:
                (root / "fixed-progress.json").write_text(json.dumps(value))
                result = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="unknown")
                self.assertEqual(result, {"diagnostics": "withheld", "passed": False})

    def test_schema_rejects_symlink_or_world_readable_record(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            D.Progress(root)
            path = root / "fixed-progress.json"
            path.chmod(0o644)
            self.assertEqual(D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="unknown")["diagnostics"], "withheld")
            original = root / "original"
            path.rename(original)
            path.symlink_to(original)
            self.assertEqual(D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="unknown")["diagnostics"], "withheld")

    def test_unknown_command_and_exception_never_leak_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            p = D.Progress(Path(directory))
            p.command("private-user-token")
            p.failed(ValueError("private URL"))
            self.assertEqual(p.value["command_stage"], "unknown")
            self.assertEqual(p.value["failure"]["reason"], "unknown")
            self.assertNotIn("private", json.dumps(p.value))

    def test_fixed_schema_matches_peer_and_command_source(self):
        import appimage_capture as capture
        self.assertEqual(D.PEER_FACTS, set(capture.FACTS))
        import ast
        names = set()
        for name in ("appimage_capture.py", "package_appimage.py", "appimage_notices.py"):
            tree = ast.parse((ROOT / "packaging/linux" / name).read_text())
            for node in ast.walk(tree):
                if isinstance(node, ast.Call) and node.args and isinstance(node.args[0], ast.Constant):
                    call = node.func.attr if isinstance(node.func, ast.Attribute) else getattr(node.func, "id", None)
                    if call in ("run", "command") and isinstance(node.args[0].value, str):
                        names.add(node.args[0].value)
        self.assertTrue(names <= D.COMMANDS, names - D.COMMANDS)


class OuterPublicationTests(unittest.TestCase):
    def invoke(self, root, *, scan_error=None, cleanup_error=None, interrupt=False):
        child = SimpleNamespace(pid=42)
        order = []
        def start(*a, **k):
            progress = D.Progress(root)
            progress.wrapper("capture")
            progress.capture("eager_bindings", "reopened_frame")
            progress.failed(C.CommandFailure("deadline"))
            return child
        def scan(*a):
            order.append("scan")
            if scan_error:
                raise scan_error
        def cleanup(*a):
            order.append("cleanup")
            if cleanup_error:
                raise cleanup_error
        output = io.StringIO()
        with mock.patch.object(P.containment, "namespace_command", return_value=["/unused"]), \
                mock.patch.object(P.subprocess, "Popen", side_effect=start), \
                mock.patch.object(P.containment, "status", side_effect=KeyboardInterrupt() if interrupt else None, return_value=1), \
                mock.patch.object(P.containment, "stop_owned", side_effect=lambda p: order.append("stop")), \
                mock.patch.object(P, "cleanup_owned_fixture", side_effect=cleanup), \
                mock.patch.object(P, "scan_private_logs", side_effect=scan), contextlib.redirect_stderr(output):
            with self.assertRaises(KeyboardInterrupt if interrupt else P.PackageFailure):
                P.contained(root, "/private/scanner", {"cargo": "/private/cargo"}, {})
        self.assertEqual(order, ["stop", "cleanup", "scan"])
        text = output.getvalue()
        self.assertNotIn("private", text)
        self.assertEqual(len(text.splitlines()), 1)
        return json.loads(text.split(" ", 1)[1])

    def test_scanned_failure_publishes_original_phase_and_separate_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            value = self.invoke(Path(directory), cleanup_error=RuntimeError("private cleanup"))
        self.assertEqual(value["diagnostics"], "scanned_fixed_facts")
        self.assertEqual(value["failure"]["capture_phase"], "reopened_frame")
        self.assertEqual(value["failure"]["reason"], "command_deadline")
        self.assertEqual(value["outer_cleanup"], "failed")
        self.assertEqual(value["outer_reason"], "namespace_exit")

    def test_failed_scan_only_emits_withheld(self):
        with tempfile.TemporaryDirectory() as directory:
            value = self.invoke(Path(directory), scan_error=RuntimeError("private scanner"))
        self.assertEqual(value, {"diagnostics": "withheld", "passed": False})

    def test_interrupt_remains_original_while_cleanup_and_scan_complete(self):
        with tempfile.TemporaryDirectory() as directory:
            value = self.invoke(Path(directory), interrupt=True, cleanup_error=RuntimeError("private"))
        self.assertEqual(value["outer_reason"], "interrupted")
        self.assertEqual(value["outer_cleanup"], "failed")


if __name__ == "__main__":
    unittest.main()
