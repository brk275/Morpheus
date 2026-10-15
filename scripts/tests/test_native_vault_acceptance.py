"""Pure/mocked wrapper contracts. Never invokes Rust, security, or any OS vault."""
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from scripts import run_native_vault_acceptance as M


class SchemaTests(unittest.TestCase):
    def record(self, phase="prepare", status="passed"):
        return {"version": 1, "phase": phase, "status": status, "stage": M.TERMINAL[phase],
                "failure": None, "checks": {key: True for key in M.CHECKS[phase]}}

    def parse(self, value, **kwargs):
        with patch.object(M, "read_json", return_value=value):
            return M.phase_record(Path("unused"), "prepare", **kwargs)

    def test_exact_success_requires_every_fixed_fact_and_terminal_state(self):
        self.assertEqual(self.parse(self.record(), final=True)["status"], "passed")
        for mutate in (lambda r: r["checks"].pop(next(iter(r["checks"]))),
                       lambda r: r["checks"].update({next(iter(r["checks"])): False}),
                       lambda r: r.update(stage="profile_open"),
                       lambda r: r.update(status="running"),
                       lambda r: r.update(version=True),
                       lambda r: r.update(stage=[]),
                       lambda r: r.update(extra="private"),
                       lambda r: r["checks"].update(private=True)):
            value = self.record(); mutate(value)
            with self.assertRaises(M.Failure):
                self.parse(value, final=True)

    def test_foreign_failure_text_is_never_published(self):
        value = self.record(); value.update(status="failed", failure="private_secret", stage="password_enable")
        reduced = self.parse(value)
        self.assertEqual(reduced["failure"], "fixture_failure")
        self.assertNotIn("private_secret", json.dumps(reduced))
        value["failure"] = "/private/path/value"
        with self.assertRaises(M.Failure):
            self.parse(value)

    def test_duplicate_json_fields_are_rejected(self):
        with self.assertRaises(M.Failure):
            json.loads('{"version":1,"version":1}', object_pairs_hook=M.unique_json)

    def test_private_file_ownership_mode_and_bound(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "result.json"
            path.write_text(json.dumps(self.record())); path.chmod(0o600)
            self.assertEqual(M.read_json(path)["version"], 1)
            path.chmod(0o644)
            with self.assertRaises(M.Failure):
                M.read_json(path)
            path.chmod(0o600); path.write_bytes(b"x" * (M.BOUND + 1))
            with self.assertRaises(M.Failure):
                M.read_json(path)

    def test_keychain_metadata_is_closed_and_preserves_order(self):
        self.assertEqual(M.metadata_paths(b'    "/one"\n    "/two"\n'), [Path("/one"), Path("/two")])
        self.assertIsNone(M.metadata_paths(b" <NULL>\n", default=True))
        self.assertEqual(M.metadata_paths(b""), [])
        for value in (b"", b"not-a-path", b'"relative"', b'"/one"\n"/two"', b'"/one\x00"'):
            with self.assertRaises(M.Failure):
                M.metadata_paths(value, default=True)

    def test_claimed_targets_only_and_profile_identity(self):
        a, b, profile = (str(M.uuid.uuid4()) for _ in range(3))
        value = {"version": 1, "run_nonce": "n", "profile_uuid": profile, "account_a": a, "account_b": b,
            "app_password": {"absent_confirmed": True, "write_claimed": True},
            "account_a_item": {"absent_confirmed": False, "write_claimed": False},
            "account_b_item": {"absent_confirmed": True, "write_claimed": True}}
        with patch.object(M, "read_json", return_value=value):
            expected = M.journal_cleanup_checks(Path("unused"), "n", a, b)
            self.assertEqual(expected, {"cleanup_profile_absent", "cleanup_app_entry", "cleanup_account_b"})
            value["account_a_item"]["write_claimed"] = True
            with self.assertRaises(M.Failure):
                M.journal_cleanup_checks(Path("unused"), "n", a, b)


class ProcessTests(unittest.TestCase):
    def run_mocked(self, *, cleanup_error=False, output=b"private-output", limit=100, deadline=False):
        process = Mock()
        process.stdout.fileno.return_value = 17
        selector = Mock()
        selector.select.return_value = [(SimpleNamespace(fd=17, fileobj=process.stdout), 1)]
        runner = M.MacRunner()
        with patch.object(M.sys, "platform", "darwin"), \
                patch.object(M.package_smoke, "require_nonreaping_wait"), \
                patch.object(M.package_smoke, "process_status", return_value=None if deadline else 0), \
                patch.object(M.MacRunner, "stop_owned", side_effect=RuntimeError("private-cleanup") if cleanup_error else None) as stop, \
                patch.object(M.subprocess, "Popen", return_value=process) as start, \
                patch.object(M.selectors, "DefaultSelector", return_value=selector), \
                patch.object(M.os, "set_blocking"), patch.object(M.os, "read", side_effect=[output, b""]), \
                patch.object(M.time, "monotonic", side_effect=[0, 2] if deadline else [0, .01, .02, .03, .04]):
            try:
                result = runner.run(["/fixture"], payload=b"synthetic-input", seconds=1, output_limit=limit)
                failure = None
            except M.Failure as error:
                result, failure = None, error
        stop.assert_called_once_with(process)
        self.assertTrue(start.call_args.kwargs["start_new_session"])
        self.assertEqual(start.call_args.kwargs["stderr"], M.subprocess.STDOUT)
        return runner, result, failure

    def test_private_bounded_output_and_owned_teardown(self):
        runner, result, failure = self.run_mocked()
        self.assertIsNone(failure)
        self.assertEqual(result, M.CommandResult(0, b"private-output"))
        self.assertTrue(runner.quiescent)

    def test_output_bound_still_stops_process(self):
        runner, _, failure = self.run_mocked(output=b"too-long", limit=1)
        self.assertEqual(failure.code, "process_output_bound")
        self.assertTrue(runner.quiescent)

    def test_original_deadline_survives_failed_cleanup(self):
        runner, _, failure = self.run_mocked(deadline=True, cleanup_error=True)
        self.assertEqual(failure.code, "process_deadline")
        self.assertTrue(failure.cleanup)
        self.assertFalse(runner.quiescent)
        with self.assertRaises(M.Failure) as raised:
            runner.run(["/must-not-start"])
        self.assertEqual(raised.exception.code, "children_unconfirmed")


class KeychainTransactionTests(unittest.TestCase):
    def instance(self, root):
        evidence = M.Evidence()
        runner = Mock(quiescent=True)
        return M.MacKeychain(root, runner, evidence), runner, evidence

    def test_restore_exact_original_paths_after_fixture_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            keychain, runner, evidence = self.instance(root)
            keychain.original_default = Path("/original/default")
            keychain.original_list = [Path("/original/first"), Path("/original/default")]
            keychain.snapshotted = keychain.may_have_changed = keychain.owned_create_attempted = True
            keychain.path.touch(mode=0o600)
            evidence.fail("remove_b", "fixture_failure")
            def command(arguments):
                if arguments[0] == "delete-keychain":
                    self.assertEqual(arguments, ["delete-keychain", keychain.path])
                    keychain.path.unlink(); return b""
                if "-s" in arguments:
                    return b""
                return b'"/original/default"\n' if arguments[0] == "default-keychain" else b'"/original/first"\n"/original/default"\n'
            with patch.object(keychain, "command", side_effect=command) as commands:
                self.assertTrue(keychain.restore())
            self.assertEqual(commands.call_args_list[0].args[0], ["list-keychains", "-d", "user", "-s", *keychain.original_list])
            self.assertEqual(commands.call_args_list[1].args[0], ["default-keychain", "-d", "user", "-s", keychain.original_default])
            self.assertEqual(evidence.first, {"stage": "remove_b", "code": "fixture_failure"})

    def test_original_null_default_and_empty_list_restore_without_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            keychain, _, _ = self.instance(Path(temporary))
            keychain.original_list = []
            keychain.snapshotted = keychain.may_have_changed = keychain.owned_create_attempted = True
            with patch.object(keychain, "command", side_effect=[b"", b"", b"<NULL>\n", b"", b"<NULL>\n", b""]) as commands:
                self.assertTrue(keychain.restore())
            self.assertEqual(commands.call_args_list[0].args[0], ["list-keychains", "-d", "user", "-s"])
            self.assertEqual(commands.call_args_list[1].args[0], ["default-keychain", "-d", "user", "-s"])

    def test_no_restoration_or_deletion_while_children_are_unconfirmed(self):
        keychain, runner, evidence = self.instance(Path("/unused"))
        keychain.snapshotted = keychain.may_have_changed = True
        runner.quiescent = False
        with patch.object(keychain, "command") as command:
            self.assertFalse(keychain.restore())
        command.assert_not_called()
        self.assertEqual(evidence.cleanup_failures, ["children_unconfirmed"])

    def test_one_failed_restore_does_not_skip_other_metadata_restore(self):
        with tempfile.TemporaryDirectory() as temporary:
            keychain, _, evidence = self.instance(Path(temporary))
            keychain.original_default = Path("/original/default"); keychain.original_list = []
            keychain.snapshotted = keychain.may_have_changed = keychain.owned_create_attempted = True
            with patch.object(keychain, "command", side_effect=[M.Failure("keychain_command"), b"", b'"/original/default"', b"", b'"/original/default"', b""]) as commands:
                self.assertFalse(keychain.restore())
            self.assertEqual(commands.call_args_list[1].args[0][0], "default-keychain")
            self.assertIn("keychain_restore", evidence.cleanup_failures)

    def test_partial_create_is_owned_and_restored_after_unlock_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            keychain, _, evidence = self.instance(Path(temporary))
            calls = []
            def command(arguments):
                calls.append(arguments)
                name = arguments[0]
                if name == "create-keychain":
                    keychain.path.touch(mode=0o600); return b""
                if name == "unlock-keychain":
                    raise M.Failure("keychain_command")
                if name == "delete-keychain":
                    keychain.path.unlink(); return b""
                if "-s" in arguments:
                    return b""
                return b'"/original/default"' if name == "default-keychain" else b'"/original/default"'
            with patch.object(keychain, "command", side_effect=command), \
                    patch.object(keychain, "secret_command", side_effect=lambda name, password: command([name])):
                with self.assertRaises(M.Failure):
                    keychain.setup()
                self.assertTrue(keychain.restore())
            self.assertTrue(keychain.owned_create_attempted)
            self.assertFalse(keychain.path.exists())
            self.assertIn(["default-keychain", "-d", "user", "-s", Path("/original/default")], calls)
            self.assertEqual(evidence.cleanup_failures, [])

    def test_setup_failure_before_snapshot_never_mutates_preferences(self):
        keychain, _, evidence = self.instance(Path("/unused"))
        with patch.object(keychain, "command", side_effect=M.Failure("keychain_command")) as command:
            with self.assertRaises(M.Failure):
                keychain.setup()
            self.assertTrue(keychain.restore())
        self.assertEqual(command.call_count, 1)
        self.assertFalse(keychain.may_have_changed)
        self.assertEqual(evidence.cleanup_failures, [])


class GuardTests(unittest.TestCase):
    def test_windows_arm_is_rejected_before_any_root_or_process(self):
        environment = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
            "GITHUB_REPOSITORY": "brk275/Morpheus", "GITHUB_REF": "refs/heads/main",
            "GITHUB_SHA": "a" * 40, "RUNNER_ARCH": "ARM64"}
        with patch.dict(os.environ, environment, clear=True), patch.object(M.sys, "platform", "win32"), \
                patch.object(M, "Acceptance") as acceptance, patch("builtins.print") as output:
            self.assertEqual(M.main(["--binary", "/unused", "--binary-sha256", "0" * 64]), 1)
        acceptance.assert_not_called()
        self.assertIn("architecture_guard", output.call_args.args[0])
        self.assertNotIn("/unused", output.call_args.args[0])

    def test_local_execution_is_rejected_before_any_os_operation(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(M, "Acceptance") as acceptance, patch("builtins.print"):
            self.assertEqual(M.main(["--binary", "/unused", "--binary-sha256", "0" * 64]), 1)
        acceptance.assert_not_called()


class PhaseTests(unittest.TestCase):
    def fixture(self, root):
        instance = M.Acceptance.__new__(M.Acceptance)
        instance.root = root
        instance.binary, instance.digest = Path("/unused-binary"), "0" * 64
        instance.nonce = "1" * 64
        instance.account_a, instance.account_b = str(M.uuid.uuid4()), str(M.uuid.uuid4())
        instance.values = {}
        instance.runner = Mock(quiescent=True)
        instance.evidence = M.Evidence()
        return instance

    def test_native_first_failure_survives_deadline_and_unconfirmed_process_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); root.chmod(0o700)
            instance = self.fixture(root)
            def run(*args, **kwargs):
                path = root / "prepare-progress.json"
                path.write_text(json.dumps({"version": 1, "phase": "prepare", "status": "failed",
                    "stage": "policy_revocation", "failure": "private_original", "checks": {}})); path.chmod(0o600)
                instance.runner.quiescent = False
                raise M.Failure("process_deadline", cleanup=True)
            instance.runner.run.side_effect = run
            with patch.object(M, "executable_identity"):
                with self.assertRaises(M.Failure):
                    instance.phase("prepare")
            self.assertEqual(instance.evidence.first, {"stage": "policy_revocation", "code": "fixture_failure"})
            self.assertEqual(instance.evidence.cleanup_failures, ["children_unconfirmed"])
            self.assertNotIn("private_original", json.dumps(instance.evidence.phases))

    def test_zero_exit_without_result_is_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); root.chmod(0o700)
            instance = self.fixture(root)
            instance.runner.run.return_value = M.CommandResult(0, b"running 0 tests")
            with patch.object(M, "executable_identity"):
                with self.assertRaises(M.Failure):
                    instance.phase("prepare")
            self.assertEqual(instance.evidence.first, {"stage": "prepare", "code": "phase_receipt_io"})

    def test_stale_receipt_rejects_before_process(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); root.chmod(0o700)
            instance = self.fixture(root)
            (root / "prepare-result.json").write_text("{}"); (root / "prepare-result.json").chmod(0o600)
            with patch.object(M, "executable_identity"):
                with self.assertRaises(M.Failure) as failure:
                    instance.phase("prepare")
            self.assertEqual(failure.exception.code, "phase_replay")
            instance.runner.run.assert_not_called()




