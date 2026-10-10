"""No Docker or Matrix accounts are required for these privacy checks."""

import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from fixture_paths import REPOSITORY, external_path, fixture_home, write_private


class FixturePathTests(unittest.TestCase):
    def test_repository_and_descendant_are_rejected(self):
        for path in (REPOSITORY, REPOSITORY / "fixture" / "accounts.json"):
            with self.assertRaises(ValueError):
                external_path(path)

    def test_environment_cannot_choose_checkout(self):
        with patch.dict(os.environ, {"MORPHEUS_MATRIX_TEST_HOME": str(REPOSITORY)}):
            with self.assertRaises(ValueError):
                fixture_home()

    def test_symlink_into_checkout_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            link = Path(directory) / "checkout"
            link.symlink_to(REPOSITORY, target_is_directory=True)
            with self.assertRaises(ValueError):
                write_private(link / "fixture.json", "test data")

    def test_atomic_private_write_and_exclusive_creation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture" / "accounts.json"
            write_private(path, "first", create=True)
            with self.assertRaises(FileExistsError):
                write_private(path, "unexpected", create=True)
            self.assertEqual(path.read_text(), "first")
            write_private(path, "updated")
            self.assertEqual(path.read_text(), "updated")
            self.assertEqual(list(path.parent.glob(".fixture-*")), [])
            if os.name != "nt":
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
                self.assertEqual(stat.S_IMODE(path.parent.stat().st_mode), 0o700)

    def test_existing_symlink_target_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            target.write_text("untouched")
            link = Path(directory) / "link"
            link.symlink_to(target)
            with self.assertRaises(ValueError):
                write_private(link, "replacement")
            self.assertEqual(target.read_text(), "untouched")


if __name__ == "__main__":
    unittest.main()
