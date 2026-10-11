import os
import contextlib
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import subprocess

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from run_matrix_acceptance import Acceptance, fixture_from_output, passed_tests


class LiveEvidenceTests(unittest.TestCase):
    def test_requires_tests_to_run(self):
        for output in ("", "test result: ok. 0 passed; 0 failed; 0 ignored;",
                       "test result: ok. 1 passed; 0 failed; 1 ignored;",
                       "Skipped local RTC test\ntest result: ok. 1 passed; 0 failed; 0 ignored;"):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                passed_tests(output)

    def test_counts_real_results(self):
        self.assertEqual(passed_tests("test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured;"), 2)

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
