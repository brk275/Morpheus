import unittest

from scripts.package_scope import package_matrix


class PackageScopeTests(unittest.TestCase):
    def test_linux_only_never_requests_mac_or_windows(self):
        for formats in ("both", "appimage", "deb"):
            matrix = package_matrix("linux", formats)["include"]
            self.assertEqual([job["os"] for job in matrix], ["ubuntu-24.04"])
        self.assertEqual(package_matrix("linux", "appimage", True)["include"][0]["formats"], "appimage")

    def test_explicit_all_selects_exactly_four_standard_targets_once(self):
        for updates, formats in ((False, "both"), (True, "appimage")):
            matrix = package_matrix("all", formats, updates)["include"]
            self.assertCountEqual([job["os"] for job in matrix], [
                "ubuntu-24.04", "windows-2025", "macos-15", "macos-15-intel"
            ])
            self.assertEqual(len({job["label"] for job in matrix}), 4)

    def test_platform_formats_do_not_cross(self):
        self.assertEqual(package_matrix("windows", "both")["include"], [
            {"os": "windows-2025", "label": "windows-x64", "formats": "nsis"}
        ])
        mac = package_matrix("macos", "deb")["include"]
        self.assertEqual([job["os"] for job in mac], ["macos-15", "macos-15-intel"])
        self.assertTrue(all(job["formats"] == "dmg" for job in mac))

    def test_updates_cannot_silently_replace_deb_validation_or_select_absent_linux(self):
        for platform, formats in (("windows", "appimage"), ("macos", "appimage"),
                                  ("linux", "both"), ("all", "deb")):
            with self.subTest(platform=platform, formats=formats), self.assertRaises(ValueError):
                package_matrix(platform, formats, True)

    def test_unknown_selection_fails_closed(self):
        for platform, formats in (("self-hosted", "both"), ("linux", "unknown")):
            with self.assertRaises(ValueError):
                package_matrix(platform, formats)

    def test_installed_capture_selects_only_linux_and_cannot_hide_other_checks(self):
        self.assertEqual(package_matrix("linux", "appimage", False, True)["include"],
                         [{"os": "ubuntu-24.04", "label": "linux-x64", "formats": "appimage"}])
        for platform, formats, updates in (("all", "appimage", False), ("windows", "appimage", False),
                                           ("macos", "appimage", False), ("linux", "deb", False),
                                           ("linux", "appimage", True)):
            with self.subTest(platform=platform, formats=formats, updates=updates), self.assertRaises(ValueError):
                package_matrix(platform, formats, updates, True)


if __name__ == "__main__":
    unittest.main()
