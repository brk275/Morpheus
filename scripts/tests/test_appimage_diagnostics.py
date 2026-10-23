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
            p.failed(RuntimeError("private URL"))
            self.assertEqual(p.value["command_stage"], "unknown")
            self.assertEqual(p.value["failure"]["reason"], "unknown")
            self.assertNotIn("private", json.dumps(p.value))

    def test_builtin_failures_have_closed_categories_without_reflecting_payloads(self):
        cases = ((FileNotFoundError, "filesystem_missing"), (FileExistsError, "filesystem_exists"),
                 (PermissionError, "filesystem_denied"), (NotADirectoryError, "filesystem_not_directory"),
                 (IsADirectoryError, "filesystem_is_directory"), (OSError, "filesystem_io"),
                 (KeyError, "python_key"), (TypeError, "python_type"), (ValueError, "python_value"))
        for kind, expected in cases:
            with self.subTest(kind=kind):
                error = kind("private filename, URL and server body")
                self.assertEqual(D.reason(error), expected)
                self.assertIn(expected, D.REASONS)
        error = UnicodeDecodeError("utf-8", b"private", 0, 1, "private body")
        self.assertEqual(D.reason(error), "python_decode")
        def forbidden(*_):
            raise AssertionError("foreign payload must not be consulted")
        for kind, _ in cases:
            foreign = type(kind.__name__, (kind,), {"__str__": forbidden, "reason": property(forbidden)})
            self.assertEqual(D.reason(foreign("private")), "unknown")
        foreign_meta = type("ForeignType", (type,), {"__hash__": forbidden, "__eq__": forbidden})
        foreign = foreign_meta("FileNotFoundError", (FileNotFoundError,), {"__str__": forbidden})
        self.assertEqual(D.reason(foreign("private")), "unknown")

    def test_original_filesystem_error_survives_runner_pipe_normalization(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            progress = D.Progress(root)
            progress.wrapper("build")
            progress.command("runtime-zstd-stage")
            progress.failed(FileExistsError("private source path"))
            progress.failed(C.CommandFailure("pipe"))
            progress.command("cleanup-env")
            self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})
            record = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertEqual(record["failure"]["reason"], "filesystem_exists")
            self.assertEqual(record["failure"]["command_stage"], "runtime-zstd-stage")
            self.assertTrue(D.valid_record(progress.value))
            self.assertNotIn("private", json.dumps(record))

    def test_only_real_inventory_fixed_subreasons_are_published(self):
        import appimage_notices as N
        reasons = ("input", "tool", "elf", "dependency", "package", "notice", "deployment", "limit",
                   "elf_build_id", "elf_needed", "elf_soname", "elf_search", "search")
        for reason in reasons:
            error = N.InventoryFailure(reason)
            error.args = ("private path and server body",)
            self.assertEqual(D.reason(error), "inventory_" + reason)
            self.assertIn(D.reason(error), D.REASONS)
        for invalid in ("private-token", {"private": "body"}, None):
            error = N.InventoryFailure("input")
            error.reason = invalid
            self.assertEqual(D.reason(error), "inventory")
        def forbidden(_self):
            raise AssertionError("foreign reason must not be read")
        foreign = type("InventoryFailure", (Exception,), {"reason": property(forbidden)})
        self.assertEqual(D.reason(foreign("private message")), "inventory")

    def test_inventory_role_and_subreason_survive_only_scanned_publication(self):
        import appimage_notices as N
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            progress = D.Progress(root)
            progress.wrapper("build")
            progress.command("inventory-elf-dependency")
            error = N.InventoryFailure("elf_build_id")
            error.args = ("private ELF report and path",)
            progress.failed(error)
            progress.command("inventory-elf-deployed")
            progress.failed(N.InventoryFailure("deployment"))
            progress.cleanup(False)
            progress.cleanup(True)
            self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})
            result = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertEqual(result["failure"]["reason"], "inventory_elf_build_id")
            self.assertEqual(result["failure"]["command_stage"], "inventory-elf-dependency")
            self.assertEqual(result["inner_cleanup"], "failed")
            self.assertNotIn("private", json.dumps(result))

    def test_package_download_facts_are_closed_and_require_a_successful_scan(self):
        self.assertTrue(C.PACKAGE_INPUT_STAGES <= D.COMMANDS)
        for reason in C.PACKAGE_INPUT_REASONS:
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                progress = D.Progress(root)
                progress.wrapper("build")
                progress.command("download-linuxdeploy")
                error = C.PackageDownloadFailure(reason)
                error.args = ("private reflected body and signed URL",)
                progress.failed(error)
                progress.command("cleanup-env")
                progress.failed(RuntimeError("private later error"))
                self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                                 {"diagnostics": "withheld", "passed": False})
                record = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
                self.assertEqual(record["failure"]["reason"], "command_" + reason)
                self.assertEqual(record["failure"]["command_stage"], "download-linuxdeploy")
                self.assertNotIn("private", json.dumps(record))
        self.assertEqual(C.PackageDownloadFailure("private").reason, "input")

    def test_fixed_schema_matches_peer_and_command_source(self):
        import appimage_capture as capture
        self.assertEqual(D.PEER_FACTS, set(capture.FACTS))
        import ast
        names = set()
        for name in ("appimage_capture.py", "appimage_capture_worker.py", "package_appimage.py", "appimage_notices.py", "appimage_runtime_build.py"):
            tree = ast.parse((ROOT / "packaging/linux" / name).read_text())
            for node in ast.walk(tree):
                if isinstance(node, ast.Call) and node.args and isinstance(node.args[0], ast.Constant):
                    call = node.func.attr if isinstance(node.func, ast.Attribute) else getattr(node.func, "id", None)
                    if call in ("run", "command", "mark") and isinstance(node.args[0].value, str):
                        names.add(node.args[0].value)
        self.assertTrue(names <= D.COMMANDS, names - D.COMMANDS)

    def package_failure_fixture(self, root, *, stdout=b"", stderr=b""):
        progress = D.Progress(root)
        progress.wrapper("build")
        progress.command("package")
        progress.failed(C.CommandFailure("exit"))
        logs = root / "command-logs"
        logs.mkdir(mode=0o700)
        for name, data in (("stdout", stdout), ("stderr", stderr)):
            path = logs / ("0042-package." + name + ".log")
            path.write_bytes(data)
            path.chmod(0o600)
        return progress, logs

    def test_packager_markers_require_scan_and_never_publish_raw_captured_text(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            secret = b"synthetic-confidential-filename-and-server-body"
            trace = (b"Error running bundle_appimage.sh script: " + secret + b"\n"
                     b"+ cp -r ../appimage_deb/data/usr morpheus-app.AppDir\n"
                     b"+ cp /bounded/cache/AppRun-x86_64 AppRun\n"
                     b"+ cp usr/share/icons/morpheus-app.png .DirIcon\n"
                     b"+ ln -sf usr/share/icons/morpheus-app.png morpheus-app.png\n"
                     b"+ ln -sf usr/share/applications/morpheus-app.desktop morpheus-app.desktop\n"
                     b"+ dd if=/dev/zero bs=1 count=3 seek=8 conv=notrunc of=/bounded/cache/linuxdeploy-x86_64.AppImage\n"
                     b"+ /bounded/cache/linuxdeploy-x86_64.AppImage --appimage-extract-and-run --appdir morpheus-app.AppDir --output appimage\n")
            self.package_failure_fixture(root, stdout=secret, stderr=trace)
            with mock.patch.object(D, "package_failure_markers", side_effect=AssertionError("not before scan")):
                self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                                 {"diagnostics": "withheld", "passed": False})
            facts = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertEqual(facts["failure"]["reason"], "command_exit")
            markers = facts["package_failure_markers"]
            self.assertEqual(markers["status"], "scanned_private_pair")
            self.assertEqual(markers["error_markers"], ["appimage_script"])
            self.assertEqual(markers["script_markers"], sorted(D.PACKAGER_SCRIPT_MARKERS))
            encoded = json.dumps(facts)
            self.assertNotIn(secret.decode(), encoded)
            self.assertNotIn("/bounded/", encoded)
            self.assertNotIn("bundle_appimage.sh", encoded)
            self.assertNotIn("--appdir", encoded)

    def test_packager_marker_reducer_is_closed_and_does_not_claim_a_cause(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.package_failure_fixture(root, stderr=b"unknown arbitrary child output\n" + b"x" * 9000)
            facts = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertEqual(facts["package_failure_markers"],
                             {"status": "scanned_private_pair", "error_markers": [], "script_markers": []})
            for token, literal in D.PACKAGER_ERROR_MARKERS.items():
                path = root / "command-logs/0042-package.stderr.log"
                path.write_bytes(literal + b" synthetic-not-published")
                facts = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
                self.assertIn(token, facts["package_failure_markers"]["error_markers"])
                self.assertTrue(set(facts["package_failure_markers"]["error_markers"]) <= set(D.PACKAGER_ERROR_MARKERS))
                self.assertNotIn("synthetic-not-published", json.dumps(facts))

    def test_packager_missing_logs_are_unavailable_and_unsafe_or_ambiguous_logs_withhold(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, logs = self.package_failure_fixture(root)
            stdout = logs / "0042-package.stdout.log"
            stderr = logs / "0042-package.stderr.log"
            stdout.unlink()
            facts = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertEqual(facts["package_failure_markers"]["status"], "unavailable")
            stdout.symlink_to(stderr)
            self.assertEqual(D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})
            stdout.unlink()
            stdout.write_bytes(b"")
            stdout.chmod(0o644)
            self.assertEqual(D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})
            stdout.chmod(0o600)
            (logs / "0043-package.stdout.log").write_bytes(b"")
            self.assertEqual(D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})

    def test_packager_log_reader_does_not_run_for_other_command_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            progress = D.Progress(root)
            progress.command("native-build")
            progress.failed(C.CommandFailure("exit"))
            with mock.patch.object(D, "package_failure_markers", side_effect=AssertionError("wrong command")):
                facts = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertNotIn("package_failure_markers", facts)
        with mock.patch.object(D.Path, "lstat", side_effect=AssertionError("not before scan")):
            self.assertEqual(D.package_failure_markers(Path("/unused"), scan_passed=False),
                             {"status": "withheld", "error_markers": [], "script_markers": []})


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