class SecretInputTests(unittest.TestCase):
    def test_security_uses_one_stdin_command_and_no_secret_in_arguments(self):
        runner = Mock(quiescent=True)
        runner.run.return_value = M.CommandResult(0, b"")
        keychain = M.MacKeychain(Path('/tmp/owned space/quote"/slash\\'), runner, M.Evidence())
        password = "a" * 64
        for operation in ("create-keychain", "unlock-keychain"):
            keychain.secret_command(operation, password)
            call = runner.run.call_args
            self.assertEqual(call.args[0], [M.SECURITY, "-q", "-i"])
            self.assertNotIn(password, repr(call.args))
            self.assertNotIn("environment", call.kwargs)
            payload = call.kwargs["payload"]
            self.assertEqual(payload.count(b"\n"), 1)
            self.assertTrue(payload.endswith(b"\n"))
            self.assertTrue(payload.startswith((operation + " -p " + password).encode()))
            self.assertIn(b'quote\\"', payload)
            self.assertIn(b'slash\\\\', payload)
            self.assertNotIn(b"quit", payload)

    def test_nonzero_single_command_cannot_pass(self):
        runner = Mock(quiescent=True)
        runner.run.return_value = M.CommandResult(1, b"private secret failure")
        keychain = M.MacKeychain(Path("/owned"), runner, M.Evidence())
        with self.assertRaises(M.Failure) as raised:
            keychain.secret_command("create-keychain", "a" * 64)
        self.assertEqual(raised.exception.code, "keychain_command")
        self.assertNotIn("private secret", str(raised.exception))

    def test_control_paths_and_multiple_commands_rejected_before_runner(self):
        runner = Mock(quiescent=True)
        for root in (Path("/bad\npath"), Path("/bad\rpath"), Path("/bad\x00path"), Path("/" + "x" * 4096)):
            keychain = M.MacKeychain(root, runner, M.Evidence())
            with self.assertRaises(M.Failure):
                keychain.setup()
            self.assertFalse(keychain.may_have_changed)
        runner.run.assert_not_called()
        with self.assertRaises(M.Failure):
            M.security_secret_payload("create-keychain\nquit", "a" * 64, Path("/owned"))


