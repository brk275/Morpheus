"""Pure runner-selection contracts; never dispatches a job."""
import unittest

from scripts.native_vault_scope import select


class NativeVaultScopeTests(unittest.TestCase):
    def test_macos_selects_both_native_architectures(self):
        self.assertEqual(select("macos")["include"], [
            {"os": "macos-15", "label": "macos-arm64", "arch": "aarch64"},
            {"os": "macos-15-intel", "label": "macos-x64", "arch": "x86_64"},
        ])

    def test_each_mac_architecture_can_be_selected_without_repeating_the_other(self):
        for choice, position in (("macos-arm64", 0), ("macos-x64", 1)):
            with self.subTest(choice=choice):
                self.assertEqual(select(choice)["include"], [select("macos")["include"][position]])
                first = select(choice)
                first["include"][0]["os"] = "unreviewed"
                self.assertEqual(select(choice)["include"], [select("macos")["include"][position]])

    def test_windows_does_not_select_another_platform(self):
        self.assertEqual(select("windows")["include"], [
            {"os": "windows-2025", "label": "windows-x64", "arch": "x86_64"},
        ])

    def test_all_has_exactly_the_three_supported_native_vault_targets(self):
        self.assertEqual(select("all")["include"], select("macos")["include"] + select("windows")["include"])
        self.assertEqual(len({item["os"] for item in select("all")["include"]}), 3)

    def test_missing_unknown_or_paid_runner_selections_are_rejected(self):
        for value in (None, "", "linux", "ubuntu-24.04", "macos-latest", "macos-15-large",
                      "windows-latest", "ALL", True, [], {"os": "self-hosted"}):
            with self.subTest(value=value), self.assertRaises(ValueError):
                select(value)

    def test_a_caller_cannot_mutate_later_selections(self):
        first = select("all")
        first["include"][0]["os"] = "unreviewed"
        self.assertEqual(select("macos")["include"][0]["os"], "macos-15")


if __name__ == "__main__":
    unittest.main()
