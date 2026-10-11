import os
import contextlib
import io
from pathlib import Path
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
                    runner.rtc_cases.assert_not_called()
                    runner.element_case.assert_not_called()
                    self.assertNotIn(call.script("local_matrix_rtc", "probe"), calls)
                else:
                    self.assertLess(calls.index(call.script("local_matrix_rtc", "probe")), calls.index(call.rtc_cases()))

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


if __name__ == "__main__":
    unittest.main()
