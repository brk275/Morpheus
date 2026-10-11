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
from run_element_acceptance import CHECKS as ELEMENT_CHECKS


class LiveEvidenceTests(unittest.TestCase):
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
