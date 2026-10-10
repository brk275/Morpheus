import tempfile
import unittest
from pathlib import Path

from scripts.package import package_config


class PackageConfigTests(unittest.TestCase):
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
