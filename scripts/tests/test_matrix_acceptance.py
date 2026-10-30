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
import run_timeline_acceptance as timeline_module
from run_element_acceptance import CHECKS as ELEMENT_CHECKS


class TimelineFailureForwardingTests(unittest.TestCase):
    def invoke(self, output, *, scanner_code=0, label="timeline_10k"):
        visible = io.StringIO()
        process = Mock(returncode=1)
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "verified-scanner")
            def child(_args, **kwargs):
                kwargs["stdout"].write(output.encode())
                return process
            def scanned(*args, **kwargs):
                self.assertNotIn(timeline_module.SUMMARY_PREFIX, visible.getvalue())
                return subprocess.CompletedProcess([], scanner_code)
            with (patch.object(acceptance_module.subprocess, "Popen", side_effect=child),
                  patch.object(acceptance_module.subprocess, "run", side_effect=scanned) as scan,
                  contextlib.redirect_stdout(visible), self.assertRaises(RuntimeError)):
                runner.run(label, ["synthetic-child"])
            scan.assert_called_once()
        return visible.getvalue()

    def test_failed_command_forwards_only_the_validated_record_after_scanning(self):
        value = timeline_module.StepFailure("enroll", "sliding", "first_render", "first_render").summary()
        raw = "private child text\n" + timeline_module.SUMMARY_PREFIX + json.dumps(value) + "\n"
        output = self.invoke(raw)
        records = [line for line in output.splitlines() if line.startswith(timeline_module.SUMMARY_PREFIX)]
        self.assertEqual(len(records), 1)
        self.assertEqual(json.loads(records[0][len(timeline_module.SUMMARY_PREFIX):]), value)
        self.assertNotIn("private child text", output)

    def test_failed_scanner_withholds_the_summary(self):
        value = timeline_module.StepFailure("enroll", "sliding", "first_render", "first_render").summary()
        output = self.invoke(timeline_module.SUMMARY_PREFIX + json.dumps(value), scanner_code=1)
        self.assertNotIn(timeline_module.SUMMARY_PREFIX, output)

    def test_other_commands_unknown_values_duplicate_records_and_success_are_not_forwarded(self):
        value = timeline_module.StepFailure("enroll", "sliding", "first_render", "first_render").summary()
        valid = timeline_module.SUMMARY_PREFIX + json.dumps(value)
        for raw in (timeline_module.SUMMARY_PREFIX + json.dumps(value | {"failure": "privatepassword"}),
                    timeline_module.SUMMARY_PREFIX + json.dumps(value | {"status": "passed"}),
                    valid + "\n" + valid):
            self.assertNotIn(timeline_module.SUMMARY_PREFIX, self.invoke(raw))
        self.assertNotIn(timeline_module.SUMMARY_PREFIX, self.invoke(valid, label="other_command"))


