"""Synthetic file-tree boundaries only; no app, signing or build execution."""

import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[1] / "seal_owner_preview.py"
spec = importlib.util.spec_from_file_location("owner_preview_producer", SOURCE)
delivery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(delivery)


class BundleBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bundle = self.root / "Morpheus.app"
        self.bundle.mkdir(mode=0o700)
        self.contents = self.bundle / "Contents"
        self.contents.mkdir(mode=0o700)
        self.file = self.contents / "fixture"
        self.file.write_bytes(b"synthetic")
        self.file.chmod(0o600)

    def test_owned_regular_tree_is_accepted_without_changing_it(self):
        before = self.file.stat()
        self.assertEqual(delivery.check_bundle(self.bundle), (2, 9))
        after = self.file.stat()
        self.assertEqual((before.st_ino, before.st_mode, before.st_mtime_ns),
                         (after.st_ino, after.st_mode, after.st_mtime_ns))
        self.assertEqual(self.file.read_bytes(), b"synthetic")

    def test_only_relative_aliases_resolving_inside_the_bundle_are_accepted(self):
        alias = self.contents / "alias"
        alias.symlink_to("fixture")
        self.assertEqual(delivery.check_bundle(self.bundle), (3, 9))
        alias.unlink()
        for target in (self.file, "../../outside", "missing", "alias"):
            with self.subTest(target=target):
                alias.symlink_to(target)
                with self.assertRaises((ValueError, OSError, RuntimeError)):
                    delivery.check_bundle(self.bundle)
                alias.unlink()

    def test_hardlinks_and_special_files_are_rejected(self):
        outside = self.root / "outside"
        os.link(self.file, outside)
        with self.assertRaises(ValueError):
            delivery.check_bundle(self.bundle)
        self.assertEqual(outside.read_bytes(), b"synthetic")
        outside.unlink()
        os.mkfifo(self.contents / "pipe")
        with self.assertRaises(ValueError):
            delivery.check_bundle(self.bundle)

    def test_both_count_and_byte_limits_are_enforced(self):
        with patch.object(delivery, "MAX_FILES", 1), self.assertRaises(ValueError):
            delivery.check_bundle(self.bundle)
        with patch.object(delivery, "MAX_BYTES", 8), self.assertRaises(ValueError):
            delivery.check_bundle(self.bundle)

    def test_writable_or_aliased_roots_and_writable_files_are_rejected(self):
        self.bundle.chmod(0o777)
        with self.assertRaises(ValueError):
            delivery.check_bundle(self.bundle)
        self.bundle.chmod(0o700)
        self.file.chmod(0o666)
        with self.assertRaises(ValueError):
            delivery.check_bundle(self.bundle)
        self.file.chmod(0o600)
        alias = self.root / "alias.app"
        alias.symlink_to(self.bundle)
        with self.assertRaises(ValueError):
            delivery.check_bundle(alias)

    def test_nonhost_main_refuses_before_files_or_programs_are_opened(self):
        with (patch.object(delivery.sys, "argv", ["producer", "--source", "/unused",
              "--certificate", "/unused", "--certificate-sha256", "0" * 64,
              "--output", "/unused"]), patch.object(delivery.sys, "platform", "linux"),
              patch.object(delivery.subprocess, "run") as run,
              patch.object(delivery.Path, "read_bytes") as read,
              self.assertRaises(ValueError)):
            delivery.main()
        run.assert_not_called()
        read.assert_not_called()
