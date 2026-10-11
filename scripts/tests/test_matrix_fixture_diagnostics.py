import contextlib
import io
from pathlib import Path
import subprocess
import sys
import unittest
import urllib.error
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import local_matrix_env as fixture
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


if __name__ == "__main__":
    unittest.main()