class DescendantTests(unittest.TestCase):
    def test_snapshot_accepts_only_anchored_zombie_group(self):
        self.assertFalse(M.group_has_live_members(b"12 Zs\n12 Z\n15 R\n", 12))
        self.assertTrue(M.group_has_live_members(b"12 Zs\n12 S+\n", 12))
        for raw in (b"15 Z\n", b"12 Z command arguments\n", b"12 \xff\n", b"12 Z\n" + b" " * 65536):
            with self.assertRaises(M.Failure):
                M.group_has_live_members(raw, 12)

    def test_waits_for_descendant_completion_before_reaping_leader(self):
        process = Mock(pid=12, returncode=None)
        events = []
        process.wait.side_effect = lambda **kwargs: (events.append("reap"), setattr(process, "returncode", -9))
        def snapshot():
            self.assertIsNone(process.returncode)
            events.append("snapshot")
            return b"12 Z\n12 S\n" if events.count("snapshot") == 1 else b"12 Z\n"
        with patch.object(M.package_smoke, "process_status", return_value=0), \
                patch.object(M.os, "killpg") as kill, \
                patch.object(M._MacSnapshotRunner, "snapshot", side_effect=snapshot), \
                patch.object(M.time, "sleep"):
            M.MacRunner().stop_owned(process)
        kill.assert_called_once_with(12, M.signal.SIGKILL)
        self.assertEqual(events, ["snapshot", "snapshot", "reap"])

    def test_unconfirmed_descendants_preserve_anchor_and_failure(self):
        process = Mock(pid=12, returncode=None)
        with patch.object(M.package_smoke, "process_status", return_value=0), \
                patch.object(M.os, "killpg"), \
                patch.object(M._MacSnapshotRunner, "snapshot", return_value=b"12 Z\n12 S\n"), \
                patch.object(M.time, "monotonic", side_effect=[0, 6]):
            with self.assertRaises(M.Failure) as raised:
                M.MacRunner().stop_owned(process)
        self.assertEqual(raised.exception.code, "process_descendants")
        process.wait.assert_not_called()

    def test_permission_error_is_not_success_without_snapshot(self):
        process = Mock(pid=12, returncode=None)
        process.wait.side_effect = lambda **kwargs: setattr(process, "returncode", 0)
        with patch.object(M.package_smoke, "process_status", return_value=0), \
                patch.object(M.os, "killpg", side_effect=PermissionError), \
                patch.object(M._MacSnapshotRunner, "snapshot", return_value=b"12 Z\n") as snapshot:
            M.MacRunner().stop_owned(process)
        snapshot.assert_called_once()
        process.wait.assert_called_once_with(timeout=5)

    def test_inspector_is_exact_system_ps_and_bounded_metadata_only(self):
        with patch.object(M.MacRunner, "run", return_value=M.CommandResult(0, b"12 Z\n")) as run:
            self.assertEqual(M._MacSnapshotRunner().snapshot(), b"12 Z\n")
        run.assert_called_once_with(["/bin/ps", "-axo", "pgid=,stat="], seconds=3, output_limit=65536)




