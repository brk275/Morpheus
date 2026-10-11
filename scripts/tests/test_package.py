import tempfile
import unittest
from pathlib import Path

from scripts.package import package_config, preflight_deb_authors


class PackageConfigTests(unittest.TestCase):
    def test_missing_deb_maintainer_fails_before_build_but_other_formats_do_not(self):
        for formats in [None, "deb", "deb,appimage", "all", "default"]:
            with self.subTest(formats=formats), self.assertRaises(ValueError):
                preflight_deb_authors({}, formats, "linux")
        for formats in ["appimage", "pacman"]:
            preflight_deb_authors({}, formats, "linux")
        preflight_deb_authors({}, "nsis", "win32")
        preflight_deb_authors({}, "app", "darwin")

    def test_deb_maintainer_needs_name_and_public_email(self):
        for author in ["Name", "name@example.org", "Name <bad>", "Name <n@example.org>\nInjected: header"]:
            with self.subTest(author=author), self.assertRaises(ValueError):
                preflight_deb_authors({"authors": [author]}, "deb", "linux")
        preflight_deb_authors({"authors": ["Fixture Maintainer <fixture@example.org>"]}, "deb", "linux")

    def test_linux_includes_platform_dependencies(self):
        config = package_config(platform="linux")
        self.assertIn("libxi6", config["deb"]["depends"])
        self.assertEqual(config["deb"]["files"]["LICENSE"], "usr/share/doc/morpheus/copyright")
        self.assertEqual(config["identifier"], "io.github.brk275.morpheus")

    def test_other_platforms_do_not_load_linux_overlay(self):
        for platform in ["darwin", "win32"]:
            self.assertNotIn("deb", package_config(platform=platform))

    def test_linux_overlay_cannot_override_shared_commands(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "packaging").mkdir()
            (root / "Packager.toml").write_text('name = "morpheus"\n')
            (root / "packaging/linux.toml").write_text('before-packaging-command = "unexpected"\n')
            with self.assertRaises(ValueError):
                package_config(root, "linux")


if __name__ == "__main__":
    unittest.main()
