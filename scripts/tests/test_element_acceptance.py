import contextlib
import io
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
import wave

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_element_acceptance as target


class EvidenceTests(unittest.TestCase):
    def test_requires_every_fixed_assertion_and_preserves_failure(self):
        complete = "\n".join(f"MORPHEUS_ELEMENT_CHECK {name}=true" for name in target.CHECKS)
        self.assertEqual(target.evidence(complete, complete=True), dict.fromkeys(target.CHECKS, True))
        for value in ("", complete.replace("browser_roster=true", "browser_roster=false"),
                      "MORPHEUS_ELEMENT_CHECK native_audio=false\n" + complete,
                      complete + "\nMORPHEUS_ELEMENT_FAILURE browser_acceptance"):
            with self.subTest(value=value), self.assertRaisesRegex(RuntimeError, "evidence"):
                target.evidence(value, complete=True)
        self.assertEqual(target.evidence("MORPHEUS_ELEMENT_CHECK secret_value=true"), {})
        # Historical eleven-fact evidence cannot establish reverse audio. A
        # later true must never erase a false reverse-audio observation.
        self.assertIn("browser_audio", target.CHECKS)
        for value in (complete.replace("MORPHEUS_ELEMENT_CHECK browser_audio=true", ""),
                      "MORPHEUS_ELEMENT_CHECK browser_audio=false\n" + complete,
                      complete + "\nMORPHEUS_ELEMENT_CHECK browser_audio=false"):
            with self.subTest(reverse_audio=value), self.assertRaisesRegex(RuntimeError, "evidence"):
                target.evidence(value, complete=True)

        # The original two-peer proof cannot satisfy the independently required
        # late third participant, same sender screen and post-rotation media.
        group = (
            "native_group_roster", "native_late_join_media", "native_group_departure", "native_key_rotation",
            "browser_group_roster", "browser_group_audio", "browser_screen", "browser_group_departure",
            "browser_key_rotation", "browser_post_rotation_screen", "browser_post_rotation_audio",
        )
        for name in group:
            self.assertIn(name, target.CHECKS)
            for value in (complete.replace(f"MORPHEUS_ELEMENT_CHECK {name}=true", ""),
                          f"MORPHEUS_ELEMENT_CHECK {name}=false\n" + complete):
                with self.subTest(group_fact=name), self.assertRaisesRegex(RuntimeError, "evidence"):
                    target.evidence(value, complete=True)

    def test_exact_named_nonempty_test_result_is_required(self):
        result = f"test {target.TEST_NAME} ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;"
        self.assertTrue(target.exact_native_pass(result))
        for value in (result.replace(target.TEST_NAME, "other"), result.replace("1 passed", "0 passed"),
                      result.replace("0 ignored", "1 ignored"), result.replace("... ok", "... FAILED"),
                      result + "\nSkipped native fixture", result.splitlines()[-1]):
            with self.subTest(value=value):
                self.assertFalse(target.exact_native_pass(value))

    def test_diagnostics_are_allowlisted(self):
        result = target.diagnostics("\n".join((
            "MORPHEUS_ELEMENT_STAGE browser_roster", "MORPHEUS_ELEMENT_STAGE opaque_credential",
            "MORPHEUS_ELEMENT_FAILURE browser_acceptance", "MORPHEUS_ELEMENT_FAILURE private_password",
        )))
        self.assertEqual(result, {"stage": "browser_roster", "failure": "browser_acceptance"})

    def test_native_group_stages_are_closed_ordered_and_bounded(self):
        lines = [target.NATIVE_STAGE_PREFIX + value for value in target.NATIVE_STAGES]
        for count in range(1, len(lines) + 1):
            output = "\n".join(lines[:count])
            self.assertEqual(target.native_stages(output), target.NATIVE_STAGES[:count])
            self.assertEqual(target.diagnostics(output), {"native_stage": target.NATIVE_STAGES[count - 1]})
        self.assertEqual(target.native_stages("ordinary private log"), ())
        for output in (lines[1], lines[0] + "\n" + lines[0], "\n".join(lines + [lines[-1]]),
                       lines[0] + " private", target.NATIVE_STAGE_PREFIX + "opaque_secret",
                       target.NATIVE_STAGE_PREFIX + "x" * 97):
            with self.subTest(output=output):
                self.assertEqual(target.native_stages(output), ("invalid",))
                self.assertEqual(target.diagnostics(output), {"native_stage": "invalid"})

    def test_native_failure_facts_follow_owned_cleanup_and_successful_scan(self):
        for scanner_status, join_failure, service_failure in ((0, False, False), (1, False, False),
                                                            (0, True, False), (0, False, True)):
            with self.subTest(scanner_status=scanner_status, join_failure=join_failure, service_failure=service_failure), \
                    tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                fixture = root / "fixture.json"
                fixture.write_text("{}")
                fixture.chmod(0o600)
                (root / "browser-ready").touch()
                runner = target.Runner(root, "scanner")
                native = self.anchored_log(runner, "native.log", (
                    "opaque-fixture-panic\nMORPHEUS_ELEMENT_CHECK native_audio=true\n"
                    + target.NATIVE_STAGE_PREFIX + "microphone_cryptor\n"
                    + target.NATIVE_STAGE_PREFIX + "late_login\n").encode())
                browser = self.anchored_log(runner, "browser.log", b"MORPHEUS_ELEMENT_STAGE browser_group_roster\n")
                native.joined = browser.joined = False
                native.status = mock.Mock(return_value=1)
                browser.status = mock.Mock(return_value=None)
                events = []
                def native_join():
                    events.append("native_join")
                    if join_failure:
                        raise RuntimeError("fixture_operation")
                    native.joined = True
                def browser_join():
                    events.append("browser_join")
                    browser.joined = True
                native.close = native_join
                browser.close = browser_join
                runner.children = [browser, native]
                def scanned(*args, **kwargs):
                    events.append("scan")
                    return subprocess.CompletedProcess([], scanner_status)
                original_snapshots = runner.log_snapshots
                def snapshot():
                    events.append("read")
                    return original_snapshots()
                visible = io.StringIO()
                with mock.patch.object(target.sys, "platform", "linux"), mock.patch.dict(os.environ, {
                    "GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux",
                }, clear=True), mock.patch.object(target, "assert_contained"), \
                        mock.patch.object(target, "Runner", return_value=runner), \
                        mock.patch.object(runner, "command", return_value=""), \
                        mock.patch.object(runner, "service", side_effect=[{"official_image_identity_verified": True},
                                                                      RuntimeError("fixture_operation") if service_failure else None]), \
                        mock.patch.object(runner, "start", side_effect=[browser, native]), \
                        mock.patch.object(target, "native_executable", return_value=Path("native")), \
                        mock.patch.object(target, "media_fixtures"), \
                        mock.patch.object(target.subprocess, "run", side_effect=scanned), \
                        mock.patch.object(runner, "log_snapshots", side_effect=snapshot), \
                        contextlib.redirect_stdout(visible), self.assertRaisesRegex(
                            RuntimeError, "fixture_operation" if join_failure or service_failure else (
                                "native_acceptance" if scanner_status == 0 else "log_scan")):
                    target.execute(root, fixture, "scanner")
                self.assertEqual(events, ["native_join", "browser_join"] + ([] if join_failure or service_failure else ["read", "scan"]))
                self.assertIsNone(native.log_anchor)
                self.assertIsNone(browser.log_anchor)
                output = visible.getvalue()
                self.assertNotIn("opaque", output)
                self.assertEqual(target.evidence(output),
                                 {"native_audio": True} if scanner_status == 0 and not join_failure and not service_failure else {})
                self.assertEqual(target.diagnostics(output).get("native_stage"),
                                 "late_login" if scanner_status == 0 and not join_failure and not service_failure else None)

    @staticmethod
    def anchored_log(runner, name, data):
        child = target.Child.__new__(target.Child)
        child.log = runner.logs / name
        child.log_anchor = os.open(child.log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        child.log_identity = child.file_identity(os.fstat(child.log_anchor))
        os.write(child.log_anchor, data)
        child.joined = True
        runner.children.append(child)
        return child

    def test_log_scan_uses_exact_original_binary_snapshot_before_decode(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            raw = b"\xffopaque-fixture-log\nMORPHEUS_ELEMENT_NATIVE_STAGE microphone_cryptor\n"
            child = self.anchored_log(runner, "native.log", raw)
            try:
                def scanned(*args, **kwargs):
                    os.ftruncate(child.log_anchor, 0)
                    os.lseek(child.log_anchor, 0, os.SEEK_SET)
                    os.write(child.log_anchor, b"changed after snapshot")
                    return subprocess.CompletedProcess([], 0)
                with mock.patch.object(target.subprocess, "run", side_effect=scanned) as scan:
                    combined, native = runner.scan(child)
                supplied = scan.call_args.kwargs["input"]
                self.assertIsInstance(supplied, bytes)
                self.assertEqual(hashlib.sha256(supplied).digest(), hashlib.sha256(raw).digest())
                self.assertEqual(combined, raw.decode(errors="replace"))
                self.assertEqual(native, combined)
            finally:
                runner.release_log_anchors()

    def test_log_snapshot_refuses_symlink_replacement_and_oversize_before_scan(self):
        for change in ("symlink", "replacement", "missing", "fifo", "alias", "oversize", "metadata"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                runner = target.Runner(Path(directory), "scanner")
                child = self.anchored_log(runner, "native.log", b"private")
                try:
                    if change == "symlink":
                        child.log.unlink()
                        other = Path(directory) / "other"
                        other.write_bytes(b"other")
                        child.log.symlink_to(other)
                    elif change == "replacement":
                        child.log.unlink()
                        child.log.write_bytes(b"replacement")
                    elif change == "missing":
                        child.log.unlink()
                    elif change == "fifo":
                        child.log.unlink()
                        os.mkfifo(child.log, 0o600)
                    elif change == "alias":
                        os.link(child.log, Path(directory) / "alias")
                    elif change == "oversize":
                        os.ftruncate(child.log_anchor, target.MAX_LOG_BYTES + 1)
                    else:
                        child.log.chmod(0o644)
                    with mock.patch.object(target.subprocess, "run") as scan, \
                            mock.patch.object(target.os, "read") as read, self.assertRaisesRegex(RuntimeError, "log_scan"):
                        runner.scan(child)
                    scan.assert_not_called()
                    read.assert_not_called()
                finally:
                    runner.release_log_anchors()

    def test_log_snapshot_refuses_change_during_read_and_bounded_tree_overflow(self):
        for change in ("during_read", "entries", "files", "depth"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                runner = target.Runner(Path(directory), "scanner")
                child = self.anchored_log(runner, "native.log", b"private")
                original_read = os.read
                def changed_read(fd, count):
                    data = original_read(fd, count)
                    if data:
                        os.ftruncate(child.log_anchor, 0)
                    return data
                try:
                    with contextlib.ExitStack() as stack:
                        if change == "during_read":
                            stack.enter_context(mock.patch.object(target.os, "read", side_effect=changed_read))
                        else:
                            stack.enter_context(mock.patch.object(target, {
                                "entries": "MAX_LOG_ENTRIES", "files": "MAX_LOG_FILES", "depth": "MAX_LOG_DEPTH",
                            }[change], 0))
                        scan = stack.enter_context(mock.patch.object(target.subprocess, "run"))
                        with self.assertRaisesRegex(RuntimeError, "log_scan"):
                            runner.scan(child)
                        scan.assert_not_called()
                finally:
                    runner.release_log_anchors()

    def test_combined_binary_log_limit_includes_record_separators(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            first = self.anchored_log(runner, "first.log", b"aaaa")
            self.anchored_log(runner, "second.log", b"bbbb")
            try:
                with mock.patch.object(target, "MAX_LOG_BYTES", 8), \
                        mock.patch.object(target.subprocess, "run") as scan, self.assertRaisesRegex(RuntimeError, "log_scan"):
                    runner.scan(first)
                scan.assert_not_called()
            finally:
                runner.release_log_anchors()

    def test_native_artifact_must_be_one_named_test_inside_target(self):
        with tempfile.TemporaryDirectory() as folder:
            repo = Path(folder)
            binary = repo / "target/debug/deps/element_call_peer-hash"
            binary.parent.mkdir(parents=True)
            binary.touch()
            entry = {"reason": "compiler-artifact", "target": {"name": "element_call_peer"},
                     "profile": {"test": True}, "executable": str(binary)}
            with mock.patch.object(target, "REPO", repo):
                self.assertEqual(target.native_executable(json.dumps(entry)), binary.resolve())
                with self.assertRaises(RuntimeError):
                    target.native_executable(json.dumps(entry) + "\n" + json.dumps(entry))
                entry["profile"]["test"] = False
                with self.assertRaises(RuntimeError):
                    target.native_executable(json.dumps(entry))

    def test_fake_capture_files_have_distinct_known_video_and_nonzero_audio(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target.media_fixtures(root)
            data = (root / "camera.y4m").read_bytes()
            header, frames = data.split(b"\n", 1)
            self.assertEqual(header, b"YUV4MPEG2 W640 H360 F15:1 Ip A1:1 C420jpeg")
            self.assertEqual(len(frames), 30 * (6 + 640 * 360 * 3 // 2))
            self.assertEqual(frames[:6], b"FRAME\n")
            self.assertEqual(frames[6:326], bytes([82]) * 320)
            self.assertEqual(frames[326:646], bytes([145]) * 320)
            with wave.open(str(root / "microphone.wav")) as wav:
                self.assertEqual((wav.getnchannels(), wav.getframerate(), wav.getnframes()), (1, 48_000, 96_000))
                self.assertTrue(any(wav.readframes(480)))
            for file in root.iterdir():
                self.assertEqual(file.stat().st_mode & 0o777, 0o600)

    def test_diagnostic_refresh_is_rejected_before_starting_children(self):
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(target.sys, "platform", "linux"), mock.patch.dict(os.environ, {
                "GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "MORPHEUS_ELEMENT_DIAG_REFRESH": "0",
            }), mock.patch.object(target, "Runner") as runner, self.assertRaises(RuntimeError):
                target.execute(Path(directory), Path(directory) / "fixture.json", "scanner")
            runner.assert_not_called()

    def test_scanner_failure_never_emits_log_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            (runner.logs / "private.log").write_text("opaque-fixture-password")
            visible = io.StringIO()
            with mock.patch.object(target.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)), contextlib.redirect_stdout(visible), self.assertRaisesRegex(RuntimeError, "log_scan"):
                runner.scan()
            self.assertNotIn("opaque", visible.getvalue())

    def test_cleanup_kills_group_before_reaping_even_after_leader_exit(self):
        child = target.Child.__new__(target.Child)
        child.closed = False
        child.process = mock.Mock(pid=12345, returncode=None)
        child.status = mock.Mock(return_value=7)
        events = []
        child.process.wait.side_effect = lambda **kwargs: events.append("reap")
        with mock.patch.object(target.os, "killpg", side_effect=lambda *_: events.append("kill")):
            child.close()
            child.close()
        self.assertEqual(events, ["kill", "reap"])

    def test_cleanup_refuses_to_signal_a_reaped_leader(self):
        child = target.Child.__new__(target.Child)
        child.closed = False
        child.process = mock.Mock(pid=12345, returncode=None)
        child.status = mock.Mock(side_effect=ChildProcessError())
        with mock.patch.object(target.os, "killpg") as kill, self.assertRaisesRegex(RuntimeError, "fixture_operation"):
            child.close()
        kill.assert_not_called()

    def test_cleanup_attempts_every_child_despite_an_ownership_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = target.Runner(Path(directory), "scanner")
            first, second = mock.Mock(), mock.Mock()
            second.close.side_effect = RuntimeError("fixture_operation")
            runner.children = [first, second]
            with self.assertRaises(RuntimeError):
                runner.close()
            first.close.assert_called_once()
            second.close.assert_called_once()

    def test_simultaneously_finished_peers_must_both_succeed(self):
        for native_status, browser_status, expected in ((0, 0, None), (0, 1, "browser_acceptance"),
                                                       (1, 0, "native_acceptance"), (1, 1, "native_acceptance")):
            native, browser = mock.Mock(), mock.Mock()
            native.status.return_value = native_status
            browser.status.return_value = browser_status
            with self.subTest(native=native_status, browser=browser_status):
                if expected:
                    with self.assertRaisesRegex(RuntimeError, expected):
                        target.wait_peers(native, browser)
                else:
                    target.wait_peers(native, browser)

    def test_peers_finishing_between_polls_still_check_terminal_status(self):
        native, browser = mock.Mock(started=time.monotonic()), mock.Mock()
        native.status.side_effect = [None, 0]
        browser.status.side_effect = [None, 1]
        with mock.patch.object(target.time, "sleep"), self.assertRaisesRegex(RuntimeError, "browser_acceptance"):
            target.wait_peers(native, browser)


@unittest.skipUnless(sys.platform == "linux", "Linux owned-group lifecycle")
class OwnedProcessTests(unittest.TestCase):
    def test_exited_leader_keeps_its_descendant_owned_until_cleanup(self):
        program = (
            "import subprocess,sys,time\n"
            "from pathlib import Path\n"
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])\n"
            "Path(sys.argv[1]).write_text(str(child.pid))\n"
            "raise SystemExit(7)\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ready = root / "child"
            child = target.Child([sys.executable, "-c", program, str(ready)], root / "raw.log", dict(os.environ), root)
            descendant = None
            descendant_start = None
            try:
                deadline = time.monotonic() + 5
                while child.status() is None:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                descendant = int(ready.read_text())
                descendant_start = Path(f"/proc/{descendant}/stat").read_text().rsplit(")", 1)[1].split()[19]
                self.assertEqual(child.status(), 7)
                self.assertIsNone(child.process.returncode, "exit observation must not reap")
                self.assertTrue(Path(f"/proc/{child.process.pid}").exists())
                child.close()
                while Path(f"/proc/{descendant}/stat").exists():
                    state = Path(f"/proc/{descendant}/stat").read_text().rsplit(")", 1)[1].split()[0]
                    if state in ("Z", "X"):
                        break
                    self.assertLess(time.monotonic(), deadline, "descendant still running")
                    time.sleep(0.01)
                self.assertEqual(child.process.returncode, 7)
            finally:
                child.close()
                child.release_log_anchor()
                if descendant is not None:
                    with contextlib.suppress(ProcessLookupError, FileNotFoundError):
                        current = Path(f"/proc/{descendant}/stat").read_text().rsplit(")", 1)[1].split()[19]
                        if current == descendant_start:
                            os.kill(descendant, signal.SIGKILL)


if __name__ == "__main__":
    unittest.main()