class PipePumpTests(unittest.TestCase):
    def test_short_writes_complete_and_close_input_for_eof(self):
        pump = M._PipePump(10, 11, b"fixture", 16)
        with patch.object(M.os, "write", side_effect=[2, 5]) as write, patch.object(M.os, "close") as close:
            pump.write()
        self.assertEqual([call.args for call in write.call_args_list], [(10, b"fixture"), (10, b"xture")])
        self.assertTrue(pump.sent and pump.input_done.is_set())
        self.assertFalse(pump.input_error.is_set())
        close.assert_called_once_with(10)

    def test_write_failure_closes_owned_input_and_never_reports_sent(self):
        pump = M._PipePump(10, 11, b"fixture", 16)
        with patch.object(M.os, "write", side_effect=OSError("private")), patch.object(M.os, "close") as close:
            pump.write()
        self.assertFalse(pump.sent)
        self.assertTrue(pump.input_error.is_set() and pump.input_done.is_set())
        close.assert_called_once_with(10)

    def test_overflow_is_bounded_but_drain_continues_until_eof(self):
        pump = M._PipePump(10, 11, b"fixture", 3)
        with patch.object(M.os, "read", side_effect=[b"ab", b"cd", b"more-private", b""]) as read, patch.object(M.os, "close") as close:
            pump.read()
        self.assertEqual(pump.output, b"ab")
        self.assertTrue(pump.overflow.is_set() and pump.eof and pump.output_done.is_set())
        self.assertEqual(read.call_count, 4)
        close.assert_called_once_with(11)

    def test_finish_requires_every_started_pump_stopped(self):
        pump = M._PipePump(10, 11, b"", 1)
        pump.writer_started = pump.reader_started = True
        pump.writer = Mock(); pump.reader = Mock()
        pump.writer.is_alive.return_value = False
        pump.reader.is_alive.return_value = True
        self.assertFalse(pump.finish())
        pump.writer.join.assert_called_once_with(timeout=2)
        pump.reader.join.assert_called_once_with(timeout=2)


    def test_unknown_thread_start_never_claims_completion_or_closes_ambiguous_fd(self):
        pump=M._PipePump(10,11,b"fixture",16)
        pump.writer=Mock(); pump.reader=Mock()
        pump.writer.start.side_effect=KeyboardInterrupt
        with self.assertRaises(KeyboardInterrupt): pump.start()
        with patch.object(M.os,"close") as close:
            self.assertFalse(pump.finish())
        close.assert_not_called()


