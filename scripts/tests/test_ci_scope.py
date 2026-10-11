import subprocess
import unittest
from unittest.mock import patch

from scripts.ci_scope import MACOS, PLATFORMS, changed_paths, classify


class ScopeTests(unittest.TestCase):
    def test_docs_do_not_build_native_code(self):
        self.assertFalse(classify(["README.md", "docs/PACKAGING.md", "AGENTS.md"])["native"])

    def test_linux_package_change_only_selects_linux(self):
        self.assertEqual(classify(["packaging/linux.toml"])["native_matrix"]["os"], ["ubuntu-24.04"])

    def test_manual_package_workflow_does_not_duplicate_native_ci(self):
        self.assertFalse(classify([".github/workflows/package-qa.yml", "scripts/package_scope.py"])["native"])
        self.assertEqual(
            classify([".github/workflows/package-qa.yml", "packaging/linux.toml"])["native_matrix"]["os"],
            ["ubuntu-24.04"],
        )

    def test_linux_acceptance_helpers_are_actually_exercised(self):
        for path in ["scripts/run_matrix_acceptance.py", "scripts/local_matrix_rtc.py", "scripts/local_matrix_env.py", "scripts/run_wayland_acceptance.py", "scripts/wayland_fixture_backend.py", "crates/morpheus-rtc/src/linux_portal.rs", "crates/morpheus-app/src/updater/linux_transaction.rs"]:
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], ["ubuntu-24.04"])
        self.assertFalse(classify(["scripts/tests/test_matrix_acceptance.py"])["native"])

    def test_macos_capture_change_selects_both_macs(self):
        actual = classify(["vendor/nokhwa-bindings-macos/src/lib.rs"])["native_matrix"]["os"]
        self.assertEqual(actual, list(MACOS))

    def test_windows_capture_fixture_only_selects_windows(self):
        for path in ("scripts/run_windows_capture_acceptance.py", "scripts/windows_capture_window.ps1",
                     "scripts/tests/test_windows_capture_acceptance.py", "crates/morpheus-rtc/tests/windows_capture.rs"):
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], ["windows-2025"])
        # Workflow and scope implementation changes remain shared inputs.
        self.assertEqual(classify([".github/workflows/ci.yml"])["native_matrix"]["os"], list(PLATFORMS))
        self.assertEqual(classify(["scripts/ci_scope.py"])["native_matrix"]["os"], list(PLATFORMS))

    def test_pipewire_isolation_inputs_only_select_linux(self):
        for path in ("vendor/webrtc-sys/pipewire_isolation.rs", "vendor/webrtc-sys/pipewire-symbols.txt"):
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], ["ubuntu-24.04"])
        # The build script and remaining upstream source also affect Mac/Windows.
        self.assertEqual(classify(["vendor/webrtc-sys/build.rs"])["native_matrix"]["os"], list(PLATFORMS))
        self.assertEqual(classify(["vendor/webrtc-sys/src/lib.rs"])["native_matrix"]["os"], list(PLATFORMS))

    def test_delegated_oauth_acceptance_only_selects_linux(self):
        for path in ("scripts/local_matrix_auth.py", "crates/morpheus-matrix/tests/oauth_local.rs"):
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], ["ubuntu-24.04"])
        self.assertFalse(classify(["scripts/tests/test_local_matrix_auth.py"])["native"])

    def test_stock_element_fixture_only_selects_linux(self):
        for path in ("scripts/run_element_acceptance.py", "scripts/element_call_stock_peer.mjs",
                     "scripts/element_peer_fixture.py", "scripts/element-peer/package.json",
                     "scripts/element-peer/package-lock.json", "crates/morpheus-rtc/tests/element_call_peer.rs",
                     "scripts/element_process_namespace.py", "scripts/tests/test_element_process_namespace.py",
                     "scripts/matrix_fixture_diagnostics.py", "scripts/tests/test_matrix_fixture_diagnostics.py"):
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], ["ubuntu-24.04"])

    def test_shared_native_or_lock_change_selects_all(self):
        for path in ["Cargo.lock", "Packager.toml", "crates/morpheus-rtc/src/video_device.rs", "vendor/nokhwa/src/lib.rs"]:
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], list(PLATFORMS))

    def test_unknown_input_fails_closed(self):
        self.assertEqual(classify(["new-build-input.conf"])["native_matrix"]["os"], list(PLATFORMS))
        self.assertEqual(classify([".github/actions/native/action.yml"])["native_matrix"]["os"], list(PLATFORMS))

    def test_config_only_and_mixed_changes_avoid_duplicate_jobs(self):
        self.assertTrue(classify(["themes/aurora.toml"])["config"])
        mixed = classify(["themes/aurora.toml", "crates/morpheus-app/src/main.rs"])
        self.assertFalse(mixed["config"])
        self.assertEqual(mixed["native_matrix"]["os"], list(PLATFORMS))

    def test_manual_full_verification(self):
        self.assertEqual(classify([], force_all=True)["native_matrix"]["os"], list(PLATFORMS))
        self.assertEqual(changed_paths({}, "workflow_dispatch"), ([], True))

    def test_initial_push_selects_all(self):
        self.assertEqual(changed_paths({"before": "0" * 40, "after": "a" * 40}, "push"), ([], True))

    def test_invalid_sha_is_not_used_as_a_git_argument(self):
        with self.assertRaises(ValueError):
            changed_paths({"before": "--help", "after": "a" * 40}, "push")

    @patch("scripts.ci_scope.subprocess.run", side_effect=subprocess.CalledProcessError(128, "git"))
    def test_missing_diff_history_selects_all(self, run):
        self.assertEqual(changed_paths({"before": "a" * 40, "after": "b" * 40}, "push"), ([], True))

    @patch("scripts.ci_scope.subprocess.run")
    def test_pr_diff_uses_merge_base_and_nul_delimiters(self, run):
        run.return_value.stdout = b"docs/a file.md\0packaging/linux.toml\0"
        paths, all_platforms = changed_paths({"pull_request": {"base": {"sha": "a" * 40}, "head": {"sha": "b" * 40}}}, "pull_request")
        self.assertEqual(paths, ["docs/a file.md", "packaging/linux.toml"])
        self.assertFalse(all_platforms)
        self.assertEqual(run.call_args.args[0][-1], "a" * 40 + "..." + "b" * 40)


if __name__ == "__main__":
    unittest.main()
