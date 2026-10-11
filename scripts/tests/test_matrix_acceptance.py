import os
import contextlib
import io
import json
from pathlib import Path
import re
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch
import subprocess

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from run_matrix_acceptance import Acceptance, OAUTH_PHASES, fixture_from_output, oauth_diagnostics, oauth_evidence, passed_tests
import run_matrix_acceptance as acceptance_module
from run_element_acceptance import CHECKS as ELEMENT_CHECKS


class StartupOwnershipTests(unittest.TestCase):
    @contextlib.contextmanager
    def preflight(self, responses):
        visible = io.StringIO()
        runner = Mock()
        with (patch.object(acceptance_module, "Acceptance", return_value=runner,
                           side_effect=RuntimeError("reached acceptance creation")) as factory,
              patch.object(acceptance_module.shutil, "which", return_value="mock-command"),
              patch.object(acceptance_module.subprocess, "run", side_effect=responses) as inventory,
              patch.object(acceptance_module.tempfile, "mkdtemp", return_value="/private/preflight-root") as temporary,
              patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner"]),
              patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "DBUS_SESSION_BUS_ADDRESS": "mock"}),
              contextlib.redirect_stdout(visible), contextlib.redirect_stderr(visible)):
            yield factory, runner, inventory, temporary, visible

    def test_each_preexisting_owned_resource_rejects_ownership_without_cleanup(self):
        for index, names in enumerate((acceptance_module.FIXTURE_CONTAINERS,
                                       acceptance_module.FIXTURE_VOLUMES,
                                       (acceptance_module.FIXTURE_NETWORK,))):
            for name in names:
                results = [subprocess.CompletedProcess([], 0, "", "") for _ in range(index)]
                results.append(subprocess.CompletedProcess([], 0, name + "\nprivate-unrelated\n", "private-diagnostic"))
                with self.subTest(kind=index, resource=name), self.preflight(results) as (factory, runner, _, temporary, visible):
                    with self.assertRaisesRegex(RuntimeError, "Pre-existing Matrix fixture found") as caught:
                        acceptance_module.main()
                factory.assert_not_called()
                temporary.assert_not_called()
                runner.script.assert_not_called()
                self.assertNotIn("private", visible.getvalue() + str(caught.exception))

    def test_query_failure_or_timeout_rejects_ownership_without_cleanup(self):
        failures = (
            subprocess.CompletedProcess([], 1, "private-inventory", "private-daemon-error"),
            OSError("private-socket-error"),
            subprocess.TimeoutExpired(["private-command"], 30, output="private-output", stderr="private-error"),
            UnicodeDecodeError("utf-8", b"private-\xff", 8, 9, "private-decoding-error"),
        )
        for index in range(3):
            for failure in failures:
                results = [subprocess.CompletedProcess([], 0, "", "") for _ in range(index)] + [failure]
                with self.subTest(kind=index, failure=type(failure).__name__), self.preflight(results) as (factory, runner, inventory, temporary, visible):
                    with self.assertRaisesRegex(RuntimeError, "Could not verify absence") as caught:
                        acceptance_module.main()
                factory.assert_not_called()
                temporary.assert_not_called()
                runner.script.assert_not_called()
                self.assertEqual(inventory.call_count, index + 1)
                self.assertNotIn("private", visible.getvalue() + str(caught.exception))

    def test_only_checked_absence_allows_creation_and_uses_bounded_private_name_queries(self):
        results = [subprocess.CompletedProcess([], 0, "private-unrelated-resource\n", "") for _ in range(3)]
        with self.preflight(results) as (factory, _, inventory, temporary, visible):
            with self.assertRaisesRegex(RuntimeError, "reached acceptance creation"):
                acceptance_module.main()
        factory.assert_called_once()
        temporary.assert_called_once()
        self.assertEqual([item.args[0] for item in inventory.call_args_list], [
            ["docker", "container", "ls", "--all", "--format", "{{.Names}}"],
            ["docker", "volume", "ls", "--format", "{{.Name}}"],
            ["docker", "network", "ls", "--format", "{{.Name}}"],
        ])
        self.assertTrue(all(item.kwargs == {"capture_output": True, "text": True, "timeout": 30}
                            for item in inventory.call_args_list))
        self.assertEqual(visible.getvalue(), "")