class WindowsProcessTests(unittest.TestCase):
    def run_mocked(self, *, deadline=False, overflow=False, cleanup_error=False, pump_stuck=False, spawn_error=False, late_input_error=False):
        from scripts import native_vault_windows as root_helper
        from scripts import run_windows_capture_acceptance as job_helper
        job = Mock()
        job.spawn.return_value = 100
        job.wait.return_value = 0
        if deadline:
            job.wait.side_effect = job_helper.Failure("deadline")
        if cleanup_error:
            job.close.side_effect = job_helper.Failure("cleanup")
        if spawn_error:
            job.spawn.side_effect = job_helper.Failure("job_assignment")
        pump = Mock(sent=True, eof=True, output=bytearray(b"private"))
        for event in ("input_error", "output_error"):
            getattr(pump, event).is_set.return_value = False
        if late_input_error:
            pump.input_error.is_set.side_effect=[False,True]
        pump.overflow.is_set.return_value = overflow
        for event in ("input_done", "output_done"):
            getattr(pump, event).is_set.return_value = not deadline
        pump.finish.return_value = not pump_stuck
        streams = [Mock(), Mock()]
        runner = M.WindowsRunner(Path("owned"))
        with patch.object(M.sys, "platform", "win32"), \
                patch.object(root_helper, "verify_private_root") as verify, \
                patch.object(job_helper, "WindowsJob", return_value=job), \
                patch.object(M.os, "pipe", side_effect=[(10,11),(12,13)]), \
                patch.object(M.os, "fdopen", side_effect=streams), \
                patch.object(M.os, "set_inheritable") as inherit, \
                patch.object(M.os, "get_inheritable", return_value=False), \
                patch.object(M.os, "close") as close, \
                patch.object(M, "_PipePump", return_value=pump), \
                patch.object(M.time, "monotonic", side_effect=[0,6] if deadline else [0]):
            try:
                result = runner.run(["fixture.exe"], payload=b"synthetic", seconds=5)
                failure = None
            except M.Failure as error:
                result, failure = None, error
        verify.assert_called_once_with(Path("owned"))
        self.assertEqual([call.args for call in inherit.call_args_list], [(10,False),(11,False),(12,False),(13,False)])
        job.close.assert_called_once()
        return runner, result, failure, job, pump, streams, close

    def test_private_pipes_use_stdin_seam_and_drain_job_before_pass(self):
        runner, result, failure, job, pump, streams, _ = self.run_mocked()
        self.assertIsNone(failure)
        self.assertEqual(result, M.CommandResult(0,b"private"))
        self.assertTrue(runner.quiescent)
        self.assertIs(job.spawn.call_args.kwargs["stdin_stream"], streams[0])
        self.assertIs(job.spawn.call_args.args[2], streams[1])
        streams[0].close.assert_called_once(); streams[1].close.assert_called_once()
        pump.finish.assert_called_once()

    def test_late_input_close_failure_cannot_pass_after_thread_join(self):
        runner, _, failure, _, _, _, _ = self.run_mocked(late_input_error=True)
        self.assertEqual(failure.code,"process_input")
        self.assertTrue(runner.quiescent)

    def test_deadline_survives_job_cleanup_failure(self):
        runner, _, failure, _, _, _, _ = self.run_mocked(deadline=True, cleanup_error=True)
        self.assertEqual(failure.code,"process_deadline")
        self.assertTrue(failure.cleanup)
        self.assertFalse(runner.quiescent)

    def test_output_overflow_or_lingering_pipe_cannot_pass(self):
        runner, _, failure, _, _, _, _ = self.run_mocked(overflow=True)
        self.assertEqual(failure.code,"process_output_bound")
        self.assertTrue(runner.quiescent)
        runner, _, failure, _, _, _, _ = self.run_mocked(pump_stuck=True)
        self.assertEqual(failure.code,"process_cleanup")
        self.assertFalse(runner.quiescent)

    def test_spawn_failure_closes_parent_pipe_ends_and_retains_job_cleanup(self):
        runner, _, failure, _, pump, streams, close = self.run_mocked(spawn_error=True)
        self.assertEqual(failure.code,"process_job")
        self.assertEqual([call.args[0] for call in close.call_args_list],[11,12])
        streams[0].close.assert_called_once(); streams[1].close.assert_called_once()
        pump.start.assert_not_called()
        self.assertTrue(runner.quiescent)