class FocusedSuiteTests(unittest.TestCase):
    def test_element_interop_requires_its_only_case_and_cannot_claim_full_acceptance(self):
        label = acceptance_module.ELEMENT_LABEL
        self.assertEqual(acceptance_module.SUITES["element-interop"], (label,))
        self.assertIn(label, acceptance_module.RTC_CHECKS)
        self.assertEqual(acceptance_module.SUITES["full"], acceptance_module.PLANNED_CHECKS)
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="element-interop")
            with self.assertRaises(RuntimeError):
                runner.finish_checks()
            runner.results = [{"check": label, "passed": 1, "seconds": 1}]
            runner.finish_checks()
            full = Acceptance(root, "scanner")
            full.results = list(runner.results)
            with self.assertRaises(RuntimeError):
                full.finish_checks()
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
            report = json.loads((root / "outcomes.json").read_text())
            self.assertEqual(report["suite"], "element-interop")
            self.assertEqual(report["not_run"], {})
            text = summary.read_text()
            self.assertIn("only the existing Element peer case ran", text)
            self.assertIn("not full acceptance", text)
            self.assertNotIn("Full suite:", text)
            self.assertNotIn("Focused draft-recovery rerun:", text)

    def test_media_only_completion_and_summary_cannot_claim_full_acceptance(self):
        self.assertEqual(acceptance_module.SUITES["media-only"], ("media_local",))
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="media-only")
            with self.assertRaises(RuntimeError):
                runner.finish_checks()
            runner.results = [{"check": "media_local", "passed": 1, "seconds": 1}]
            runner.finish_checks()
            full = Acceptance(root, "scanner")
            full.results = list(runner.results)
            with self.assertRaises(RuntimeError):
                full.finish_checks()
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
            report = json.loads((root / "outcomes.json").read_text())
            self.assertEqual(report["suite"], "media-only")
            self.assertEqual(report["not_run"], {})
            self.assertIn("Only media_local ran", summary.read_text())
            self.assertIn("not full acceptance", summary.read_text())
            self.assertNotIn("Full suite:", summary.read_text())

    def test_app_timeline_completion_and_summary_cannot_claim_other_checks(self):
        self.assertEqual(acceptance_module.SUITES["app-timeline"], ("app-live-timeline",))
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="app-timeline")
            with self.assertRaises(RuntimeError):
                runner.finish_checks()
            runner.results = [{"check": "app-live-timeline", "passed": 1, "seconds": 1}]
            runner.finish_checks()
            full = Acceptance(root, "scanner")
            full.results = list(runner.results)
            with self.assertRaises(RuntimeError):
                full.finish_checks()
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
            report = json.loads((root / "outcomes.json").read_text())
            self.assertEqual(report["suite"], "app-timeline")
            self.assertEqual(report["not_run"], {})
            text = summary.read_text()
            self.assertIn("Only app-live-timeline ran", text)
            self.assertIn("member publication and cleanup", text)
            self.assertIn("No draft recovery, moderation, OAuth or RTC checks", text)
            self.assertIn("not full acceptance", text)
            self.assertNotIn("Full suite:", text)
            self.assertNotIn("Focused draft-recovery rerun:", text)
            self.assertNotIn("formatting prerequisite", text)

    def test_completion_requires_every_selected_check_and_default_still_requires_full(self):
        with tempfile.TemporaryDirectory() as folder:
            focused = Acceptance(Path(folder), "scanner", suite="draft-recovery")
            focused.results = [{"check": label} for label in acceptance_module.DRAFT_RECOVERY_CHECKS]
            focused.finish_checks()
            full = Acceptance(Path(folder), "scanner")
            full.results = list(focused.results)
            with self.assertRaisesRegex(RuntimeError, "failed or unexecuted"):
                full.finish_checks()
            for missing in acceptance_module.DRAFT_RECOVERY_CHECKS:
                focused.results = [{"check": label} for label in acceptance_module.DRAFT_RECOVERY_CHECKS
                                   if label != missing]
                with self.subTest(missing=missing), self.assertRaises(RuntimeError):
                    focused.finish_checks()

    def test_drafts_only_completion_and_summary_cannot_claim_other_checks(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="drafts-only")
            with self.assertRaises(RuntimeError):
                runner.finish_checks()
            runner.results = [{"check": "drafts_local", "passed": 2, "seconds": 1}]
            runner.finish_checks()
            full = Acceptance(root, "scanner")
            full.results = list(runner.results)
            with self.assertRaises(RuntimeError):
                full.finish_checks()
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
            report = json.loads((root / "outcomes.json").read_text())
            self.assertEqual(report["suite"], "drafts-only")
            self.assertEqual(report["not_run"], {})
            self.assertIn("No app recovery, moderation, OAuth or RTC checks", summary.read_text())
            self.assertIn("not full acceptance", summary.read_text())

    def test_timeline_read_completion_requires_all_selected_checks_and_reports_partial_scope(self):
        expected = ("live_local", "formatted_local", "media_local", "moderation_local",
                    "search_local", "search_local-classic", "recovery_local", "unsupported_transport_local", "app-live-timeline")
        self.assertEqual(acceptance_module.TIMELINE_READ_CHECKS, expected)
        self.assertEqual(acceptance_module.SUITES["full"], acceptance_module.PLANNED_CHECKS)
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-reads")
            for missing in expected:
                runner.results = [{"check": label} for label in expected if label != missing]
                with self.subTest(missing=missing), self.assertRaises(RuntimeError):
                    runner.finish_checks()
            runner.results = [{"check": label, "passed": 1, "seconds": 1} for label in expected]
            runner.finish_checks()
            full = Acceptance(root, "scanner")
            full.results = list(runner.results)
            with self.assertRaises(RuntimeError):
                full.finish_checks()
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
            report = json.loads((root / "outcomes.json").read_text())
            self.assertEqual(report["suite"], "timeline-reads")
            self.assertEqual(report["not_run"], {})
            self.assertIn("not full acceptance", summary.read_text())
            self.assertNotIn("Full suite:", summary.read_text())

    def test_timeline_read_order_keeps_both_sync_paths_and_recovery_fixture(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-reads")
            fixture, recovery, gate = root / "fixture.json", root / "recovery.json", root / "rtc-gate.json"
            with patch.object(runner, "test") as test, \
                    patch.object(runner, "script", side_effect=["private-recovery", "private-gate"]) as script, \
                    patch.object(acceptance_module, "fixture_from_output", side_effect=[recovery, gate]) as read, \
                    patch.object(runner, "app_timeline") as app:
                runner.timeline_read_cases(fixture)
            self.assertEqual([call.args for call in test.call_args_list], [
                ("live_local", fixture), ("formatted_local", fixture), ("media_local", fixture),
                ("moderation_local", fixture), ("search_local", fixture),
                ("search_local", fixture), ("recovery_local", recovery), ("unsupported_transport_local", gate),
            ])
            self.assertEqual([call.kwargs for call in test.call_args_list], [
                {}, {}, {}, {}, {}, {"extra": {"MORPHEUS_MATRIX_SEARCH_CLASSIC": "1"}},
                {"extra": {"MORPHEUS_MATRIX_TEST_RECOVERY": "1"}},
                {"rtc": True, "extra": {"MORPHEUS_MATRIX_RTC_GATE_TEST_ACCOUNTS": str(gate)}},
            ])
            self.assertEqual([call.args for call in script.call_args_list], [
                ("local_matrix_env", "recovery-fixture"), ("local_matrix_env", "rtc-gate-fixture"),
            ])
            self.assertEqual([call.args for call in read.call_args_list], [
                ("private-recovery", root), ("private-gate", root),
            ])
            app.assert_called_once_with(fixture)
            self.assertEqual(runner.not_run, {})

    def test_timeline_failed_formatting_skips_moderation_without_hiding_failure(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-reads")
            failure = acceptance_module.CheckFailure("formatted_local", "test_failed")
            with patch.object(runner, "test", side_effect=[None, failure, None, None, None, None, None]) as test, \
                    patch.object(runner, "script", return_value="private-fixture"), \
                    patch.object(acceptance_module, "fixture_from_output", return_value=root / "recovery.json"), \
                    patch.object(runner, "app_timeline") as app:
                runner.timeline_read_cases(root / "fixture.json")
            self.assertEqual([call.args[0] for call in test.call_args_list], [
                "live_local", "formatted_local", "media_local", "search_local", "search_local", "recovery_local", "unsupported_transport_local",
            ])
            app.assert_called_once_with(root / "fixture.json")
            self.assertEqual(runner.not_run, {"moderation_local": "dependency_failed"})
            self.assertEqual(runner.failures, [{"check": "formatted_local", "reason": "test_failed"}])
            with self.assertRaises(RuntimeError):
                runner.finish_checks()

    def test_unknown_suite_rejected_before_creating_directories(self):
        with patch.object(acceptance_module, "private_directory") as directory:
            with self.assertRaisesRegex(ValueError, "Unknown acceptance suite"):
                Acceptance(Path("unused"), "scanner", suite="arbitrary")
        directory.assert_not_called()

    def test_focused_order_includes_moderation_prerequisite_and_all_app_guards(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner", suite="draft-recovery")
            fixture = Path(folder) / "fixture.json"
            with patch.object(runner, "test") as test, patch.object(runner, "app_recovery") as app:
                runner.draft_recovery_cases(fixture)
            self.assertEqual([call.args for call in test.call_args_list], [
                ("drafts_local", fixture), ("formatted_local", fixture), ("moderation_local", fixture),
            ])
            app.assert_called_once_with(fixture)
            self.assertEqual(runner.not_run, {})

    def test_failed_prerequisite_skips_moderation_but_runs_independent_recovery(self):
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(io.StringIO()):
            runner = Acceptance(Path(folder), "scanner", suite="draft-recovery")
            failure = acceptance_module.CheckFailure("formatted_local", "test_failed")
            with patch.object(runner, "test", side_effect=[None, failure]) as test, \
                    patch.object(runner, "app_recovery") as app:
                runner.draft_recovery_cases(Path(folder) / "fixture.json")
            self.assertEqual(test.call_count, 2)
            app.assert_called_once()
            self.assertEqual(runner.not_run, {"moderation_local": "dependency_failed"})
            with self.assertRaises(RuntimeError):
                runner.finish_checks()

    def test_summary_labels_focused_evidence_and_does_not_require_unselected_checks(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="draft-recovery")
            runner.results = [{"check": label, "passed": 1, "seconds": 1}
                              for label in acceptance_module.DRAFT_RECOVERY_CHECKS]
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
            report = json.loads((root / "outcomes.json").read_text())
            self.assertEqual(report["suite"], "draft-recovery")
            self.assertEqual(report["not_run"], {})
            self.assertIn("not full acceptance", summary.read_text())
            self.assertNotIn("Full suite:", summary.read_text())


class TimelineFailureSuiteTests(unittest.TestCase):
    def test_closed_retry_requires_all_three_cases_and_cannot_satisfy_full_acceptance(self):
        expected = ("formatted_local", "moderation_local", "recovery_local")
        self.assertEqual(acceptance_module.SUITES["timeline-failures"], expected)
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-failures")
            for missing in expected:
                runner.results = [{"check": label} for label in expected if label != missing]
                with self.subTest(missing=missing), self.assertRaises(RuntimeError):
                    runner.finish_checks()
            runner.results = [{"check": label} for label in expected]
            runner.finish_checks()
            full = Acceptance(root, "scanner")
            full.results = list(runner.results)
            with self.assertRaises(RuntimeError):
                full.finish_checks()

    def test_retry_preserves_moderation_prerequisite_and_fresh_recovery_credentials(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-failures")
            fixture, recovery = root / "fixture.json", root / "recovery.json"
            with patch.object(runner, "test") as test, \
                    patch.object(runner, "script", return_value="private-fixture") as script, \
                    patch.object(acceptance_module, "fixture_from_output", return_value=recovery) as read:
                runner.timeline_failure_cases(fixture)
            self.assertEqual([call.args for call in test.call_args_list], [
                ("formatted_local", fixture), ("moderation_local", fixture), ("recovery_local", recovery),
            ])
            self.assertEqual([call.kwargs for call in test.call_args_list], [
                {}, {}, {"extra": {"MORPHEUS_MATRIX_TEST_RECOVERY": "1"}},
            ])
            script.assert_called_once_with("local_matrix_env", "recovery-fixture")
            read.assert_called_once_with("private-fixture", root)
            self.assertEqual(runner.failures, [])
            self.assertEqual(runner.not_run, {})

    def test_formatting_failure_blocks_moderation_but_does_not_hide_recovery(self):
        visible = io.StringIO()
        with tempfile.TemporaryDirectory() as folder, contextlib.redirect_stdout(visible):
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-failures")
            failure = acceptance_module.CheckFailure("formatted_local", "test_failed")
            with patch.object(runner, "test", side_effect=[failure, None]) as test, \
                    patch.object(runner, "script", return_value="private-fixture"), \
                    patch.object(acceptance_module, "fixture_from_output", return_value=root / "recovery.json"):
                runner.timeline_failure_cases(root / "fixture.json")
            self.assertEqual([call.args[0] for call in test.call_args_list], ["formatted_local", "recovery_local"])
            self.assertEqual(runner.not_run, {"moderation_local": "dependency_failed"})
            self.assertEqual(runner.failures, [{"check": "formatted_local", "reason": "test_failed"}])
            with self.assertRaises(RuntimeError):
                runner.finish_checks()
        self.assertNotIn("private-fixture", visible.getvalue())

    def test_summary_names_only_the_selected_retry_scope(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-failures")
            runner.results = [{"check": label, "passed": 1, "seconds": 1}
                              for label in acceptance_module.TIMELINE_FAILURE_CHECKS]
            summary = root / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(summary)}):
                runner.summary()
            report = json.loads((root / "outcomes.json").read_text())
            self.assertEqual(report["suite"], "timeline-failures")
            self.assertEqual(report["not_run"], {})
            self.assertIn("not full acceptance", summary.read_text())
            self.assertNotIn("Full suite:", summary.read_text())


class AppTimelineEvidenceTests(unittest.TestCase):
    @staticmethod
    def output():
        # Representative records from the reviewed native harness, including
        # the real queued_drain phase. Keep these independent of the parser's
        # allowlists so an incompatible whitelist fails this contract.
        records = (
            "STAGE setup", "STAGE setup_profile", "STAGE setup_config",
            "STAGE setup_window", "STAGE setup_controller", "STAGE setup_dispatch",
            "STAGE setup_login", "STAGE setup_rooms", "STAGE setup_ready",
            "STAGE activation", "STAGE room_open",
            "STAGE room_delivery", "CHECK room_delivery=true",
            "STAGE thread_delivery", "CHECK thread_delivery=true",
            "CHECK thread_root_excluded=true",
            "STAGE message_copy_callbacks", "CHECK message_copy_callbacks=true",
            "CHECK feeds_running=true",
            "STAGE revocation", "STAGE revocation_settle", "STAGE revocation_member_pause",
            "STAGE revocation_login", "STAGE revocation_members", "STAGE revocation_join",
            "CHECK old_lease_revoked=true", "STAGE revocation_feeds",
            "CHECK idle_feeds_stopped=true", "STAGE renewal",
            "CHECK message_copy_account_aba=true",
            "CHECK late_events_in_core=true", "CHECK stale_room_rejected=true",
            "CHECK stale_thread_rejected=true", "STAGE reopen",
            "CHECK renewed_room_delivery=true", "CHECK message_copy_renewed_delivery=true",
            "CHECK renewed_thread_delivery=true",
            "STAGE queued_revocation", "CHECK queued_callbacks_observed=true",
            "STAGE queued_drain", "CHECK queued_room_rejected=true",
            "CHECK queued_thread_rejected=true", "STAGE queued_reopen",
            "STAGE link_continuation", "CHECK link_follow_up_stale_retry_rejected=true",
            "CHECK link_follow_up_owned_retry=true",
            "CHECK link_follow_up_held_focus_rejected=true",
            "CHECK link_follow_up_owned_focus=true",
            "STAGE member_publication", "CHECK member_publication=true",
            "STAGE cleanup", "STAGE cleanup_logout_primary", "STAGE cleanup_logout_other",
            "STAGE cleanup_manager_stop", "STAGE cleanup_drop_live", "STAGE cleanup_drop_ui",
            "STAGE cleanup_wait_profile", "STAGE cleanup_drop_manager", "STAGE cleanup_remove_directory",
            "CHECK cleanup_complete=true", "STAGE complete",
        )
        lines = ["running 1 test"]
        lines.extend("MORPHEUS_LIVE_TIMELINE_" + record for record in records)
        lines.append("test result: ok. 1 passed; 0 failed; 0 ignored;")
        return "\n".join(lines) + "\n"

    def test_one_harness_requires_all_twenty_three_observable_facts_and_cleanup(self):
        checks = acceptance_module.app_timeline_evidence(self.output())
        self.assertEqual(len(checks), 23)
        self.assertEqual(set(checks), acceptance_module.APP_TIMELINE_CHECKS)
        self.assertTrue(all(checks.values()))
        self.assertIn("queued_callbacks_observed", checks)
        self.assertIn("queued_room_rejected", checks)
        self.assertIn("queued_thread_rejected", checks)
        self.assertIn("link_follow_up_stale_retry_rejected", checks)
        self.assertIn("link_follow_up_held_focus_rejected", checks)
        self.assertIn("member_publication", checks)
        self.assertIn("message_copy_callbacks", checks)
        self.assertIn("message_copy_account_aba", checks)
        self.assertIn("message_copy_renewed_delivery", checks)
        self.assertIn("member_publication", acceptance_module.APP_TIMELINE_STAGES)
        self.assertIn("cleanup_complete", checks)

    def test_every_missing_fact_is_fatal_even_after_a_green_test_summary(self):
        for name in acceptance_module.APP_TIMELINE_CHECKS:
            incomplete = self.output().replace(f"MORPHEUS_LIVE_TIMELINE_CHECK {name}=true\n", "")
            with self.subTest(missing=name), self.assertRaisesRegex(RuntimeError, "Live timeline UI evidence is incomplete"):
                acceptance_module.app_timeline_evidence(incomplete)

    def test_false_duplicate_unknown_and_malformed_records_never_pass(self):
        line = "MORPHEUS_LIVE_TIMELINE_CHECK room_delivery=true"
        for replacement in (line.replace("true", "false"), line + "\n" + line,
                            "MORPHEUS_LIVE_TIMELINE_CHECK unknown=true",
                            "MORPHEUS_LIVE_TIMELINE_CHECK room_delivery=1",
                            line + " private-server-value", "MORPHEUS_LIVE_TIMELINE_CHECK private=secret",
                            "MORPHEUS_LIVE_TIMELINE_CHECK"):
            with self.subTest(value=replacement), self.assertRaises(RuntimeError) as caught:
                acceptance_module.app_timeline_evidence(self.output().replace(line, replacement))
            self.assertNotIn("private", str(caught.exception))
            self.assertNotIn("secret", str(caught.exception))

    def test_member_publication_fact_cannot_be_false_duplicated_or_malformed(self):
        line = "MORPHEUS_LIVE_TIMELINE_CHECK member_publication=true"
        for replacement in (line.replace("true", "false"), line + "\n" + line,
                            "MORPHEUS_LIVE_TIMELINE_CHECK member_publication=1",
                            "MORPHEUS_LIVE_TIMELINE_CHECK member_publication_unknown=true"):
            with self.subTest(record=replacement), self.assertRaises(RuntimeError):
                acceptance_module.app_timeline_evidence(self.output().replace(line, replacement))

    def test_missing_or_repeated_lifecycle_markers_cannot_stand_in_for_completion(self):
        setup = "MORPHEUS_LIVE_TIMELINE_STAGE setup\n"
        complete = "MORPHEUS_LIVE_TIMELINE_STAGE complete\n"
        outputs = (self.output().replace(setup, ""), self.output().replace(complete, ""),
                   self.output().replace(setup, setup + setup), self.output().replace(complete, complete + complete),
                   self.output().replace(setup, "MORPHEUS_LIVE_TIMELINE_STAGE private-secret\n"))
        for output in outputs:
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                acceptance_module.app_timeline_evidence(output)

    def test_completion_must_follow_assertions_and_setup_must_precede_them(self):
        setup = "MORPHEUS_LIVE_TIMELINE_STAGE setup\n"
        complete = "MORPHEUS_LIVE_TIMELINE_STAGE complete\n"
        outputs = (complete + self.output().replace(complete, ""),
                   self.output().replace(setup, "") + setup)
        for output in outputs:
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                acceptance_module.app_timeline_evidence(output)

    def test_wrong_count_or_skipped_harness_is_not_live_coverage(self):
        summary = "test result: ok. 1 passed; 0 failed; 0 ignored;"
        for replacement in ("test result: ok. 0 passed; 0 failed; 1 ignored;",
                            "test result: ok. 2 passed; 0 failed; 0 ignored;", ""):
            with self.subTest(summary=replacement), self.assertRaises(RuntimeError):
                acceptance_module.app_timeline_evidence(self.output().replace(summary, replacement))

    def test_excessive_stage_records_are_rejected_before_unbounded_proof_growth(self):
        expanded = self.output().replace("MORPHEUS_LIVE_TIMELINE_STAGE complete",
                                       "MORPHEUS_LIVE_TIMELINE_STAGE queued_drain\n" * 65
                                       + "MORPHEUS_LIVE_TIMELINE_STAGE complete")
        with self.assertRaises(RuntimeError):
            acceptance_module.app_timeline_evidence(expanded)

    def test_diagnostics_reveal_only_fixed_entered_and_stage_values(self):
        output = "private-token\nMORPHEUS_LIVE_TIMELINE_STAGE setup\nMORPHEUS_LIVE_TIMELINE_STAGE queued_drain\nMORPHEUS_LIVE_TIMELINE_STAGE private_token\n"
        self.assertEqual(acceptance_module.app_timeline_diagnostics(output),
                         {"test_entered": True, "stage": "queued_drain"})
        self.assertEqual(acceptance_module.app_timeline_diagnostics("private-token"), {"test_entered": False})
        for stage in (
            "setup_profile", "setup_config", "setup_window", "setup_controller",
            "setup_dispatch", "setup_login", "setup_rooms", "setup_ready",
            "cleanup_logout_primary", "cleanup_logout_other", "cleanup_manager_stop",
            "cleanup_drop_live", "cleanup_drop_ui", "cleanup_wait_profile",
            "cleanup_drop_manager", "cleanup_remove_directory",
        ):
            prefix = "MORPHEUS_LIVE_TIMELINE_STAGE setup\n"
            marker = f"MORPHEUS_LIVE_TIMELINE_STAGE {stage}"
            self.assertEqual(acceptance_module.app_timeline_diagnostics(prefix + marker + "\n"),
                             {"test_entered": True, "stage": stage})
            self.assertEqual(acceptance_module.app_timeline_diagnostics(prefix + marker + " private-token\n"),
                             {"test_entered": True, "stage": "setup"})

    def test_tail_diagnostics_are_closed_bounded_and_do_not_replace_completion(self):
        prefix = "MORPHEUS_LIVE_TIMELINE_STAGE setup\nMORPHEUS_LIVE_TIMELINE_STAGE complete\n"
        for stage in acceptance_module.APP_TIMELINE_TAIL_STAGES:
            marker = f"MORPHEUS_LIVE_TIMELINE_TAIL {stage}\n"
            self.assertEqual(acceptance_module.app_timeline_diagnostics(prefix + marker),
                             {"test_entered": True, "stage": "complete", "tail_stage": stage})
        for suffix in ("private_token", "people_login private-token", "x" * 49):
            self.assertEqual(acceptance_module.app_timeline_diagnostics(
                prefix + f"MORPHEUS_LIVE_TIMELINE_TAIL {suffix}\n")["tail_stage"], "invalid")
        marker = "MORPHEUS_LIVE_TIMELINE_TAIL people_login\n"
        self.assertEqual(acceptance_module.app_timeline_diagnostics(prefix + marker * 17)["tail_stage"],
                         "invalid")
        # Tail diagnostics cannot manufacture a passed test, missing check or completion.
        self.assertEqual(acceptance_module.app_timeline_evidence(self.output() + marker),
                         {name: True for name in sorted(acceptance_module.APP_TIMELINE_CHECKS)})
        for output in (marker, self.output().replace(
                "MORPHEUS_LIVE_TIMELINE_STAGE complete", marker)):
            with self.assertRaises(RuntimeError):
                acceptance_module.app_timeline_evidence(output)

    def test_failure_progress_reports_observations_without_claiming_an_active_phase(self):
        progress = ("running 1 test\n"
                    "MORPHEUS_UI_PHASE phase=97 elapsed_ms=100 phase_ms=2\n"
                    "MORPHEUS_UI_PHASE phase=15 elapsed_ms=102 phase_ms=0\n")
        self.assertEqual(acceptance_module.app_timeline_failure_progress(progress), {
            "native_harness_entered": True, "common_ui_progress": {
                "records": 2, "observation_cap_reached": False,
                "last_observed_phase": 15, "elapsed_ms": 102, "phase_ms": 0}})
        self.assertEqual(acceptance_module.app_timeline_diagnostics(progress), {"test_entered": False})
        with self.assertRaises(RuntimeError):
            acceptance_module.app_timeline_evidence(progress)
        self.assertEqual(acceptance_module.app_timeline_evidence(self.output() + progress),
                         {name: True for name in sorted(acceptance_module.APP_TIMELINE_CHECKS)})

    def test_failure_progress_rejects_spoofs_duplicate_fields_and_unbounded_records(self):
        marker = "MORPHEUS_UI_PHASE phase=0 elapsed_ms=1 phase_ms=1\n"
        prefix = "running 1 test\n"
        # Repeated values may come from legitimate millisecond observations.
        # Duplicates still consume the exact64-record source bound.
        observed = acceptance_module.app_timeline_failure_progress(prefix + marker * 64)
        self.assertEqual(observed["common_ui_progress"]["records"], 64)
        self.assertTrue(observed["common_ui_progress"]["observation_cap_reached"])
        invalid = {"native_harness_entered": True, "common_ui_progress": {"invalid": True}}
        self.assertEqual(acceptance_module.app_timeline_failure_progress(prefix + marker * 65), invalid)
        malformed = (
            marker * 65,
            "MORPHEUS_UI_PHASE phase=1 phase=1 elapsed_ms=1 phase_ms=1\n",
            "MORPHEUS_UI_PHASE phase=True elapsed_ms=1 phase_ms=1\n",
            "MORPHEUS_UI_PHASE phase=01 elapsed_ms=1 phase_ms=1\n",
            "MORPHEUS_UI_PHASE phase=-1 elapsed_ms=1 phase_ms=1\n",
            "MORPHEUS_UI_PHASE phase=101 elapsed_ms=1 phase_ms=1\n",
            "MORPHEUS_UI_PHASE phase=1 elapsed_ms=600001 phase_ms=1\n",
            "MORPHEUS_UI_PHASE phase=1 elapsed_ms=1 phase_ms=2\n",
            "MORPHEUS_UI_PHASE phase=1 elapsed_ms=1 phase_ms=600001\n",
            "MORPHEUS_UI_PHASE phase=1 elapsed_ms=1.0 phase_ms=1\n",
            "MORPHEUS_UI_PHASE phase=1 elapsed_ms=1 phase_ms=1 PRIVATE\n",
            "MORPHEUS_UI_PHASE phase=" + "9" * 1000 + "\n",
            "MORPHEUS_UI_PHASE phase=1 elapsed_ms=2 phase_ms=1\n" + marker,
        )
        for record in malformed:
            with self.subTest(record=record):
                self.assertEqual(acceptance_module.app_timeline_failure_progress(
                    prefix + marker + record + marker), invalid)
        spoofed = "PRIVATE " + marker + "quoted running 1 test\n"
        self.assertEqual(acceptance_module.app_timeline_failure_progress(spoofed), {
            "native_harness_entered": False,
            "common_ui_progress": {"records": 0, "observation_cap_reached": False}})
        self.assertEqual(acceptance_module.app_timeline_failure_progress(
            prefix + "MORPHEUS_UI_PHASE phase=100 elapsed_ms=600000 phase_ms=600000\n")
            ["common_ui_progress"]["last_observed_phase"], 100)

    def test_failure_progress_rejects_non_text_without_coercion(self):
        class UnsafeText(str):
            def splitlines(self, *args):
                raise AssertionError("must not invoke supplied text methods")
        for value in (None, True, 1, 1.0, b"private", {}, UnsafeText("private")):
            with self.subTest(value=type(value)):
                self.assertEqual(acceptance_module.app_timeline_failure_progress(value), {"invalid": True})

    def test_common_ui_progress_is_forwarded_only_after_scanned_failure(self):
        progress = ("running 1 test\nMORPHEUS_UI_PHASE phase=97 elapsed_ms=100 phase_ms=2\n"
                    "PRIVATE_CHILD_PAYLOAD\n")
        for scanner_code in (0, 1):
            with self.subTest(scanner_code=scanner_code), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with LiveStageTests.child(self, runner, progress, timeout=True,
                                          scanner_code=scanner_code) as (visible, _):
                    with self.assertRaisesRegex(RuntimeError, "deadline" if scanner_code == 0 else "credential scanning"):
                        runner.run(acceptance_module.APP_TIMELINE_LABEL, ["mock-test"], test=True, seconds=1)
                records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
                if scanner_code == 0:
                    self.assertEqual(records[0]["app_timeline_failure_progress"],
                                     acceptance_module.app_timeline_failure_progress(progress))
                else:
                    self.assertEqual(records, [])
                self.assertNotIn("PRIVATE_CHILD_PAYLOAD", visible.getvalue())
                self.assertEqual(runner.results, [])
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with LiveStageTests.child(self, runner, self.output() + progress) as (visible, _):
                runner.run(acceptance_module.APP_TIMELINE_LABEL, ["mock-test"], test=True)
            self.assertEqual(len(runner.results), 1)
            self.assertNotIn("app_timeline_failure_progress", runner.results[0])
            self.assertNotIn("app_timeline_failure_progress", visible.getvalue())

    def test_validated_record_requires_native_test_mode_and_attaches_safe_facts(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner", suite="timeline-reads")
            arguments = {"ring_case": None, "oauth_checks": None, "element_checks": None}
            record = runner.validated_record(acceptance_module.APP_TIMELINE_LABEL, self.output(), test=True, **arguments)
            self.assertEqual(record["passed"], 1)
            self.assertEqual(len(record["app_timeline_checks"]), 23)
            self.assertTrue(all(record["app_timeline_checks"].values()))
            with self.assertRaises(RuntimeError):
                runner.validated_record(acceptance_module.APP_TIMELINE_LABEL, self.output(), test=False, **arguments)
            with self.assertRaises(RuntimeError):
                runner.validated_record(acceptance_module.APP_TIMELINE_LABEL,
                                        "test result: ok. 1 passed; 0 failed; 0 ignored;", test=True, **arguments)

    def test_wrapper_uses_exact_ui_harness_fixture_and_process_containment(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            runner = Acceptance(root, "scanner", suite="timeline-reads")
            fixture = root / "fixture.json"
            with patch.object(runner, "run") as run:
                runner.app_timeline(fixture)
            args, options = run.call_args
            self.assertEqual(args[0], acceptance_module.APP_TIMELINE_LABEL)
            self.assertEqual(args[1], ["cargo", "test", "--locked", "-p", "morpheus-app", "--features", "native-calls",
                                      "--bin", "morpheus-app", acceptance_module.APP_TIMELINE_CASE,
                                      "--", "--exact", "--nocapture", "--test-threads=1"])
            self.assertEqual(options, {"extra": {"MORPHEUS_MATRIX_TEST_ACCOUNTS": str(fixture),
                                                "SLINT_EMIT_DEBUG_INFO": "1"},
                                       "seconds": 480, "test": True, "pid_namespace": True})

    def test_live_ui_case_remains_required_in_full_and_timeline_profiles(self):
        for suite in ("full", "timeline-reads"):
            with tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "scanner", suite=suite)
                self.assertIn(acceptance_module.APP_TIMELINE_LABEL, runner.planned_checks)
                runner.results = [{"check": label} for label in runner.planned_checks
                                  if label != acceptance_module.APP_TIMELINE_LABEL]
                with self.subTest(suite=suite), self.assertRaises(RuntimeError):
                    runner.finish_checks()
        self.assertNotIn(acceptance_module.APP_TIMELINE_LABEL, acceptance_module.DRAFT_RECOVERY_CHECKS)
        self.assertNotIn(acceptance_module.APP_TIMELINE_LABEL, acceptance_module.DRAFT_ONLY_CHECKS)


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

    def test_focused_suite_uses_only_its_declared_checks_and_preserves_cleanup(self):
        with self.mocked_main() as (runner, _):
            with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner",
                                           "--suite", "draft-recovery"]):
                self.assertIsNone(acceptance_module.main())
        runner.draft_recovery_cases.assert_called_once()
        runner.finish_checks.assert_called_once()
        runner.test.assert_not_called()
        runner.app_recovery.assert_not_called()
        runner.oauth_cases.assert_not_called()
        runner.outgoing_ring_cases.assert_not_called()
        runner.rtc_cases.assert_not_called()
        runner.element_case.assert_not_called()
        self.assert_both_cleanups(runner)

    def test_element_interop_runs_only_the_existing_case_and_preserves_cleanup_on_failure(self):
        for failure_at in (None, "element_case", "finish_checks"):
            with self.subTest(failure_at=failure_at), self.mocked_main() as (runner, _):
                failure = RuntimeError("fixed incomplete Element result")
                if failure_at:
                    getattr(runner, failure_at).side_effect = failure
                with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner",
                                               "--suite", "element-interop"]):
                    if failure_at:
                        with self.assertRaises(RuntimeError) as caught:
                            acceptance_module.main()
                        self.assertIs(caught.exception, failure)
                    else:
                        self.assertIsNone(acceptance_module.main())
                runner.group.assert_called_once()
                self.assertEqual(runner.group.call_args.args[0], (acceptance_module.ELEMENT_LABEL,))
                runner.element_case.assert_called_once_with()
                if failure_at == "element_case":
                    runner.finish_checks.assert_not_called()
                else:
                    runner.finish_checks.assert_called_once_with()
                self.assertEqual([call.args for call in runner.script.call_args_list], [
                    ("local_matrix_env", "up"), ("local_matrix_rtc", "up"),
                    ("local_matrix_rtc", "probe"), ("local_matrix_rtc", "down"),
                    ("local_matrix_env", "down"),
                ])
                runner.start_keyring.assert_called_once_with()
                runner.require_containment.assert_called_once_with()
                runner.test.assert_not_called()
                runner.app_timeline.assert_not_called()
                runner.app_recovery.assert_not_called()
                runner.oauth_cases.assert_not_called()
                runner.outgoing_ring_cases.assert_not_called()
                runner.rtc_cases.assert_not_called()
                runner.rtc_recovery_cases.assert_not_called()
                runner.draft_recovery_cases.assert_not_called()
                runner.timeline_read_cases.assert_not_called()
                self.assert_both_cleanups(runner)

    def test_oauth_only_runs_all_existing_phases_without_seeded_or_rtc_services_and_keeps_cleanup(self):
        for failure_at in (None, "oauth_cases", "finish_checks"):
            with self.subTest(failure_at=failure_at), self.mocked_main() as (runner, _):
                failure = RuntimeError("fixed incomplete OAuth result")
                if failure_at:
                    getattr(runner, failure_at).side_effect = failure
                with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner", "--suite", "oauth-only"]):
                    if failure_at:
                        with self.assertRaises(RuntimeError) as caught:
                            acceptance_module.main()
                        self.assertIs(caught.exception, failure)
                    else:
                        self.assertIsNone(acceptance_module.main())
                runner.group.assert_called_once_with(acceptance_module.OAUTH_LABELS, runner.oauth_cases)
                runner.oauth_cases.assert_called_once_with()
                runner.start_keyring.assert_called_once_with()
                runner.require_containment.assert_called_once_with()
                self.assertEqual(acceptance_module.SUITES["oauth-only"], acceptance_module.OAUTH_LABELS)
                self.assertEqual([call.args for call in runner.script.call_args_list],
                                 [("local_matrix_rtc", "down"), ("local_matrix_env", "down")])
                runner.test.assert_not_called()
                runner.app_timeline.assert_not_called()
                runner.app_recovery.assert_not_called()
                runner.rtc_cases.assert_not_called()
                runner.outgoing_ring_cases.assert_not_called()
                runner.element_case.assert_not_called()
                self.assert_both_cleanups(runner)

    def test_drafts_only_main_runs_only_drafts_and_preserves_cleanup(self):
        with self.mocked_main() as (runner, _):
            with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner",
                                           "--suite", "drafts-only"]):
                self.assertIsNone(acceptance_module.main())
        self.assertEqual(len(runner.test.call_args_list), 1)
        self.assertEqual(runner.test.call_args.args[0], "drafts_local")
        runner.finish_checks.assert_called_once()
        runner.draft_recovery_cases.assert_not_called()
        runner.app_recovery.assert_not_called()
        runner.oauth_cases.assert_not_called()
        runner.rtc_cases.assert_not_called()
        runner.outgoing_ring_cases.assert_not_called()
        runner.element_case.assert_not_called()
        self.assert_both_cleanups(runner)

    def test_app_timeline_selection_keeps_cleanup_after_success_or_incomplete_results(self):
        for failure_at in (None, "app_timeline", "finish_checks"):
            with self.subTest(failure_at=failure_at), self.mocked_main() as (runner, _):
                failure = RuntimeError("fixed incomplete live UI result")
                if failure_at:
                    getattr(runner, failure_at).side_effect = failure
                with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner",
                                               "--suite", "app-timeline"]):
                    if failure_at:
                        with self.assertRaises(RuntimeError) as caught:
                            acceptance_module.main()
                        self.assertIs(caught.exception, failure)
                    else:
                        self.assertIsNone(acceptance_module.main())
                runner.app_timeline.assert_called_once()
                runner.test.assert_not_called()
                runner.timeline_read_cases.assert_not_called()
                runner.draft_recovery_cases.assert_not_called()
                runner.app_recovery.assert_not_called()
                runner.oauth_cases.assert_not_called()
                runner.rtc_cases.assert_not_called()
                runner.outgoing_ring_cases.assert_not_called()
                runner.element_case.assert_not_called()
                self.assertNotIn(("local_matrix_rtc", "up"),
                                 [call.args for call in runner.script.call_args_list])
                self.assert_both_cleanups(runner)

    def test_media_only_main_runs_only_media_and_preserves_cleanup(self):
        with self.mocked_main() as (runner, _):
            with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner", "--suite", "media-only"]):
                self.assertIsNone(acceptance_module.main())
        runner.test.assert_called_once()
        self.assertEqual(runner.test.call_args.args[0], "media_local")
        runner.finish_checks.assert_called_once()
        runner.timeline_read_cases.assert_not_called()
        runner.draft_recovery_cases.assert_not_called()
        runner.app_recovery.assert_not_called()
        runner.oauth_cases.assert_not_called()
        runner.rtc_cases.assert_not_called()
        runner.outgoing_ring_cases.assert_not_called()
        runner.element_case.assert_not_called()
        self.assert_both_cleanups(runner)

    def test_media_only_incomplete_results_still_fail_after_cleanup(self):
        original = RuntimeError("fixed incomplete media result")
        with self.mocked_main() as (runner, _):
            runner.finish_checks.side_effect = original
            with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner", "--suite", "media-only"]):
                with self.assertRaises(RuntimeError) as caught:
                    acceptance_module.main()
        self.assertIs(caught.exception, original)
        self.assert_both_cleanups(runner)

    def test_timeline_read_main_uses_only_selected_checks_and_preserves_cleanup(self):
        with self.mocked_main() as (runner, _):
            with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner",
                                           "--suite", "timeline-reads"]):
                self.assertIsNone(acceptance_module.main())
        runner.timeline_read_cases.assert_called_once()
        runner.finish_checks.assert_called_once()
        runner.test.assert_not_called()
        runner.draft_recovery_cases.assert_not_called()
        runner.app_recovery.assert_not_called()
        runner.oauth_cases.assert_not_called()
        runner.rtc_cases.assert_not_called()
        runner.outgoing_ring_cases.assert_not_called()
        runner.element_case.assert_not_called()
        self.assert_both_cleanups(runner)

    def test_timeline_read_incomplete_results_still_fail_after_owned_cleanup(self):
        original = RuntimeError("fixed incomplete timeline result")
        with self.mocked_main() as (runner, _):
            runner.finish_checks.side_effect = original
            with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner",
                                           "--suite", "timeline-reads"]):
                with self.assertRaises(RuntimeError) as caught:
                    acceptance_module.main()
        self.assertIs(caught.exception, original)
        self.assert_both_cleanups(runner)

    def test_focused_suite_failure_cannot_become_success_and_still_cleans_up(self):
        original = RuntimeError("fixed focused failure")
        with self.mocked_main() as (runner, _):
            runner.finish_checks.side_effect = original
            with patch.object(sys, "argv", ["acceptance", "--gitleaks", "mock-scanner",
                                           "--suite", "draft-recovery"]):
                with self.assertRaises(RuntimeError) as caught:
                    acceptance_module.main()
        self.assertIs(caught.exception, original)
        self.assert_both_cleanups(runner)

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


