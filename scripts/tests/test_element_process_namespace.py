import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import element_process_namespace as target


@unittest.skipIf(os.name == "nt", "POSIX process containment source checks")
class NamespaceTests(unittest.TestCase):
    def test_command_drops_all_privileges_before_isolated_repository_entry(self):
        readlink = os.readlink
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(target.sys, "platform", "linux"), \
                mock.patch.object(target, "trusted_program") as trusted, \
                mock.patch.object(target.os, "getuid", return_value=1001), \
                mock.patch.object(target.os, "geteuid", return_value=1001), \
                mock.patch.object(target.os, "getgid", return_value=1002), \
                mock.patch.object(target.os, "getegid", return_value=1002), \
                mock.patch.object(target.os, "getgroups", return_value=[1002, 123]), \
                mock.patch.object(target.os, "readlink", side_effect=lambda path, **kw: "pid:[100]" if str(path) == "/proc/self/ns/pid" else readlink(path, **kw)):
            env = {"GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "PRIVATE_FIXTURE_VALUE": "private-test-value"}
            args = [sys.executable, "scripts/run_element_acceptance.py"]
            command = target.namespace_command(args, Path(directory), env)
            self.assertEqual(command[:9], ["/usr/bin/sudo", "--non-interactive", "--", "/usr/bin/unshare",
                                         "--pid", "--fork", "--kill-child=KILL", "--mount-proc", "/usr/bin/setpriv"])
            self.assertEqual(command[9:17], ["--reuid=1001", "--regid=1002", "--groups=123,1002",
                                            "--inh-caps=-all", "--ambient-caps=-all", "--bounding-set=-all",
                                            "--no-new-privs", "--pdeathsig=KILL"])
            self.assertEqual(command[18], "-I")
            self.assertNotIn("private-test-value", " ".join(command))
            self.assertEqual(trusted.call_args_list, [mock.call(value) for value in target.TRUSTED])
            control = Path(directory) / "control.json"
            self.assertEqual(control.stat().st_mode & 0o777, 0o600)
            self.assertEqual(json.loads(control.read_text())["env"], env)

    def test_init_handle_is_not_promoted_when_pid_identity_changed(self):
        for observed in ({}, {12345: 2}):
            with self.subTest(observed=observed), \
                    mock.patch.object(target.os, "pidfd_open", return_value=17, create=True), \
                    mock.patch.object(target, "namespace_members", return_value=observed), \
                    mock.patch.object(target.os, "close") as close, self.assertRaises(RuntimeError):
                target.open_init_handle(12345, "pid:[101]")
            close.assert_called_once_with(17)

    def test_private_control_rejects_symlinks_and_open_permissions(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "control.json"
            path.write_text('{}')
            path.chmod(0o644)
            with self.assertRaises(RuntimeError):
                target.read_control(path)
            path.chmod(0o600)
            link = Path(directory) / "link"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                target.read_control(link)
            self.assertEqual(target.read_control(path), {})

    def test_wait_observation_never_reaps_and_cleanup_kills_before_reaping(self):
        process = mock.Mock(pid=12345, returncode=None)
        events = []
        with mock.patch.object(target, "status", return_value=0) as observe, \
                mock.patch.object(target.os, "killpg", side_effect=lambda *_: events.append("kill")):
            process.wait.side_effect = lambda **_: events.append("reap")
            self.assertEqual(target.wait_owned(process, 1), 0)
            process.wait.assert_not_called()
            target.stop_owned(process)
            self.assertEqual(observe.call_count, 2)
            self.assertEqual(events, ["kill", "reap"])

    def test_cleanup_never_signals_a_reaped_anchor(self):
        process = mock.Mock(pid=12345, returncode=0)
        with mock.patch.object(target.os, "killpg") as kill, self.assertRaises(RuntimeError):
            target.stop_owned(process)
        kill.assert_not_called()

    def test_entry_rejects_missing_namespace_before_running_any_target(self):
        with mock.patch.object(target, "read_control", return_value={}), \
                mock.patch.object(target.os, "execvpe") as execute, self.assertRaises(RuntimeError):
            target.inside(Path("unused"))
        execute.assert_not_called()

    def test_guard_requires_actual_pid_one_zero_capabilities_and_original_identity(self):
        control = {"uid": 1001, "gid": 1002, "groups": [123, 1002], "outer_namespace": "pid:[100]"}
        fields = "NSpid:\t1\nNoNewPrivs:\t1\n" + "".join(key + ":\t00000000\n" for key in target.CAPABILITIES)
        with mock.patch.object(target.os, "getresuid", return_value=(1001,) * 3, create=True), \
                mock.patch.object(target.os, "getresgid", return_value=(1002,) * 3, create=True), \
                mock.patch.object(target.os, "getgroups", return_value=[123, 1002]), \
                mock.patch.object(target.os, "getpid", return_value=1) as pid, \
                mock.patch.object(target.os, "getppid", return_value=0), \
                mock.patch.object(target.os, "readlink", return_value="pid:[101]") as link, \
                mock.patch.object(target.Path, "read_text", return_value=fields) as text:
            self.assertEqual(target.assert_init_identity(control), "pid:[101]")
            pid.return_value = 42
            with self.assertRaises(RuntimeError):
                target.assert_init_identity(control)
            pid.return_value = 1
            link.return_value = "pid:[100]"
            with self.assertRaises(RuntimeError):
                target.assert_init_identity(control)
            link.return_value = "pid:[101]"
            for key in target.CAPABILITIES:
                text.return_value = fields.replace(key + ":\t00000000", key + ":\t00000001")
                with self.subTest(capability=key), self.assertRaises(RuntimeError):
                    target.assert_init_identity(control)


@unittest.skipUnless(sys.platform == "linux" and os.environ.get("MORPHEUS_RUN_NAMESPACE_REGRESSION") == "1",
                     "Opt-in hosted sudo/PID-namespace regression; never a local unit-test service")
class HostedNamespaceTests(unittest.TestCase):
    def test_actual_non_tty_sudo_wrapper_kill_stops_pid_one_and_detached_child(self):
        # The Matrix acceptance harness invokes this same check unconditionally
        # before native/browser interop; this entry also permits isolated reruns.
        with tempfile.TemporaryDirectory() as directory:
            target.containment_check(Path(directory))


if __name__ == "__main__":
    unittest.main()
