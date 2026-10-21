import contextlib
import io
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
                if descendant is not None:
                    with contextlib.suppress(ProcessLookupError, FileNotFoundError):
                        current = Path(f"/proc/{descendant}/stat").read_text().rsplit(")", 1)[1].split()[19]
                        if current == descendant_start:
                            os.kill(descendant, signal.SIGKILL)


if __name__ == "__main__":
    unittest.main()