class StickerFailureForwardingTests(unittest.TestCase):
    def invoke(self, output, **kwargs):
        return TimelineFailureForwardingTests.invoke(self, output, label="media_local", **kwargs)

    def test_timeout_stage_is_forwarded_after_scanning_without_private_payload(self):
        output = self.invoke("private-server-response\nMORPHEUS_STICKER_STAGE room_receive\n"
                             "MORPHEUS_STICKER_FAILURE scenario_deadline\n")
        records = [json.loads(line) for line in output.splitlines() if line.startswith("{")]
        self.assertEqual(records[0]["sticker_diagnostics"]["stage"], "room_receive")
        self.assertNotIn("private-server-response", output)

    def test_failed_scanner_does_not_publish_even_the_closed_stage(self):
        output = self.invoke("MORPHEUS_STICKER_STAGE room_receive\n"
                             "MORPHEUS_STICKER_FAILURE scenario_deadline\n", scanner_code=1)
        self.assertNotIn("sticker_diagnostics", output)
        self.assertNotIn("room_receive", output)

    def test_check_failure_withholds_potential_security_boundary_details(self):
        output = self.invoke("MORPHEUS_STICKER_STAGE consent\n"
                             "MORPHEUS_STICKER_FAILURE check_failed\n"
                             "private-server-response\n")
        self.assertIn('"stage": "withheld"', output)
        self.assertNotIn("consent", output)
        self.assertNotIn("private-server-response", output)


