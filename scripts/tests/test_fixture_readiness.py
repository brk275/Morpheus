"""Offline startup retry regressions; no sockets, containers or services."""

import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import local_matrix_env as matrix
import local_matrix_auth as oauth


class Clock:
    def __init__(self):
        self.now = 0
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class ReadinessTests(unittest.TestCase):
    def test_synapse_retries_unwrapped_reset_and_empty_responses(self):
        clock = Clock()
        with (patch.object(matrix, "time", clock),
              patch.object(matrix, "api", side_effect=[ConnectionResetError(104, "reset"),
                           {}, [], {"versions": ["v1.1"]}]) as request):
            matrix.wait_ready(matrix.SERVERS[0])
        self.assertEqual(request.call_count, 4)
        self.assertEqual(clock.sleeps, [2, 2, 2])

    def test_oauth_retries_reset_and_preserves_exact_issuer_validation(self):
        clock = Clock()
        issuer = "http://127.0.0.1:12001/"
        opener = Mock()
        opener.open.side_effect = [ConnectionResetError(104, "reset"),
                                  io.BytesIO(b"[]"),
                                  io.BytesIO(b'{"issuer":"http://127.0.0.1:12002/"}'),
                                  io.BytesIO(json.dumps({"issuer": issuer}).encode())]
        with patch.object(oauth, "time", clock), patch.object(oauth, "http_opener", return_value=opener):
            oauth.wait_ready(issuer, expected_issuer=issuer)
        self.assertEqual(opener.open.call_count, 4)
        self.assertEqual(clock.sleeps, [1, 1, 1])

    def test_persistent_resets_stop_at_existing_deadline_without_printing_error(self):
        for module in (matrix, oauth):
            with self.subTest(module=module.__name__):
                clock = Clock()
                request = Mock(side_effect=ConnectionResetError(104, "private-reset-detail"))
                opener = Mock(open=request)
                output = io.StringIO()
                with (patch.object(module, "time", clock),
                      patch.object(matrix, "api", request),
                      patch.object(oauth, "http_opener", return_value=opener),
                      patch("sys.stderr", output), patch("sys.stdout", output),
                      self.assertRaises(RuntimeError) as error):
                    if module is matrix:
                        matrix.wait_ready(matrix.SERVERS[0])
                    else:
                        oauth.wait_ready("http://127.0.0.1:12001/")
                self.assertEqual(clock.now, 120)
                self.assertEqual(request.call_count, 60 if module is matrix else 120)
                self.assertNotIn("private-reset-detail", str(error.exception) + output.getvalue())

    def test_non_network_os_failure_is_not_retried(self):
        for module in (matrix, oauth):
            with self.subTest(module=module.__name__):
                clock = Clock()
                request = Mock(side_effect=PermissionError(13, "denied"))
                opener = Mock(open=request)
                with (patch.object(module, "time", clock), patch.object(matrix, "api", request),
                      patch.object(oauth, "http_opener", return_value=opener),
                      self.assertRaises(PermissionError)):
                    if module is matrix:
                        matrix.wait_ready(matrix.SERVERS[0])
                    else:
                        oauth.wait_ready("http://127.0.0.1:12001/")
                self.assertEqual(request.call_count, 1)
                self.assertEqual(clock.sleeps, [])


if __name__ == "__main__":
    unittest.main()