class WindowsRootLifecycleTests(unittest.TestCase):
    def instance(self, root):
        instance = M.Acceptance.__new__(M.Acceptance)
        instance.root, instance.parent = root, root.parent
        instance.nonce = "a" * 64
        instance.account_a, instance.account_b = "a", "b"
        instance.values = {}
        instance.runner = Mock(quiescent=True)
        instance.evidence = M.Evidence()
        instance.created = instance.root_verified = instance.phase_started = instance.owned_items_resolved = False
        instance.platform_restored = instance.profile_removed = False
        return instance

    def test_acl_creation_precedes_files_and_nonce_is_restored(self):
        from scripts import native_vault_windows as helper
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)/"fresh"
            instance = self.instance(root)
            events=[]
            def create(path):
                self.assertFalse(path.exists())
                self.assertEqual(os.environ["MORPHEUS_NATIVE_VAULT_ISOLATED"], instance.nonce)
                path.mkdir(); events.append("create")
            with patch.object(M.sys,"platform","win32"), patch.object(helper,"prepare_private_root",side_effect=create), \
                    patch.object(helper,"verify_private_root"), patch.object(M,"private_object"), \
                    patch.object(instance,"phase",side_effect=lambda name: events.append(name)), \
                    patch.dict(os.environ,{"MORPHEUS_NATIVE_VAULT_ISOLATED":"previous"}):
                instance.run()
                self.assertEqual(os.environ["MORPHEUS_NATIVE_VAULT_ISOLATED"],"previous")
            self.assertEqual(events,["create","prepare","verify_remove"])
            self.assertTrue(instance.profile_removed and instance.platform_restored)

    def test_partial_acl_failure_allows_only_empty_rmdir(self):
        from scripts import native_vault_windows as helper
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)/"fresh"; instance=self.instance(root)
            def create(path):
                path.mkdir(); (path/"unexpected").touch()
                raise helper.NativeVaultWindowsError("verify",created=True)
            with patch.object(M.sys,"platform","win32"), patch.object(helper,"prepare_private_root",side_effect=create), \
                    patch.object(M.shutil,"rmtree") as remove:
                with self.assertRaises(M.Failure): instance.run()
            remove.assert_not_called()
            self.assertTrue((root/"unexpected").exists())
            self.assertIn("root_cleanup",instance.evidence.cleanup_failures)

    def test_unconfirmed_owned_items_leave_journal_and_profile(self):
        from scripts import native_vault_windows as helper
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)/"fresh"; instance=self.instance(root)
            def create(path): path.mkdir()
            def fail(name):
                instance.phase_started=True
                raise M.Failure("fixture_exit")
            with patch.object(M.sys,"platform","win32"), patch.object(helper,"prepare_private_root",side_effect=create), \
                    patch.object(instance,"phase",side_effect=fail), patch.object(M.shutil,"rmtree") as remove:
                with self.assertRaises(M.Failure): instance.run()
            remove.assert_not_called()
            self.assertTrue(root.exists())
            self.assertFalse(instance.platform_restored)
            self.assertIn("owned_items_unconfirmed",instance.evidence.cleanup_failures)


if __name__ == "__main__":
    unittest.main()
