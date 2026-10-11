import contextlib
import io
from pathlib import Path
import subprocess
import sys
import unittest
import urllib.error
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import local_matrix_env as fixture
import local_matrix_rtc as rtc
from matrix_fixture_diagnostics import is_progress_line, matrix_fixture_diagnostics


class FixtureDiagnosticsTests(unittest.TestCase):
    def test_failure_reporting_survives_closed_or_broken_stderr(self):
        closed = io.StringIO()
        closed.close()
        with contextlib.redirect_stderr(closed):
            fixture.report_failure(RuntimeError("private-error"))
        class BrokenStream:
            def write(self, _value):
                raise BrokenPipeError("private-pipe-error")
            def flush(self):
                raise BrokenPipeError("private-pipe-error")
        with contextlib.redirect_stderr(BrokenStream()):
            fixture.report_failure(OSError(5, "private-os-error"))

    def test_reports_only_fixed_names_and_bounded_numbers(self):
        text = "\n".join((
            "MORPHEUS_MATRIX_FIXTURE_STAGE network",
            "MORPHEUS_MATRIX_FIXTURE_STAGE generate_a",
            "MORPHEUS_MATRIX_FIXTURE_STAGE private_password",
            "MORPHEUS_MATRIX_FIXTURE_STAGE verify trailing-password",
            "MORPHEUS_MATRIX_FIXTURE_FAILURE docker_registry",
            "MORPHEUS_MATRIX_FIXTURE_FAILURE password_value",
            "MORPHEUS_MATRIX_FIXTURE_NUMBER docker_exit=125",
            "MORPHEUS_MATRIX_FIXTURE_NUMBER os_errno=1000",
            "MORPHEUS_MATRIX_FIXTURE_NUMBER http_status=700",
            "MORPHEUS_MATRIX_FIXTURE_NUMBER access_token=123",
            "MORPHEUS_MATRIX_FIXTURE_NUMBER http_status=401 trailing-password",
        ))
        self.assertEqual(matrix_fixture_diagnostics(text),
                         {"stage": "generate_a", "failure": "docker_registry", "docker_exit": 125})
        self.assertTrue(is_progress_line("MORPHEUS_MATRIX_FIXTURE_STAGE rtc_gate"))
        self.assertFalse(is_progress_line("MORPHEUS_MATRIX_FIXTURE_STAGE rtc_gate secret"))

    def test_docker_error_never_reflects_arguments_or_response(self):
        response = subprocess.CompletedProcess(["docker", "--password=private-password"], 125,
                                               "private-token", "unauthorized: private-password")
        with patch.object(fixture.subprocess, "run", return_value=response):
            with self.assertRaises(fixture.DockerFailure) as raised:
                fixture.docker("exec", "--password=private-password")
        error = raised.exception
        visible = io.StringIO()
        with contextlib.redirect_stderr(visible):
            fixture.report_failure(error)
        self.assertNotIn("private", str(error) + repr(vars(error)) + visible.getvalue())
        self.assertEqual(matrix_fixture_diagnostics(visible.getvalue()),
                         {"failure": "docker_registry", "docker_exit": 125})

    def test_http_error_reports_status_without_url_body_or_headers(self):
        error = urllib.error.HTTPError("http://example.invalid/private-token", 403,
                                       "private-password", {"Authorization": "private-token"}, None)
        visible = io.StringIO()
        with contextlib.redirect_stderr(visible):
            fixture.report_failure(error)
        error.close()
        self.assertNotIn("private", visible.getvalue())
        self.assertEqual(matrix_fixture_diagnostics(visible.getvalue()),
                         {"failure": "http", "http_status": 403})

    def test_failure_stage_is_last_attempted_operation(self):
        visible = io.StringIO()
        with (patch.object(fixture.shutil, "disk_usage", return_value=type("Disk", (), {"free": 3 * 1024**3})()),
              patch.object(fixture, "exists", return_value=False),
              patch.object(fixture, "docker", side_effect=[None, None, RuntimeError("private-error")]),
              contextlib.redirect_stderr(visible)):
            with self.assertRaises(RuntimeError) as raised:
                fixture.start_servers()
            fixture.report_failure(raised.exception)
        self.assertNotIn("private-error", visible.getvalue())
        self.assertEqual(matrix_fixture_diagnostics(visible.getvalue()),
                         {"stage": "generate_a", "failure": "fixture_operation"})

    def test_invalid_stage_is_rejected_without_printing_it(self):
        visible = io.StringIO()
        with contextlib.redirect_stderr(visible), self.assertRaises(ValueError):
            fixture.stage("private-password")
        self.assertEqual(visible.getvalue(), "")