class NativeProcessDiagnosticTests(unittest.TestCase):
    def test_exit_facts_are_bounded_and_timeout_is_not_reported_as_a_kill(self):
        for code, expected in ((0, ("exited", "exit_code", 0)),
                               (101, ("exited", "exit_code", 101)),
                               (255, ("exited", "exit_code", 255)),
                               (-9, ("signalled", "signal", 9)),
                               (-64, ("signalled", "signal", 64))):
            result = acceptance_module.native_process_diagnostics("private", code, False)
            self.assertEqual(result["result"], expected[0])
            self.assertEqual(result[expected[1]], expected[2])
            self.assertNotIn("private", json.dumps(result))
        for code in (None, True, "101", 1.5, 256, -65):
            result = acceptance_module.native_process_diagnostics("", code, False)
            self.assertEqual(result["result"], "unknown")
            self.assertNotIn("signal", result)
            self.assertNotIn("exit_code", result)
        result = acceptance_module.native_process_diagnostics("", -9, True)
        self.assertEqual(result["result"], "deadline")
        self.assertNotIn("signal", result)

    def test_known_panic_families_never_publish_private_source_or_message(self):
        for source, family in (
                ("crates/morpheus-matrix/src/lib.rs:42:7:", "morpheus"),
                ("/home/runner/.cargo/git/checkouts/matrix-rust-sdk-123abc/c6ad52e/crates/matrix-sdk-ui/src/timeline/mod.rs:42:7:", "matrix_sdk"),
                ("/home/runner/.cargo/registry/src/index.crates.io-123abc/rusqlite-0.38.0/src/lib.rs:42:7:", "sqlite"),
                ("/private/opaque-key/src/lib.rs:42:7:", "other"),
                ("https://private.example/token:42:7:", "other")):
            output = f"thread 'worker' (123) panicked at {source}\nprivate-server-response\nassertion `left == right` failed\n"
            result = acceptance_module.native_process_diagnostics(output, 101, False)
            self.assertTrue(result["panic_observed"])
            self.assertTrue(result["assertion_observed"])
            self.assertEqual(result["panic_family"], family)
            for withheld in (source, "private", "worker"):
                self.assertNotIn(withheld, json.dumps(result))
        result = acceptance_module.native_process_diagnostics("server quoted panicked at private\n", 1, False)
        self.assertFalse(result["panic_observed"])
        self.assertNotIn("panic_family", result)

    def test_stack_overflow_and_cargo_signals_use_only_fixed_codes(self):
        output = ("thread 'private-thread' has overflowed its stack\n"
                  "fatal runtime error: stack overflow, aborting\n"
                  "  process didn't exit successfully: `/private/command --password private` (signal: 6, SIGABRT: process abort signal)\n")
        result = acceptance_module.native_process_diagnostics(output, 101, False)
        self.assertTrue(result["stack_overflow"])
        self.assertEqual((result["test_signal"], result["test_signal_name"]), (6, "SIGABRT"))
        self.assertNotIn("private", json.dumps(result))
        for suffix in ("signal: 9, SIGABRT: contradictory", "signal: 99, SIGPRIVATE", "signal: 6, private"):
            result = acceptance_module.native_process_diagnostics(
                "  process didn't exit successfully: private (" + suffix + ")\n", 101, False)
            self.assertNotIn("test_signal", result)
            self.assertNotIn("test_signal_name", result)

    def test_contained_diagnostics_retain_observed_status_before_cleanup(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            runner.containment_verified = True
            process = Mock(returncode=None)
            visible = io.StringIO()
            output = "running 1 test\nMORPHEUS_LIVE_STAGE fixture\nMORPHEUS_LIVE_STAGE encrypted_restart_message\nprivate-server-response\n"
            def start(_args, **kwargs):
                kwargs["stdout"].write(output.encode())
                return process
            def cleanup(child):
                # The observed pre-cleanup status is the diagnostic authority.
                child.returncode = -9
            with (patch.object(acceptance_module, "namespace_command", return_value=["owned"]),
                  patch.object(acceptance_module.subprocess, "Popen", side_effect=start),
                  patch.object(acceptance_module, "wait_owned", return_value=101),
                  patch.object(acceptance_module, "stop_owned", side_effect=cleanup),
                  patch.object(acceptance_module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)),
                  contextlib.redirect_stdout(visible),
                  self.assertRaises(acceptance_module.CheckFailure)):
                runner.run("live_local", ["mock-test"], test=True, pid_namespace=True)
            records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
            self.assertEqual(records[0]["native_process"]["exit_code"], 101)
            self.assertNotIn("signal", records[0]["native_process"])
            self.assertEqual(records[0]["live_diagnostics"]["stage"], "encrypted_restart_message")
            self.assertNotIn("private-server", visible.getvalue())


    def test_all_native_test_failures_report_bounded_abort_facts_without_payload(self):
        for label in ("formatted_local", "recovery_local"):
            with self.subTest(label=label), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                runner.containment_verified = True
                process = Mock(returncode=None)
                visible = io.StringIO()
                output = ("running 1 test\nthread 'private-worker' has overflowed its stack\n"
                          "  process didn't exit successfully: private-command (signal: 6, SIGABRT: abort)\n"
                          "private-server-response\n")
                def start(_args, **kwargs):
                    kwargs["stdout"].write(output.encode())
                    return process
                def cleanup(child):
                    child.returncode = -9
                with (patch.object(acceptance_module, "namespace_command", return_value=["owned"]),
                      patch.object(acceptance_module.subprocess, "Popen", side_effect=start),
                      patch.object(acceptance_module, "wait_owned", return_value=101),
                      patch.object(acceptance_module, "stop_owned", side_effect=cleanup),
                      patch.object(acceptance_module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)),
                      contextlib.redirect_stdout(visible), self.assertRaises(acceptance_module.CheckFailure)):
                    runner.run(label, ["mock-test"], test=True, pid_namespace=True)
                records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
                self.assertEqual(records[0]["failed_tests"], [])
                self.assertEqual(records[0]["panic_locations"], [])
                process_facts = records[0]["native_process"]
                self.assertEqual(process_facts["exit_code"], 101)
                self.assertTrue(process_facts["stack_overflow"])
                self.assertEqual((process_facts["test_signal"], process_facts["test_signal_name"]), (6, "SIGABRT"))
                self.assertNotIn("private", visible.getvalue())
                self.assertEqual(runner.results, [])


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
        # Stage helpers can follow the test in the file. Check entry/completion
        # in the actual coordinator, not the lexical order of helper bodies.
        coordinator = re.search(
            r"(?ms)^fn two_homeservers_switch_restore_and_encrypted_restart\(\) \{\n(.*?)^\}",
            source,
        )
        self.assertIsNotNone(coordinator, "the live acceptance coordinator must exist")
        outer = re.findall(r"phase!\(([a-z_]+)\)", coordinator.group(1))
        self.assertEqual(outer[0], "fixture")
        self.assertEqual(outer[-1], "complete")
        source += (Path(__file__).resolve().parents[2]
                   / "crates/morpheus-matrix/tests/live_local/account_access.rs").read_text()
        emitted = re.findall(r"phase!\(([a-z_]+)\)", source)
        self.assertEqual(set(emitted), acceptance_module.LIVE_STAGES)
        self.assertEqual(len(emitted), len(set(emitted)))

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


