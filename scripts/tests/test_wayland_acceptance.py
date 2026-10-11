import contextlib
import importlib.util
import io
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("wayland_acceptance", Path(__file__).resolve().parents[1] / "run_wayland_acceptance.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class WaylandFixtureTests(unittest.TestCase):
    def test_cleanup_rejects_a_reaped_leader_without_signalling_its_pid(self):
        process = mock.Mock(pid=1234, returncode=0)
        with mock.patch.object(MODULE.os, "killpg") as kill:
            with self.assertRaisesRegex(RuntimeError, "reaped before group cleanup"):
                MODULE.stop_owned_groups([process])
        kill.assert_not_called()

    def test_timeout_fallback_never_signals_reused_pid(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "owned-groups.json").write_text('[{"pid": 1234, "start": "old"}]')
            with mock.patch.object(MODULE, "process_identity", return_value="new"), mock.patch.object(MODULE.os, "killpg") as kill:
                MODULE.force_registered_groups(root)
            kill.assert_not_called()

    def test_timeout_fallback_requires_its_original_group_leader(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "owned-groups.json").write_text('[{"pid": 1234, "start": "same"}]')
            with mock.patch.object(MODULE, "process_identity", return_value="same"), mock.patch.object(MODULE.os, "getpgid", return_value=9999), mock.patch.object(MODULE.os, "killpg") as kill:
                MODULE.force_registered_groups(root)
            kill.assert_not_called()

    def test_fixture_only_authorizes_chosen_output_and_has_no_x11(self):
        with tempfile.TemporaryDirectory(prefix="fixture with spaces ") as directory:
            root = Path(directory)
            sway, portal = MODULE.configs(root)
            self.assertIn("xwayland disable", sway.read_text())
            self.assertIn("chooser_type=simple", portal.read_text())
            self.assertNotIn("output_name=", portal.read_text())
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(MODULE.chooser(root / "chooser-mode"), 0)
            self.assertEqual(output.getvalue(), "HEADLESS-2\n")
            (root / "chooser-mode").write_text("deny")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(MODULE.chooser(root / "chooser-mode"), 0)
            self.assertEqual(output.getvalue(), "")

    def test_private_runtime_preserves_home_and_removes_existing_desktop(self):
        with tempfile.TemporaryDirectory() as directory:
            env = MODULE.private_environment(Path(directory))
            self.assertEqual(env.get("HOME"), os.environ.get("HOME"))
            for name in ("DISPLAY", "WAYLAND_DISPLAY", "SWAYSOCK", "DBUS_SESSION_BUS_ADDRESS"):
                self.assertNotIn(name, env)
            self.assertEqual(env["WLR_HEADLESS_OUTPUTS"], "2")
            self.assertEqual(Path(env["XDG_RUNTIME_DIR"]).stat().st_mode & 0o777, 0o700)


@unittest.skipUnless(sys.platform == "linux", "requires Linux nonreaping waitid and /proc")
class WaylandProcessTreeTests(unittest.TestCase):
    def test_completed_command_keeps_group_identity_until_cleanup(self):
        process = subprocess.Popen([sys.executable, "-c", "raise SystemExit(7)"], start_new_session=True)
        identity = MODULE.process_identity(process.pid)
        try:
            self.assertEqual(MODULE.wait_owned_process(process, timeout=3), 7)
            self.assertIsNone(process.returncode, "observing exit must not reap the leader")
            self.assertIsNotNone(identity)
            self.assertEqual(MODULE.process_identity(process.pid), identity)
            self.assertEqual(MODULE.peek_returncode(process), 7)
        finally:
            MODULE.stop_owned_groups([process])
        self.assertEqual(process.returncode, 7)

    def test_timed_out_command_remains_owned_until_cleanup(self):
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], start_new_session=True)
        identity = MODULE.process_identity(process.pid)
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                MODULE.wait_owned_process(process, timeout=0.05)
            self.assertIsNone(process.returncode)
            self.assertEqual(MODULE.process_identity(process.pid), identity)
        finally:
            MODULE.stop_owned_groups([process])
        self.assertIsNotNone(process.returncode)

    def test_cleanup_kills_term_resistant_descendant_after_leader_exits(self):
        child_program = (
            "import os, signal, sys, time\n"
            "from pathlib import Path\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "ready = Path(sys.argv[1])\n"
            "temporary = ready.with_suffix('.tmp')\n"
            "temporary.write_text(str(os.getpid()))\n"
            "temporary.replace(ready)\n"
            "while True: time.sleep(1)\n"
        )
        parent_program = (
            "import subprocess, sys, time\n"
            "subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]])\n"
            "while True: time.sleep(1)\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            ready = Path(directory) / "child-ready"
            process = subprocess.Popen(
                [sys.executable, "-c", parent_program, child_program, str(ready)],
                start_new_session=True,
            )
            identity = MODULE.process_identity(process.pid)
            child_pid = child_identity = None
            try:
                deadline = time.monotonic() + 3
                while not ready.exists():
                    self.assertIsNone(MODULE.peek_returncode(process))
                    self.assertLess(time.monotonic(), deadline, "child did not initialize")
                    time.sleep(0.01)
                child_pid = int(ready.read_text())
                child_identity = MODULE.process_identity(child_pid)
                self.assertIsNotNone(identity)
                self.assertIsNotNone(child_identity)
                kill_observations = []
                killpg = os.killpg

                def observe_kill(group, sig):
                    if sig == signal.SIGKILL:
                        kill_observations.append((group, MODULE.process_identity(group), MODULE.peek_returncode(process)))
                    killpg(group, sig)

                with mock.patch.object(MODULE.os, "killpg", side_effect=observe_kill):
                    MODULE.stop_owned_groups([process])
                self.assertEqual(process.returncode, -signal.SIGTERM)
                # The TERM-exited leader must still anchor the process group
                # at KILL, so a recycled PID can never be targeted instead.
                self.assertEqual(kill_observations, [(process.pid, identity, -signal.SIGTERM)])
                deadline = time.monotonic() + 3
                while self.child_is_running(child_pid, child_identity):
                    self.assertLess(time.monotonic(), deadline, "cleanup left its descendant running")
                    time.sleep(0.01)
            finally:
                if process.returncode is None:
                    MODULE.stop_owned_groups([process])
                # Also clean up if run against the old, broken implementation.
                if child_pid is not None and MODULE.process_identity(child_pid) == child_identity:
                    with contextlib.suppress(ProcessLookupError):
                        os.kill(child_pid, signal.SIGKILL)

    @staticmethod
    def child_is_running(pid: int, identity: str) -> bool:
        if MODULE.process_identity(pid) != identity:
            return False
        try:
            state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
        except FileNotFoundError:
            return False
        # An orphan may await reaping by the container/runner's init process.
        return state not in ("Z", "X")


if __name__ == "__main__":
    unittest.main()