class FinalizationTests(unittest.TestCase):
    @contextlib.contextmanager
    def mocked_main(self, *, body_error=None, summary_error=None, cleanup_errors=None,
                    absence_error=None, stderr=None):
        cleanup_errors = cleanup_errors or {}
        runner = Mock()
        runner.group.side_effect = lambda _labels, action: action()
        runner.start_keyring.side_effect = body_error
        runner.summary.side_effect = summary_error
        runner.verify_fixture_absence.side_effect = absence_error

        def script(name, command):
            if command == "down" and name in cleanup_errors:
                raise cleanup_errors[name]
            return "private-fixture"

        runner.script.side_effect = script
        visible = io.StringIO() if stderr is None else stderr
        with tempfile.TemporaryDirectory() as folder, contextlib.ExitStack() as stack:
            stack.enter_context(patch.object(acceptance_module, "Acceptance", return_value=runner))
            stack.enter_context(patch.object(acceptance_module.shutil, "which", return_value="mock-command"))
            stack.enter_context(patch.object(acceptance_module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "")))
            stack.enter_context(patch.object(acceptance_module.tempfile, "mkdtemp", return_value=folder))
            stack.enter_context(patch.object(acceptance_module, "fixture_from_output", return_value=Path(folder) / "fixture.json"))
            stack.enter_context(patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner"]))
            stack.enter_context(patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "DBUS_SESSION_BUS_ADDRESS": "mock"}))
            stack.enter_context(contextlib.redirect_stderr(visible))
            yield runner, visible

    def assert_both_cleanups(self, runner):
        self.assertEqual(
            [item.args for item in runner.script.call_args_list if item.args[1] == "down"],
            [("local_matrix_rtc", "down"), ("local_matrix_env", "down")],
        )
        runner.verify_fixture_absence.assert_called_once_with()
        self.assertEqual(runner.mock_calls[-1][0], "verify_fixture_absence")

    def test_successful_body_and_cleanup_return_success(self):
        with self.mocked_main() as (runner, visible):
            self.assertIsNone(acceptance_module.main())
        self.assert_both_cleanups(runner)
        self.assertEqual(visible.getvalue(), "")

    def test_outgoing_ring_failure_keeps_original_failure_and_cleans_both_services(self):
        original = RuntimeError("fixed outgoing ring failure")
        with self.mocked_main() as (runner, _):
            runner.outgoing_ring_cases.side_effect = original
            with self.assertRaises(RuntimeError) as caught:
                acceptance_module.main()
        self.assertIs(caught.exception, original)
        runner.rtc_cases.assert_not_called()
        runner.element_case.assert_not_called()
        self.assert_both_cleanups(runner)

    def test_known_chat_or_late_join_failure_does_not_starve_independent_element_check(self):
        for failed in ("live_local", "local_rtc-late-join", "local_rtc-sticky-2025-late-join"):
            with self.subTest(failed=failed), tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
                collector = Acceptance(Path(folder), "scanner")
                with self.mocked_main() as (runner, _):
                    runner.group.side_effect = collector.group
                    runner.finish_checks.side_effect = collector.finish_checks
                    runner.rtc_cases.side_effect = lambda: Acceptance.rtc_cases(runner)
                    def test(name, _fixture, **kwargs):
                        extra = kwargs.get("extra", {})
                        label = name + ("-sticky-2025" if extra.get("MORPHEUS_RTC_TEST_MODE") == "sticky-2025" else "")
                        label += "-late-join" if extra.get("MORPHEUS_RTC_LATE_JOIN") else ""
                        if label == failed:
                            raise acceptance_module.CheckFailure(label, "test_failed")
                    runner.test.side_effect = test
                    with self.assertRaisesRegex(RuntimeError, "failed or unexecuted"):
                        acceptance_module.main()
                runner.oauth_cases.assert_called_once()
                runner.outgoing_ring_cases.assert_called_once()
                runner.element_case.assert_called_once()
                self.assert_both_cleanups(runner)
                self.assertEqual(collector.failures, [{"check": failed, "reason": "test_failed"}])
                if failed == "live_local":
                    self.assertIn("drafts_local", [item.args[0] for item in runner.test.call_args_list])
                    self.assertIn("recovery_local", [item.args[0] for item in runner.test.call_args_list])

    def test_chat_assertion_failures_collect_independent_cases_and_keep_exact_prerequisite(self):
        for failed in acceptance_module.CHAT_CHECKS:
            with self.subTest(failed=failed), tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
                collector = Acceptance(Path(folder), "scanner")
                # Other groups are successful in this isolated orchestration
                # test, so finish_checks must reject the known chat failure.
                collector.results = [{"check": label} for label in acceptance_module.PLANNED_CHECKS
                                     if label not in acceptance_module.CHAT_CHECKS]
                visited = []
                with self.mocked_main() as (runner, _):
                    runner.not_run = collector.not_run
                    runner.group.side_effect = collector.group
                    runner.finish_checks.side_effect = collector.finish_checks
                    def test(name, _fixture, **kwargs):
                        label = name + ("-classic" if kwargs.get("extra", {}).get("MORPHEUS_MATRIX_SEARCH_CLASSIC") else "")
                        if label not in acceptance_module.CHAT_CHECKS:
                            return
                        visited.append(label)
                        if label == failed:
                            raise acceptance_module.CheckFailure(label, "test_failed")
                        collector.results.append({"check": label})
                    runner.test.side_effect = test
                    with self.assertRaisesRegex(RuntimeError, "failed or unexecuted"):
                        acceptance_module.main()
                expected = [name for name in acceptance_module.CHAT_CHECKS
                            if not (failed == "formatted_local" and name == "moderation_local")]
                self.assertEqual(visited, expected)
                self.assertEqual(collector.failures, [{"check": failed, "reason": "test_failed"}])
                self.assertEqual(collector.not_run, {"moderation_local": "dependency_failed"}
                                 if failed == "formatted_local" else {})
                runner.oauth_cases.assert_called_once()
                runner.outgoing_ring_cases.assert_called_once()
                runner.element_case.assert_called_once()
                self.assert_both_cleanups(runner)

    def test_chat_infrastructure_failure_remains_fatal_before_later_cases(self):
        for message in ("credential scanner failed", "containment failed", "owned process cleanup failed"):
            original = RuntimeError(message)
            with self.subTest(failure=message), tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
                collector = Acceptance(Path(folder), "scanner")
                with self.mocked_main() as (runner, _):
                    runner.group.side_effect = collector.group
                    def test(name, _fixture, **_kwargs):
                        if name == "formatted_local":
                            raise original
                    runner.test.side_effect = test
                    with self.assertRaises(RuntimeError) as caught:
                        acceptance_module.main()
                self.assertIs(caught.exception, original)
                self.assertEqual([item.args[0] for item in runner.test.call_args_list],
                                 list(acceptance_module.CHAT_TESTS[:4]))
                runner.oauth_cases.assert_not_called()
                runner.outgoing_ring_cases.assert_not_called()
                self.assert_both_cleanups(runner)
                self.assertEqual(collector.failures, [])

    def test_chat_budget_exhaustion_does_not_start_more_commands_or_extend_deadline(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
            collector = Acceptance(Path(folder), "scanner")
            deadline = collector.deadline
            with self.mocked_main() as (runner, _), patch.object(collector, "available", return_value=0):
                runner.not_run = collector.not_run
                runner.group.side_effect = collector.group
                runner.finish_checks.side_effect = collector.finish_checks
                with self.assertRaisesRegex(RuntimeError, "failed or unexecuted"):
                    acceptance_module.main()
            runner.test.assert_not_called()
            self.assertEqual(collector.deadline, deadline)
            self.assertEqual(set(collector.not_run), set(acceptance_module.PLANNED_CHECKS))
            self.assertEqual(collector.not_run["moderation_local"], "dependency_failed")
            self.assertTrue(all(reason == "budget_spent" for label, reason in collector.not_run.items()
                                if label != "moderation_local"))
            self.assert_both_cleanups(runner)

    def test_each_cleanup_failure_is_fatal_after_both_attempts_and_never_formats_payload(self):
        class UnprintableFailure(RuntimeError):
            def __str__(self):
                raise AssertionError("private exception must not be formatted")

        for names in (("local_matrix_rtc",), ("local_matrix_env",),
                      ("local_matrix_rtc", "local_matrix_env")):
            with self.subTest(names=names), self.mocked_main(
                    cleanup_errors={name: UnprintableFailure() for name in names}) as (runner, visible):
                with self.assertRaisesRegex(RuntimeError, "service cleanup did not complete"):
                    acceptance_module.main()
            self.assert_both_cleanups(runner)
            self.assertEqual(visible.getvalue(), "".join(f"Cleanup incomplete: {name}\n" for name in names))

    def test_original_body_exception_or_interrupt_survives_summary_and_cleanup_failures(self):
        for original in (RuntimeError("original body failure"), KeyboardInterrupt()):
            with self.subTest(kind=type(original).__name__), self.mocked_main(
                    body_error=original, summary_error=OSError("private-summary"),
                    cleanup_errors={"local_matrix_rtc": KeyboardInterrupt(),
                                    "local_matrix_env": RuntimeError("private-cleanup")},
                    absence_error=RuntimeError("private inventory")) as (runner, visible):
                with self.assertRaises(type(original)) as caught:
                    acceptance_module.main()
            self.assertIs(caught.exception, original)
            self.assert_both_cleanups(runner)
            self.assertNotIn("private", visible.getvalue())
            self.assertIn("Acceptance summary could not be written", visible.getvalue())

    def test_summary_failure_stays_distinct_and_still_cleans_both_services(self):
        with self.mocked_main(summary_error=OSError("private-summary")) as (runner, visible):
            with self.assertRaisesRegex(RuntimeError, "summary could not be written"):
                acceptance_module.main()
        self.assert_both_cleanups(runner)
        self.assertEqual(visible.getvalue(), "Acceptance summary could not be written\n")

    def test_cleanup_interrupt_does_not_skip_second_service(self):
        with self.mocked_main(cleanup_errors={"local_matrix_rtc": KeyboardInterrupt()}) as (runner, _):
            with self.assertRaisesRegex(RuntimeError, "service cleanup did not complete"):
                acceptance_module.main()
        self.assert_both_cleanups(runner)

    def test_unverified_absence_is_fatal_even_when_both_down_commands_succeed(self):
        with self.mocked_main(absence_error=RuntimeError("private daemon detail")) as (runner, visible):
            with self.assertRaisesRegex(RuntimeError, "service cleanup did not complete"):
                acceptance_module.main()
        self.assert_both_cleanups(runner)
        self.assertEqual(visible.getvalue(), "Cleanup incomplete: fixture_absence\n")

    def test_broken_diagnostic_stream_does_not_replace_original_failure(self):
        original = RuntimeError("original body failure")
        closed = io.StringIO()
        closed.close()
        with self.mocked_main(body_error=original, summary_error=OSError("private-summary"),
                              cleanup_errors={"local_matrix_rtc": RuntimeError("private-cleanup")},
                              stderr=closed) as (runner, _):
            with self.assertRaises(RuntimeError) as caught:
                acceptance_module.main()
        self.assertIs(caught.exception, original)
        self.assert_both_cleanups(runner)


class FixtureAbsenceTests(unittest.TestCase):
    def test_clean_checked_inventories_ignore_unowned_resources_without_printing_them(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            with (patch.object(runner, "run", side_effect=["private-unrelated-container\n", "private-unrelated-network\n"]) as run,
                  contextlib.redirect_stdout(visible), contextlib.redirect_stderr(visible)):
                runner.verify_fixture_absence()
        self.assertEqual(visible.getvalue(), "")
        self.assertEqual([item.args[1] for item in run.call_args_list], [
            ["docker", "container", "ls", "--all", "--format", "{{.Names}}"],
            ["docker", "network", "ls", "--format", "{{.Name}}"],
        ])
        self.assertTrue(all(item.kwargs == {"seconds": 30, "cleanup": True} for item in run.call_args_list))

    def test_every_owned_container_or_network_remaining_rejects_cleanup(self):
        cases = [(name + "\n", "") for name in acceptance_module.FIXTURE_CONTAINERS]
        cases.append(("", acceptance_module.FIXTURE_NETWORK + "\n"))
        for outputs in cases:
            with self.subTest(outputs=outputs), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with patch.object(runner, "run", side_effect=outputs) as run:
                    with self.assertRaisesRegex(RuntimeError, "resources remain"):
                        runner.verify_fixture_absence()
                self.assertEqual(run.call_count, 2)

    def test_failed_inventory_cannot_be_treated_as_empty_and_both_queries_are_attempted(self):
        for outputs in ((RuntimeError("private daemon output"), ""),
                        ("", OSError("private Docker socket")),
                        (RuntimeError("private daemon output"), OSError("private Docker socket"))):
            with self.subTest(failed_queries=sum(isinstance(item, BaseException) for item in outputs)), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                visible = io.StringIO()
                with (patch.object(runner, "run", side_effect=outputs) as run,
                      contextlib.redirect_stdout(visible), contextlib.redirect_stderr(visible),
                      self.assertRaisesRegex(RuntimeError, "absence could not be verified") as caught):
                    runner.verify_fixture_absence()
                self.assertEqual(run.call_count, 2)
                self.assertNotIn("private", visible.getvalue() + str(caught.exception))


class LiveStageTests(unittest.TestCase):
    def test_stalls_and_fixed_stages_never_reflect_unknown_values(self):
        self.assertEqual(acceptance_module.live_diagnostics("private compiler diagnostic\n"),
                         {"test_entered": False})
        self.assertEqual(acceptance_module.live_diagnostics(
            "MORPHEUS_LIVE_STAGE fixture\nMORPHEUS_LIVE_STAGE stop_primary\n"
            "MORPHEUS_LIVE_STAGE private_token\nMORPHEUS_LIVE_STAGE https://private.example\n"),
            {"test_entered": True, "stage": "stop_primary"})

    def test_source_emits_exact_allowlisted_stage_identifiers(self):
        source = (Path(__file__).resolve().parents[2]
                  / "crates/morpheus-matrix/tests/live_local.rs").read_text()
        emitted = re.findall(r"phase!\(([a-z_]+)\)", source)
        self.assertEqual(set(emitted), acceptance_module.LIVE_STAGES)
        self.assertEqual(len(emitted), len(set(emitted)))
        self.assertEqual(emitted[0], "fixture")
        self.assertEqual(emitted[-1], "complete")

    @contextlib.contextmanager
    def child(self, runner, output, *, timeout=False, scanner_code=0):
        process = Mock(returncode=0, pid=123456)
        if timeout:
            process.communicate.side_effect = subprocess.TimeoutExpired("mock", 1)
        visible = io.StringIO()
        def start(_args, **kwargs):
            kwargs["stdout"].write(output.encode())
            return process
        with (patch.object(acceptance_module.subprocess, "Popen", side_effect=start),
              patch.object(acceptance_module.subprocess, "run",
                           return_value=subprocess.CompletedProcess([], scanner_code)),
              patch.object(acceptance_module.os, "killpg") as kill,
              contextlib.redirect_stdout(visible)):
            yield visible, kill

    def test_timeout_publishes_only_fixed_last_stage_after_private_scan(self):
        output = ("MORPHEUS_LIVE_STAGE fixture\nMORPHEUS_LIVE_STAGE stop_primary\n"
                  "private-server-response\n")
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with self.child(runner, output, timeout=True) as (visible, kill):
                with self.assertRaisesRegex(RuntimeError, "deadline"):
                    runner.run("live_local", ["mock-test"], test=True, seconds=1)
            kill.assert_called_once_with(123456, acceptance_module.signal.SIGKILL)
            self.assertIn('"stage": "stop_primary"', visible.getvalue())
            self.assertIn('"failure": "deadline"', visible.getvalue())
            self.assertNotIn("private-server", visible.getvalue())
            self.assertEqual(runner.results, [])

    def test_rejected_log_scan_prevents_stage_publication(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with self.child(runner, "MORPHEUS_LIVE_STAGE fixture\n", scanner_code=1) as (visible, _):
                with self.assertRaisesRegex(RuntimeError, "credential scanning"):
                    runner.run("live_local", ["mock-test"], test=True)
            self.assertNotIn("live_diagnostics", visible.getvalue())
            self.assertEqual(runner.results, [])

    def test_success_requires_entered_and_completed_real_test(self):
        result = "test result: ok. 1 passed; 0 failed; 0 ignored;\n"
        for stages, accepted in (("", False), ("MORPHEUS_LIVE_STAGE complete\n", False),
                                 ("MORPHEUS_LIVE_STAGE fixture\n", False),
                                 ("MORPHEUS_LIVE_STAGE fixture\nMORPHEUS_LIVE_STAGE complete\n", True)):
            with self.subTest(stages=stages), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with self.child(runner, stages + result) as (visible, _):
                    if accepted:
                        runner.run("live_local", ["mock-test"], test=True)
                    else:
                        with self.assertRaisesRegex(RuntimeError, "completion evidence"):
                            runner.run("live_local", ["mock-test"], test=True)
                self.assertEqual(len(runner.results), int(accepted))


class LiveEvidenceTests(unittest.TestCase):
    def test_rtc_bootstrap_follows_independent_checks_and_failure_stays_fatal(self):
        for failed in (False, True):
            with self.subTest(rtc_fails=failed), tempfile.TemporaryDirectory() as folder:
                runner = Mock()
                runner.group.side_effect = lambda _labels, action: action()
                def script(name, command):
                    if failed and (name, command) == ("local_matrix_rtc", "up"):
                        raise RuntimeError("fixed RTC bootstrap failure")
                    return "private-fixture"
                runner.script.side_effect = script
                with (patch.object(acceptance_module, "Acceptance", return_value=runner),
                      patch.object(acceptance_module.shutil, "which", return_value="mock-command"),
                      patch.object(acceptance_module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "")),
                      patch.object(acceptance_module.tempfile, "mkdtemp", return_value=folder),
                      patch.object(acceptance_module, "fixture_from_output", return_value=Path(folder) / "fixture.json"),
                      patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner"]),
                      patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "DBUS_SESSION_BUS_ADDRESS": "mock"})):
                    if failed:
                        with self.assertRaisesRegex(RuntimeError, "fixed RTC bootstrap failure"):
                            acceptance_module.main()
                    else:
                        acceptance_module.main()
                calls = runner.mock_calls
                from unittest.mock import call
                rtc_up = calls.index(call.script("local_matrix_rtc", "up"))
                self.assertLess(calls.index(call.oauth_cases()), rtc_up)
                for name in (*acceptance_module.CHAT_TESTS, "recovery_local"):
                    indexes = [i for i, item in enumerate(calls) if item[0] == "test" and item.args[0] == name]
                    self.assertTrue(indexes)
                    self.assertTrue(all(i < rtc_up for i in indexes))
                self.assertEqual(calls[-4:], [call.summary(), call.script("local_matrix_rtc", "down"),
                                             call.script("local_matrix_env", "down"), call.verify_fixture_absence()])
                if failed:
                    runner.outgoing_ring_cases.assert_not_called()
                    runner.rtc_cases.assert_not_called()
                    runner.element_case.assert_not_called()
                    self.assertNotIn(call.script("local_matrix_rtc", "probe"), calls)
                else:
                    probe = calls.index(call.script("local_matrix_rtc", "probe"))
                    gate = next(i for i, item in enumerate(calls)
                                if item[0] == "test" and item.args[0] == "unsupported_transport_local")
                    ring = calls.index(call.outgoing_ring_cases())
                    self.assertLess(probe, gate)
                    self.assertLess(gate, ring)
                    self.assertLess(ring, calls.index(call.rtc_cases()))
                    self.assertLess(calls.index(call.rtc_cases()), calls.index(call.element_case()))

    def test_element_containment_is_required_before_any_fixture_or_peer(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with patch.object(runner, "run", side_effect=RuntimeError("containment")) as run, \
                    patch.object(runner, "script") as script, self.assertRaisesRegex(RuntimeError, "containment"):
                runner.element_case()
            self.assertEqual(run.call_args.args[0], "acceptance-process-containment")
            script.assert_not_called()

    def test_contained_timeout_uses_unreaped_anchor_cleanup_without_communicate(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            runner.containment_verified = True
            process = Mock(returncode=None)
            def cleanup(child):
                self.assertIs(child, process)
                self.assertIsNone(child.returncode)
                child.returncode = -9
            with patch("run_matrix_acceptance.namespace_command", return_value=["namespace-wrapper"]) as command, \
                    patch("run_matrix_acceptance.subprocess.Popen", return_value=process) as popen, \
                    patch("run_matrix_acceptance.wait_owned", side_effect=subprocess.TimeoutExpired("contained", 1)), \
                    patch("run_matrix_acceptance.stop_owned", side_effect=cleanup) as stop, \
                    patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), \
                    contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(RuntimeError, "deadline"):
                runner.run("element-timeout", ["original"], seconds=1, pid_namespace=True, element_checks=True)
            command.assert_called_once()
            stop.assert_called_once_with(process)
            process.communicate.assert_not_called()
            self.assertEqual(popen.call_args.kwargs["stdin"], subprocess.DEVNULL)
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            self.assertEqual(runner.results, [])

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_matrix_fixture_failure_publishes_only_fixed_stage_and_status(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            output = "\n".join((
                "MORPHEUS_MATRIX_FIXTURE_STAGE generate_a",
                "MORPHEUS_MATRIX_FIXTURE_FAILURE docker_registry",
                "MORPHEUS_MATRIX_FIXTURE_NUMBER docker_exit=125",
                "MORPHEUS_MATRIX_FIXTURE_STAGE opaque_credential",
                "MORPHEUS_MATRIX_FIXTURE_NUMBER http_status=999",
                "private-password-and-url",
            ))
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(visible), self.assertRaises(RuntimeError):
                runner.run("local_matrix_env-up", [sys.executable, "-c", f"print({output!r}); raise SystemExit(1)"])
            self.assertIn('"matrix_fixture_diagnostics": {"stage": "generate_a", "failure": "docker_registry", "docker_exit": 125}', visible.getvalue())
            for private in ("opaque_credential", "private-password", "http_status", "999"):
                self.assertNotIn(private, visible.getvalue())
            self.assertEqual(runner.results, [])

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_element_success_requires_all_protocol_facts_without_leaking_child_text(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            facts = "\n".join(f"MORPHEUS_ELEMENT_CHECK {name}=true" for name in ELEMENT_CHECKS)
            visible = io.StringIO()
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(visible):
                runner.run("element-call-rc-sticky2025-presynced", [sys.executable, "-c", f"print({facts!r}); print('opaque-private-value')"], element_checks=True)
            self.assertEqual(runner.results[0]["element_checks"], dict.fromkeys(ELEMENT_CHECKS, True))
            self.assertEqual(runner.results[0]["element_peer"]["baseline"], "presynced")
            self.assertNotIn("opaque-private-value", visible.getvalue())
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(RuntimeError):
                runner.run("element-incomplete", [sys.executable, "-c", "print('MORPHEUS_ELEMENT_CHECK browser_roster=true')"], element_checks=True)
            self.assertEqual(len(runner.results), 1)

    def test_summary_does_not_claim_element_ran_before_its_check(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "unused-scanner")
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(root / "summary.md")}):
                runner.summary()
            self.assertNotIn("Element peer passed", (root / "summary.md").read_text())

    def test_requires_tests_to_run(self):
        for output in ("", "test result: ok. 0 passed; 0 failed; 0 ignored;",
                       "test result: ok. 1 passed; 0 failed; 1 ignored;",
                       "Skipped local RTC test\ntest result: ok. 1 passed; 0 failed; 0 ignored;"):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                passed_tests(output)

    def test_counts_real_results(self):
        self.assertEqual(passed_tests("test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured;"), 2)

    def test_oauth_evidence_requires_each_stage_and_never_overwrites_failure(self):
        required = OAUTH_PHASES["login_refresh"]
        complete = "\n".join(f"MORPHEUS_OAUTH_CHECK {name}=true" for name in required)
        self.assertEqual(oauth_evidence(complete, required), dict.fromkeys(required, True))
        for output in ("", complete.replace("login=true", "login=opaque-credential"),
                       "MORPHEUS_OAUTH_CHECK login=false\n" + complete,
                       complete + "\nMORPHEUS_OAUTH_CHECK login=false"):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                oauth_evidence(output, required)
        self.assertEqual(oauth_evidence("MORPHEUS_OAUTH_CHECK access_token=true\n"
                                       "MORPHEUS_OAUTH_CHECK login=opaque-credential\n"
                                       "MORPHEUS_OAUTH_CHECK login=true trailing-data"), {})

    def test_oauth_diagnostics_allow_only_fixed_stage_and_failure_codes(self):
        output = "\n".join((
            "MORPHEUS_OAUTH_STAGE login_begin", "MORPHEUS_OAUTH_STAGE login_finish",
            "MORPHEUS_OAUTH_STAGE opaque_credential", "MORPHEUS_OAUTH_STAGE login_finish?code=opaque",
            "MORPHEUS_OAUTH_FAILURE rust_acceptance", "MORPHEUS_OAUTH_FAILURE opaque_credential",
            "MORPHEUS_OAUTH_FAILURE fixture_operation trailing-secret",
        ))
        self.assertEqual(oauth_diagnostics(output), {"stage": "login_finish", "failure": "rust_acceptance"})
        self.assertEqual(oauth_diagnostics("MORPHEUS_OAUTH_STAGE password_value\n"
                                           "MORPHEUS_OAUTH_FAILURE https://example.org/private"), {})

    def test_browser_diagnostics_are_preserved_through_all_three_allowlists(self):
        import local_matrix_auth
        from run_matrix_acceptance import OAUTH_FAILURES
        source = (Path(__file__).resolve().parents[2] / "crates/morpheus-matrix/tests/oauth_local.rs").read_text()
        rust_list = source.split("const BROWSER_FAILURES: &[&str] = &[", 1)[1].split("];", 1)[0]
        rust_codes = set(re.findall(r'"([a-z_]+)"', rust_list))
        helper_codes = {failure.value for failure in local_matrix_auth.Failure
                        if failure.value.startswith("browser_")}
        self.assertLessEqual(helper_codes, rust_codes)
        self.assertLessEqual(rust_codes, OAUTH_FAILURES)
        for code in rust_codes:
            self.assertEqual(oauth_diagnostics("MORPHEUS_OAUTH_STAGE browser_callback\n"
                                              f"MORPHEUS_OAUTH_FAILURE {code}\n"
                                              f"MORPHEUS_OAUTH_FAILURE {code} private-code\n"),
                             {"stage": "browser_callback", "failure": code})

    def test_oauth_logout_diagnostics_and_evidence_are_exact_and_complete(self):
        self.assertEqual(oauth_diagnostics("MORPHEUS_OAUTH_STAGE logout_remote\n"
                                          "MORPHEUS_OAUTH_FAILURE logout_remote\n"
                                          "MORPHEUS_OAUTH_FAILURE logout_remote private-token\n"),
                         {"stage": "logout_remote", "failure": "logout_remote"})
        required = OAUTH_PHASES["restore_refresh"]
        complete = "\n".join(f"MORPHEUS_OAUTH_CHECK {name}=true" for name in required)
        self.assertEqual(oauth_evidence(complete, required), dict.fromkeys(required, True))
        for name in ("remote_logout", "account_removed", "vault_removed", "logout"):
            with self.subTest(name=name), self.assertRaises(RuntimeError):
                oauth_evidence(complete.replace(f"MORPHEUS_OAUTH_CHECK {name}=true", ""), required)

    def test_moderation_diagnostics_accept_only_fixed_facts_and_preserve_false(self):
        output = "\n".join((
            "MORPHEUS_MODERATION_STAGE invite_initial",
            "MORPHEUS_MODERATION_STAGE invite_after_kick",
            "MORPHEUS_MODERATION_STAGE private_account",
            "MORPHEUS_MODERATION_FAILURE history_sender_unverified",
            "MORPHEUS_MODERATION_FAILURE private_server_response",
            "MORPHEUS_MODERATION_FAILURE sdk trailing-private-data",
            "MORPHEUS_MODERATION_CHECK reinvite_identity_unverified=false",
            "MORPHEUS_MODERATION_CHECK reinvite_identity_unverified=true",
            "MORPHEUS_MODERATION_CHECK private_account=true",
            "MORPHEUS_MODERATION_CHECK reinvite_after_kick=true trailing-private-data",
        ))
        self.assertEqual(acceptance_module.moderation_diagnostics(output), {
            "stage": "invite_after_kick", "failure": "history_sender_unverified",
            "checks": {"reinvite_identity_unverified": False},
        })

    def test_moderation_success_requires_actual_unverified_reinvites_and_unchanged_trust(self):
        output = "\n".join(f"MORPHEUS_MODERATION_CHECK {name}=true"
                           for name in acceptance_module.MODERATION_CHECKS)
        self.assertEqual(acceptance_module.moderation_evidence(output),
                         dict.fromkeys(acceptance_module.MODERATION_CHECKS, True))
        for invalid in ("", output.replace("reinvite_identity_unverified=true", "reinvite_identity_unverified=false"),
                        output + "\nMORPHEUS_MODERATION_FAILURE sdk",
                        "MORPHEUS_MODERATION_CHECK reinvite_trust_unchanged=false\n" + output):
            with self.subTest(output=invalid), self.assertRaisesRegex(RuntimeError, "Moderation acceptance evidence"):
                acceptance_module.moderation_evidence(invalid)

    def test_every_room_settings_fact_is_required_and_failed_facts_cannot_be_repaired_by_later_true(self):
        complete = "\n".join(f"MORPHEUS_MODERATION_CHECK {name}=true"
                             for name in acceptance_module.MODERATION_CHECKS)
        settings = {
            "settings_permission_denial", "settings_avatar_roundtrip", "settings_encrypted_preview_opt_in",
            "settings_avatar_remove", "settings_stale_write_rejected", "settings_access_rules_roundtrip",
            "settings_directory_unchanged", "settings_cancelled_form_rejected",
            "settings_text_roundtrip",
        }
        self.assertTrue(settings <= acceptance_module.MODERATION_CHECKS)
        for name in settings:
            line = f"MORPHEUS_MODERATION_CHECK {name}=true"
            for invalid in (complete.replace(line, ""),
                            complete.replace(line, line.replace("=true", "=false")),
                            f"MORPHEUS_MODERATION_CHECK {name}=false\n" + complete):
                with self.subTest(name=name), self.assertRaises(RuntimeError):
                    acceptance_module.moderation_evidence(invalid)

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_moderation_failure_emits_category_but_never_private_error(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            output = "\n".join((
                "MORPHEUS_MODERATION_STAGE invite_after_kick",
                "MORPHEUS_MODERATION_FAILURE history_sender_unverified",
                "MORPHEUS_MODERATION_FAILURE private_payload",
                "private-error-body",
            ))
            with (patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)),
                  contextlib.redirect_stdout(visible), self.assertRaises(RuntimeError)):
                runner.run("moderation_local", [sys.executable, "-c", f"print({output!r}); raise SystemExit(1)"])
            self.assertIn('"moderation_diagnostics": {"stage": "invite_after_kick", "failure": "history_sender_unverified"}',
                          visible.getvalue())
            self.assertNotIn("private", visible.getvalue())
            self.assertEqual(runner.results, [])

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_green_process_without_moderation_checks_cannot_pass_acceptance(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with (patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)),
                  contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(RuntimeError, "Moderation acceptance evidence")):
                runner.run("moderation_local", [sys.executable, "-c", "print('private-error-body')"])
            self.assertEqual(runner.results, [])

    def test_oauth_cases_restore_the_same_vaulted_profile_in_a_separate_process(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            services = Path(folder) / "oauth-services"
            services.mkdir(mode=0o700)
            fixture = services / "oauth-accounts.json"
            fixture.write_text("{}")
            fixture.chmod(0o600)
            with patch.object(runner, "run", side_effect=[str(fixture), "", "", ""]) as run:
                runner.oauth_cases()
            up, login, restore, down = run.call_args_list
            self.assertEqual(up.args[0], "oauth-services-up")
            self.assertEqual(down.args[0], "oauth-services-down")
            self.assertNotIn("--keep-data", down.args[1])
            self.assertEqual(login.args[0], "oauth-local-login-refresh")
            self.assertEqual(restore.args[0], "oauth-local-restore-refresh")
            self.assertEqual(login.args[1], restore.args[1], "reuse the same compiled test binary")
            for invocation, phase in ((login, "login_refresh"), (restore, "restore_refresh")):
                self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_MATRIX_OAUTH_PHASE"], phase)
                self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_MATRIX_OAUTH_FIXTURE"], str(fixture.resolve()))
                self.assertEqual(invocation.kwargs["oauth_checks"], OAUTH_PHASES[phase])
            self.assertEqual(login.kwargs["extra"]["MORPHEUS_MATRIX_OAUTH_PROFILE"],
                             restore.kwargs["extra"]["MORPHEUS_MATRIX_OAUTH_PROFILE"])
            self.assertEqual((Path(folder) / "oauth-profile").stat().st_mode & 0o777, 0o700)

    def test_oauth_cases_always_attempt_owned_cleanup(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with patch.object(runner, "run", side_effect=[RuntimeError("fixture failed"), ""]) as run:
                with self.assertRaisesRegex(RuntimeError, "fixture failed"):
                    runner.oauth_cases()
            self.assertEqual([call.args[0] for call in run.call_args_list],
                             ["oauth-services-up", "oauth-services-down"])
            self.assertIn("--keep-data", run.call_args_list[-1].args[1])

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_missing_oauth_markers_cannot_create_a_pass_record(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(RuntimeError):
                runner.run("oauth-incomplete", [sys.executable, "-c", "print('test result: ok. 1 passed; 0 failed; 0 ignored;')"],
                           test=True, oauth_checks=OAUTH_PHASES["login_refresh"])
            self.assertEqual(runner.results, [])

    def test_rtc_cases_use_separate_fixtures_and_labels(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            fixtures = [Path(folder) / name for name in ("normal.json", "late.json", "sticky-late.json")]
            for fixture in fixtures:
                fixture.write_text("{}")
                fixture.chmod(0o600)
            with patch.object(runner, "script", side_effect=[str(path) for path in fixtures]) as script, patch.object(runner, "run") as run:
                runner.rtc_cases()
            self.assertEqual(script.call_count, 3)
            first, second, third = run.call_args_list
            self.assertEqual(first.args[0], "local_rtc")
            self.assertEqual(second.args[0], "local_rtc-late-join")
            self.assertEqual(third.args[0], "local_rtc-sticky-2025-late-join")
            self.assertNotIn("MORPHEUS_RTC_LATE_JOIN", first.kwargs["extra"])
            self.assertEqual(second.kwargs["extra"]["MORPHEUS_RTC_LATE_JOIN"], "1")
            self.assertEqual(third.kwargs["extra"]["MORPHEUS_RTC_LATE_JOIN"], "1")
            self.assertEqual([call.kwargs["extra"]["MORPHEUS_RTC_TEST_MODE"] for call in run.call_args_list],
                             ["current", "current", "sticky-2025"])
            self.assertNotEqual(first.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"],
                                second.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"])
            self.assertEqual(first.args[1], second.args[1], "reuse the same compiled test binary")
            self.assertEqual(first.args[1], third.args[1], "reuse the same compiled test binary")
            self.assertEqual(len({call.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"]
                                  for call in run.call_args_list}), 3)

    def test_late_join_success_requires_new_decrypted_membership_in_each_dialect(self):
        summary = "test result: ok. 1 passed; 0 failed; 0 ignored;\n"
        markers = ["MORPHEUS_RTC_LATE_JOIN historical_member=false",
                   "MORPHEUS_RTC_LATE_JOIN renewed_member=true"]
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            for label in ("local_rtc-late-join", "local_rtc-sticky-2025-late-join"):
                def validate(lines):
                    return runner.validated_record(label, summary + "\n".join(lines), test=True,
                                                   ring_case=None, oauth_checks=None, element_checks=None)
                self.assertEqual(validate(markers)["late_join_checks"],
                                 {"historical_member_absent": True, "new_membership_decrypted": True})
                for lines in ([], markers[:1], markers[1:], markers[::-1],
                              [markers[0], markers[1].replace("true", "false")],
                              [markers[0].replace("false", "true"), markers[1]],
                              markers + markers, markers + ["MORPHEUS_RTC_LATE_JOIN renewed_member=unknown"]):
                    with self.subTest(label=label, lines=lines), self.assertRaisesRegex(RuntimeError, "renewal evidence"):
                        validate(lines)

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_failure_only_publishes_strict_late_join_boolean(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            code = "print('MORPHEUS_RTC_LATE_JOIN historical_member=false'); print('MORPHEUS_RTC_LATE_JOIN historical_member=opaque-fixture-credential'); raise SystemExit(1)"
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(visible), self.assertRaises(RuntimeError):
                runner.run("late-join-failure", [sys.executable, "-c", code])
            self.assertIn('"late_join_historical_member": [false]', visible.getvalue())
            self.assertNotIn("opaque-fixture-credential", visible.getvalue())

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_failure_only_publishes_allowlisted_oauth_booleans(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            code = "print('MORPHEUS_OAUTH_CHECK login=false'); print('MORPHEUS_OAUTH_CHECK login=opaque-fixture-credential'); print('MORPHEUS_OAUTH_CHECK opaque_credential=true'); raise SystemExit(1)"
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(visible), self.assertRaises(RuntimeError):
                runner.run("oauth-failure", [sys.executable, "-c", code], oauth_checks=OAUTH_PHASES["login_refresh"])
            self.assertIn('"oauth_checks": {"login": false}', visible.getvalue())
            self.assertNotIn("opaque", visible.getvalue())

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_helper_failure_publishes_only_fixed_diagnostics_without_rust_test_names(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            code = "print('MORPHEUS_OAUTH_STAGE postgres_ready'); print('MORPHEUS_OAUTH_FAILURE fixture_operation'); print('MORPHEUS_OAUTH_STAGE opaque_credential'); print('private-password-and-url'); raise SystemExit(1)"
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(visible), self.assertRaises(RuntimeError):
                runner.run("oauth-services-up", [sys.executable, "-c", code])
            self.assertIn('"oauth_diagnostics": {"stage": "postgres_ready", "failure": "fixture_operation"}', visible.getvalue())
            self.assertNotIn("opaque", visible.getvalue())
            self.assertNotIn("private-password", visible.getvalue())

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_oauth_deadline_preserves_last_fixed_stage_after_scanning_log(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            code = "import time; print('MORPHEUS_OAUTH_STAGE mas_ready', flush=True); print('private-diagnostic', flush=True); time.sleep(10)"
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)) as scan, contextlib.redirect_stdout(visible), self.assertRaisesRegex(RuntimeError, "deadline"):
                runner.run("oauth-services-up", [sys.executable, "-c", code], seconds=0.2)
            scan.assert_called_once()
            self.assertIn('"oauth_diagnostics": {"stage": "mas_ready", "failure": "deadline"}', visible.getvalue())
            self.assertNotIn("private-diagnostic", visible.getvalue())
            self.assertEqual(runner.results, [])

    @unittest.skipIf(os.name == "nt", "Linux hosted harness")
    def test_child_output_is_not_published_even_if_scanner_misses_it(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            visible = io.StringIO()
            with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(visible):
                returned = runner.run("privacy-probe", [sys.executable, "-c", "print('opaque-fixture-credential')"])
            self.assertIn("opaque-fixture-credential", returned)
            self.assertNotIn("opaque-fixture-credential", visible.getvalue())
            log = next(runner.logs.glob("*.log"))
            self.assertEqual(log.stat().st_mode & 0o777, 0o600)

    @unittest.skipIf(os.name == "nt", "POSIX fixture permissions")
    def test_fixture_must_be_private_and_within_this_run(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as other:
            root = Path(folder)
            fixture = root / "accounts.json"
            fixture.write_text("{}")
            fixture.chmod(0o600)
            self.assertEqual(fixture_from_output(str(fixture) + "\n", root), fixture.resolve())
            self.assertEqual(fixture_from_output("MORPHEUS_OAUTH_STAGE registration\n" + str(fixture) + "\n", root), fixture.resolve())
            self.assertEqual(fixture_from_output("MORPHEUS_MATRIX_FIXTURE_STAGE recovery\n" + str(fixture) + "\n", root), fixture.resolve())
            for unknown in ("MORPHEUS_MATRIX_FIXTURE_STAGE private_credential",
                            "MORPHEUS_MATRIX_FIXTURE_STAGE recovery trailing-secret"):
                with self.subTest(progress=unknown), self.assertRaises(RuntimeError):
                    fixture_from_output(unknown + "\n" + str(fixture), root)
            with self.assertRaises(RuntimeError):
                fixture_from_output("MORPHEUS_OAUTH_STAGE opaque_credential\n" + str(fixture), root)
            with self.assertRaises(RuntimeError):
                fixture_from_output(str(fixture) + "\n" + str(fixture), root)
            fixture.chmod(0o644)
            with self.assertRaises(RuntimeError):
                fixture_from_output(str(fixture), root)
            outside = Path(other) / "accounts.json"
            outside.write_text("{}")
            outside.chmod(0o600)
            with self.assertRaises(RuntimeError):
                fixture_from_output(str(outside), root)
            with self.assertRaises(RuntimeError):
                fixture_from_output(str(root / "missing.json"), root)


class OutgoingRingTests(unittest.TestCase):
    def test_hosted_rust_facts_and_stages_match_public_schema(self):
        source = (acceptance_module.REPO / "crates/morpheus-app/src/outgoing_ring/live_tests.rs").read_text()
        facts = set(re.findall(r'(?<![A-Za-z_])fact\("([a-z_]+)"\)', source))
        # Cleanup can be false, so its Rust marker prints the actual boolean.
        self.assertIn('MORPHEUS_RING_CHECK cleanup={clean}', source)
        facts.add("cleanup")
        self.assertEqual(facts, acceptance_module.RING_CHECKS)
        ring_stage_source = source.split("impl Stage {", 1)[1].split("\nfn phase(", 1)[0]
        self.assertEqual(set(re.findall(r'Self::\w+ => "([a-z_]+)"', ring_stage_source)),
                         acceptance_module.RING_STAGES)
        observer_results = set(re.findall(r'ObserverError::\w+ => "([a-z_]+)"', source))
        observer_results.update(re.findall(r'diagnostic.record\("([a-z_]+)"\)', source))
        observer_results.add("not_sampled")
        self.assertEqual(observer_results, acceptance_module.RING_OBSERVER_RESULTS)
        self.assertIn('Self(Some("not_sampled"))', source)
        self.assertIn('\\nMORPHEUS_RING_OBSERVER result={result}', source)
        self.assertEqual(set(re.findall(r'AdmissionFailure::\w+ => "([a-z_]+)"', source)),
                         acceptance_module.RING_ADMISSION_RESULTS)
        self.assertIn('\\nMORPHEUS_RING_ADMISSION result={result}', source)
        self.assertEqual(source.count('.map_err(admission_failure)?'), 2)

    def output(self, ring_case=("current", "dm")):
        facts = "\n".join(f"MORPHEUS_RING_CHECK {name}=true" for name in sorted(acceptance_module.RING_CHECKS))
        incoming = set(acceptance_module.INCOMING_CHECKS)
        if ring_case == ("current", "dm"):
            incoming.add(acceptance_module.INCOMING_REPLAY_CHECK)
        actor = "\n".join(f"MORPHEUS_INCOMING_CHECK {name}=true" for name in sorted(incoming))
        return (f"test {acceptance_module.RING_TEST} ... ok\n{facts}\n{actor}\n"
                "MORPHEUS_RING_RESULT passed=true\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 80 filtered out; finished in 1.00s\n")

    def test_exact_named_test_and_every_fixed_fact_are_required(self):
        complete = self.output()
        self.assertEqual(acceptance_module.ring_passed_tests(complete, ("current", "dm")), 1)
        invalid = ["", complete.replace(acceptance_module.RING_TEST, "other::test"),
                   complete.replace(" ... ok", " ... ignored"),
                   complete.replace("0 ignored", "1 ignored"),
                   complete.replace("1 passed", "0 passed"),
                   complete.replace("1 passed", "2 passed"),
                   complete.replace("MORPHEUS_RING_RESULT passed=true", ""),
                   complete + "MORPHEUS_RING_RESULT passed=false\n",
                   complete + "MORPHEUS_RING_FAILURE cleanup\n",
                   complete + "MORPHEUS_RING_OBSERVER result=empty_roster\n",
                   complete + "MORPHEUS_RING_RESULT passed=true\n",
                   "Skipped live checks\n" + complete,
                   complete + "test result: ok. 0 passed; 0 failed; 0 ignored;\n"]
        for name in acceptance_module.RING_CHECKS:
            marker = f"MORPHEUS_RING_CHECK {name}=true"
            invalid.extend((complete.replace(marker, ""), complete.replace(marker, marker + " private-data"),
                            f"MORPHEUS_RING_CHECK {name}=false\n" + complete,
                            complete + f"MORPHEUS_RING_CHECK {name}=false\n"))
        for index, output in enumerate(invalid):
            with self.subTest(case=index), self.assertRaises(RuntimeError):
                acceptance_module.ring_passed_tests(output, ("current", "dm"))

    def test_evidence_allows_only_exact_fixed_booleans_and_preserves_false(self):
        output = "\n".join((
            "MORPHEUS_RING_CHECK encrypted_membership=false",
            "MORPHEUS_RING_CHECK encrypted_membership=true",
            "MORPHEUS_RING_CHECK invitation_receipt=true",
            "MORPHEUS_RING_CHECK private_account=true",
            "MORPHEUS_RING_CHECK cleanup=true private-token",
            "MORPHEUS_RING_CHECK cleanup=private-token",
        ))
        self.assertEqual(acceptance_module.ring_evidence(output), {
            "encrypted_membership": False, "invitation_receipt": True,
        })

    def test_diagnostics_allow_only_fixed_stages_and_failure_categories(self):
        output = "\n".join((
            "MORPHEUS_RING_STAGE fixture", "MORPHEUS_RING_STAGE redact",
            "MORPHEUS_RING_STAGE private_account", "MORPHEUS_RING_STAGE cleanup trailing-secret",
            "MORPHEUS_RING_FAILURE redaction", "MORPHEUS_RING_FAILURE private_server_response",
            "MORPHEUS_RING_FAILURE cleanup trailing-secret", "MORPHEUS_RING_RESULT passed=false",
            "MORPHEUS_RING_RESULT passed=true", "MORPHEUS_RING_CHECK cleanup=false",
        ))
        self.assertEqual(acceptance_module.ring_diagnostics(output), {
            "checks": {"cleanup": False}, "stage": "redact", "failure": "redaction", "passed": False,
        })

    def test_observer_diagnostics_preserve_only_fixed_categories(self):
        for category in acceptance_module.RING_OBSERVER_RESULTS:
            with self.subTest(category=category):
                output = f"MORPHEUS_RING_OBSERVER result={category}\n"
                self.assertEqual(acceptance_module.ring_diagnostics(output), {
                    "checks": {}, "observer_result": category,
                })
        for stage in ("observer_open", "observer_roster", "actor_setup", "observer_leave"):
            with self.subTest(stage=stage):
                output = f"MORPHEUS_RING_STAGE {stage}\nMORPHEUS_RING_FAILURE {stage}\n"
                self.assertEqual(acceptance_module.ring_diagnostics(output), {
                    "checks": {}, "stage": stage, "failure": stage,
                })

    def test_observer_diagnostics_reject_suffixes_unknown_payloads_and_prefixed_lines(self):
        valid = "MORPHEUS_RING_OBSERVER result=current_event_mismatch"
        malformed = (
            valid + " private-event", "private-prefix " + valid,
            "MORPHEUS_RING_OBSERVER result=private_server_error",
            "MORPHEUS_RING_OBSERVER result=InvalidMembership",
            "MORPHEUS_RING_OBSERVER event_id=empty_roster",
            "MORPHEUS_RING_OBSERVER result=empty_roster\\nprivate-payload",
        )
        for line in malformed:
            with self.subTest(line=line):
                self.assertEqual(acceptance_module.ring_diagnostics(line + "\n"), {"checks": {}})
        self.assertEqual(acceptance_module.ring_diagnostics(valid + "\n" + "\n".join(malformed)), {
            "checks": {}, "observer_result": "current_event_mismatch",
        })

    def test_admission_diagnostics_preserve_only_fixed_categories_and_poison_a_pass(self):
        for category in acceptance_module.RING_ADMISSION_RESULTS:
            with self.subTest(category=category):
                marker = f"MORPHEUS_RING_ADMISSION result={category}\n"
                self.assertEqual(acceptance_module.ring_diagnostics(marker), {
                    "checks": {}, "admission_result": category,
                })
                for output in (marker + self.output(), self.output() + marker):
                    with self.assertRaises(RuntimeError):
                        acceptance_module.ring_passed_tests(output, ("current", "dm"))

    def test_admission_diagnostics_reject_suffixes_unknown_payloads_and_prefixed_lines(self):
        valid = "MORPHEUS_RING_ADMISSION result=policy"
        malformed = (
            valid + " private-event", "private-prefix " + valid,
            "MORPHEUS_RING_ADMISSION result=private_server_error",
            "MORPHEUS_RING_ADMISSION result=Policy",
            "MORPHEUS_RING_ADMISSION event_id=policy",
            "MORPHEUS_RING_ADMISSION result=policy\\nprivate-payload",
        )
        for line in malformed:
            with self.subTest(line=line):
                self.assertEqual(acceptance_module.ring_diagnostics(line + "\n"), {"checks": {}})
        self.assertEqual(acceptance_module.ring_diagnostics(valid + "\n" + "\n".join(malformed)), {
            "checks": {}, "admission_result": "policy",
        })

    def test_four_cases_use_fresh_fixtures_and_the_same_exact_ignored_app_test(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            fixtures = [Path(folder) / f"fixture-{index}.json" for index in range(4)]
            for fixture in fixtures:
                fixture.write_text("{}")
                fixture.chmod(0o600)
            with (patch.object(runner, "script", side_effect=[str(path) for path in fixtures]) as script,
                  patch.object(runner, "run") as run):
                runner.outgoing_ring_cases()
            self.assertEqual([item.args for item in script.call_args_list],
                             [("local_matrix_rtc", "rtc-fixture")] * 4)
            cases = [(mode, target) for mode in ("current", "sticky-2025") for target in ("dm", "room")]
            for invocation, fixture, case in zip(run.call_args_list, fixtures, cases, strict=True):
                mode, target = case
                self.assertEqual(invocation.args[0], f"outgoing-ring-{mode}-{target}")
                self.assertEqual(invocation.args[1], [
                    "cargo", "test", "--locked", "-p", "morpheus-app", "--features", "native-calls",
                    "--bin", "morpheus-app", acceptance_module.RING_TEST, "--", "--ignored", "--exact",
                    "--show-output", "--test-threads=1",
                ])
                self.assertEqual(invocation.kwargs, {
                    "extra": {"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": str(fixture.resolve()),
                              "MORPHEUS_RING_TEST_MODE": mode, "MORPHEUS_RING_TEST_TARGET": target},
                    "ring_case": case, "seconds": 300, "pid_namespace": True,
                })

    def test_first_failed_case_stops_later_cases(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            fixture = Path(folder) / "fixture.json"
            fixture.write_text("{}")
            fixture.chmod(0o600)
            with (patch.object(runner, "script", return_value=str(fixture)) as script,
                  patch.object(runner, "run", side_effect=RuntimeError("fixed failure")) as run,
                  self.assertRaisesRegex(RuntimeError, "fixed failure")):
                runner.outgoing_ring_cases()
            script.assert_called_once()
            run.assert_called_once()

    @contextlib.contextmanager
    def mocked_run(self, runner, output, *, returncode=0, scanner_code=0):
        process = Mock(returncode=returncode)
        visible = io.StringIO()
        def child(_args, **kwargs):
            kwargs["stdout"].write(output.encode())
            return process
        with (patch.object(acceptance_module.subprocess, "Popen", side_effect=child) as popen,
              patch.object(acceptance_module.subprocess, "run",
                           return_value=subprocess.CompletedProcess([], scanner_code)) as scanner,
              contextlib.redirect_stdout(visible)):
            yield visible, popen, scanner

    def test_success_logs_only_fixed_facts_after_scan_and_retains_private_log(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            output = self.output() + "private-token-and-room-id\n"
            with self.mocked_run(runner, output) as (visible, _, scanner):
                runner.run("outgoing-ring-current-dm", ["mock-app-test"], ring_case=("current", "dm"))
            scanner.assert_called_once()
            self.assertNotIn("private-token", visible.getvalue())
            self.assertEqual(runner.results[0]["passed"], 1)
            self.assertEqual(runner.results[0]["ring_checks"], dict.fromkeys(acceptance_module.RING_CHECKS, True))
            self.assertEqual((runner.results[0]["ring_mode"], runner.results[0]["ring_target"]), ("current", "dm"))
            log = next(runner.logs.glob("*.log"))
            self.assertEqual(log.stat().st_mode & 0o777, 0o600)
            self.assertEqual(log.read_text(), output)

    def test_failed_scan_or_incomplete_case_cannot_publish_a_pass_record(self):
        for case, output, code, scan in (
                ("scanner", self.output(), 0, 1),
                ("incomplete", "private-server-body\n", 0, 0),
                ("process", self.output() + "private-server-body\n", 1, 0)):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with (self.mocked_run(runner, output, returncode=code, scanner_code=scan) as (visible, _, _),
                      self.assertRaises(RuntimeError)):
                    runner.run("outgoing-ring-current-dm", ["mock-app-test"], ring_case=("current", "dm"))
                self.assertEqual(runner.results, [])
                self.assertNotIn("private-server", visible.getvalue())
                self.assertNotIn('"status": "passed"', visible.getvalue())
                if scan:
                    self.assertNotIn("ring_checks", visible.getvalue())
                    self.assertNotIn("ring_diagnostics", visible.getvalue())

    def test_invalid_case_is_rejected_before_starting_a_child(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with (patch.object(acceptance_module.subprocess, "Popen") as popen,
                  self.assertRaisesRegex(RuntimeError, "Invalid outgoing ring acceptance case")):
                runner.run("outgoing-ring", ["mock-app-test"], ring_case=("private-mode", "dm"))
            popen.assert_not_called()

    def test_summary_distinguishes_unsolicited_receipts_from_incoming_ringing(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "unused-scanner")
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
                self.assertNotIn("Invitation rows verify", summary.read_text())
                runner.results.append({"check": "outgoing-ring-current-dm", "passed": 1, "seconds": 10,
                                       "ring_checks": dict.fromkeys(acceptance_module.RING_CHECKS, True)})
                runner.summary()
            text = summary.read_text()
            self.assertIn("unsolicited encrypted sync receipt with authenticated device metadata", text)
            self.assertIn("independently fetched events", text)
            self.assertIn("do not verify incoming call UI, audible ringing, offline/restart replay", text)


class IncomingActorTests(unittest.TestCase):
    def output(self, case=("current", "dm")):
        return OutgoingRingTests().output(case)

    def test_rust_actor_markers_match_the_exact_public_schema(self):
        root = acceptance_module.REPO / "crates/morpheus-app/src/outgoing_ring"
        source = (root / "live_tests/actor_tests.rs").read_text()
        parent = (root / "live_tests.rs").read_text()
        facts = set(re.findall(r'actor_fact\("([a-z_]+)"\)', source))
        facts.add("actor_cleanup")
        self.assertEqual(facts, acceptance_module.INCOMING_CHECKS | {acceptance_module.INCOMING_REPLAY_CHECK})
        self.assertIn('\\nMORPHEUS_INCOMING_CHECK actor_cleanup={clean}', parent)
        self.assertIn('\\nMORPHEUS_INCOMING_FAILURE actor_cleanup', parent)
        stage_source = source.split("impl ActorStage {", 1)[1].split("\nfn actor_fact(", 1)[0]
        self.assertEqual(set(re.findall(r'Self::\w+ => "([a-z_]+)"', stage_source)),
                         acceptance_module.INCOMING_STAGES)
        errors = set(re.findall(r'\berror\("([a-z_]+)"\)', source))
        error_source = source.split("fn controller_error(", 1)[1].split("async fn bounded", 1)[0]
        errors.update(re.findall(r'=>\s*(?:\{\s*)?"([a-z_]+)"', error_source))
        self.assertEqual(errors, acceptance_module.INCOMING_ERRORS)
        for marker in ('CHECK {name}=true', 'STAGE {}', 'FAILURE {}', 'ERROR {category}'):
            self.assertIn('\\nMORPHEUS_INCOMING_' + marker, source)

    def test_departure_and_invitation_categories_are_fixed_failures_not_success_facts(self):
        categories = {value for value in acceptance_module.INCOMING_ERRORS
                      if value.startswith(("observer_", "invitation_"))}
        self.assertEqual(len(categories), 29)
        self.assertIn("observer_roster_not_current", categories)
        self.assertIn("observer_invalid_membership", categories)
        self.assertIn("invitation_suppressed_occupied", categories)
        for category in categories:
            marker = f"MORPHEUS_INCOMING_ERROR {category}\n"
            with self.subTest(category=category):
                self.assertEqual(acceptance_module.incoming_diagnostics(marker),
                                 {"checks": {}, "error": category})
                with self.assertRaises(RuntimeError):
                    acceptance_module.ring_passed_tests(self.output() + marker, ("current", "dm"))
                malformed = marker.rstrip() + " private-payload\n"
                evidence = acceptance_module.incoming_diagnostics(malformed)
                self.assertEqual(evidence, {"checks": {}, "malformed": True})
                self.assertNotIn("private-payload", json.dumps(evidence))

    def test_actor_schema_does_not_replace_the_thirteen_sender_checks(self):
        self.assertEqual(len(acceptance_module.RING_CHECKS), 13)
        self.assertEqual(acceptance_module.INCOMING_CHECKS, {
            "actor_discovered", "exact_key_once", "actor_withdrew", "admitted_target_preserved",
            "accepted_join_identity", "slot_unchanged", "quiet_receive_only",
            "established_after_withdrawal", "accepted_leave_observed", "actor_cleanup",
        })
        self.assertEqual(acceptance_module.INCOMING_REPLAY_CHECK, "account_replay_rejected")

    def test_all_four_cases_require_actor_facts_and_only_current_dm_exercises_replay(self):
        for case in acceptance_module.RING_CASES:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as folder:
                output = self.output(case)
                self.assertEqual(acceptance_module.ring_passed_tests(output, case), 1)
                runner = Acceptance(Path(folder), "scanner")
                record = runner.validated_record("fixed-label", output, test=False, ring_case=case,
                                                 oauth_checks=(), element_checks=False)
                expected = dict.fromkeys(acceptance_module.INCOMING_CHECKS, True)
                if case == ("current", "dm"):
                    expected[acceptance_module.INCOMING_REPLAY_CHECK] = True
                self.assertEqual(record["incoming_checks"], expected)
                self.assertEqual(record["ring_checks"], dict.fromkeys(acceptance_module.RING_CHECKS, True))
                self.assertIs(record["account_replay_exercised"], case == ("current", "dm"))
                marker = "MORPHEUS_INCOMING_CHECK account_replay_rejected=true\n"
                invalid = output.replace(marker, "") if case == ("current", "dm") else output + marker
                with self.assertRaises(RuntimeError):
                    acceptance_module.ring_passed_tests(invalid, case)

    def test_every_actor_fact_is_mandatory_and_false_is_permanent_for_every_case(self):
        for case in acceptance_module.RING_CASES:
            complete = self.output(case)
            names = acceptance_module.INCOMING_CHECKS | (
                {acceptance_module.INCOMING_REPLAY_CHECK} if case == ("current", "dm") else set())
            for name in names:
                marker = f"MORPHEUS_INCOMING_CHECK {name}=true"
                for failure, output in (
                    ("missing", complete.replace(marker, "")),
                    ("suffix", complete.replace(marker, marker + " private-payload")),
                    ("early-false", f"MORPHEUS_INCOMING_CHECK {name}=false\n" + complete),
                    ("late-false", complete + f"MORPHEUS_INCOMING_CHECK {name}=false\n"),
                ):
                    with self.subTest(case=case, fact=name, failure=failure), self.assertRaises(RuntimeError):
                        acceptance_module.ring_passed_tests(output, case)

    def test_each_known_error_or_failure_poisons_otherwise_complete_evidence(self):
        for kind, allowed in (("ERROR", acceptance_module.INCOMING_ERRORS),
                              ("FAILURE", acceptance_module.INCOMING_STAGES)):
            for value in allowed:
                marker = f"MORPHEUS_INCOMING_{kind} {value}\n"
                with self.subTest(kind=kind, value=value):
                    evidence = acceptance_module.incoming_diagnostics(marker)
                    self.assertEqual(evidence[kind.lower()], value)
                    for output in (marker + self.output(), self.output() + marker):
                        with self.assertRaises(RuntimeError):
                            acceptance_module.ring_passed_tests(output, ("current", "dm"))

    def test_unknown_or_malformed_actor_markers_fail_closed_without_payload(self):
        malformed = (
            "MORPHEUS_INCOMING_CHECK actor_cleanup=true private-payload",
            "MORPHEUS_INCOMING_CHECK actor_cleanup=TRUE",
            "MORPHEUS_INCOMING_CHECK private_account=true",
            "MORPHEUS_INCOMING_STAGE first_discovery private-payload",
            "MORPHEUS_INCOMING_STAGE private_stage",
            "MORPHEUS_INCOMING_FAILURE first_discovery private-payload",
            "MORPHEUS_INCOMING_FAILURE private_stage",
            "MORPHEUS_INCOMING_ERROR unavailable private-payload",
            "MORPHEUS_INCOMING_ERROR private_error",
            "MORPHEUS_INCOMING_DATA private-payload",
        )
        for marker in malformed:
            with self.subTest(marker=marker):
                self.assertEqual(acceptance_module.incoming_diagnostics(marker), {
                    "checks": {}, "malformed": True,
                })
                with self.assertRaises(RuntimeError):
                    acceptance_module.ring_passed_tests(self.output() + marker + "\n", ("current", "dm"))
        # Arbitrary text cannot become a diagnostic merely by containing a marker.
        self.assertEqual(acceptance_module.incoming_diagnostics(
            "private-prefix MORPHEUS_INCOMING_ERROR unavailable\n"), {"checks": {}})

    def test_fixed_diagnostics_preserve_false_and_last_known_stage(self):
        output = ("MORPHEUS_INCOMING_STAGE first_discovery\n"
                  "MORPHEUS_INCOMING_CHECK actor_discovered=false\n"
                  "MORPHEUS_INCOMING_CHECK actor_discovered=true\n"
                  "MORPHEUS_INCOMING_STAGE accepted_join\n"
                  "MORPHEUS_INCOMING_FAILURE accepted_join\n"
                  "MORPHEUS_INCOMING_ERROR account_changed\n")
        expected = {"checks": {"actor_discovered": False}, "stage": "accepted_join",
                    "failure": "accepted_join", "error": "account_changed"}
        self.assertEqual(acceptance_module.incoming_diagnostics(output), expected)
        self.assertEqual(acceptance_module.ring_diagnostics(output), {"checks": {}, "incoming": expected})

    def test_actual_run_publishes_case_specific_actor_facts_only_after_clean_scan(self):
        for case in acceptance_module.RING_CASES:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                output = self.output(case) + "private-account-and-keys\n"
                with OutgoingRingTests().mocked_run(runner, output) as (visible, _, scanner):
                    runner.run(f"outgoing-ring-{case[0]}-{case[1]}", ["test"], ring_case=case)
                scanner.assert_called_once()
                record = json.loads(next(line for line in visible.getvalue().splitlines() if line.startswith("{")))
                self.assertEqual(record["status"], "passed")
                self.assertIs(record["account_replay_exercised"], case == ("current", "dm"))
                self.assertEqual(record["incoming_checks"], acceptance_module.incoming_evidence(output))
                self.assertEqual(record["quiet_evidence"], "sampled_cache_and_media_state")
                self.assertEqual(record["withdrawal_evidence"], "joined_after_sdk_cache_removal")
                self.assertIs(record["actor_withdrawal_processing_confirmed"], False)
                self.assertNotIn("private-account", visible.getvalue())

    def test_scan_failure_withholds_actor_diagnostics(self):
        output = ("running 1 test\nMORPHEUS_INCOMING_STAGE accepted_join\n"
                  "MORPHEUS_INCOMING_ERROR rtc\nMORPHEUS_INCOMING_FAILURE accepted_join\n"
                  "private-account-and-keys\n")
        for scan in (0, 1):
            with self.subTest(scan=scan), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                with (OutgoingRingTests().mocked_run(runner, output, returncode=1, scanner_code=scan)
                      as (visible, _, scanner), self.assertRaises(RuntimeError)):
                    runner.run("outgoing-ring-current-dm", ["test"], ring_case=("current", "dm"))
                scanner.assert_called_once()
                self.assertNotIn("private-account", visible.getvalue())
                if scan:
                    self.assertNotIn("incoming", visible.getvalue())
                else:
                    self.assertEqual(json.loads(next(line for line in visible.getvalue().splitlines() if line.startswith("{")))["ring_diagnostics"]["incoming"], {
                        "checks": {}, "stage": "accepted_join", "error": "rtc", "failure": "accepted_join",
                    })

    def test_actor_cleanup_failure_stays_fatal_after_contained_test_entry(self):
        for marker in ("MORPHEUS_INCOMING_CHECK actor_cleanup=false",
                       "MORPHEUS_INCOMING_FAILURE actor_cleanup",
                       "MORPHEUS_INCOMING_ERROR cleanup"):
            with self.subTest(marker=marker), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                # Later error/stage messages must never hide a cleanup failure.
                output = ("running 1 test\n" + marker + "\n"
                          "MORPHEUS_INCOMING_FAILURE accepted_join\n"
                          "MORPHEUS_INCOMING_ERROR rtc\n"
                          "MORPHEUS_INCOMING_CHECK actor_cleanup=true\n")
                with (ScopedContinuationTests().contained_child(runner, output),
                      self.assertRaises(RuntimeError) as caught):
                    runner.run("outgoing-ring-current-dm", ["test"], ring_case=("current", "dm"), pid_namespace=True)
                self.assertNotIsInstance(caught.exception, acceptance_module.CheckFailure)

    def test_noncleanup_actor_failure_is_collectible_only_after_scan_and_containment(self):
        output = ("running 1 test\nMORPHEUS_INCOMING_CHECK actor_cleanup=true\n"
                  "MORPHEUS_INCOMING_STAGE accepted_join\nMORPHEUS_INCOMING_FAILURE accepted_join\n"
                  "MORPHEUS_INCOMING_ERROR rtc\nprivate-account-and-keys\n")
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner")
            with (ScopedContinuationTests().contained_child(runner, output) as (visible, *_),
                  self.assertRaises(acceptance_module.CheckFailure)):
                runner.run("outgoing-ring-current-dm", ["test"], ring_case=("current", "dm"), pid_namespace=True)
            self.assertNotIn("private-account", visible.getvalue())
            self.assertEqual(runner.results, [])


class ScopedContinuationTests(unittest.TestCase):
    def test_failed_admission_publishes_fixed_category_only_after_scan(self):
        output = ("running 1 test\nMORPHEUS_RING_STAGE admission\n"
                  "MORPHEUS_RING_ADMISSION result=trust\n"
                  "MORPHEUS_RING_FAILURE admission\n"
                  "MORPHEUS_RING_ADMISSION result=private-server-data\n"
                  "MORPHEUS_RING_CHECK cleanup=true\nprivate-account-event-key\n")
        for scanner_code in (0, 1):
            with self.subTest(scanner_code=scanner_code), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                with self.contained_child(runner, output, scanner_code=scanner_code) as (
                    visible, _, _, _, _, scanner,
                ), self.assertRaises(RuntimeError):
                    runner.run("outgoing-ring-current-dm", ["test"],
                               ring_case=("current", "dm"), pid_namespace=True)
                scanner.assert_called_once()
                self.assertNotIn("private-server-data", visible.getvalue())
                self.assertNotIn("private-account-event-key", visible.getvalue())
                if scanner_code == 0:
                    evidence = json.loads(next(
                        line for line in visible.getvalue().splitlines() if line.startswith("{")
                    ))
                    self.assertEqual(evidence["ring_diagnostics"], {
                        "checks": {"cleanup": True}, "stage": "admission",
                        "failure": "admission", "admission_result": "trust",
                    })
                else:
                    self.assertNotIn("admission_result", visible.getvalue())

    def test_failed_observer_publishes_fixed_stage_and_result_only_after_scan(self):
        output = ("running 1 test\nMORPHEUS_RING_STAGE observer_roster\n"
                  "MORPHEUS_RING_OBSERVER result=slot_unavailable\n"
                  "MORPHEUS_RING_FAILURE observer_roster\n"
                  "MORPHEUS_RING_OBSERVER result=private-server-data\n"
                  "MORPHEUS_RING_CHECK cleanup=true\nprivate-account-event-key\n")
        for scanner_code in (0, 1):
            with self.subTest(scanner_code=scanner_code), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                with self.contained_child(runner, output, scanner_code=scanner_code) as (
                    visible, _, _, _, _, scanner,
                ), self.assertRaises(RuntimeError):
                    runner.run("outgoing-ring-current-dm", ["test"],
                               ring_case=("current", "dm"), pid_namespace=True)
                scanner.assert_called_once()
                self.assertNotIn("private-server-data", visible.getvalue())
                self.assertNotIn("private-account-event-key", visible.getvalue())
                if scanner_code == 0:
                    evidence = json.loads(next(
                        line for line in visible.getvalue().splitlines() if line.startswith("{")
                    ))
                    self.assertEqual(evidence["ring_diagnostics"], {
                        "checks": {"cleanup": True}, "stage": "observer_roster",
                        "failure": "observer_roster", "observer_result": "slot_unavailable",
                    })
                else:
                    self.assertNotIn("observer_result", visible.getvalue())

    def test_late_join_decryption_accepts_only_fixed_categories_and_sampled_booleans(self):
        for category in acceptance_module.LATE_JOIN_DECRYPT_RESULTS:
            with self.subTest(category=category):
                output = ("MORPHEUS_RTC_LATE_JOIN_DECRYPT alice_current=true "
                          f"bob_current=false result={category}\n")
                self.assertEqual(acceptance_module.rtc_diagnostics(output), {
                    "late_join_decryption": {
                        "alice_current": True, "bob_current": False, "result": category,
                    },
                })
        self.assertEqual(acceptance_module.rtc_diagnostics(
            "MORPHEUS_RTC_LATE_JOIN_DECRYPT alice_current=false bob_current=false result=unavailable\n"
        ), {"late_join_decryption": {
            "alice_current": False, "bob_current": False, "result": "unavailable",
        }})

    def test_late_join_decryption_rejects_unknown_or_embellished_markers(self):
        valid = ("MORPHEUS_RTC_LATE_JOIN_DECRYPT alice_current=true "
                 "bob_current=false result=MissingMegolmSession")
        for invalid in (
            valid + " private-event-id", "private-prefix " + valid,
            valid.replace("MissingMegolmSession", "PrivateError"),
            valid.replace("alice_current=true", "alice_current=private"),
            valid.replace("bob_current=false", "bob_current=0"),
            valid.replace("result=", "event_id="),
        ):
            with self.subTest(invalid=invalid):
                self.assertEqual(acceptance_module.rtc_diagnostics(invalid + "\n"), {})
        self.assertEqual(acceptance_module.rtc_diagnostics(valid + "\n" + invalid + "\n"), {
            "late_join_decryption": {
                "alice_current": True, "bob_current": False, "result": "MissingMegolmSession",
            },
        })

    def test_failed_late_join_publishes_decryption_only_after_clean_scan(self):
        output = ("running 1 test\nMORPHEUS_RTC_STAGE initial_bob_roster\n"
                  "MORPHEUS_RTC_ROSTER expected_present=true observed_present=false\n"
                  "MORPHEUS_RTC_LATE_JOIN_DECRYPT alice_current=true bob_current=false "
                  "result=UnknownMegolmMessageIndex\nprivate-event-key-error\n")
        for scanner_code in (0, 1):
            with self.subTest(scanner_code=scanner_code), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                with self.contained_child(runner, output, scanner_code=scanner_code) as (
                    visible, _, _, _, _, scanner,
                ), self.assertRaises(RuntimeError):
                    runner.run("local_rtc-late-join", ["test"], test=True, pid_namespace=True)
                scanner.assert_called_once()
                self.assertNotIn("private-event-key-error", visible.getvalue())
                if scanner_code == 0:
                    evidence = json.loads(next(
                        line for line in visible.getvalue().splitlines() if line.startswith("{")
                    ))
                    self.assertEqual(evidence["rtc_diagnostics"]["late_join_decryption"], {
                        "alice_current": True, "bob_current": False,
                        "result": "UnknownMegolmMessageIndex",
                    })
                else:
                    self.assertNotIn("late_join_decryption", visible.getvalue())

    def test_rtc_diagnostics_reject_ids_unknown_stages_suffixes_and_unobserved_defaults(self):
        self.assertEqual(acceptance_module.rtc_diagnostics("private server response"), {})
        output = ("MORPHEUS_RTC_STAGE initial_bob_roster\n"
                  "MORPHEUS_RTC_ROSTER expected_present=true observed_present=false\n"
                  "MORPHEUS_RTC_STAGE private_room_id\n"
                  "MORPHEUS_RTC_STAGE rejoined_bob_roster private-token\n"
                  "MORPHEUS_RTC_ROSTER expected_present=true observed_present=true private-token\n"
                  "MORPHEUS_RTC_ROSTER expected_present=false observed_present=private-token\n")
        self.assertEqual(acceptance_module.rtc_diagnostics(output), {
            "stage": "initial_bob_roster", "expected_present": True, "observed_present": False,
        })

    def test_actual_native_roster_stage_and_observation_are_printed_only_after_scan(self):
        output = ("running 1 test\nMORPHEUS_RTC_STAGE rejoined_bob_roster\n"
                  "MORPHEUS_RTC_ROSTER expected_present=true observed_present=false\nprivate-error\n")
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner")
            with self.contained_child(runner, output) as (visible, _, _, _, _, _), self.assertRaises(acceptance_module.CheckFailure):
                runner.run("local_rtc-late-join", ["test"], test=True, pid_namespace=True)
            self.assertIn('"rtc_diagnostics": {"stage": "rejoined_bob_roster", "expected_present": true, "observed_present": false}', visible.getvalue())
            self.assertNotIn("private-error", visible.getvalue())

    def test_roster_stage_allowlist_matches_native_source(self):
        source = (acceptance_module.REPO / "crates/morpheus-rtc/tests/local_rtc.rs").read_text()
        values = set(re.findall(r'"((?:initial|departed|rejoined)_[a-z]+_roster)"', source))
        self.assertEqual(values, acceptance_module.RTC_STAGES)
        self.assertIn('\\nMORPHEUS_RTC_ROSTER expected_present={present} observed_present={observed}', source)

    @contextlib.contextmanager
    def contained_child(self, runner, output, *, returncode=101, timeout=False,
                        scanner_code=0, cleanup_error=None):
        runner.containment_verified = True
        process = Mock(returncode=None)
        visible = io.StringIO()
        def child(_args, **kwargs):
            kwargs["stdout"].write(output.encode())
            return process
        def cleanup(_process):
            self.assertIsNone(process.returncode, "group must remain anchored until cleanup")
            if cleanup_error:
                raise cleanup_error
            process.returncode = returncode
        with (patch.object(acceptance_module, "namespace_command", return_value=["owned-wrapper"]) as command,
              patch.object(acceptance_module.subprocess, "Popen", side_effect=child) as popen,
              patch.object(acceptance_module, "wait_owned", side_effect=(subprocess.TimeoutExpired("private", 1)
                                                                       if timeout else None)) as wait,
              patch.object(acceptance_module, "stop_owned", side_effect=cleanup) as stop,
              patch.object(acceptance_module.subprocess, "run", return_value=subprocess.CompletedProcess([], scanner_code)) as scan,
              contextlib.redirect_stdout(visible)):
            yield visible, command, popen, wait, stop, scan
        process.communicate.assert_not_called()

    def test_only_executed_scanned_contained_failures_are_collectible(self):
        for timeout in (False, True):
            with self.subTest(timeout=timeout), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                with self.contained_child(runner, "running 1 test\nprivate-server-body\n", timeout=timeout) as (visible, _, _, _, stop, scan):
                    with self.assertRaises(acceptance_module.CheckFailure) as caught:
                        runner.run("local_rtc-late-join", ["cargo-test"], test=True, pid_namespace=True)
                self.assertEqual(caught.exception.label, "local_rtc-late-join")
                self.assertEqual(caught.exception.reason, "deadline" if timeout else "test_failed")
                stop.assert_called_once()
                scan.assert_called_once()
                self.assertEqual(scan.call_args.kwargs["timeout"], 30)
                self.assertNotIn("private", visible.getvalue() + str(caught.exception))

    def test_namespace_compile_scanner_and_cleanup_errors_remain_fatal(self):
        cases = (
            ("private compiler diagnostic\n", 0, None),
            ("running 1 test\nElement process containment failed; raw diagnostics withheld\n", 0, None),
            ("running 1 test\n", 1, None),
            ("running 1 test\n", 0, RuntimeError("private ownership error")),
        )
        for output, scan_code, cleanup_error in cases:
            with self.subTest(scanner=scan_code, cleanup=cleanup_error is not None), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                with self.contained_child(runner, output, scanner_code=scan_code, cleanup_error=cleanup_error) as (visible, _, _, _, _, scan):
                    with self.assertRaises(RuntimeError) as caught:
                        runner.run("local_rtc", ["cargo-test"], test=True, pid_namespace=True)
                self.assertNotIsInstance(caught.exception, acceptance_module.CheckFailure)
                self.assertNotIn("private", visible.getvalue() + str(caught.exception))
                if cleanup_error:
                    scan.assert_not_called()

    def test_missing_required_evidence_stays_failed_after_clean_contained_exit(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner")
            with self.contained_child(runner, "running 1 test\ntest result: ok. 0 passed; 0 failed; 0 ignored;\n", returncode=0):
                with self.assertRaises(acceptance_module.CheckFailure) as caught:
                    runner.run("local_rtc", ["cargo-test"], test=True, pid_namespace=True)
            self.assertEqual(caught.exception.reason, "evidence_incomplete")
            self.assertEqual(runner.results, [])

    def test_private_environment_is_forwarded_to_same_existing_namespace_contract(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner")
            runner.env.update({"DBUS_SESSION_BUS_ADDRESS": "private-bus", "HOME": "private-home"})
            output = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
            with self.contained_child(runner, output, returncode=0) as (visible, command, popen, _, _, _):
                runner.run("local_rtc", ["cargo-test"], test=True, pid_namespace=True,
                           extra={"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": "private-fixture"})
            target, directory, environment = command.call_args.args
            self.assertEqual(target, ["cargo-test"])
            self.assertEqual(directory.parent, runner.root)
            self.assertEqual(environment["DBUS_SESSION_BUS_ADDRESS"], "private-bus")
            self.assertEqual(environment["HOME"], "private-home")
            self.assertEqual(environment["XDG_DATA_HOME"], runner.env["XDG_DATA_HOME"])
            self.assertEqual(environment["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"], "private-fixture")
            self.assertEqual(popen.call_args.kwargs["stdin"], subprocess.DEVNULL)
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            self.assertNotIn("private", visible.getvalue())

    def test_no_native_child_before_successful_fresh_containment_preflight(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner")
            with patch.object(acceptance_module.subprocess, "Popen") as popen, self.assertRaisesRegex(RuntimeError, "has not been verified"):
                runner.run("local_rtc", ["cargo-test"], test=True, pid_namespace=True)
            popen.assert_not_called()
            with patch.object(runner, "run", side_effect=RuntimeError("fixed containment failure")), self.assertRaises(RuntimeError):
                runner.require_containment()
            self.assertFalse(runner.containment_verified)
            with patch.object(runner, "run") as run:
                runner.require_containment()
                runner.require_containment()
            self.assertTrue(runner.containment_verified)
            run.assert_called_once()

    def test_shared_group_skips_remaining_dependents_but_not_next_group(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
            runner = Acceptance(Path(folder), "scanner")
            later = Mock()
            with self.assertRaises(RuntimeError):
                runner.group(acceptance_module.CHAT_CHECKS, lambda: (_ for _ in ()).throw(RuntimeError("infrastructure")))
            self.assertEqual(runner.failures, [])
            self.assertFalse(runner.group(acceptance_module.CHAT_CHECKS, lambda: (_ for _ in ()).throw(
                acceptance_module.CheckFailure("live_local", "test_failed"))))
            runner.group(("recovery_local",), later)
            later.assert_called_once()
            self.assertEqual(set(runner.not_run), set(acceptance_module.CHAT_CHECKS) - {"live_local"})
            self.assertEqual(set(runner.not_run.values()), {"dependency_failed"})
            with self.assertRaisesRegex(RuntimeError, "failed or unexecuted"):
                runner.finish_checks()

    def test_rtc_failure_continues_with_new_fixture_for_late_join(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
            runner = Acceptance(Path(folder), "scanner")
            fixtures = [Path(folder) / name for name in ("first.json", "second.json", "third.json")]
            for path in fixtures:
                path.write_text("{}")
                path.chmod(0o600)
            with (patch.object(runner, "script", side_effect=list(map(str, fixtures))) as script,
                  patch.object(runner, "test", side_effect=[acceptance_module.CheckFailure("local_rtc", "test_failed"), None, None]) as test):
                runner.rtc_cases()
            self.assertEqual(script.call_count, 3)
            self.assertEqual([call.args[1] for call in test.call_args_list], [path.resolve() for path in fixtures])
            self.assertEqual(runner.failures, [{"check": "local_rtc", "reason": "test_failed"}])

    def test_ring_failure_continues_four_fresh_cases_but_cleanup_failure_is_fatal(self):
        for failure in (acceptance_module.CheckFailure("outgoing-ring-current-dm", "test_failed"),
                        RuntimeError("cleanup failed")):
            with self.subTest(kind=type(failure).__name__), tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
                runner = Acceptance(Path(folder), "scanner")
                fixtures = [Path(folder) / f"{index}.json" for index in range(4)]
                for path in fixtures:
                    path.write_text("{}")
                    path.chmod(0o600)
                with (patch.object(runner, "script", side_effect=list(map(str, fixtures))) as script,
                      patch.object(runner, "run", side_effect=[failure, None, None, None]) as run):
                    if isinstance(failure, acceptance_module.CheckFailure):
                        runner.outgoing_ring_cases()
                        self.assertEqual(run.call_count, 4)
                    else:
                        with self.assertRaises(RuntimeError):
                            runner.outgoing_ring_cases()
                        self.assertEqual(run.call_count, 1)
                self.assertEqual(script.call_count, run.call_count)
                self.assertEqual(len(set(call.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"]
                                         for call in run.call_args_list)), run.call_count)

    def test_oauth_login_failure_skips_restore_and_cleanup_failure_cannot_be_collected(self):
        for cleanup_fails in (False, True):
            with self.subTest(cleanup=cleanup_fails), tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
                runner = Acceptance(Path(folder), "scanner")
                services = Path(folder) / "oauth-services"
                services.mkdir(mode=0o700)
                fixture = services / "accounts.json"
                fixture.write_text("{}")
                fixture.chmod(0o600)
                failure = acceptance_module.CheckFailure("oauth-local-login-refresh", "test_failed")
                with patch.object(runner, "run", side_effect=[str(fixture), failure,
                                                            RuntimeError("private cleanup") if cleanup_fails else None]) as run:
                    if cleanup_fails:
                        with self.assertRaisesRegex(RuntimeError, "cleanup incomplete") as caught:
                            runner.group(acceptance_module.OAUTH_LABELS, runner.oauth_cases)
                        self.assertNotIsInstance(caught.exception, acceptance_module.CheckFailure)
                    else:
                        self.assertFalse(runner.group(acceptance_module.OAUTH_LABELS, runner.oauth_cases))
                        self.assertEqual(runner.not_run, {"oauth-local-restore-refresh": "dependency_failed"})
                self.assertEqual([call.args[0] for call in run.call_args_list],
                                 ["oauth-services-up", "oauth-local-login-refresh", "oauth-services-down"])
                self.assertTrue(run.call_args.kwargs["cleanup"])

    def test_element_cleanup_error_stays_fatal_even_after_collectible_test_failure(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
            runner = Acceptance(Path(folder), "scanner")
            runner.containment_verified = True
            with (patch.object(runner, "script", return_value="unused"),
                  patch.object(acceptance_module, "fixture_from_output", return_value=Path(folder) / "fixture.json"),
                  patch.object(runner, "run", side_effect=[acceptance_module.CheckFailure(acceptance_module.ELEMENT_LABEL, "test_failed"),
                                                         RuntimeError("private cleanup")]) as run,
                  self.assertRaisesRegex(RuntimeError, "cleanup incomplete") as caught):
                runner.group((acceptance_module.ELEMENT_LABEL,), runner.element_case)
            self.assertNotIsInstance(caught.exception, acceptance_module.CheckFailure)
            self.assertEqual(run.call_count, 2)
            self.assertTrue(run.call_args.kwargs["cleanup"])

    def test_inner_scan_and_ring_cleanup_failure_never_become_collectible(self):
        for output, extra in (("MORPHEUS_ELEMENT_STAGE native_test\nMORPHEUS_ELEMENT_FAILURE log_scan\n", {"element_checks": True}),
                              ("running 1 test\nMORPHEUS_RING_CHECK cleanup=false\n", {"ring_case": ("current", "dm")})):
            with self.subTest(extra=extra), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                label = acceptance_module.ELEMENT_LABEL if "element_checks" in extra else "outgoing-ring-current-dm"
                with self.contained_child(runner, output), self.assertRaises(RuntimeError) as caught:
                    runner.run(label, ["test"], pid_namespace=True, **extra)
                self.assertNotIsInstance(caught.exception, acceptance_module.CheckFailure)

    def test_element_setup_deadline_is_fatal_and_only_actual_peer_stages_are_collectible(self):
        from run_element_acceptance import STAGES
        for stage in STAGES:
            with self.subTest(stage=stage), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner")
                output = f"MORPHEUS_ELEMENT_STAGE {stage}\nMORPHEUS_ELEMENT_FAILURE deadline\n"
                with self.contained_child(runner, output, timeout=True), self.assertRaises(RuntimeError) as caught:
                    runner.run(acceptance_module.ELEMENT_LABEL, ["peer-test"], element_checks=True, pid_namespace=True)
                self.assertEqual(isinstance(caught.exception, acceptance_module.CheckFailure),
                                 stage in acceptance_module.ELEMENT_TEST_STAGES)

    def test_budget_reports_unexecuted_checks_without_starting_commands(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()) as visible:
            runner = Acceptance(Path(folder), "scanner", started=100)
            now = 100 + acceptance_module.TOTAL_SECONDS - acceptance_module.WORK_RESERVE
            with patch.object(acceptance_module.time, "monotonic", return_value=now), patch.object(runner, "run") as run:
                self.assertFalse(runner.group(acceptance_module.RING_LABELS, runner.outgoing_ring_cases))
            run.assert_not_called()
            self.assertEqual(runner.not_run, dict.fromkeys(acceptance_module.RING_LABELS, "budget_spent"))
            self.assertIn('"status": "not_run"', visible.getvalue())
            with self.assertRaisesRegex(RuntimeError, "unexecuted"):
                runner.finish_checks()

    def test_work_deadline_caps_child_wait_and_keeps_nested_and_final_cleanup_reserves(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner", started=100)
            now = 100 + acceptance_module.TOTAL_SECONDS - acceptance_module.WORK_RESERVE - acceptance_module.COMMAND_FINISH_RESERVE - 12
            output = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
            with patch.object(acceptance_module.time, "monotonic", return_value=now):
                self.assertEqual(runner.available(), 12)
                self.assertEqual(runner.available(cleanup=True), 192)
                with self.contained_child(runner, output, returncode=0) as (_, _, _, wait, _, _):
                    runner.run("local_rtc", ["test"], test=True, pid_namespace=True, seconds=900)
                self.assertEqual(wait.call_args.args[1], 12)
                runner.finalizing = True
                self.assertEqual(runner.available(cleanup=True), 672)

    def test_summary_records_failures_and_not_run_without_claiming_green(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner")
            runner.failures = [{"check": "local_rtc-late-join", "reason": "test_failed"}]
            runner.not_run = {acceptance_module.ELEMENT_LABEL: "budget_spent"}
            path = Path(folder) / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(path)}):
                runner.summary()
            self.assertIn("Failed: test_failed", path.read_text())
            self.assertIn("Not run: budget_spent", path.read_text())
            self.assertIn("passing rows do not make this run pass", path.read_text())
            self.assertEqual((Path(folder) / "outcomes.json").stat().st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