class RtcFixtureDiagnosticsTests(unittest.TestCase):
    def test_rtc_docker_error_is_classified_without_private_response(self):
        response = subprocess.CompletedProcess(["private-argument"], 125,
                                               "private-output", "manifest unknown: private-token")
        visible = io.StringIO()
        with (patch.object(rtc.subprocess, "run", return_value=response),
              patch.object(rtc, "main", side_effect=lambda: rtc.docker("private-argument")),
              contextlib.redirect_stderr(visible)):
            self.assertEqual(rtc.run_cli(), 1)
        self.assertNotIn("private", visible.getvalue())
        self.assertEqual(matrix_fixture_diagnostics(visible.getvalue()),
                         {"failure": "docker_registry", "docker_exit": 125})

    def test_rtc_reports_exact_startup_boundary_without_exception_text(self):
        for operation, phase in (("write_configs", "rtc_configs"),
                                 ("start_federation_bridge", "rtc_federation")):
            visible = io.StringIO()
            with (contextlib.ExitStack() as stack, contextlib.redirect_stderr(visible)):
                stack.enter_context(patch.object(rtc.shutil, "disk_usage", return_value=Mock(free=9 * 1024**3)))
                stack.enter_context(patch.object(rtc, "exists", return_value=True))
                stack.enter_context(patch.object(rtc, "stop_native_sfu"))
                stack.enter_context(patch.object(rtc, "write_configs"))
                stack.enter_context(patch.object(rtc, operation, side_effect=RuntimeError("private-value")))
                stack.enter_context(patch.object(rtc, "main", side_effect=rtc.up))
                self.assertEqual(rtc.run_cli(), 1)
            self.assertNotIn("private", visible.getvalue())
            self.assertEqual(matrix_fixture_diagnostics(visible.getvalue()),
                             {"stage": phase, "failure": "fixture_operation"})

    def test_rtc_http_failure_and_broken_report_stream_are_safe(self):
        error = urllib.error.HTTPError("http://fixture.invalid/private-token", 401,
                                       "private-body", {"Authorization": "private-header"}, None)
        visible = io.StringIO()
        with (patch.object(rtc, "main", side_effect=error), contextlib.redirect_stderr(visible)):
            self.assertEqual(rtc.run_cli(), 1)
        self.assertEqual(matrix_fixture_diagnostics(visible.getvalue()), {"failure": "http", "http_status": 401})
        self.assertNotIn("private", visible.getvalue())
        visible.close()
        with (patch.object(rtc, "main", side_effect=RuntimeError("private-value")),
              contextlib.redirect_stderr(visible)):
            self.assertEqual(rtc.run_cli(), 1)
        error.close()

    def test_rtc_readiness_retries_direct_connection_reset_then_succeeds(self):
        response = Mock(status=200)
        context = Mock()
        context.__enter__ = Mock(return_value=response)
        context.__exit__ = Mock(return_value=False)
        with (patch.object(rtc.urllib.request, "urlopen", side_effect=[ConnectionResetError("private"), context]) as request,
              patch.object(rtc.time, "monotonic", side_effect=[0, 0, .2, 1.2]),
              patch.object(rtc.time, "sleep") as sleep):
            rtc.wait_http("http://fixture.invalid/health", duration=5)
        self.assertEqual(request.call_count, 2)
        sleep.assert_called_once_with(1)

    def test_rtc_readiness_deadline_bounds_last_request_and_sleep(self):
        with (patch.object(rtc.urllib.request, "urlopen", side_effect=ConnectionResetError("private")) as request,
              patch.object(rtc.time, "monotonic", side_effect=[0, .75, .9, 1.0]),
              patch.object(rtc.time, "sleep") as sleep,
              self.assertRaises(RuntimeError)):
            rtc.wait_http("http://fixture.invalid/health", duration=1)
        self.assertEqual(request.call_args.kwargs["timeout"], .25)
        self.assertAlmostEqual(sleep.call_args.args[0], .1)

    def test_rtc_readiness_wrong_status_cannot_pass(self):
        response = Mock(status=204)
        context = Mock()
        context.__enter__ = Mock(return_value=response)
        context.__exit__ = Mock(return_value=False)
        with (patch.object(rtc.urllib.request, "urlopen", return_value=context),
              patch.object(rtc.time, "monotonic", side_effect=[0, 0, 1, 1]),
              patch.object(rtc.time, "sleep"), self.assertRaises(RuntimeError)):
            rtc.wait_http("http://fixture.invalid/health", duration=1)

    def test_rtc_malformed_error_never_formats_its_payload(self):
        class UnprintableError(ValueError):
            def __str__(self):
                raise AssertionError("private-payload-must-not-be-formatted")
        visible = io.StringIO()
        with (patch.object(rtc, "main", side_effect=UnprintableError()),
              contextlib.redirect_stderr(visible)):
            self.assertEqual(rtc.run_cli(), 1)
        self.assertEqual(matrix_fixture_diagnostics(visible.getvalue()), {"failure": "invalid_data"})
        self.assertNotIn("private", visible.getvalue())

    def test_rtc_phase_allowlist_does_not_reflect_server_or_path(self):
        self.assertEqual(matrix_fixture_diagnostics(
            "MORPHEUS_MATRIX_FIXTURE_STAGE rtc_auth_service\n"
            "MORPHEUS_MATRIX_FIXTURE_STAGE rtc_private_credential\n"
            "MORPHEUS_MATRIX_FIXTURE_STAGE rtc_transports trailing-private\n"),
            {"stage": "rtc_auth_service"})
        self.assertTrue(is_progress_line("MORPHEUS_MATRIX_FIXTURE_STAGE rtc_token_exchange"))


if __name__ == "__main__":
    unittest.main()
