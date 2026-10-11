import os
import contextlib
import io
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
        self.assertTrue(all(item.kwargs == {"seconds": 30} for item in run.call_args_list))

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
            self.assertEqual(run.call_args.args[0], "element-process-containment")
            script.assert_not_called()

    def test_contained_timeout_uses_unreaped_anchor_cleanup_without_communicate(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
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
            fixtures = [Path(folder) / name for name in ("normal.json", "late.json")]
            for fixture in fixtures:
                fixture.write_text("{}")
                fixture.chmod(0o600)
            with patch.object(runner, "script", side_effect=[str(path) for path in fixtures]) as script, patch.object(runner, "run") as run:
                runner.rtc_cases()
            self.assertEqual(script.call_count, 2)
            first, second = run.call_args_list
            self.assertEqual(first.args[0], "local_rtc")
            self.assertEqual(second.args[0], "local_rtc-late-join")
            self.assertNotIn("MORPHEUS_RTC_LATE_JOIN", first.kwargs["extra"])
            self.assertEqual(second.kwargs["extra"]["MORPHEUS_RTC_LATE_JOIN"], "1")
            self.assertNotEqual(first.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"],
                                second.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"])
            self.assertEqual(first.args[1], second.args[1], "reuse the same compiled test binary")

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
    def output(self):
        facts = "\n".join(f"MORPHEUS_RING_CHECK {name}=true" for name in sorted(acceptance_module.RING_CHECKS))
        return (f"test {acceptance_module.RING_TEST} ... ok\n{facts}\n"
                "MORPHEUS_RING_RESULT passed=true\n"
                "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 80 filtered out; finished in 1.00s\n")

    def test_exact_named_test_and_every_fixed_fact_are_required(self):
        complete = self.output()
        self.assertEqual(acceptance_module.ring_passed_tests(complete), 1)
        invalid = ["", complete.replace(acceptance_module.RING_TEST, "other::test"),
                   complete.replace(" ... ok", " ... ignored"),
                   complete.replace("0 ignored", "1 ignored"),
                   complete.replace("1 passed", "0 passed"),
                   complete.replace("1 passed", "2 passed"),
                   complete.replace("MORPHEUS_RING_RESULT passed=true", ""),
                   complete + "MORPHEUS_RING_RESULT passed=false\n",
                   complete + "MORPHEUS_RING_FAILURE cleanup\n",
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
                acceptance_module.ring_passed_tests(output)

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
                    "ring_case": case, "seconds": 300,
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

    def test_summary_distinguishes_fetched_events_from_incoming_ringing(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "unused-scanner")
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
                self.assertNotIn("Outgoing invitation rows verify", summary.read_text())
                runner.results.append({"check": "outgoing-ring-current-dm", "passed": 1, "seconds": 10,
                                       "ring_checks": dict.fromkeys(acceptance_module.RING_CHECKS, True)})
                runner.summary()
            text = summary.read_text()
            self.assertIn("fetched and decrypted encrypted events by receipt", text)
            self.assertIn("do not verify incoming ringing", text)


if __name__ == "__main__":
    unittest.main()