class DraftStageTests(unittest.TestCase):
    child = LiveStageTests.child

    def test_exact_fixed_tokens_and_no_reflected_payloads(self):
        for stage in acceptance_module.DRAFTS_STAGES:
            with self.subTest(stage=stage):
                self.assertEqual(acceptance_module.drafts_diagnostics(
                    f"MORPHEUS_DRAFTS_STAGE {stage}\n"),
                    {"test_entered": stage in ("fixture", "recovery_fixture"), "stage": stage})
        for payload in ("private_token", "https://private.example", "/private/path",
                        "complete private-password", "complete\rprivate-password", "x" * 1000):
            with self.subTest(payload=payload):
                self.assertEqual(acceptance_module.drafts_diagnostics(
                    "MORPHEUS_DRAFTS_STAGE fixture\n"
                    "MORPHEUS_DRAFTS_STAGE tracked_enqueue\n"
                    f"MORPHEUS_DRAFTS_STAGE {payload}\n"),
                    {"test_entered": True, "stage": "tracked_enqueue"})

    def test_logout_observations_stop_at_first_panic_and_cannot_claim_completion(self):
        stage = "MORPHEUS_DRAFTS_STAGE recovery_fixture\nMORPHEUS_DRAFTS_STAGE recovery_coverage\n"
        marker = "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage="
        panic = ("thread 'pending_sends_are_recovered_from_real_sdk_requests' panicked at "
                 "crates/morpheus-matrix/tests/drafts_local.rs:2740:14:\nPRIVATE_ERROR\n")
        observed = (marker + "logout_pending\n"
                    + marker + "existing_finished complete=0 credentials=1 store=0\n")
        output = (marker + "registry_remaining\n" + stage + observed + panic
                  + "MORPHEUS_DRAFTS_STAGE recovery_complete\n" + marker + "store_remaining\n")
        result = acceptance_module.drafts_diagnostics(output)
        self.assertEqual(result, {
            "test_entered": True, "stage": "recovery_coverage",
            "account_removal_observations": [
                {"stage": "logout_pending"},
                {"stage": "existing_finished", "complete": False, "credentials": True, "store": False},
            ],
        })
        self.assertNotIn("PRIVATE", json.dumps(result))
        self.assertFalse(acceptance_module.drafts_completed(stage + observed))
        for name in acceptance_module.ACCOUNT_REMOVAL_STAGES:
            with self.subTest(stage=name):
                result = acceptance_module.drafts_diagnostics(stage + marker + name + "\n")
                self.assertEqual(result["account_removal_observations"], [{"stage": name}])
        for invalid in (
                "unknown", "existing_finished complete=0", "existing_finished complete=0 credentials=1",
                "existing_finished complete=0 credentials=1 store=2",
                "existing_finished complete=0 credentials=1 store=0 PRIVATE_VALUE",
                "x" * (acceptance_module.ACCOUNT_REMOVAL_MAX_LINE_LENGTH + 1)):
            with self.subTest(invalid=invalid):
                self.assertNotIn("account_removal_observations", acceptance_module.drafts_diagnostics(
                    stage + marker + invalid + "\n"))
        excess = (marker + "logout_pending\n") * (acceptance_module.ACCOUNT_REMOVAL_MAX_LINES + 1)
        self.assertNotIn("account_removal_observations", acceptance_module.drafts_diagnostics(stage + excess))

    def test_logout_observations_use_the_existing_scan_gate_and_never_publish_private_errors(self):
        output = ("MORPHEUS_DRAFTS_STAGE recovery_fixture\n"
                  "MORPHEUS_DRAFTS_STAGE recovery_coverage\n"
                  "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=logout_error\nPRIVATE_ERROR\n")
        for scanner_code in (0, 1):
            with self.subTest(scanner_code=scanner_code), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with self.child(runner, output, timeout=True, scanner_code=scanner_code) as (visible, _):
                    with self.assertRaisesRegex(RuntimeError, "deadline" if scanner_code == 0 else "credential scanning"):
                        runner.run("drafts_local", ["mock-test"], test=True, seconds=1)
                records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
                if scanner_code == 0:
                    self.assertEqual(len(records), 1)
                    self.assertEqual(records[0]["drafts_diagnostics"]["account_removal_observations"],
                                     [{"stage": "logout_error"}])
                else:
                    self.assertEqual(records, [])
                self.assertNotIn("PRIVATE", visible.getvalue())
                self.assertEqual(runner.results, [])

    def test_source_emits_all_unique_stages_from_entry_through_cleanup(self):
        source = (Path(__file__).resolve().parents[2]
                  / "crates/morpheus-matrix/tests/drafts_local.rs").read_text()
        emitted = re.findall(r"phase!\(([a-z_]+)\)", source)
        self.assertEqual(set(emitted), acceptance_module.DRAFTS_STAGES)
        self.assertEqual(len(emitted), len(set(emitted)))
        self.assertEqual((emitted[0], emitted[-1]), ("fixture", "recovery_complete"))
        self.assertLess(source.index("phase!(tracked_enqueue)"),
                        source.index(".send_text_tracked_for("))
        self.assertLess(source.index('.expect("Cannot queue tracked message")'),
                        source.index("phase!(tracked_outcome)"))
        self.assertLess(source.index("phase!(tracked_outcome)"),
                        source.index("queued.wait_outcome()"))
        self.assertLess(source.index('fs::remove_dir_all(root)'),
                        source.index("phase!(complete)"))
        self.assertLess(source.rindex('fs::remove_dir_all(root)'),
                        source.index("phase!(recovery_complete)"))

    def test_success_requires_positive_test_result_entry_and_completed_cleanup(self):
        passed = "test result: ok. 2 passed; 0 failed; 0 ignored;\n"
        entry = "MORPHEUS_DRAFTS_STAGE fixture\n"
        complete = "MORPHEUS_DRAFTS_STAGE complete\n"
        recovery = ("MORPHEUS_DRAFTS_STAGE recovery_fixture\n"
                    "MORPHEUS_DRAFTS_STAGE recovery_complete\n")
        for output, accepted in ((passed, False), (complete + passed, False),
                                 (entry + passed, False), (entry + complete + passed, False),
                                 (entry + complete + recovery + passed, True),
                                 (recovery + entry + complete + passed, True),
                                 (entry + complete + recovery + passed.replace("2 passed", "1 passed"), False),
                                 (entry + complete + "test result: ok. 0 passed; 0 failed; 1 ignored;\n", False)):
            with self.subTest(output=output), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with self.child(runner, output):
                    if accepted:
                        runner.run("drafts_local", ["mock-test"], test=True)
                    else:
                        with self.assertRaises(RuntimeError):
                            runner.run("drafts_local", ["mock-test"], test=True)
                self.assertEqual(len(runner.results), int(accepted))

    def test_deadline_retains_last_stage_and_withholds_private_output(self):
        output = ("MORPHEUS_DRAFTS_STAGE fixture\n"
                  "MORPHEUS_DRAFTS_STAGE tracked_enqueue\nprivate-server-response\n")
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with self.child(runner, output, timeout=True) as (visible, kill):
                with self.assertRaisesRegex(RuntimeError, "deadline"):
                    runner.run("drafts_local", ["mock-test"], test=True, seconds=1)
            kill.assert_called_once_with(123456, acceptance_module.signal.SIGKILL)
            records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0]["drafts_diagnostics"],
                             {"test_entered": True, "stage": "tracked_enqueue", "failure": "deadline"})
            self.assertEqual(records[0]["native_process"]["result"], "deadline")
            self.assertNotIn("private-server", visible.getvalue())
            self.assertEqual(runner.results, [])

    def test_rejected_scan_prevents_publication(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with self.child(runner, "MORPHEUS_DRAFTS_STAGE fixture\n", scanner_code=1) as (visible, _):
                with self.assertRaisesRegex(RuntimeError, "credential scanning"):
                    runner.run("drafts_local", ["mock-test"], test=True)
            self.assertNotIn("drafts_diagnostics", visible.getvalue())
            self.assertEqual(runner.results, [])


class AppRecoveryEvidenceTests(unittest.TestCase):
    child = LiveStageTests.child
    cases = {
        "stale_observers_end_on_revocation_and_never_touch_the_departed_cache":
            "observer_revocation",
        "interrupted_prepare_keeps_its_committed_record_for_discovery_and_leaves_the_cache_consistent":
            "interrupted_prepare",
        "rollback_racing_a_switch_leaves_cache_and_row_agreeing":
            "rollback_race",
        "a_background_flush_cannot_commit_a_record_before_its_preparation_writes_it":
            "writer_transaction",
        "the_thread_index_is_durable_before_any_record_exists":
            "thread_index",
        "revocation_after_the_release_leaves_the_record_in_cache_and_row":
            "release_revocation",
        "revocation_during_the_rollback_write_converges_under_the_release":
            "write_revocation",
        "a_preparation_waiting_for_the_writer_lock_creates_nothing_once_its_selection_ended":
            "writer_lock_revocation",
        "a_submission_typed_over_during_preparation_is_kept_on_a_definite_failure":
            "typed_retention",
        "typing_while_the_rollback_waits_after_the_release_keeps_the_submission":
            "late_typing_retention",
        "a_revoked_rollback_retains_a_typed_over_uncommitted_record":
            "revoked_retention",
        "a_thread_submission_typed_over_during_preparation_is_kept_with_its_index":
            "thread_retention",
    }

    @staticmethod
    def complete():
        return "".join(
            f"MORPHEUS_APP_RECOVERY_STAGE {name}\nMORPHEUS_APP_RECOVERY_CHECK {name}=true\n"
            for name in sorted(acceptance_module.APP_RECOVERY_CASES)
        ) + (acceptance_module.APP_MARKER_AUTHORITY_COMPLETE + "\n"
             "test result: ok. 13 passed; 0 failed; 0 ignored;\n")

    def test_requires_every_live_case_once_and_no_opt_out(self):
        complete = self.complete()
        name = sorted(acceptance_module.APP_RECOVERY_CASES)[0]
        self.assertEqual(acceptance_module.app_recovery_evidence(complete),
                         dict.fromkeys(sorted(acceptance_module.APP_RECOVERY_CASES), True))
        for output in (
            "test result: ok. 13 passed; 0 failed; 0 ignored;\n",
            complete.replace(f"MORPHEUS_APP_RECOVERY_STAGE {name}\n", ""),
            complete.replace(f"MORPHEUS_APP_RECOVERY_CHECK {name}=true\n", ""),
            complete + f"MORPHEUS_APP_RECOVERY_CHECK {name}=true\n",
            complete + f"MORPHEUS_APP_RECOVERY_CHECK {name}=false\n",
            complete.replace("13 passed", "12 passed"),
            complete.replace("13 passed", "17 passed"),
            complete.replace("0 ignored", "1 ignored"),
            complete + "Skipped local fixture\n",
        ):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                acceptance_module.app_recovery_evidence(output)

    def test_exact_source_tokens_cover_each_real_test_and_follow_cleanup(self):
        source = (Path(__file__).resolve().parents[2] / "crates/morpheus-app/src/live.rs").read_text()
        source = source.split("mod stale_observer_tests {", 1)[1]
        bodies = re.split(r"#\[tokio::test\]", source)[1:]
        tests = {}
        for body in bodies:
            name = re.match(r"\s*async fn ([a-z_]+)\s*\(\s*\)", body)
            self.assertIsNotNone(name)
            self.assertNotIn(name[1], tests)
            tests[name[1]] = body
        # These native regressions share the module but have no recovery protocol.
        other_cases = {
            "a_departed_selection_neither_reveals_nor_focuses_after_a_switch_back",
            "a_created_room_opens_only_for_its_own_click_and_admitted_snapshot",
            "a_stale_snapshots_member_load_never_claims_over_a_newer_one",
            "stale_creation_follow_ups_keep_a_newer_rooms_feed_and_transition",
            "a_withdrawn_created_room_open_gives_up_held_waits_and_its_locks",
            "rollback_pause_wait_rejects_an_early_ended_worker",
            "rollback_pause_wait_does_not_release_the_reached_write_boundary",
        }
        self.assertEqual(tests.keys(), self.cases.keys() | other_cases)
        for name in other_cases:
            self.assertNotRegex(tests.pop(name), r"recovery_(?:phase|check)!\(")
        self.assertEqual(len(tests), len(acceptance_module.APP_RECOVERY_CASES))
        self.assertEqual(acceptance_module.APP_RECOVERY_TESTS,
                         tuple("live::stale_observer_tests::" + name for name in self.cases))
        completed = []
        for name, body in tests.items():
            phases = re.findall(r"recovery_phase!\(([a-z_]+)\)", body)
            checks = re.findall(r"recovery_check!\(([a-z_]+)\)", body)
            self.assertEqual(phases, checks)
            self.assertEqual(checks, [self.cases[name]])
            self.assertEqual(len(checks), 1)
            self.assertLess(body.index("fixture()"), body.index("recovery_phase!("))
            self.assertLess(body.index("close_profile(directory, live).await;"),
                            body.index("recovery_check!("))
            completed.extend(checks)
        self.assertEqual(set(completed), acceptance_module.APP_RECOVERY_CASES)
        self.assertEqual(len(completed), len(set(completed)))

    def test_runs_actual_app_tests_contained_with_fixture_and_serial_execution(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with patch.object(runner, "run") as run:
                runner.app_recovery(Path(folder) / "fixture.json")
            label, args = run.call_args.args
            self.assertEqual(label, acceptance_module.APP_RECOVERY_LABEL)
            self.assertEqual(args, ["cargo", "test", "--locked", "-p", "morpheus-app", "--features", "native-calls",
                                    "--bin", "morpheus-app", "--", "--exact",
                                    *("live::stale_observer_tests::" + name for name in self.cases),
                                    acceptance_module.APP_MARKER_AUTHORITY_TEST,
                                    "--nocapture", "--test-threads=1"])
            self.assertEqual(run.call_args.kwargs, {
                "extra": {"MORPHEUS_MATRIX_TEST_ACCOUNTS": str(Path(folder) / "fixture.json"),
                          "SLINT_EMIT_DEBUG_INFO": "1"},
                "test": True, "pid_namespace": True,
            })

    def test_marker_authority_requires_exact_once_completion_and_the_same_thirteen_tests(self):
        complete = self.complete()
        marker = acceptance_module.APP_MARKER_AUTHORITY_COMPLETE
        for output in (
            complete.replace(marker + "\n", ""),
            complete + marker + "\n",
            complete.replace(marker, marker.replace("true", "false")),
            complete.replace(marker, marker + " PRIVATE_VALUE"),
            complete.replace(marker, "quoted " + marker),
            complete.replace(marker, "MORPHEUS_APP_MARKER_AUTHORITY_CHECK unknown=true"),
            complete + "test result: ok. 1 passed; 0 failed; 0 ignored;\n",
            complete.replace("13 passed", "12 passed"),
            complete + "Skipped navigation regression; no fixture\n",
        ):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                acceptance_module.app_recovery_evidence(output)
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            record = runner.validated_record(
                acceptance_module.APP_RECOVERY_LABEL, complete, test=True,
                ring_case=None, oauth_checks=(), element_checks=False,
            )
            self.assertEqual(record["passed"], 13)
            self.assertEqual(record["app_recovery_checks"], dict.fromkeys(
                sorted(acceptance_module.APP_RECOVERY_CASES), True))
            self.assertEqual(len(record["app_recovery_checks"]), 12)
            self.assertEqual(record["app_marker_authority_checks"],
                             {"account_selection_and_close": True})
            with self.assertRaises(RuntimeError):
                runner.validated_record(
                    acceptance_module.APP_RECOVERY_LABEL, complete, test=False,
                    ring_case=None, oauth_checks=(), element_checks=False,
                )

    def test_scanner_rejection_precedes_any_evidence_publication(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with self.child(runner, self.complete(), scanner_code=1) as (visible, _):
                with self.assertRaisesRegex(RuntimeError, "credential scanning"):
                    runner.run(acceptance_module.APP_RECOVERY_LABEL, ["mock-test"], test=True)
            self.assertNotIn("app_recovery", visible.getvalue())
            self.assertEqual(runner.results, [])

    def test_recovery_stage_stops_at_first_panic_before_later_serial_cases(self):
        output = ("MORPHEUS_APP_RECOVERY_STAGE typed_retention\n"
                  "MORPHEUS_APP_RECOVERY_STAGE observer_revocation\n"
                  "MORPHEUS_APP_RECOVERY_STAGE PRIVATE_TOKEN\n"
                  "thread 'worker' (123) panicked at crates/morpheus-app/src/live.rs:42:7:\n"
                  "PRIVATE_SERVER_RESPONSE\n"
                  "MORPHEUS_APP_RECOVERY_STAGE late_typing_retention\n"
                  "thread 'later' panicked at /PRIVATE_PATH/source.rs:7:1:\n")
        result = acceptance_module.app_recovery_diagnostics(output)
        self.assertEqual(result, {"stage": "observer_revocation"})
        process = acceptance_module.native_process_diagnostics(output, 101, False)
        self.assertTrue(process["panic_observed"])
        self.assertEqual(process["panic_family"], "morpheus")
        self.assertNotIn("PRIVATE", json.dumps((result, process)))

    def test_write_revocation_boundaries_are_closed_and_belong_to_the_current_case(self):
        stage = "MORPHEUS_APP_RECOVERY_STAGE write_revocation\n"
        for boundary in acceptance_module.APP_RECOVERY_WRITE_BOUNDARIES:
            marker = "MORPHEUS_APP_RECOVERY_BOUNDARY write_revocation " + boundary + "\n"
            with self.subTest(boundary=boundary):
                self.assertEqual(acceptance_module.app_recovery_diagnostics(stage + marker),
                                 {"stage": "write_revocation", "boundary": boundary})
                self.assertEqual(acceptance_module.app_recovery_diagnostics(marker), {})
                self.assertEqual(acceptance_module.app_recovery_diagnostics(
                    stage + marker + "MORPHEUS_APP_RECOVERY_STAGE typed_retention\n"),
                    {"stage": "typed_retention"})
        for marker in (
                "MORPHEUS_APP_RECOVERY_BOUNDARY write_revocation PRIVATE_ACCOUNT\n",
                "MORPHEUS_APP_RECOVERY_BOUNDARY write_revocation unknown_boundary\n",
                "MORPHEUS_APP_RECOVERY_BOUNDARY write_revocation rollback_pause PRIVATE_TEXT\n",
                "MORPHEUS_APP_RECOVERY_BOUNDARY other_case rollback_pause\n"):
            with self.subTest(marker=marker):
                result = acceptance_module.app_recovery_diagnostics(stage + marker)
                self.assertEqual(result, {"stage": "write_revocation"})
                self.assertNotIn("PRIVATE", json.dumps(result))

    def test_write_revocation_boundary_stops_at_first_panic_and_deadline_is_scan_gated(self):
        output = ("MORPHEUS_APP_RECOVERY_STAGE write_revocation\n"
                  "MORPHEUS_APP_RECOVERY_BOUNDARY write_revocation rollback_pause\n")
        panic = ("thread 'worker' panicked at crates/morpheus-app/src/live.rs:42:7:\n"
                 "PRIVATE_SERVER_RESPONSE\n"
                 "MORPHEUS_APP_RECOVERY_BOUNDARY write_revocation complete\n")
        expected = {"stage": "write_revocation", "boundary": "rollback_pause"}
        self.assertEqual(acceptance_module.app_recovery_diagnostics(output + panic), expected)
        for scanner_code in (0, 1):
            with self.subTest(scanner_code=scanner_code), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with self.child(runner, output, timeout=True, scanner_code=scanner_code) as (visible, _):
                    with self.assertRaisesRegex(RuntimeError, "deadline" if scanner_code == 0 else "credential scanning"):
                        runner.run(acceptance_module.APP_RECOVERY_LABEL, ["mock-test"], test=True, seconds=1)
                records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
                if scanner_code == 0:
                    self.assertEqual(len(records), 1)
                    self.assertEqual(records[0]["app_recovery_diagnostics"], expected)
                else:
                    self.assertEqual(records, [])
                self.assertNotIn("PRIVATE", visible.getvalue())
                self.assertEqual(runner.results, [])

    def test_logout_panic_exposes_only_exact_variant_categories_not_payloads(self):
        header = ("MORPHEUS_APP_RECOVERY_STAGE observer_revocation\nthread '"
                  + acceptance_module.APP_RECOVERY_TESTS[0]
                  + "' (123) panicked at crates/morpheus-app/src/live.rs:42:7:\n"
                  "Cannot remove first account: ")
        for value, category in (
                ("AccountAccessBusy", "account_access_busy"),
                ("AccountRemovalPending", "account_removal_pending"),
                ('AccountNotFound("PRIVATE_ACCOUNT")', "account_not_found"),
                ('Io(Os { code: 5, message: "PRIVATE_PATH" })', "io"),
                ('Json(Error("PRIVATE_JSON"))', "json"),
                ('Keyring("PRIVATE_KEYRING")', "keyring"),
                ('Sdk("PRIVATE_SERVER_RESPONSE")', "sdk"),
                ('InvalidRegistry("PRIVATE_REGISTRY")', "other"),
                ('AccountAccessBusy("PRIVATE_PAYLOAD")', "other"),
                ('AccountAccessBusyPRIVATE', "other"),
                ('IoPRIVATE("PRIVATE_PAYLOAD")', "other"),
                ('Io', "other"), ("PRIVATE_TOKEN" * 1000, "other")):
            with self.subTest(category=category):
                output = (header + value + "\nMORPHEUS_APP_RECOVERY_STAGE late_typing_retention\n")
                result = acceptance_module.app_recovery_diagnostics(output)
                self.assertEqual(result, {"stage": "observer_revocation", "cleanup_failure": {
                    "case": "observer_revocation", "operation": "first_account_logout",
                    "category": category}})
                self.assertNotIn("PRIVATE", json.dumps(result))

    def test_cleanup_category_requires_the_first_exact_case_location_and_prefix(self):
        stage = "MORPHEUS_APP_RECOVERY_STAGE observer_revocation\n"
        header = ("thread '" + acceptance_module.APP_RECOVERY_TESTS[0]
                  + "' panicked at crates/morpheus-app/src/live.rs:42:7:\n")
        message = "Cannot remove first account: Io(PRIVATE_PAYLOAD)\n"
        for output in (
                stage + header.replace(acceptance_module.APP_RECOVERY_TESTS[0], "other_test") + message,
                stage + header.replace("src/live.rs", "src/other.rs") + message,
                stage + header + message.replace("first account", "other account"),
                stage + "thread 'first' panicked at /PRIVATE/source.rs:1:1:\n" + header + message,
                stage + header + "PRIVATE_LINE\n" + message):
            with self.subTest(output=output):
                self.assertEqual(acceptance_module.app_recovery_diagnostics(output),
                                 {"stage": "observer_revocation"})
        result = acceptance_module.app_recovery_diagnostics(
            "MORPHEUS_APP_RECOVERY_STAGE PRIVATE_TOKEN\n" + header + message + stage)
        self.assertEqual(result, {})
        self.assertNotIn("PRIVATE", json.dumps(result))

    def test_account_removal_observations_keep_only_actual_closed_facts_before_first_panic(self):
        stage = "MORPHEUS_APP_RECOVERY_STAGE observer_revocation\n"
        marker = "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage="
        panic = ("thread '" + acceptance_module.APP_RECOVERY_TESTS[0]
                 + "' panicked at crates/morpheus-app/src/live.rs:42:7:\n"
                 "Cannot remove first account: AccountRemovalPending\n")
        observed = (marker + "logout_pending\n"
                    + marker + "existing_finished complete=0 credentials=1 store=0\n")
        output = (marker + "registry_remaining\n" + stage + observed + panic
                  + marker + "wrong_account complete=1 credentials=1 store=1\nPRIVATE_RESPONSE\n")
        result = acceptance_module.app_recovery_diagnostics(output)
        self.assertEqual(result["cleanup_failure"]["category"], "account_removal_pending")
        self.assertEqual(result["account_removal_observations"], [
            {"stage": "logout_pending"},
            {"stage": "existing_finished", "complete": False, "credentials": True, "store": False},
        ])
        self.assertNotIn("PRIVATE", json.dumps(result))
        for name in acceptance_module.ACCOUNT_REMOVAL_STAGES:
            with self.subTest(stage=name):
                result = acceptance_module.app_recovery_diagnostics(stage + marker + name + "\r\n" + panic)
                self.assertEqual(result["account_removal_observations"], [{"stage": name}])
        for output in (stage + observed, stage + observed + panic.replace("first account", "other account"),
                       "MORPHEUS_APP_RECOVERY_STAGE typed_retention\n" + observed + panic):
            self.assertNotIn("account_removal_observations", acceptance_module.app_recovery_diagnostics(output))

    def test_native_partial_removal_receipt_remains_diagnostic_and_cannot_pass_recovery(self):
        stage = "MORPHEUS_APP_RECOVERY_STAGE observer_revocation\n"
        marker = "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage="
        panic = ("thread '" + acceptance_module.APP_RECOVERY_TESTS[0]
                 + "' panicked at crates/morpheus-app/src/live.rs:42:7:\n"
                 "Cannot remove first account: AccountRemovalPending\n")
        output = (stage + marker + "owned_store_not_empty\n"
                  + marker + "owned_delete_finished complete=0 credentials=1 store=0\n"
                  + marker + "logout_result complete=0 credentials=1 store=0\n"
                  + marker + "logout_pending\n"
                  + marker + "existing_observation_error\n" + panic)
        result = acceptance_module.app_recovery_diagnostics(output)
        self.assertEqual(result["account_removal_observations"], [
            {"stage": "owned_store_not_empty"},
            {"stage": "owned_delete_finished", "complete": False, "credentials": True, "store": False},
            {"stage": "logout_result", "complete": False, "credentials": True, "store": False},
            {"stage": "logout_pending"}, {"stage": "existing_observation_error"},
        ])
        with self.assertRaisesRegex(RuntimeError, "incomplete"):
            acceptance_module.app_recovery_evidence(output)
        contaminated = output.replace("owned_store_not_empty\n", "owned_store_not_empty PRIVATE_RESPONSE\n")
        self.assertNotIn("account_removal_observations", acceptance_module.app_recovery_diagnostics(contaminated))
        self.assertNotIn("PRIVATE", json.dumps(acceptance_module.app_recovery_diagnostics(contaminated)))

    def test_account_removal_observations_reject_partial_bad_oversized_and_excess_lines(self):
        stage = "MORPHEUS_APP_RECOVERY_STAGE observer_revocation\n"
        marker = "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage="
        panic = ("thread '" + acceptance_module.APP_RECOVERY_TESTS[0]
                 + "' panicked at crates/morpheus-app/src/live.rs:42:7:\n"
                 "Cannot remove first account: AccountRemovalPending\n")
        expected = acceptance_module.app_recovery_diagnostics(stage + panic)
        for invalid in (
                "PRIVATE_STAGE", "unknown", "existing_finished complete=0",
                "existing_finished complete=0 credentials=0", "existing_finished complete=true credentials=0 store=0",
                "existing_finished complete=0 credentials=0 store=2",
                "existing_finished complete=0 credentials=0 store=0 PRIVATE_RESPONSE",
                "existing_finished complete=0 credentials=0 store=0 complete=1",
                "existing_finished\rPRIVATE_RESPONSE", "x" * (acceptance_module.ACCOUNT_REMOVAL_MAX_LINE_LENGTH + 1)):
            with self.subTest(invalid=invalid):
                output = stage + marker + "logout_pending\n" + marker + invalid + "\n" + panic
                self.assertEqual(acceptance_module.app_recovery_diagnostics(output), expected)
        excess = (marker + "logout_pending\n") * (acceptance_module.ACCOUNT_REMOVAL_MAX_LINES + 1)
        self.assertEqual(acceptance_module.app_recovery_diagnostics(stage + excess + panic), expected)

    def test_account_removal_observations_are_published_only_after_the_existing_scan_gate(self):
        output = ("MORPHEUS_APP_RECOVERY_STAGE observer_revocation\n"
                  "MORPHEUS_ACCOUNT_REMOVAL_OBSERVATION stage=existing_observation_error\n"
                  "thread '" + acceptance_module.APP_RECOVERY_TESTS[0]
                  + "' panicked at crates/morpheus-app/src/live.rs:42:7:\n"
                  "Cannot remove first account: AccountRemovalPending\nPRIVATE_RESPONSE\n")
        for scanner_code in (0, 1):
            with self.subTest(scanner_code=scanner_code), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                with self.child(runner, output, timeout=True, scanner_code=scanner_code) as (visible, _):
                    with self.assertRaisesRegex(RuntimeError, "deadline" if scanner_code == 0 else "credential scanning"):
                        runner.run(acceptance_module.APP_RECOVERY_LABEL, ["mock-test"], test=True, seconds=1)
                records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
                if scanner_code == 0:
                    self.assertEqual(len(records), 1)
                    self.assertEqual(records[0]["app_recovery_diagnostics"]["account_removal_observations"],
                                     [{"stage": "existing_observation_error"}])
                else:
                    self.assertEqual(records, [])
                self.assertNotIn("PRIVATE", visible.getvalue())
                self.assertEqual(runner.results, [])

    def test_nonpanic_quotes_and_malformed_headers_keep_last_allowed_stage(self):
        output = ("MORPHEUS_APP_RECOVERY_STAGE observer_revocation\n"
                  "server quoted panicked at PRIVATE_PATH\n"
                  "thread 'PRIVATE' panicked at \n"
                  "MORPHEUS_APP_RECOVERY_STAGE late_typing_retention\n"
                  "MORPHEUS_APP_RECOVERY_STAGE late_typing_retention PRIVATE_TOKEN\n")
        self.assertEqual(acceptance_module.app_recovery_diagnostics(output),
                         {"stage": "late_typing_retention"})
        self.assertFalse(acceptance_module.native_process_diagnostics(output, 1, False)["panic_observed"])

    def test_deadline_exposes_only_fixed_stage_and_process_facts(self):
        output = ("MORPHEUS_APP_RECOVERY_STAGE typed_retention\n"
                  "MORPHEUS_APP_RECOVERY_STAGE private_token\nprivate-server-response\n")
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "unused-scanner")
            with self.child(runner, output, timeout=True) as (visible, _):
                with self.assertRaisesRegex(RuntimeError, "deadline"):
                    runner.run(acceptance_module.APP_RECOVERY_LABEL, ["mock-test"], test=True, seconds=1)
            records = [json.loads(line) for line in visible.getvalue().splitlines()
                       if line.startswith("{")]
            self.assertEqual(len(records), 1)
            record = records[0]
            self.assertEqual(record["app_recovery_diagnostics"], {"stage": "typed_retention"})
            self.assertEqual(record["native_process"]["result"], "deadline")
            self.assertNotIn("private", visible.getvalue())


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
                self.assertLess(calls.index(call.app_recovery(Path(folder) / "fixture.json")), rtc_up)
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

            # The outer collector must also reject the formerly complete
            # eleven-fact peer and any explicit reverse-audio failure.
            for incomplete in (facts.replace("MORPHEUS_ELEMENT_CHECK browser_audio=true", ""),
                               facts + "\nMORPHEUS_ELEMENT_CHECK browser_audio=false"):
                with patch("run_matrix_acceptance.subprocess.run", return_value=subprocess.CompletedProcess([], 0)), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(RuntimeError):
                    runner.run("element-reverse-audio-incomplete", [sys.executable, "-c", f"print({incomplete!r})"], element_checks=True)
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

    def test_oauth_pause_and_token_failures_remain_exact_closed_diagnostics(self):
        for code in ("sync_pause_selection", "sync_pause_timeout", "sync_pause_operation",
                     "delegated_session_missing", "access_token_missing", "refresh_token_missing"):
            with self.subTest(code=code):
                self.assertEqual(oauth_diagnostics("MORPHEUS_OAUTH_STAGE restore_identity\n"
                                                  f"MORPHEUS_OAUTH_FAILURE {code}\n"
                                                  f"MORPHEUS_OAUTH_FAILURE {code} private-body\n"),
                                 {"stage": "restore_identity", "failure": code})
                self.assertEqual(oauth_diagnostics(f"MORPHEUS_OAUTH_FAILURE {code}_unknown\n"
                                                  f"MORPHEUS_OAUTH_FAILURE {code} private-token\n"), {})
        self.assertEqual(oauth_diagnostics("MORPHEUS_OAUTH_FAILURE access_token_value\n"
                                          "MORPHEUS_OAUTH_FAILURE https://example.org/private\n"), {})

    def test_registration_outcomes_survive_only_the_existing_closed_oauth_reducer(self):
        import local_matrix_auth
        expected = {"registration_command", "registration_exit_two", "registration_exit_exec", "registration_exit_other"}
        helper = {failure.value for failure in local_matrix_auth.Failure if failure.value.startswith("registration_")}
        self.assertEqual(helper, expected)
        self.assertLessEqual(helper, acceptance_module.OAUTH_FAILURES)
        for code in expected:
            self.assertEqual(oauth_diagnostics("MORPHEUS_OAUTH_STAGE registration\n"
                                              f"MORPHEUS_OAUTH_FAILURE {code}\n"
                                              f"MORPHEUS_OAUTH_FAILURE {code} private-body\n"),
                             {"stage": "registration", "failure": code})
        self.assertEqual(oauth_diagnostics("MORPHEUS_OAUTH_FAILURE registration_exit_two_unknown\n"
                                          "MORPHEUS_OAUTH_FAILURE registration_exit_other private-token\n"), {})

    def test_incoming_discovery_counters_are_bounded_and_never_relay_unknown_values(self):
        for name in acceptance_module.INCOMING_DIAGNOSTICS:
            output = f"MORPHEUS_INCOMING_DIAGNOSTIC {name}=65535\nMORPHEUS_INCOMING_DIAGNOSTIC {name}=0"
            self.assertEqual(acceptance_module.incoming_diagnostics(output), {
                "checks": {}, "discovery": {name: 65535},
            })
        for line in ("worker_candidate_queued=-1", "worker_candidate_queued=65536",
                     "worker_candidate_queued=999999", "worker_candidate_queued=01",
                     "worker_candidate_queued=1 private-token", "private_account=1",
                     "worker_candidate_queued=private-token"):
            result = acceptance_module.incoming_diagnostics("MORPHEUS_INCOMING_DIAGNOSTIC " + line)
            self.assertEqual(result, {"checks": {}, "malformed": True})
            self.assertNotIn("private", json.dumps(result))

    def test_incoming_discovery_counter_names_match_fixed_rust_sources(self):
        repo = Path(__file__).resolve().parents[2]
        counters = (repo / "crates/morpheus-app/src/incoming_ring/worker/diagnostics.rs").read_text()
        fixtures = (repo / "crates/morpheus-app/src/outgoing_ring/live_tests/actor_tests.rs").read_text()
        names = set(re.findall(r'"(worker_[a-z_]+)"', counters))
        names.update(re.findall(r'"(fresh_(?:before|after)_[a-z_]+)"', fixtures))
        self.assertEqual(names, acceptance_module.INCOMING_DIAGNOSTICS)

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

    def test_settings_failure_categories_and_operations_never_reflect_server_text(self):
        for operation in acceptance_module.SETTINGS_OPERATIONS:
            for category in acceptance_module.SETTINGS_FAILURES:
                output = "\n".join((
                    f"MORPHEUS_SETTINGS_OPERATION {operation}",
                    f"MORPHEUS_SETTINGS_FAILURE {category}",
                    "MORPHEUS_SETTINGS_OPERATION private_operation",
                    "MORPHEUS_SETTINGS_FAILURE private_credential",
                    "MORPHEUS_SETTINGS_FAILURE rate_limited private_response",
                    "prefix MORPHEUS_SETTINGS_FAILURE unavailable",
                    "private-server-body",
                ))
                with self.subTest(operation=operation, category=category):
                    self.assertEqual(acceptance_module.moderation_diagnostics(output), {
                        "settings_operation": operation, "settings_failure": category,
                    })

    def test_settings_failure_cannot_be_hidden_by_success_facts(self):
        complete = "\n".join(f"MORPHEUS_MODERATION_CHECK {name}=true"
                             for name in acceptance_module.MODERATION_CHECKS)
        for category in acceptance_module.SETTINGS_FAILURES:
            with self.subTest(category=category), self.assertRaisesRegex(RuntimeError, "Moderation acceptance evidence"):
                acceptance_module.moderation_evidence(
                    "MORPHEUS_SETTINGS_FAILURE " + category + "\n" + complete)

    def test_account_replay_subphases_distinguish_offers_from_cancellation(self):
        phases = {"account_fresh_offer", "account_answer", "account_alternate_login",
                  "account_cancellation", "account_restore", "account_old_key",
                  "account_old_join", "account_roster"}
        self.assertTrue(phases <= acceptance_module.INCOMING_STAGES)
        for phase in phases:
            output = f"MORPHEUS_INCOMING_STAGE {phase}\nMORPHEUS_INCOMING_FAILURE {phase}\nMORPHEUS_INCOMING_ERROR timed_out"
            with self.subTest(phase=phase):
                self.assertEqual(acceptance_module.incoming_diagnostics(output), {
                    "checks": {}, "stage": phase, "failure": phase, "error": "timed_out",
                })
                self.assertNotIn("private", json.dumps(acceptance_module.incoming_diagnostics(
                    output + "\nMORPHEUS_INCOMING_STAGE account_private_payload")))

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


class OwnedSfuRecoveryTests(unittest.TestCase):
    def binary(self):
        binary = Mock()
        binary.path = Path("/synthetic/target/debug/deps/morpheus_app-0123456789abcdef")
        binary.cwd = Path("/synthetic/crates/morpheus-app")
        binary.extra = {"LD_LIBRARY_PATH": "/synthetic/target/debug/deps", "SLINT_EMIT_DEBUG_INFO": "1"}
        binary.sha256 = "a" * 64
        binary.identity = (1, 2, 0o100755, 100, 3, 4)
        binary.command.side_effect = lambda selector, *flags: [str(binary.path), selector, *flags]
        return binary

    def artifact_fixture(self, folder, *, release_ui=False):
        repo = Path(folder) / "checkout"
        package = repo / "crates/morpheus-app"
        package.mkdir(parents=True)
        target = repo / ("target/release" if release_ui else "target/debug")
        binary = target / "deps/morpheus_app-0123456789abcdef"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"disposable non-executed artifact bytes")
        binary.chmod(0o755)
        native = target / "build/native-fixture/out"
        native.mkdir(parents=True)
        rust = Path(folder) / "toolchain/lib/rustlib/x86_64-unknown-linux-gnu/lib"
        rust.mkdir(parents=True)
        artifact = {
            "reason": "compiler-artifact", "package_id": "path+" + package.as_uri() + "#0.1.0",
            "manifest_path": str(package / "Cargo.toml"),
            "target": {"name": "morpheus-app", "kind": ["bin"], "crate_types": ["bin"],
                       "edition": "2024", "src_path": str(package / "src/main.rs"), "test": True},
            "profile": {"test": True, "opt_level": "3" if release_ui else "0", "debuginfo": 0,
                        "debug_assertions": not release_ui, "overflow_checks": not release_ui},
            "features": ["default", "native-calls"], "filenames": [str(binary)],
            "executable": str(binary), "fresh": False,
        }
        linked = {"reason": "build-script-executed", "linked_paths": ["native=" + str(native), "native=/outside/not-added"]}
        output = "\n".join(map(json.dumps, [linked, artifact, {"reason": "build-finished", "success": True}]))
        return repo, binary, rust, native, artifact, output

    def test_release_ui_artifact_requires_exact_instrumented_profile_and_retained_file(self):
        env = {"MORPHEUS_TIMELINE_ACCEPTANCE_BUILD": "1", "CARGO_PROFILE_RELEASE_DEBUG": "0"}
        with tempfile.TemporaryDirectory() as folder:
            repo, path, rust, native, artifact, output = self.artifact_fixture(folder, release_ui=True)
            binary = acceptance_module.RecoveryTestBinary(output, str(rust) + "\n", env,
                                                          repo=repo, release_ui=True)
            try:
                self.assertEqual(binary.command(acceptance_module.APP_TIMELINE_CASE, "--exact"),
                                 [str(path), acceptance_module.APP_TIMELINE_CASE, "--exact"])
                self.assertEqual(binary.extra["LD_LIBRARY_PATH"].split(os.pathsep),
                                 [str(native), str(repo / "target/release"), str(path.parent), str(rust)])
                path.unlink()
                path.write_bytes(b"replacement not executed")
                path.chmod(0o755)
                with self.assertRaises(RuntimeError):
                    binary.command(acceptance_module.APP_TIMELINE_CASE)
            finally:
                binary.close()
        for changes in ({}, {"MORPHEUS_TIMELINE_ACCEPTANCE_BUILD": "0"},
                        {"CARGO_PROFILE_RELEASE_DEBUG": "1"}):
            with tempfile.TemporaryDirectory() as folder:
                repo, _, rust, _, _, output = self.artifact_fixture(folder, release_ui=True)
                selected = {} if not changes else env | changes
                with self.assertRaises(RuntimeError):
                    acceptance_module.RecoveryTestBinary(output, str(rust) + "\n", selected,
                                                        repo=repo, release_ui=True)
        for release in (False, 1, "true"):
            with tempfile.TemporaryDirectory() as folder:
                repo, _, rust, _, _, output = self.artifact_fixture(folder, release_ui=True)
                with self.assertRaises(RuntimeError):
                    acceptance_module.RecoveryTestBinary(output, str(rust) + "\n", env,
                                                        repo=repo, release_ui=release)
        with tempfile.TemporaryDirectory() as folder:
            repo, _, rust, _, _, output = self.artifact_fixture(folder)
            with self.assertRaises(RuntimeError):
                acceptance_module.RecoveryTestBinary(output, str(rust) + "\n", env,
                                                    repo=repo, release_ui=True)

    @staticmethod
    def ui_output():
        proof = "\n".join("MORPHEUS_RTC_UI_CHECK " + name + "=true"
                          for name in acceptance_module.RTC_UI_CHECKS)
        return AppTimelineEvidenceTests.output() + proof + "\nMORPHEUS_RTC_UI_RESULT passed=true\n"

    def test_received_ui_requires_original23_and_exact_ordered_six_receipts(self):
        output = self.ui_output()
        self.assertEqual(acceptance_module.rtc_ui_evidence(output),
                         {name: True for name in acceptance_module.RTC_UI_CHECKS})
        invalid = ["", output + "MORPHEUS_RTC_UI_RESULT passed=true\n",
                   output.replace("passed=true", "passed=false"), output.replace("1 passed", "0 passed"),
                   output + "Skipped optional UI fixture\n",
                   output.replace("MORPHEUS_LIVE_TIMELINE_CHECK room_delivery=true", ""),
                   output + "MORPHEUS_RTC_UI_CHECK unknown=true\n",
                   output.replace("MORPHEUS_RTC_UI_CHECK cleanup=true", "quoted MORPHEUS_RTC_UI_CHECK cleanup=true")]
        for name in acceptance_module.RTC_UI_CHECKS:
            marker = "MORPHEUS_RTC_UI_CHECK " + name + "=true\n"
            invalid += [output.replace(marker, ""), output.replace(marker, marker * 2),
                        output.replace(marker, marker.replace("=true", "=false"))]
        first, second = acceptance_module.RTC_UI_CHECKS[:2]
        invalid.append(output.replace(first, "SWAP").replace(second, first).replace("SWAP", second))
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                acceptance_module.rtc_ui_evidence(value)
        for value in (None, True, 1, b"private"):
            with self.assertRaises(RuntimeError):
                acceptance_module.rtc_ui_evidence(value)

    def test_received_ui_invokes_one_existing_window_with_both_owned_fixtures(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"GITHUB_REPOSITORY": "brk275/Morpheus"}):
            root = Path(folder)
            timeline = root / "accounts.json"
            timeline.write_text("{}")
            timeline.chmod(0o600)
            rtc = root / "rtc/accounts.json"
            rtc.parent.mkdir()
            rtc.write_text("{}")
            rtc.chmod(0o600)
            runner = Acceptance(root, "scanner", suite="rtc-ui-video")
            runner.containment_verified = True
            binary = self.binary()
            binary.path = Path("/synthetic/target/release/deps/morpheus_app-0123456789abcdef")
            binary.extra["LD_LIBRARY_PATH"] = "/synthetic/target/release/deps"
            def run(label, *args, **kwargs):
                if label == "rtc-ui-video-rust-target-libdir":
                    return "/toolchain/lib/rustlib/x86_64-unknown-linux-gnu/lib\n"
                if label == "rtc-ui-video-exact-test-list":
                    return acceptance_module.APP_TIMELINE_CASE + ": test\n"
                return "compiled"
            def script(name, command):
                return str(rtc) + "\n" if command == "rtc-group-fixture" else ""
            with (patch.object(runner, "run", side_effect=run) as called,
                  patch.object(runner, "script", side_effect=script) as services,
                  patch.object(acceptance_module, "RecoveryTestBinary", return_value=binary) as admitted):
                runner.rtc_ui_video_case()
            self.assertTrue(admitted.call_args.kwargs["release_ui"])
            self.assertEqual(admitted.call_args.args[2]["MORPHEUS_TIMELINE_ACCEPTANCE_BUILD"], "1")
            self.assertEqual(called.call_args_list[1].args[1][:3], ["cargo", "test", "--release"])
            self.assertEqual(services.call_args_list, [unittest.mock.call("local_matrix_env", "up"),
                unittest.mock.call("local_matrix_rtc", "up"), unittest.mock.call("local_matrix_rtc", "probe"),
                unittest.mock.call("local_matrix_rtc", "rtc-group-fixture")])
            invocation = called.call_args_list[-1]
            self.assertEqual(invocation.args[0], acceptance_module.RTC_UI_LABEL)
            self.assertEqual(invocation.args[1][1:], [acceptance_module.APP_TIMELINE_CASE,
                                                    "--exact", "--nocapture", "--test-threads=1"])
            self.assertEqual(invocation.kwargs["seconds"], 540)
            self.assertTrue(invocation.kwargs["native_binary"] and invocation.kwargs["pid_namespace"])
            self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_MATRIX_TEST_ACCOUNTS"], str(timeline.resolve()))
            self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"], str(rtc.resolve()))
            self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_RTC_OUTAGE_TARGET"], "room")
            self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_CALL_AUDIO_ROOT"],
                             str(root / "rtc-playback-room"))
            self.assertEqual(invocation.kwargs["extra"]["ALSA_CONFIG_PATH"],
                             str(root / "rtc-playback-room/alsa.conf"))
            binary.close.assert_called_once()
            with self.assertRaises(RuntimeError):
                runner.run(acceptance_module.RTC_UI_LABEL, ["not executed"], test=True)
            record = runner.validated_record(acceptance_module.RTC_UI_LABEL, self.ui_output(), test=True,
                                            ring_case=None, oauth_checks=(), element_checks=False)
            self.assertEqual(len(record["app_timeline_checks"]), 23)
            self.assertEqual(len(record["rtc_ui_video_checks"]), 6)
            with self.assertRaises(RuntimeError):
                runner.validated_record(acceptance_module.RTC_UI_LABEL, self.ui_output(), test=False,
                                        ring_case=None, oauth_checks=(), element_checks=False)

    def test_compiler_artifact_retains_one_exact_native_file_and_cargo_loader_paths(self):
        with tempfile.TemporaryDirectory() as folder:
            repo, path, rust, native, _, output = self.artifact_fixture(folder)
            env = {"CARGO_PROFILE_TEST_DEBUG": "0", "LD_LIBRARY_PATH": "/current/fixture/path"}
            binary = acceptance_module.RecoveryTestBinary(output, str(rust) + "\n", env, repo=repo)
            fd = binary.fd
            try:
                self.assertEqual(binary.command("exact::case", "--list", "--exact"),
                                 [str(path), "exact::case", "--list", "--exact"])
                self.assertEqual(binary.cwd, repo / "crates/morpheus-app")
                self.assertEqual(binary.extra["LD_LIBRARY_PATH"].split(os.pathsep),
                                 [str(native), str(repo / "target/debug"), str(path.parent), str(rust), "/current/fixture/path"])
                self.assertEqual(binary.extra["CARGO_MANIFEST_PATH"], str(binary.cwd / "Cargo.toml"))
                self.assertEqual(os.fstat(fd).st_ino, path.stat().st_ino)
                # Same pathname is insufficient: replacement must not be admitted.
                path.unlink()
                path.write_bytes(b"different disposable artifact")
                path.chmod(0o755)
                with self.assertRaises(RuntimeError):
                    binary.command("exact::case", "--exact")
            finally:
                binary.close()
            with self.assertRaises(OSError):
                os.fstat(fd)

    def test_artifact_stream_refuses_foreign_missing_duplicate_malformed_and_wrong_profile(self):
        with tempfile.TemporaryDirectory() as folder:
            repo, _, rust, _, artifact, output = self.artifact_fixture(folder)
            finished = json.dumps({"reason": "build-finished", "success": True})
            invalid = ["", finished, json.dumps(artifact), output + "\n" + finished,
                       json.dumps(artifact) + "\n" + json.dumps(artifact) + "\n" + finished,
                       '{"reason":"build-finished","success":true,"success":false}',
                       '{malformed', "[]\n" + finished,
                       json.dumps(artifact) + "\n" + json.dumps({"reason": "build-finished", "success": False})]
            for field, value in (("package_id", "registry+https://example.invalid#morpheus-app@0.1.0"),
                                 ("manifest_path", "/foreign/Cargo.toml"), ("features", ["default"]),
                                 ("executable", "/foreign/morpheus_app-0123456789abcdef"),
                                 ("fresh", "false"), ("filenames", []), ("executable", None)):
                invalid.append(json.dumps(artifact | {field: value}) + "\n" + finished)
            for field, value in (("test", False), ("debug_assertions", False), ("opt_level", "3"), ("debuginfo", False)):
                invalid.append(json.dumps(artifact | {"profile": artifact["profile"] | {field: value}}) + "\n" + finished)
            for field, value in (("name", "foreign"), ("kind", ["test"]), ("src_path", "/foreign/main.rs")):
                invalid.append(json.dumps(artifact | {"target": artifact["target"] | {field: value}}) + "\n" + finished)
            for stream in invalid:
                with self.subTest(stream=stream[:40]), self.assertRaises(RuntimeError):
                    acceptance_module.RecoveryTestBinary(stream, str(rust) + "\n", {"CARGO_PROFILE_TEST_DEBUG": "0"}, repo=repo)
            for limit in ("MAX_LINE", "MAX_OUTPUT"):
                with patch.object(acceptance_module.RecoveryTestBinary, limit, 32), self.assertRaises(RuntimeError):
                    acceptance_module.RecoveryTestBinary(output, str(rust) + "\n", {"CARGO_PROFILE_TEST_DEBUG": "0"}, repo=repo)

    def test_artifact_admission_refuses_symlink_nonexecutable_and_foreign_layout(self):
        for mode in ("symlink_file", "symlink_deps", "nonexecutable", "foreign_layout"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as folder:
                repo, path, rust, _, artifact, output = self.artifact_fixture(folder)
                if mode == "symlink_file":
                    target = path.with_name("original")
                    path.rename(target)
                    path.symlink_to(target)
                elif mode == "symlink_deps":
                    original = path.parent.with_name("original-deps")
                    path.parent.rename(original)
                    path.parent.symlink_to(original, target_is_directory=True)
                elif mode == "nonexecutable":
                    path.chmod(0o644)
                else:
                    foreign = repo / "target/x86_64-unknown-linux-gnu/debug/deps" / path.name
                    foreign.parent.mkdir(parents=True)
                    path.rename(foreign)
                    artifact.update(executable=str(foreign), filenames=[str(foreign)])
                    output = json.dumps(artifact) + '\n{"reason":"build-finished","success":true}'
                with self.assertRaises(RuntimeError):
                    acceptance_module.RecoveryTestBinary(output, str(rust) + "\n", {"CARGO_PROFILE_TEST_DEBUG": "0"}, repo=repo)

    def prerequisite_output(self, label):
        selectors = dict(acceptance_module.OUTAGE_HELPER_TESTS)
        if label.endswith("-list"):
            selector = (acceptance_module.OUTAGE_TEST if label == "rtc-recovery-exact-test-list"
                        else selectors.get(label[:-5]))
            return selector + ": test\n" if selector else ""
        if label in selectors:
            return (f"test {selectors[label]} ... ok\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 90 filtered out; finished in 0.01s\n")
        return ""

    def output(self, target="dm"):
        facts = acceptance_module.OUTAGE_CHECKS | {acceptance_module.OUTAGE_TARGET[target]}
        return ("\n".join(f"MORPHEUS_RTC_OUTAGE_CHECK {name}=true" for name in sorted(facts))
                + f"\ntest {acceptance_module.OUTAGE_TEST} ... ok\n"
                + "MORPHEUS_RTC_OUTAGE_RESULT passed=true\n"
                + "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 90 filtered out; finished in 1.00s\n")

    def test_outage_evidence_requires_exact_live_case_all_facts_and_no_extra_markers(self):
        for target in ("dm", "room"):
            output = self.output(target)
            expected = acceptance_module.OUTAGE_CHECKS | {acceptance_module.OUTAGE_TARGET[target]}
            self.assertEqual(set(acceptance_module.outage_evidence(output, target)), expected)
            self.assertTrue({"native_audio_before", "native_audio_after"}.issubset(expected))
            invalid = ["", output.replace(acceptance_module.OUTAGE_TEST, "other::case"),
                       output.replace("1 passed", "0 passed"), output.replace("0 ignored", "1 ignored"),
                       output + "MORPHEUS_RTC_OUTAGE_RESULT passed=true\n",
                       output + "MORPHEUS_RTC_OUTAGE_CHECK private_payload=true\n",
                       output + "MORPHEUS_RTC_OUTAGE_STAGE private_payload\n",
                       output + "MORPHEUS_RTC_OUTAGE_FAILURE outcome=error call=joined\n",
                       output + "MORPHEUS_RTC_OUTAGE_MEDIA_FAILURE push_transport\n",
                       output + "MORPHEUS_RTC_OUTAGE_MEDIA_PROGRESS mask=ffff\n",
                       output + "MORPHEUS_RTC_OUTAGE_DELIVERY polled=false scheduled=false invalid_dimensions=false invalid_plane=false stale_epoch=false accepted=false worker_join_failure=false epoch=false sequence=false\n",
                       "Skipped live fixture\n" + output, self.output("room" if target == "dm" else "dm")]
            for name in expected:
                marker = f"MORPHEUS_RTC_OUTAGE_CHECK {name}=true"
                invalid.extend((output.replace(marker, ""), output.replace(marker, marker + " private"),
                                output + marker + "\n", output.replace(marker, marker.replace("true", "false"))))
            for bad in invalid:
                with self.assertRaises(RuntimeError):
                    acceptance_module.outage_evidence(bad, target)

    def test_outage_failure_keeps_work_phase_and_only_closed_precleanup_facts(self):
        for phase in ("peer_join", "initial_checkpoint", "initial_audio_source", "initial_media", "initial_audio",
                      "recovered_checkpoint", "recovered_audio_source"):
            for outcome in ("error", "timeout", "panic"):
                output = (f"MORPHEUS_RTC_OUTAGE_STAGE {phase}\n"
                          f"MORPHEUS_RTC_OUTAGE_FAILURE outcome={outcome} call=joined\n"
                          "MORPHEUS_RTC_OUTAGE_STAGE cleanup\n"
                          "MORPHEUS_RTC_OUTAGE_CHECK cleanup=true\nprivate server text\n")
                self.assertEqual(acceptance_module.outage_diagnostics(output), {
                    "stage": phase, "cleanup_started": True, "failure": outcome,
                    "call_state": "joined", "checks": {"cleanup": True}})
        valid = "MORPHEUS_RTC_OUTAGE_FAILURE outcome=error call=failed\n"
        for raw in (valid + valid, valid + "MORPHEUS_RTC_OUTAGE_FAILURE outcome=private call=failed\n",
                    valid.replace("error", "private"), valid.replace("failed", "private"),
                    valid.rstrip() + " reason=private\n"):
            self.assertEqual(acceptance_module.outage_diagnostics(raw), {"failure": "invalid"})
        for site in acceptance_module.OUTAGE_MEDIA_FAILURE_SITES:
            marker = f"MORPHEUS_RTC_OUTAGE_MEDIA_FAILURE {site}\n"
            self.assertEqual(acceptance_module.outage_diagnostics(marker), {"media_failure": site})
        marker = "MORPHEUS_RTC_OUTAGE_MEDIA_FAILURE push_transport\n"
        for raw in (marker + marker, marker + "MORPHEUS_RTC_OUTAGE_MEDIA_FAILURE private_payload\n",
                    marker.replace("push_transport", "private_payload"), marker.rstrip() + " private_payload\n",
                    "MORPHEUS_RTC_OUTAGE_MEDIA_FAILURE\n"):
            self.assertEqual(acceptance_module.outage_diagnostics(raw), {"media_failure": "invalid"})
        for mask in (0, 1, 0x1000, 0xffff):
            marker = f"MORPHEUS_RTC_OUTAGE_MEDIA_PROGRESS mask={mask:04x}\n"
            expected = {name: bool(mask & (1 << bit))
                        for bit, name in enumerate(acceptance_module.OUTAGE_MEDIA_PROGRESS_FIELDS)}
            self.assertEqual(acceptance_module.outage_diagnostics(marker), {"media_progress": expected})
        marker = "MORPHEUS_RTC_OUTAGE_MEDIA_PROGRESS mask=0001\n"
        for raw in (marker + marker, marker.rstrip() + " private_payload\n",
                    marker.replace("0001", "10000"), marker.replace("0001", "FFFF"),
                    marker.replace("0001", "private_payload"), "MORPHEUS_RTC_OUTAGE_MEDIA_PROGRESS\n"):
            self.assertEqual(acceptance_module.outage_diagnostics(raw), {"media_progress": "invalid"})
        for bits in (0, 1, 0x100, 0x1ff):
            expected = {name: bool(bits & (1 << bit))
                        for bit, name in enumerate(acceptance_module.OUTAGE_DELIVERY_FIELDS)}
            marker = "MORPHEUS_RTC_OUTAGE_DELIVERY " + " ".join(
                name + "=" + str(value).lower() for name, value in expected.items()) + "\n"
            self.assertEqual(acceptance_module.outage_diagnostics(marker), {"delivery": expected})
        for raw in (marker + marker, marker.rstrip() + " private_payload\n",
                    marker.replace("polled=true", "polled=1"), marker.replace("sequence=true", "sequence=TRUE"),
                    marker.replace(" epoch=true", ""), marker.replace("polled=true", "other=true"),
                    "MORPHEUS_RTC_OUTAGE_DELIVERY " + "x" * 385 + "\n",
                    "MORPHEUS_RTC_OUTAGE_DELIVERY\n"):
            self.assertEqual(acceptance_module.outage_diagnostics(raw), {"delivery": "invalid"})
        self.assertEqual(acceptance_module.outage_diagnostics("private server text\n"), {})
        self.assertEqual(acceptance_module.outage_diagnostics("MORPHEUS_RTC_OUTAGE_STAGE cleanup\n"),
                         {"cleanup_started": True})

    def test_outage_transport_is_positive_lower_bounds_or_unknown_and_never_success_evidence(self):
        receiver = ("MORPHEUS_RTC_OUTAGE_TRANSPORT role=receiver peer=0 sampled=true "
                    "tx_video_encoded=unknown tx_video_packets=unknown rx_video_packets=9 "
                    "rx_video_decoded=unknown capture_submitted=unknown capture_admitted=unknown\n")
        sender = ("MORPHEUS_RTC_OUTAGE_TRANSPORT role=sender peer=1 sampled=true "
                  "tx_video_encoded=18446744073709551615 tx_video_packets=4 rx_video_packets=unknown "
                  "rx_video_decoded=unknown capture_submitted=true capture_admitted=true\n")
        rows = acceptance_module.outage_diagnostics(receiver + sender)["transport"]
        self.assertEqual(rows[0], {"role": "receiver", "peer": 0, "sampled": True,
                         "tx_video_encoded": None, "tx_video_packets": None, "rx_video_packets": 9,
                         "rx_video_decoded": None, "capture_submitted": None, "capture_admitted": None})
        self.assertEqual(rows[1]["tx_video_encoded"], (1 << 64) - 1)
        unknown = receiver.replace("sampled=true", "sampled=unknown").replace("rx_video_packets=9", "rx_video_packets=unknown")
        self.assertIsNone(acceptance_module.outage_diagnostics(unknown)["transport"][0]["sampled"])
        for target in ("dm", "room"):
            with self.assertRaises(RuntimeError):
                acceptance_module.outage_evidence(self.output(target) + receiver + sender, target)

    def test_outage_transport_rejects_duplicates_overflow_zero_extra_data_and_untrusted_fields(self):
        receiver = ("MORPHEUS_RTC_OUTAGE_TRANSPORT role=receiver peer=0 sampled=true "
                    "tx_video_encoded=unknown tx_video_packets=unknown rx_video_packets=9 "
                    "rx_video_decoded=unknown capture_submitted=unknown capture_admitted=unknown\n")
        for raw in (receiver + receiver, receiver.replace("role=receiver", "role=sender"),
                    receiver.replace("peer=0", "peer=3"), receiver.replace("rx_video_packets=9", "rx_video_packets=0"),
                    receiver.replace("rx_video_packets=9", "rx_video_packets=18446744073709551616"),
                    receiver.replace("rx_video_packets=9", "rx_video_packets=09"),
                    receiver.replace("sampled=true", "sampled=unknown"),
                    receiver.replace("capture_admitted=unknown", "capture_admitted=true"),
                    receiver.rstrip() + " private_payload\n", receiver.replace("rx_video_packets=9", "private_payload=9"),
                    "MORPHEUS_RTC_OUTAGE_TRANSPORT " + "x" * 513 + "\n"):
            self.assertEqual(acceptance_module.outage_diagnostics(raw), {"transport": "invalid"})
        self.assertEqual(acceptance_module.outage_diagnostics("private server text\n"), {})

    def test_only_explicit_suite_mints_capability_and_preserves_default_plan(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {
                "GITHUB_REPOSITORY": "brk275/Morpheus", "MORPHEUS_MATRIX_SFU_RUN_ID": "ambient"}):
            default = Acceptance(Path(folder), "scanner")
            self.assertNotIn("MORPHEUS_MATRIX_SFU_RUN_ID", default.env)
            self.assertIs(default.planned_checks, acceptance_module.PLANNED_CHECKS)
            self.assertFalse(set(acceptance_module.OUTAGE_LABELS).intersection(default.planned_checks))
            with patch.object(acceptance_module.secrets, "token_hex", return_value="a" * 32) as nonce:
                selected = Acceptance(Path(folder), "scanner", suite="rtc-recovery")
            nonce.assert_called_once_with(16)
            self.assertEqual(selected.env["MORPHEUS_MATRIX_SFU_RUN_ID"], "a" * 32)
            self.assertEqual(selected.planned_checks, acceptance_module.OUTAGE_LABELS)

    def test_two_cases_compile_once_list_exact_and_use_distinct_fresh_fixtures(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"GITHUB_REPOSITORY": "brk275/Morpheus"}):
            runner = Acceptance(Path(folder), "scanner", suite="rtc-recovery")
            fixtures = [Path(folder) / f"fixture-{index}.json" for index in range(2)]
            for fixture in fixtures:
                fixture.write_text("{}")
                fixture.chmod(0o600)
            def output(label, *args, **kwargs):
                return self.prerequisite_output(label)
            with patch.object(runner, "script", side_effect=[str(path) for path in fixtures]) as script, \
                    patch.object(runner, "run", side_effect=output) as run, \
                    patch.object(acceptance_module, "RecoveryTestBinary", return_value=self.binary()) as artifact:
                runner.rtc_recovery_cases()
            self.assertEqual([item.args for item in script.call_args_list], [
                ("local_matrix_rtc", "rtc-fixture"), ("local_matrix_rtc", "rtc-group-fixture")])
            prerequisite_count = 3 + 2 * len(acceptance_module.OUTAGE_HELPER_TESTS)
            self.assertEqual(run.call_count, prerequisite_count + 2)
            self.assertEqual(run.call_args_list[0].args[1], ["rustc", "--print", "target-libdir"])
            self.assertEqual(run.call_args_list[0].kwargs["seconds"], 60)
            self.assertEqual(run.call_args_list[1].args[1][-2:], ["--no-run", "--message-format=json"])
            self.assertEqual(sum(call.args[1][0] == "cargo" for call in run.call_args_list), 1)
            artifact.assert_called_once()
            binary = artifact.return_value
            binary.close.assert_called_once()
            for invocation in run.call_args_list[2:]:
                self.assertEqual(invocation.args[1][0], str(binary.path))
                self.assertEqual(invocation.kwargs["cwd"], binary.cwd)
                self.assertTrue(invocation.kwargs["pid_namespace"])
                self.assertEqual(invocation.kwargs["extra"]["LD_LIBRARY_PATH"], binary.extra["LD_LIBRARY_PATH"])
            for index, (label, selector) in enumerate(acceptance_module.OUTAGE_HELPER_TESTS):
                listed, executed = run.call_args_list[2 + index * 2:4 + index * 2]
                self.assertEqual(listed.args[0], label + "-list")
                self.assertEqual(listed.args[1][-3:], [selector, "--list", "--exact"])
                self.assertEqual(executed.args[0], label)
                self.assertEqual(executed.args[1][-4:], [selector, "--exact", "--nocapture", "--test-threads=2"])
                self.assertTrue(listed.kwargs["native_binary"])
                self.assertEqual(listed.kwargs["seconds"], 60)
                self.assertEqual(executed.kwargs["seconds"], 60)
                self.assertTrue(executed.kwargs["test"])
                self.assertTrue(executed.kwargs["pid_namespace"])
            self.assertEqual(run.call_args_list[prerequisite_count - 1].args[1][-3:], [acceptance_module.OUTAGE_TEST, "--list", "--exact"])
            for invocation, target, fixture in zip(run.call_args_list[prerequisite_count:], ("dm", "room"), fixtures, strict=True):
                self.assertEqual(invocation.args[0], f"rtc-recovery-current-{target}")
                self.assertEqual(invocation.args[1][-5:], [acceptance_module.OUTAGE_TEST, "--ignored", "--exact", "--nocapture", "--test-threads=2"])
                self.assertTrue(invocation.kwargs["pid_namespace"])
                self.assertEqual(invocation.kwargs["seconds"], 540)
                self.assertEqual(invocation.kwargs["outage_target"], target)
                self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"], str(fixture.resolve()))
                audio_root = Path(folder) / f"rtc-playback-{target}"
                self.assertEqual(invocation.kwargs["extra"]["MORPHEUS_CALL_AUDIO_ROOT"], str(audio_root))
                self.assertEqual(invocation.kwargs["extra"]["ALSA_CONFIG_PATH"], str(audio_root / "alsa.conf"))
                self.assertFalse(audio_root.exists())  # Helper exclusively owns creation.

    def test_missing_exact_selector_stops_before_creating_accounts(self):
        labels = [label + "-list" for label, _ in acceptance_module.OUTAGE_HELPER_TESTS]
        for missing in labels + ["rtc-recovery-exact-test-list"]:
            with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"GITHUB_REPOSITORY": "brk275/Morpheus"}):
                runner = Acceptance(Path(folder), "scanner", suite="rtc-recovery")
                def output(label, *args, **kwargs):
                    return "" if label == missing else self.prerequisite_output(label)
                with patch.object(runner, "run", side_effect=output), patch.object(runner, "script") as script, \
                        patch.object(acceptance_module, "RecoveryTestBinary", return_value=self.binary()) as artifact:
                    with self.assertRaises(RuntimeError):
                        runner.rtc_recovery_cases()
                script.assert_not_called()
                artifact.return_value.close.assert_called_once()

    def test_helper_runs_require_exact_named_once_nonzero_completion_before_accounts(self):
        for label, selector in acceptance_module.OUTAGE_HELPER_TESTS:
            valid = self.prerequisite_output(label)
            for bad in (valid.replace(selector, "other::case"),
                        valid.replace("1 passed", "0 passed"),
                        valid + f"test {selector} ... ok\n",
                        valid + "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n",
                        valid.replace("0 ignored", "1 ignored")):
                with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"GITHUB_REPOSITORY": "brk275/Morpheus"}):
                    runner = Acceptance(Path(folder), "scanner", suite="rtc-recovery")
                    def output(actual, *args, **kwargs):
                        return bad if actual == label else self.prerequisite_output(actual)
                    with patch.object(runner, "run", side_effect=output), patch.object(runner, "script") as script, \
                        patch.object(acceptance_module, "RecoveryTestBinary", return_value=self.binary()) as artifact:
                        with self.assertRaises(RuntimeError):
                            runner.rtc_recovery_cases()
                    script.assert_not_called()
                    artifact.return_value.close.assert_called_once()

    def test_outage_label_cannot_publish_a_generic_success_record(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"GITHUB_REPOSITORY": "brk275/Morpheus"}):
            runner = Acceptance(Path(folder), "scanner", suite="rtc-recovery")
            with self.assertRaises(RuntimeError):
                runner.validated_record("rtc-recovery-current-dm", self.output(),
                    test=False, ring_case=None, oauth_checks=(), element_checks=False)
            with patch.object(acceptance_module.subprocess, "Popen") as spawn, self.assertRaises(RuntimeError):
                runner.run("rtc-recovery-current-dm", ["no-execution"])
            spawn.assert_not_called()

    def test_failed_outage_does_not_retry_shared_sfu_in_the_next_case(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"GITHUB_REPOSITORY": "brk275/Morpheus"}):
            runner = Acceptance(Path(folder), "scanner", suite="rtc-recovery")
            def output(label, *args, **kwargs):
                return self.prerequisite_output(label)
            with patch.object(runner, "run", side_effect=output), patch.object(runner, "group", return_value=False) as group, \
                    patch.object(acceptance_module, "RecoveryTestBinary", return_value=self.binary()):
                runner.rtc_recovery_cases()
            group.assert_called_once()
            self.assertEqual(group.call_args.args[0], ("rtc-recovery-current-dm",))
            self.assertEqual(runner.not_run, {"rtc-recovery-current-room": "dependency_failed"})
            with self.assertRaises(RuntimeError):
                runner.finish_checks()

    def test_bad_compiler_artifact_stops_before_any_native_or_fixture_command(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"GITHUB_REPOSITORY": "brk275/Morpheus"}):
            runner = Acceptance(Path(folder), "scanner", suite="rtc-recovery")
            with (patch.object(runner, "run", return_value="") as run,
                  patch.object(runner, "script") as script,
                  patch.object(acceptance_module, "RecoveryTestBinary", side_effect=RuntimeError("invalid artifact")),
                  self.assertRaises(RuntimeError)):
                runner.rtc_recovery_cases()
            self.assertEqual(run.call_count, 2)
            script.assert_not_called()

    def test_direct_list_failure_keeps_owned_cleanup_scan_and_closed_process_facts(self):
        with tempfile.TemporaryDirectory() as folder:
            runner = Acceptance(Path(folder), "scanner")
            runner.containment_verified = True
            process = Mock(returncode=0, pid=12345)
            visible = io.StringIO()
            def child(args, **kwargs):
                kwargs["stdout"].write(b"private synthetic list text\n")
                return process
            with (patch.object(acceptance_module, "namespace_command", return_value=["owned-wrapper"]) as namespace,
                  patch.object(acceptance_module.subprocess, "Popen", side_effect=child),
                  patch.object(acceptance_module, "wait_owned", side_effect=subprocess.TimeoutExpired("owned", 60)),
                  patch.object(acceptance_module, "stop_owned") as stop,
                  patch.object(acceptance_module.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as scanner,
                  contextlib.redirect_stdout(visible), self.assertRaises(RuntimeError)):
                runner.run("rtc-recovery-audio-spectrum-list", ["/synthetic/original", "exact::case", "--list", "--exact"],
                           seconds=60, pid_namespace=True, native_binary=True)
            self.assertEqual(namespace.call_args.args[0][0], "/synthetic/original")
            stop.assert_called_once_with(process)
            scanner.assert_called_once()
            records = [json.loads(line) for line in visible.getvalue().splitlines() if line.startswith("{")]
            self.assertEqual(records[-1]["native_process"]["result"], "deadline")
            self.assertNotIn("private synthetic list text", visible.getvalue())



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
                operations = Mock()
                operations.attach_mock(run, "run")
                operations.attach_mock(script, "script")
                runner.outgoing_ring_cases()
            self.assertEqual([item.args for item in script.call_args_list],
                             [("local_matrix_rtc", "rtc-fixture")] * 4)
            self.assertEqual([item[0] for item in operations.mock_calls],
                             ["run", "script", "run", "script", "run", "script", "run", "script", "run"])
            self.assertEqual(run.call_count, 5)
            compile = run.call_args_list[0]
            self.assertEqual(compile.args, ("outgoing-ring-app-test-compile", [
                "cargo", "test", "--locked", "-p", "morpheus-app", "--features", "native-calls",
                "--bin", "morpheus-app", "--no-run",
            ]))
            self.assertEqual(compile.kwargs, {"extra": {"SLINT_EMIT_DEBUG_INFO": "1"}})
            cases = [(mode, target) for mode in ("current", "sticky-2025") for target in ("dm", "room")]
            for invocation, fixture, case in zip(run.call_args_list[1:], fixtures, cases, strict=True):
                mode, target = case
                self.assertEqual(invocation.args[0], f"outgoing-ring-{mode}-{target}")
                self.assertEqual(invocation.args[1], [
                    "cargo", "test", "--locked", "-p", "morpheus-app", "--features", "native-calls",
                    "--bin", "morpheus-app", acceptance_module.RING_TEST, "--", "--ignored", "--exact",
                    "--nocapture", "--test-threads=2",
                ])
                self.assertEqual(invocation.args[1][:9], compile.args[1][:-1])
                self.assertEqual({name: invocation.kwargs["extra"][name] for name in compile.kwargs["extra"]},
                                 compile.kwargs["extra"])
                self.assertEqual(invocation.kwargs, {
                    "extra": {"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": str(fixture.resolve()),
                              "MORPHEUS_RING_TEST_MODE": mode, "MORPHEUS_RING_TEST_TARGET": target,
                              "SLINT_EMIT_DEBUG_INFO": "1"},
                    "ring_case": case, "seconds": 300, "pid_namespace": True,
                })

    def test_first_failed_case_stops_later_cases(self):
        for failure_at in ("compile", "case"):
            with self.subTest(failure_at=failure_at), tempfile.TemporaryDirectory() as folder:
                runner = Acceptance(Path(folder), "unused-scanner")
                fixture = Path(folder) / "fixture.json"
                fixture.write_text("{}")
                fixture.chmod(0o600)
                failure = RuntimeError("fixed failure")
                outcomes = [failure] if failure_at == "compile" else [None, failure]
                with (patch.object(runner, "script", return_value=str(fixture)) as script,
                      patch.object(runner, "run", side_effect=outcomes) as run,
                      self.assertRaisesRegex(RuntimeError, "fixed failure") as caught):
                    runner.outgoing_ring_cases()
                self.assertIs(caught.exception, failure)
                if failure_at == "compile":
                    script.assert_not_called()
                    run.assert_called_once()
                else:
                    script.assert_called_once()
                    self.assertEqual(run.call_count, 2)
                self.assertEqual(run.call_args_list[0].args[0], "outgoing-ring-app-test-compile")
                self.assertEqual(runner.failures, [])

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
            runner.env.update({"DBUS_SESSION_BUS_ADDRESS": "private-bus", "HOME": "private-home",
                               "SLINT_EMIT_DEBUG_INFO": "0"})
            output = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"
            with self.contained_child(runner, output, returncode=0) as (visible, command, popen, _, _, _):
                runner.run("local_rtc", ["cargo-test"], test=True, pid_namespace=True,
                           extra={"MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS": "private-fixture", "SLINT_EMIT_DEBUG_INFO": "1"})
            target, directory, environment = command.call_args.args
            self.assertEqual(target, ["cargo-test"])
            self.assertEqual(directory.parent, runner.root)
            self.assertEqual(environment["DBUS_SESSION_BUS_ADDRESS"], "private-bus")
            self.assertEqual(environment["HOME"], "private-home")
            self.assertEqual(environment["XDG_DATA_HOME"], runner.env["XDG_DATA_HOME"])
            self.assertEqual(environment["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"], "private-fixture")
            self.assertEqual(environment["SLINT_EMIT_DEBUG_INFO"], "1")
            self.assertEqual(popen.call_args.kwargs["env"]["SLINT_EMIT_DEBUG_INFO"], "1")
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
                      patch.object(runner, "run", side_effect=[None, failure, None, None, None]) as run):
                    if isinstance(failure, acceptance_module.CheckFailure):
                        runner.outgoing_ring_cases()
                        self.assertEqual(run.call_count, 5)
                        self.assertEqual(runner.failures, [{"check": "outgoing-ring-current-dm", "reason": "test_failed"}])
                    else:
                        with self.assertRaises(RuntimeError):
                            runner.outgoing_ring_cases()
                        self.assertEqual(run.call_count, 2)
                        self.assertEqual(runner.failures, [])
                self.assertEqual(run.call_args_list[0].args[0], "outgoing-ring-app-test-compile")
                case_calls = run.call_args_list[1:]
                self.assertEqual(script.call_count, len(case_calls))
                self.assertEqual(len(set(call.kwargs["extra"]["MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"]
                                         for call in case_calls)), len(case_calls))

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
        self.assertIn("browser_receive_audio", acceptance_module.ELEMENT_TEST_STAGES)
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
