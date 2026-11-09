"""Fixed public reduction only: no native commands, subprocesses or services."""
import contextlib
import io
import json
import os
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
    def test_source_failure_phase_is_exact_owned_closed_latched_and_scan_gated(self):
        import corresponding_source as source
        for phase in source.SOURCE_PHASES:
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                root = Path(directory); progress = D.Progress(root)
                progress.wrapper("source"); progress.command("source-objects")
                failure = source.SourceFailure("identity")
                with self.assertRaises(source.SourceFailure):
                    with source.source_phase(phase):
                        raise failure
                progress.failed(failure)
                first = dict(progress.value["failure"])
                self.assertEqual(first["source_phase"], phase)
                self.assertEqual(first["reason"], "source_identity")
                progress.failed(source.SourceFailure("missing"))
                self.assertEqual(progress.value["failure"], first)
                self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                                 {"diagnostics": "withheld", "passed": False})
                visible = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
                self.assertEqual(visible["failure"], first)
                self.assertFalse(visible["passed"])
                self.assertNotIn("private", json.dumps(visible))

    def test_source_phase_rejects_foreign_properties_unknown_values_and_other_failure_kinds(self):
        import corresponding_source as source
        class ForeignFailure(source.SourceFailure):
            @property
            def source_phase(self):
                raise AssertionError("foreign property must not be read")
        self.assertIsNone(D.source_phase(ForeignFailure.__new__(ForeignFailure)))
        class ForeignString(str):
            pass
        failure = source.SourceFailure("identity")
        for invalid in (None, [], "private path", ForeignString("native_receipt")):
            failure.source_phase = invalid
            self.assertIsNone(D.source_phase(failure))
        with tempfile.TemporaryDirectory() as directory:
            progress = D.Progress(Path(directory)); progress.failed(failure)
            self.assertNotIn("source_phase", progress.value["failure"])
            for phase, reason in (("private path", "source_identity"), ([], "source_identity"),
                                  ("native_receipt", "command_exit")):
                candidate = json.loads(json.dumps(progress.value))
                candidate["failure"].update(source_phase=phase, reason=reason)
                self.assertFalse(D.valid_record(candidate))

    def test_capture_wait_reasons_require_exact_loaded_owner_and_closed_values(self):
        import appimage_capture as capture
        expected = {"capture": "capture", "process_exit": "capture_process_exit",
                    "log_failure": "capture_log_failure", "log_limit": "capture_log_limit",
                    "wait_timeout": "capture_wait_timeout"}
        self.assertEqual(set(expected), capture.CAPTURE_FAILURE_REASONS)
        for value, token in expected.items():
            with self.subTest(value=value):
                failure = capture.CaptureFailure(value)
                self.assertEqual(D.reason(failure), token)
                self.assertIn(token, D.REASONS)
                with tempfile.TemporaryDirectory() as directory:
                    progress = D.Progress(Path(directory))
                    progress.failed(failure)
                    original = dict(progress.value["failure"])
                    self.assertEqual(original["reason"], token)
                    progress.failed(RuntimeError("private replacement"))
                    self.assertEqual(progress.value["failure"], original)
                    self.assertNotIn("private", json.dumps(progress.value))
        class ForeignString(str):
            pass
        failure = capture.CaptureFailure()
        for invalid in (None, [], "private unknown body", ForeignString("capture")):
            failure.reason = invalid
            self.assertEqual(D.reason(failure), "unknown")
        class CaptureFailure(capture.CaptureFailure):
            def __init__(self):
                RuntimeError.__init__(self, "private body")
            @property
            def reason(self):
                raise AssertionError("foreign property must not be read")
        foreign = CaptureFailure()
        self.assertEqual(D.reason(foreign), "capture")
        with mock.patch.dict(sys.modules, {"appimage_capture": None}):
            self.assertEqual(D.reason(foreign), "capture")
        class UnrelatedFailure(RuntimeError):
            @property
            def reason(self):
                raise AssertionError("foreign property must not be read")
        self.assertEqual(D.reason(UnrelatedFailure("private body")), "unknown")

    def test_native_capture_codes_match_static_rust_and_do_not_reflect_private_text(self):
        import re
        source = (Path(__file__).resolve().parents[2] /
                  "crates/morpheus-rtc/src/media.rs").read_text()
        block = source.split("impl CaptureFailureCode {", 1)[1].split(
            "fn write_capture_failure(", 1)[0]
        actual = {code.encode("ascii") for code in re.findall(
            r'Self::[A-Za-z]+ => "([a-z_]+)"', block)}
        self.assertEqual(actual, D.CAPTURE_FAILURE_CODES)
        for code in D.CAPTURE_FAILURE_CODES:
            with self.subTest(code=code):
                data = b"private room and source body\n" + D.CAPTURE_FAILURE_PREFIX + code + b"\r\n"
                result = D._capture_marker_codes(data)
                self.assertEqual(result, {"status": "known_codes",
                                          "codes": [code.decode("ascii")]})
                self.assertNotIn("private", json.dumps(result))

    def test_native_capture_unknown_malformed_and_overlimit_markers_are_closed(self):
        known = D.CAPTURE_FAILURE_PREFIX + b"first_buffer\n"
        for data in (D.CAPTURE_FAILURE_PREFIX + b"private account body\n",
                     b"MORPHEUS_NATIVE_CAPTURE_FAILUREfirst_buffer\n",
                     known + D.CAPTURE_FAILURE_PREFIX + b"foreign URL\n",
                     D.CAPTURE_FAILURE_PREFIX + b"first_buffer private suffix\n",
                     D.CAPTURE_FAILURE_PREFIX + b"first_buffer\r\r\n",
                     known * 17, b"x" * (8 * 1024 * 1024 + 1), b"\n" * 65537,
                     bytearray(known)):
            with self.subTest(kind=type(data).__name__):
                self.assertEqual(D._capture_marker_codes(data), {"status": "invalid", "codes": []})
        self.assertEqual(D._capture_marker_codes(b"private foreign text"),
                         {"status": "known_codes", "codes": []})

    @unittest.skipIf(sys.platform == "win32", "POSIX private capture log identity")
    def test_native_capture_private_log_requires_scan_and_exact_owned_regular_file(self):
        with mock.patch.object(D, "Path", side_effect=AssertionError("must not inspect")):
            self.assertEqual(D.capture_failure_markers("unused", "ordinary", scan_passed=False),
                             {"status": "withheld", "codes": []})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "ordinary").mkdir(mode=0o700)
            session = root / "ordinary/session"; session.mkdir(mode=0o700)
            log = session / "app.stderr.log"
            log.write_bytes(D.CAPTURE_FAILURE_PREFIX + b"portal_select\n"); log.chmod(0o600)
            self.assertEqual(D.capture_failure_markers(root, "ordinary", scan_passed=True),
                             {"status": "scanned_private_app_stderr", "codes": ["portal_select"]})
            log.chmod(0o644)
            with self.assertRaises(ValueError):
                D.capture_failure_markers(root, "ordinary", scan_passed=True)
            log.chmod(0o600)
            alias = session / "alias.log"; os.link(log, alias)
            with self.assertRaises(ValueError):
                D.capture_failure_markers(root, "ordinary", scan_passed=True)
            alias.unlink()
            log.rename(alias); log.symlink_to(alias.name)
            with self.assertRaises(ValueError):
                D.capture_failure_markers(root, "ordinary", scan_passed=True)

    def test_native_capture_observation_cannot_rescue_or_replace_latched_failure(self):
        from linux_package_ui import DiagnosticFailure
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); progress = D.Progress(root)
            progress.wrapper("capture"); progress.command("capture-yellow")
            progress.capture("ordinary", "reopened_frame")
            progress.failed(DiagnosticFailure("wait_timeout")); progress.cleanup(True)
            original = dict(progress.value["failure"])
            with mock.patch.object(D, "capture_failure_markers", return_value={
                    "status": "scanned_private_app_stderr", "codes": ["capture_worker"]}):
                result = D.public_failure(root, scan_passed=True,
                                          outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertFalse(result["passed"])
            self.assertEqual(result["failure"], original)
            self.assertEqual(progress.value["failure"], original)
            with mock.patch.object(D, "capture_failure_markers", side_effect=OSError("private error")):
                self.assertEqual(D.public_failure(root, scan_passed=True,
                                                 outer_cleanup="passed", outer_reason="namespace_exit"),
                                 {"diagnostics": "withheld", "passed": False})
            self.assertEqual(progress.value["failure"], original)

    def test_share_control_schema_rejects_unknown_fields_types_and_unrelated_positions(self):
        good = {"observation": "after_timeout", "state": "share", "enabled": False, "checked": True}
        self.assertTrue(D.valid_share_control(good))
        for change in ({"state": "private source"}, {"enabled": 1}, {"checked": "true"},
                       {"observation": True}, {"private": "foreign body"},
                       {"state": "missing", "enabled": True}):
            with self.subTest(change=change):
                self.assertFalse(D.valid_share_control(good | change))
        for value in (None, [], {}, {key: value for key, value in good.items() if key != "checked"}):
            self.assertFalse(D.valid_share_control(value))
        from linux_package_ui import DiagnosticFailure
        with tempfile.TemporaryDirectory() as directory:
            progress = D.Progress(Path(directory))
            progress.wrapper("capture"); progress.command("capture-yellow")
            progress.capture("ordinary", "reopened_frame")
            progress.failed(DiagnosticFailure("wait_timeout"), share_control=good)
            for key, value in (("capture_phase", "static_frame"), ("command_stage", "capture-green"),
                               ("wrapper_phase", "build"), ("reason", "command_exit")):
                candidate = json.loads(json.dumps(progress.value))
                candidate["failure"][key] = value
                self.assertFalse(D.valid_record(candidate))
            candidate = json.loads(json.dumps(progress.value))
            candidate["failure"]["unexpected"] = "private"
            self.assertFalse(D.valid_record(candidate))

    def test_share_control_failure_is_first_latched_private_and_independently_scan_gated(self):
        from linux_package_ui import DiagnosticFailure
        observation = {"observation": "after_timeout", "state": "cancel", "enabled": True, "checked": False}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); progress = D.Progress(root)
            progress.wrapper("capture"); progress.command("capture-yellow")
            progress.capture("ordinary", "reopened_frame")
            progress.failed(DiagnosticFailure("wait_timeout"), share_control=observation)
            original = dict(progress.value["failure"])
            progress.command("capture-inventory")
            progress.failed(RuntimeError("private later failure"), share_control={"foreign": "private"})
            progress.cleanup(True)
            self.assertEqual(progress.value["failure"], original)
            self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})
            result = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
            self.assertEqual(result["failure"]["share_control"], observation)
            self.assertEqual(result["failure"]["command_stage"], "capture-yellow")
            self.assertFalse(result["passed"])
            self.assertNotIn("private", json.dumps(result))
            value = json.loads(progress.path.read_bytes())
            value["failure"]["share_control"]["foreign"] = "private body"
            progress.path.write_text(json.dumps(value))
            self.assertEqual(D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})

    def test_native_source_phase_commands_and_reasons_are_closed_and_scan_gated(self):
        from corresponding_source import SourceFailure
        commands = {"source-cargo-version", "source-rustc-version", "source-cargo-fetch", "cache-git-commit",
                    "source-commit", "source-tree", "source-objects"}
        self.assertTrue(commands <= D.COMMANDS)
        reasons = {"input", "path", "bounds", "identity", "lock", "missing", "archive",
                   "link", "submodule", "git", "manifest", "output"}
        for reason in reasons:
            self.assertEqual(D.reason(SourceFailure(reason)), "source_" + reason)
        class ForeignFailure(SourceFailure):
            @property
            def reason(self):
                raise AssertionError("foreign properties must never be inspected")
        foreign = ForeignFailure.__new__(ForeignFailure)
        self.assertEqual(D.reason(foreign), "unknown")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            progress = D.Progress(root)
            progress.wrapper("source")
            progress.command("source-cargo-fetch")
            progress.failed(SourceFailure("missing"))
            self.assertTrue(D.valid_record(progress.value))
            self.assertEqual(progress.value["failure"]["wrapper_phase"], "source")
            self.assertEqual(progress.value["failure"]["command_stage"], "source-cargo-fetch")
            self.assertEqual(progress.value["failure"]["reason"], "source_missing")
            withheld = D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="source_missing")
            self.assertEqual(withheld, {"diagnostics": "withheld", "passed": False})
            visible = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="source_missing")
            self.assertEqual(visible["failure"]["reason"], "source_missing")

    def test_owned_ui_reasons_are_fixed_and_foreign_properties_are_not_read(self):
        from linux_package_ui import DiagnosticFailure
        for code in ("wait_timeout", "app_exited", "tree_bound"):
            self.assertEqual(D.reason(DiagnosticFailure(code)), "ui_" + code)
        for code in ("private account and password", None, ["wait_timeout"]):
            self.assertEqual(D.reason(DiagnosticFailure(code)), "unknown")
        class ForeignFailure(DiagnosticFailure):
            @property
            def code(self):
                raise AssertionError("foreign code must never be inspected")
        self.assertEqual(D.reason(ForeignFailure.__new__(ForeignFailure)), "unknown")

    def test_login_subphases_and_ui_failure_are_latched_and_scan_gated(self):
        from linux_package_ui import DiagnosticFailure
        for phase in ("login_homeserver", "login_username", "login_password", "login_submit",
                      "login_room_wait", "login_room_open", "login_call_stage"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                progress = D.Progress(root)
                progress.wrapper("capture")
                progress.command("capture-window")
                progress.capture("ordinary", phase)
                progress.failed(DiagnosticFailure("wait_timeout"))
                progress.command("capture-inventory")
                progress.cleanup(True)
                self.assertTrue(D.valid_record(progress.value))
                self.assertEqual(set(progress.value), D.FIELDS)
                self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                                 {"diagnostics": "withheld", "passed": False})
                visible = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
                self.assertEqual(visible["failure"]["capture_phase"], phase)
                self.assertEqual(visible["failure"]["reason"], "ui_wait_timeout")
                self.assertEqual(visible["failure"]["command_stage"], "capture-window")
                self.assertEqual(visible["command_stage"], "capture-inventory")
                self.assertNotIn("private", json.dumps(visible))

    def test_display_subphases_are_closed_latched_and_scan_gated(self):
        for phase in ("display_start", "display_wayland_wait", "display_ipc_wait"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                progress = D.Progress(root)
                progress.wrapper("capture")
                progress.capture("ordinary", phase)
                progress.failed(RuntimeError("private path and service body"))
                progress.command("capture-inventory")
                progress.cleanup(True)
                self.assertTrue(D.valid_record(progress.value))
                self.assertEqual(set(progress.value), D.FIELDS)
                self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed", outer_reason="namespace_exit"),
                                 {"diagnostics": "withheld", "passed": False})
                visible = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
                self.assertEqual(visible["failure"]["capture_phase"], phase)
                self.assertEqual(visible["failure"]["command_stage"], "none")
                self.assertEqual(visible["command_stage"], "capture-inventory")
                self.assertNotIn("private", json.dumps(visible))

    @unittest.skipIf(sys.platform == "win32", "POSIX private peer log identity")
    def test_peer_panic_reducer_is_postscan_private_and_does_not_replace_failure(self):
        import os
        frame = b"thread 'receiving_peer' panicked at packaging/linux/appimage_peer.rs:380:10:\n"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            session = root / "ordinary/session"
            session.mkdir(parents=True, mode=0o700)
            (root / "ordinary").chmod(0o700)
            path = session / "peer.stderr.log"
            def log(body):
                path.write_bytes(body)
                path.chmod(0o600)
            progress = D.Progress(root)
            progress.wrapper("capture")
            progress.capture("ordinary", "portal")
            progress.command("capture-portal-owner")
            progress.failed(C.CommandFailure("exit"))
            prior = dict(progress.value["failure"])
            progress.capture("ordinary", "peer")
            progress.cleanup(True)
            for label, token in D.PEER_PANIC_LABELS.items():
                log(frame + label + b": private server error and key material\n")
                result = D.public_failure(root, scan_passed=True, outer_cleanup="passed", outer_reason="namespace_exit")
                self.assertEqual(result["peer_failure_marker"],
                                 {"status": "scanned_private_peer_stderr", "panic_site": token})
                self.assertEqual(result["failure"], prior)
                self.assertNotIn("private", json.dumps(result["peer_failure_marker"]).replace("scanned_private_peer_stderr", ""))
            for prefix in (b"tests/../../../", b"crates/morpheus-rtc/tests/../../../",
                           b"/synthetic/private-build/crates/morpheus-rtc/tests/../../../"):
                log(frame.replace(b"packaging/linux/", prefix + b"packaging/linux/")
                    + b"Peer login failed: private\n")
                self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=True)["panic_site"], "login")
            modern = frame.replace(b"' panicked", b"' (2147483647) panicked")
            log(modern + b"Peer login failed: private\n")
            self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=True)["panic_site"], "login")
            for malformed in (b"' (not-a-number) panicked", b"' (12345678901) panicked"):
                log(frame.replace(b"' panicked", malformed) + b"Peer login failed: private\n")
                self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=True)["panic_site"], "unknown")
            log(frame + b"Peer login failed: private\n" + b"\n" * 65536
                + modern + b"Peer join failed: private\n")
            self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=True)["panic_site"], "unknown")
            log(frame + b"Peer login failed: private\n" + frame + b"Peer join failed: private\n")
            self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=True)["panic_site"], "ambiguous")
            for body in (b"Peer login failed: private\n", frame + b"unrecognized private message\n",
                         frame.replace(b"appimage_peer.rs", b"foreign.rs") + b"Peer login failed: private\n"):
                log(body)
                self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=True)["panic_site"], "unknown")
            with mock.patch.object(D.Path, "lstat", side_effect=AssertionError("must not read")):
                self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=False),
                                 {"status": "withheld", "panic_site": "unknown"})
            with mock.patch.object(D, "peer_failure_marker", side_effect=AssertionError("cleanup must precede read")):
                result = D.public_failure(root, scan_passed=True, outer_cleanup="failed", outer_reason="namespace_exit")
                self.assertNotIn("peer_failure_marker", result)
            path.unlink()
            self.assertEqual(D.peer_failure_marker(root, "ordinary", scan_passed=True)["status"], "unavailable")
            target = session / "foreign"
            target.write_bytes(frame + b"Peer login failed: private\n")
            target.chmod(0o600)
            path.symlink_to(target)
            with self.assertRaises(ValueError):
                D.peer_failure_marker(root, "ordinary", scan_passed=True)
            path.unlink()
            os.link(target, path)
            with self.assertRaises(ValueError):
                D.peer_failure_marker(root, "ordinary", scan_passed=True)
            path.unlink()
            log(frame + b"Peer login failed: private\n")
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                D.peer_failure_marker(root, "ordinary", scan_passed=True)

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

    def test_packager_colored_cli_failure_preserves_scan_gate_and_private_text(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            secret = b"synthetic-private-config-path"
            self.package_failure_fixture(root, stderr=(
                b"\x1b[31mFailed to parse\x1b[0m config: " + secret + b"\n"
                b"I/O Error (" + secret + b"): Permission denied (os error 13)\n"
                b"\x1b[33m+ cp /bounded/cache/AppRun-x86_64 AppRun\x1b[0m\n"))
            self.assertEqual(D.public_failure(root, scan_passed=False, outer_cleanup="passed",
                                             outer_reason="namespace_exit"),
                             {"diagnostics": "withheld", "passed": False})
            facts = D.public_failure(root, scan_passed=True, outer_cleanup="passed",
                                     outer_reason="namespace_exit")
            self.assertEqual(facts["package_failure_markers"], {
                "status": "scanned_private_pair",
                "error_markers": ["cli_config", "filesystem_denied", "io"],
                "script_markers": ["copy_apprun"]})
            self.assertFalse(facts["passed"])
            self.assertEqual(facts["failure"]["reason"], "command_exit")
            self.assertNotIn(secret.decode(), json.dumps(facts))
            self.assertNotIn("/bounded/", json.dumps(facts))

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
