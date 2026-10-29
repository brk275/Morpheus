import ast
import hashlib
import os
import json
import tempfile
import subprocess
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from scripts import ci_scope
from scripts.ci_scope import (MACOS, PLATFORMS, WAYLAND_UNRELATED_NATIVE_INPUTS,
                              WAYLAND_UNRELATED_THEME_INPUTS,
                              WAYLAND_UNRELATED_MATRIX_DIAGNOSTIC_INPUTS, changed_paths, classify,
                              helpers_host_provisioning_only, scanner_step_only,
                              event_selection, MANUAL_PLATFORMS, MANUAL_CHECKS, MATRIX_SUITES)


class ScopeTests(unittest.TestCase):
    def test_manual_native_source_producer_does_not_trigger_app_native_jobs(self):
        paths = [".github/workflows/native-webrtc-source.yml",
                 "native/webrtc/linux/pipeline-plan.json",
                 "native/webrtc/linux/patches/01-sif-validation.patch",
                 "scripts/run_native_webrtc_linux.py",
                 "scripts/native_webrtc/linux_materials.py"]
        self.assertEqual(classify(paths)["native_matrix"]["os"], [])
        self.assertEqual(classify(paths, force_all=True)["native_matrix"]["os"], list(PLATFORMS))
        for shared in ("Cargo.lock", "vendor/libwebrtc/src/lib.rs",
                       ".github/workflows/ci.yml", "native/webrtc/unknown-platform/input.json"):
            with self.subTest(shared=shared):
                self.assertEqual(classify(paths + [shared])["native_matrix"]["os"], list(PLATFORMS))

    def test_manual_windows_native_source_producer_keeps_affected_scope(self):
        paths = [".github/workflows/native-webrtc-windows-source.yml",
                 "native/webrtc/windows/pipeline-plan.json",
                 "native/webrtc/windows/required-inputs.json",
                 "scripts/build_native_webrtc_windows.py",
                 "scripts/run_native_webrtc_windows.py",
                 "scripts/native_webrtc/windows_preflight.ps1",
                 "scripts/tests/test_native_webrtc_windows_inputs.py"]
        for selected in ([path] for path in paths):
            with self.subTest(path=selected[0]):
                self.assertFalse(classify(selected)["native"])
        self.assertFalse(classify(paths)["native"])
        self.assertEqual(classify(paths, force_all=True)["native_matrix"]["os"], list(PLATFORMS))
        for shared in ("Cargo.lock", "vendor/libwebrtc/src/lib.rs", ".github/workflows/ci.yml",
                       ".github/workflows/unknown-windows-producer.yml",
                       "native/webrtc/unknown-platform/input.json", "crates/morpheus-rtc/src/media.rs"):
            with self.subTest(shared=shared):
                self.assertEqual(classify(paths + [shared])["native_matrix"]["os"], list(PLATFORMS))
        self.assertEqual(classify(paths + ["scripts/run_windows_capture_acceptance.py"])
                         ["native_matrix"]["os"], ["windows-2025"])

    def test_manual_mac_native_source_producer_keeps_affected_scope(self):
        paths = [".github/workflows/native-webrtc-macos-source.yml",
                 "native/webrtc/macos/mac-arm64/pipeline-plan.json",
                 "native/webrtc/macos/mac-x64/source-plan.json",
                 "scripts/run_native_webrtc_macos.py",
                 "scripts/native_webrtc/macos_materials.py",
                 "scripts/build_native_webrtc_unix.py",
                 ".github/workflows/native-webrtc-windows-source.yml",
                 "native/webrtc/windows/pipeline-plan.json"]
        self.assertFalse(classify(paths)["native"])
        self.assertEqual(classify(paths, force_all=True)["native_matrix"]["os"], list(PLATFORMS))
        for shared in ("Cargo.lock", "vendor/libwebrtc/src/lib.rs", ".github/workflows/unknown-mac-producer.yml",
                       "native/webrtc/unknown-platform/input.json", "crates/morpheus-rtc/src/media.rs"):
            with self.subTest(shared=shared):
                self.assertEqual(classify(paths + [shared])["native_matrix"]["os"], list(PLATFORMS))


    def test_matrix_diagnostics_preserve_native_scope_without_unrelated_portal(self):
        reviewed = {"crates/morpheus-matrix/tests/drafts_local.rs",
                    "scripts/run_matrix_acceptance.py", "scripts/tests/test_matrix_acceptance.py"}
        self.assertEqual(WAYLAND_UNRELATED_MATRIX_DIAGNOSTIC_INPUTS, reviewed)
        expected = {
            "crates/morpheus-matrix/tests/drafts_local.rs": list(PLATFORMS),
            "scripts/run_matrix_acceptance.py": ["ubuntu-24.04"],
            "scripts/tests/test_matrix_acceptance.py": [],
        }
        for path, platforms in expected.items():
            with self.subTest(path=path):
                selected = classify([path])
                self.assertEqual(selected["native_matrix"]["os"], platforms)
                self.assertFalse(selected["wayland"])
                self.assertTrue(classify([path], force_all=True)["wayland"])
        combined = sorted(reviewed | WAYLAND_UNRELATED_NATIVE_INPUTS)
        self.assertEqual(classify(combined)["native_matrix"]["os"], list(PLATFORMS))
        self.assertFalse(classify(combined)["wayland"])
        for shared in ("Cargo.lock", "crates/morpheus-matrix/src/lib.rs",
                       "crates/morpheus-matrix/Cargo.toml", "scripts/element_process_namespace.py",
                       "scripts/run_wayland_acceptance.py", "crates/morpheus-matrix/tests/new.rs",
                       "crates/morpheus-rtc/tests/wayland_capture.rs", "scripts/unknown.py"):
            with self.subTest(shared=shared):
                self.assertTrue(classify(combined + [shared])["wayland"])

    def test_reviewed_presentation_inputs_keep_full_native_without_system_wayland(self):
        reviewed = {"crates/morpheus-app/src/account_access_ui_tests.rs", "crates/morpheus-app/src/appearance.rs",
                    "crates/morpheus-app/src/live.rs", "crates/morpheus-app/src/main.rs",
                    "crates/morpheus-app/src/timeline_model.rs",
                    "crates/morpheus-app/ui/app-window.slint", "crates/morpheus-config/src/lib.rs"}
        self.assertEqual(WAYLAND_UNRELATED_NATIVE_INPUTS, reviewed)
        paths = sorted(reviewed) + ["docs/CUSTOMIZATION.md"]
        for selected in ([path] for path in paths[:-1]):
            result = classify(selected)
            self.assertEqual(result["native_matrix"]["os"], list(PLATFORMS))
            self.assertFalse(result["wayland"])
        result = classify(paths)
        self.assertEqual(result["native_matrix"]["os"], list(PLATFORMS))
        self.assertFalse(result["config"] or result["wayland"])

    def test_timeline_model_keeps_four_native_targets_and_exact_dependency_scope(self):
        timeline = "crates/morpheus-app/src/timeline_model.rs"
        result = classify([timeline])
        self.assertEqual(result["native_matrix"]["os"], list(PLATFORMS))
        self.assertFalse(result["wayland"])
        self.assertTrue(classify([timeline], force_all=True)["wayland"])
        for dependency in ("Cargo.lock", "crates/morpheus-matrix/src/lib.rs",
                           "crates/morpheus-rtc/Cargo.toml", "crates/morpheus-app/build.rs",
                           "crates/morpheus-app/src/timeline_model_extra.rs"):
            with self.subTest(dependency=dependency):
                mixed = classify([timeline, dependency])
                self.assertEqual(mixed["native_matrix"]["os"], list(PLATFORMS))
                self.assertTrue(mixed["wayland"])

    def test_wayland_dependencies_mixed_or_unknown_inputs_fail_closed(self):
        theme = "crates/morpheus-app/src/appearance.rs"
        for changed in ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml",
                        ".github/workflows/ci.yml", "crates/morpheus-app/build.rs",
                        "crates/morpheus-rtc/src/linux_portal.rs", "crates/morpheus-rtc/Cargo.toml",
                        "crates/morpheus-matrix/src/lib.rs", "crates/morpheus-matrix/Cargo.toml",
                        "vendor/webrtc-sys/pipewire_isolation.rs", "vendor/webrtc-sys/build.rs",
                        "packaging/linux/appimage_peer.rs", "scripts/run_wayland_acceptance.py",
                        "scripts/wayland_fixture_backend.py", "scripts/element_process_namespace.py",
                        "crates/morpheus-rtc/tests/support/pipewire_probe.rs", "new-build-input.conf",
                        "crates/morpheus-app/src/new-input.rs", "scripts/new-input.py",
                        "themes/unknown.toml", "themes/Cargo.toml", "themes/build.rs"):
            with self.subTest(path=changed):
                result = classify([theme, changed])
                self.assertEqual(result["native_matrix"]["os"], list(PLATFORMS))
                self.assertTrue(result["wayland"])

    def test_shipped_theme_data_keeps_app_checks_without_unrelated_portal_fixture(self):
        themes = {"themes/aurora.toml", "themes/nocturne.toml",
                  "themes/paper.toml", "themes/terminal.toml"}
        self.assertEqual(WAYLAND_UNRELATED_THEME_INPUTS, themes)
        for path in sorted(themes):
            with self.subTest(path=path):
                self.assertTrue(classify([path])["config"])
                self.assertFalse(classify([path])["native"])
                combined = classify([path, "crates/morpheus-app/src/appearance.rs"])
                self.assertEqual(combined["native_matrix"]["os"], list(PLATFORMS))
                self.assertFalse(combined["wayland"])
                self.assertTrue(classify([path], force_all=True)["wayland"])
        combined = classify(sorted(themes | WAYLAND_UNRELATED_NATIVE_INPUTS))
        self.assertEqual(combined["native_matrix"]["os"], list(PLATFORMS))
        self.assertFalse(combined["config"] or combined["wayland"])

    def test_wayland_requires_linux_and_forced_full_always_selects_it(self):
        for paths in ([], ["README.md"], ["themes/aurora.toml"], ["scripts/ci_scope.py"],
                      ["scripts/run_windows_capture_acceptance.py"], ["packaging/macos/Info.plist"]):
            self.assertFalse(classify(paths)["wayland"])
            self.assertTrue(classify(paths, force_all=True)["wayland"])
        self.assertTrue(classify(sorted(WAYLAND_UNRELATED_NATIVE_INPUTS), force_all=True)["wayland"])
        self.assertTrue(classify(["scripts/run_wayland_acceptance.py"])["wayland"])

    def test_docs_do_not_build_native_code(self):
        self.assertFalse(classify(["README.md", "docs/PACKAGING.md", "AGENTS.md"])["native"])

    def test_publishing_readme_screenshots_does_not_rebuild_native_code(self):
        self.assertFalse(classify(["README.md", "docs/screenshots/desktop-preview.png",
                                   "docs/screenshots/account-switcher.png"])["native"])
        self.assertEqual(classify(["scripts/encode_preview_pngs.py"])["native_matrix"]["os"], ["ubuntu-24.04"])
        for path in ("assets/icon.png", "docs/screenshots/build.rs"):
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], list(PLATFORMS))

    def test_linux_package_change_only_selects_linux(self):
        self.assertEqual(classify(["packaging/linux.toml"])["native_matrix"]["os"], ["ubuntu-24.04"])

    def test_explicit_startup_probe_workflow_is_separate_and_rust_support_is_linux_only(self):
        self.assertFalse(classify([".github/workflows/wayland-startup-probe.yml", "scripts/run_pipewire_startup_probe.py"])["native"])
        self.assertEqual(classify(["crates/morpheus-rtc/tests/support/pipewire_probe.rs"])["native_matrix"]["os"], ["ubuntu-24.04"])

    def test_manual_package_workflow_does_not_duplicate_native_ci(self):
        self.assertFalse(classify([".github/workflows/package-qa.yml", "scripts/package_scope.py"])["native"])
        self.assertEqual(
            classify([".github/workflows/package-qa.yml", "packaging/linux.toml"])["native_matrix"]["os"],
            ["ubuntu-24.04"],
        )
        # The shared packaging CLI is checked by helpers, not native Cargo.
        # This selection does not replace actual platform package/install QA.
        wrapper = "scripts/package.py"
        self.assertEqual(classify([wrapper]), {
            "native": False, "native_matrix": {"os": []}, "config": False, "wayland": False,
        })
        for linux in ("packaging/linux.toml", "packaging/linux/runtime-build-host.json"):
            with self.subTest(linux=linux):
                selected = classify([wrapper, linux])
                self.assertEqual(selected["native_matrix"]["os"], ["ubuntu-24.04"])
                self.assertTrue(selected["wayland"])
        for package, platforms in (("packaging/macos/Info.plist", list(MACOS)),
                                   ("packaging/windows/installer.nsi", ["windows-2025"])):
            with self.subTest(package=package):
                self.assertEqual(classify([wrapper, package])["native_matrix"]["os"], platforms)
        for shared in ("Cargo.lock", "crates/morpheus-app/src/main.rs",
                       "crates/morpheus-app/build.rs", "Packager.toml",
                       "packaging/new-platform/config.toml", "new-build-input.conf"):
            with self.subTest(shared=shared):
                selected = classify([wrapper, shared])
                self.assertEqual(selected["native_matrix"]["os"], list(PLATFORMS))
                self.assertTrue(selected["wayland"])
        forced = classify([wrapper], force_all=True)
        self.assertEqual(forced["native_matrix"]["os"], list(PLATFORMS))
        self.assertTrue(forced["wayland"])

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
        # Native workflow changes remain shared inputs.
        self.assertEqual(classify([".github/workflows/ci.yml"])["native_matrix"]["os"], list(PLATFORMS))

    def test_manual_capture_driver_changes_do_not_rerun_system_native_acceptance(self):
        for path in ("packaging/linux/package_appimage.py",
                     "packaging/linux/appimage_capture.py",
                     "packaging/linux/appimage_capture_worker.py",
                     "packaging/linux/appimage_diagnostics.py",
                     "packaging/linux/appimage_notices.py",
                     "packaging/linux/appimage_process.py",
                     "packaging/linux/appimage_runtime_build.py",
                     "packaging/linux/runtime-build.json",
                     "packaging/linux/runtime-patches/type2-runtime-fedfad51-path-bounds.patch",
                     "packaging/linux/runtime-patches/type2-runtime-path-regression.c",
                     "packaging/linux/corresponding_source.py",
                     "packaging/linux/source_bundle_qa.py"):
            with self.subTest(path=path):
                self.assertFalse(classify([path])["native"])
                self.assertEqual(classify([path, "crates/morpheus-rtc/src/linux_portal.rs"])
                                 ["native_matrix"]["os"], ["ubuntu-24.04"])
                self.assertEqual(classify([path, "Cargo.lock"])["native_matrix"]["os"], list(PLATFORMS))
                self.assertEqual(classify([path], force_all=True)["native_matrix"]["os"], list(PLATFORMS))
        # The peer is compiled by native CI; unknown package inputs fail closed.
        for path in ("packaging/linux/appimage_peer.rs", "packaging/linux/new-helper.py",
                     "packaging/linux/AppRun", "packaging/linux/appimage-tools.json",
                     "packaging/linux/runtime-build-local.json",
                     "packaging/linux/runtime-patches/type2-runtime-fedfad51-path-bounds.patch.extra",
                     "packaging/linux/runtime-patches/type2-runtime-path-regression-other.c"):
            with self.subTest(path=path):
                self.assertEqual(classify([path])["native_matrix"]["os"], ["ubuntu-24.04"])

    def test_native_vault_manual_fixture_does_not_duplicate_desktop_ci(self):
        for path in ("crates/morpheus-matrix/tests/native_vault.rs",
                     ".github/workflows/native-vault-qa.yml"):
            with self.subTest(path=path):
                self.assertFalse(classify([path])["native"])
                self.assertEqual(classify([path, "scripts/run_windows_capture_acceptance.py"])
                                 ["native_matrix"]["os"], ["windows-2025"])
                self.assertEqual(classify([path, "scripts/tests/test_windows_capture_acceptance.py"])
                                 ["native_matrix"]["os"], ["windows-2025"])
                for shared in ("crates/morpheus-matrix/src/lib.rs", "Cargo.lock",
                               ".github/workflows/ci.yml", "crates/morpheus-matrix/tests/unknown.rs"):
                    self.assertEqual(classify([path, shared])["native_matrix"]["os"], list(PLATFORMS))
                self.assertEqual(classify([path], force_all=True)["native_matrix"]["os"], list(PLATFORMS))

    def test_classifier_validation_does_not_suppress_other_affected_checks(self):
        classifier = "scripts/ci_scope.py"
        self.assertFalse(classify([classifier, "scripts/tests/test_ci_scope.py"])["native"])
        self.assertTrue(classify([classifier, "themes/aurora.toml"])["config"])
        for path in ("crates/morpheus-app/src/main.rs", ".github/workflows/ci.yml",
                     "Cargo.lock", "new-build-input.conf"):
            with self.subTest(path=path):
                self.assertEqual(classify([classifier, path])["native_matrix"]["os"], list(PLATFORMS))
        self.assertEqual(classify([classifier], force_all=True)["native_matrix"]["os"], list(PLATFORMS))

    def test_native_fixed_client_build_inputs_select_linux_and_wayland(self):
        for path in ("scripts/build_linux_native.py", "scripts/pipewire_startup_probe.py",
                     "crates/linux_native_pipewire.rs"):
            with self.subTest(path=path):
                selected = classify([path])
                self.assertEqual(selected["native_matrix"]["os"], ["ubuntu-24.04"])
                self.assertTrue(selected["wayland"])
                self.assertEqual(classify([path, "Cargo.lock"])["native_matrix"]["os"], list(PLATFORMS))

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

    def test_manual_defaults_keep_all_four_platforms_and_every_existing_check(self):
        selected = event_selection({}, "workflow_dispatch")
        self.assertEqual(selected, {
            "native": True, "native_matrix": {"os": list(PLATFORMS)}, "config": False,
            "wayland": True, "manual": True, "manual_platform": "all",
            "check_route": "all", "matrix_suite": "full",
        })
        for platform, platforms in MANUAL_PLATFORMS.items():
            with self.subTest(platform=platform):
                result = event_selection({"inputs": {"platform": platform}}, "workflow_dispatch")
                self.assertEqual(result["native_matrix"], {"os": list(platforms)})
                self.assertEqual(result["wayland"], "ubuntu-24.04" in platforms)
                self.assertEqual((result["check_route"], result["matrix_suite"]), ("all", "full"))

    def test_manual_routes_are_exact_existing_checks_and_matrix_suites(self):
        self.assertEqual(MANUAL_CHECKS, {"all", "models", "desktop", "desktop-video", "devices", "vendor", "sdk",
                                        "matrix", "wayland", "config", "helpers"})
        self.assertEqual(MATRIX_SUITES, {"full", "draft-recovery", "drafts-only", "timeline-reads",
                                         "timeline-failures", "media-only", "app-timeline",
                                         "outgoing-ring", "native-call", "oauth-only", "rtc-recovery", "element-interop"})
        source = ast.parse((Path(__file__).resolve().parents[1]/"run_matrix_acceptance.py").read_text())
        suites = [node.value for node in source.body if isinstance(node,ast.Assign)
                  and any(isinstance(target,ast.Name) and target.id=="SUITES" for target in node.targets)]
        self.assertEqual(len(suites),1)
        self.assertIsInstance(suites[0],ast.Dict)
        self.assertEqual({ast.literal_eval(key) for key in suites[0].keys},MATRIX_SUITES)
        for route in MANUAL_CHECKS:
            platform = "linux" if route in {"matrix", "wayland"} else "all"
            selected = event_selection({"inputs": {"platform": platform, "checks": route}}, "workflow_dispatch")
            self.assertEqual(selected["config"], route == "config")
            self.assertEqual(selected["native"], route not in {"config", "helpers"})
            self.assertEqual(selected["wayland"], route in {"all", "wayland"})
        for suite in MATRIX_SUITES:
            selected = event_selection({"inputs": {"platform": "linux", "checks": "matrix", "matrix_suite": suite}},
                                       "workflow_dispatch")
            self.assertEqual(selected["matrix_suite"], suite)
            self.assertEqual(selected["native_matrix"], {"os": ["ubuntu-24.04"]})
            self.assertFalse(selected["wayland"])

    def test_desktop_video_batches_only_affected_checks_on_fixed_native_platforms(self):
        for platform, platforms in MANUAL_PLATFORMS.items():
            with self.subTest(platform=platform):
                selected = event_selection({"inputs": {"platform": platform, "checks": "desktop-video"}},
                                           "workflow_dispatch")
                self.assertEqual(selected, {
                    "native": True, "native_matrix": {"os": list(platforms)}, "config": False,
                    "wayland": False, "manual": True, "manual_platform": platform,
                    "check_route": "desktop-video", "matrix_suite": "full",
                })
                for suite in MATRIX_SUITES - {"full"}:
                    with self.subTest(suite=suite), self.assertRaises(ValueError):
                        event_selection({"inputs": {"platform": platform, "checks": "desktop-video",
                                                    "matrix_suite": suite}}, "workflow_dispatch")

    def test_element_interop_requires_explicit_linux_matrix_selection(self):
        inputs = {"platform": "linux", "checks": "matrix", "matrix_suite": "element-interop"}
        self.assertEqual(event_selection({"inputs": inputs}, "workflow_dispatch"), {
            "native": True, "native_matrix": {"os": ["ubuntu-24.04"]}, "config": False,
            "wayland": False, "manual": True, "manual_platform": "linux",
            "check_route": "matrix", "matrix_suite": "element-interop",
        })
        for change in ({"platform": "all"}, {"platform": "windows"}, {"checks": "all"},
                       {"checks": "helpers"}, {"matrix_suite": "element-interop-extra"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                event_selection({"inputs": inputs | change}, "workflow_dispatch")

    def test_invalid_or_incompatible_manual_inputs_cannot_select_arbitrary_runner_or_command(self):
        values = ({"platform": "self-hosted"}, {"platform": "ubuntu-24.04"}, {"checks": "skip"},
                  {"matrix_suite": "full; echo unsafe"}, {"ref": "main"}, {"platform": []},
                  {"checks": None}, {"platform": "windows", "checks": "matrix"},
                  {"checks": "wayland"}, {"checks": "all", "matrix_suite": "drafts-only"},
                  {"platform": "windows", "checks": "config"}, {"platform": "linux", "checks": "helpers"},
                  {"platform": "all", "checks": "matrix", "matrix_suite": "oauth-only"},
                  {"platform": "linux", "checks": "all", "matrix_suite": "oauth-only"})
        for value in values:
            with self.subTest(value=value), self.assertRaises(ValueError):
                event_selection({"inputs": value}, "workflow_dispatch")
        for value in (None, [], "all"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                event_selection({"inputs": value}, "workflow_dispatch")

    @patch("scripts.ci_scope.changed_paths")
    def test_automatic_events_ignore_manual_inputs_and_preserve_impact_scope(self, changed):
        for paths in (["README.md"], ["themes/aurora.toml"], ["scripts/run_matrix_acceptance.py"],
                      ["Cargo.lock"], ["crates/morpheus-app/src/main.rs"], ["unknown-build-input"]):
            changed.return_value = (paths, False)
            for event in ("push", "pull_request"):
                with self.subTest(paths=paths, event=event):
                    selected = event_selection({"inputs": {"platform": "windows", "checks": "helpers"}}, event)
                    self.assertEqual(selected, classify(paths) | {"manual": False, "manual_platform": "all",
                                                               "check_route": "all", "matrix_suite": "full"})

    def test_github_outputs_keep_closed_route_strings_unquoted_and_matrix_as_json(self):
        with tempfile.TemporaryDirectory() as directory:
            event, output = Path(directory)/"event.json", Path(directory)/"output"
            event.write_text(json.dumps({"inputs":{"platform":"windows","checks":"vendor"}}))
            with patch("sys.argv",["ci_scope.py","--github-event"]), patch.dict(ci_scope.os.environ,{
                    "GITHUB_EVENT_PATH":str(event),"GITHUB_EVENT_NAME":"workflow_dispatch","GITHUB_OUTPUT":str(output)}), \
                    patch("builtins.print"):
                ci_scope.main()
            values=dict(line.split("=",1) for line in output.read_text().splitlines())
            self.assertEqual(values["check_route"],"vendor")
            self.assertEqual(values["manual_platform"],"windows")
            self.assertEqual(values["matrix_suite"],"full")
            self.assertEqual(values["manual"],"true")
            self.assertEqual(json.loads(values["native_matrix"]),{"os":["windows-2025"]})


    @patch("scripts.ci_scope.changed_paths")
    def test_exact_automatic_appearance_and_producer_batches_select_existing_routes(self, changed):
        app = set((
                'crates/morpheus-app/src/account_access_ui_tests.rs',
                'crates/morpheus-app/ui/app-window.slint',
                'crates/morpheus-app/src/theme_studio/mod.rs',
                'crates/morpheus-app/src/theme_studio/session.rs',
            ))
        producer = set((
                'native/webrtc/linux/pipeline-plan.json',
                'native/webrtc/linux/reference/schema5-consumer-llvm-binding.json',
                'native/webrtc/macos/evidence/LLVM-LICENSE.TXT',
                'native/webrtc/macos/evidence/dependency-caller-proof.json',
                'native/webrtc/macos/evidence/llvm-source-license-binding.json',
                'native/webrtc/macos/mac-arm64/dependency-decisions.json',
                'native/webrtc/macos/mac-arm64/pipeline-plan.json',
                'native/webrtc/macos/mac-arm64/selected-mac-materials.json',
                'native/webrtc/macos/mac-x64/dependency-decisions.json',
                'native/webrtc/macos/mac-x64/pipeline-plan.json',
                'native/webrtc/macos/mac-x64/selected-mac-materials.json',
                'scripts/build_native_webrtc_unix.py',
                'scripts/native_webrtc/macos_materials.py',
                'scripts/native_webrtc/source_importer.py',
                'scripts/run_native_webrtc_linux.py',
                'scripts/run_native_webrtc_macos.py',
                'scripts/tests/test_native_webrtc_unix_inputs.py',
            ))
        self.assertEqual(ci_scope.AUTOMATIC_DESKTOP_INPUTS, app)
        self.assertEqual(ci_scope.AUTOMATIC_PRODUCER_INPUTS, producer)
        pack = {"native/webrtc/linux/pipeline-plan.json", "native/webrtc/macos/mac-arm64/pipeline-plan.json",
                "native/webrtc/macos/mac-x64/pipeline-plan.json", "scripts/run_native_webrtc_linux.py",
                "scripts/run_native_webrtc_macos.py", "scripts/tests/test_native_webrtc_unix_inputs.py"}
        batches = [(set(), "all"), (app, "desktop"), (app | pack, "desktop"),
                   (app | producer, "desktop"), (producer, "helpers")]
        # The generated-UI regression and exact markup change retain the
        # complete desktop matrix without unrelated protocol/device binaries.
        batches += [({"crates/morpheus-app/ui/app-window.slint",
                      "crates/morpheus-app/src/account_access_ui_tests.rs"}, "desktop")]
        batches += [({path}, "desktop") for path in app]
        batches += [({path}, "helpers") for path in producer]
        for paths, route in batches:
            changed.return_value = (sorted(paths), False)
            for event in ("push", "pull_request"):
                with self.subTest(paths=paths, event=event):
                    selected = event_selection({"inputs": {"checks": "helpers", "platform": "linux"}}, event)
                    self.assertEqual(selected["check_route"], route)
                    self.assertEqual(selected["matrix_suite"], "full")
                    self.assertFalse(selected["manual"])
                    if route != "all":
                        self.assertEqual(selected["native_matrix"], {"os": list(PLATFORMS) if route == "desktop" else []})
                        self.assertFalse(selected["config"] or selected["wayland"])
        for dependency in ("Cargo.lock", "Cargo.toml", "crates/morpheus-app/Cargo.toml",
                           "crates/morpheus-app/src/main.rs", "crates/morpheus-app/src/live.rs",
                           "crates/morpheus-app/src/appearance_editor.rs", "crates/morpheus-config/src/lib.rs",
                           "crates/morpheus-app/src/settings_search.rs", "crates/morpheus-app/src/account_access.rs",
                           "crates/morpheus-app/ui/unknown.slint", "crates/morpheus-app/ui/call-bar.slint",
                           "crates/morpheus-app/build.rs", "crates/morpheus-ui-check/build.rs",
                           "crates/morpheus-app/src/theme_studio/scope.rs", "crates/morpheus-app/src/theme_studio/fonts.rs",
                           "crates/morpheus-app/src/theme_studio/packs.rs", "crates/morpheus-rtc/src/media.rs",
                           "vendor/libwebrtc/src/lib.rs", ".github/workflows/ci.yml", "scripts/ci_scope.py",
                           "scripts/scan_history.py", ".gitleaksignore", "scripts/unknown.py",
                           "native/webrtc/macos/new-asset.json", "README.md"):
            changed.return_value = (sorted(app | producer | {dependency}), False)
            selected = event_selection({}, "push")
            self.assertEqual(selected["check_route"], "all", dependency)
            self.assertEqual(selected["native_matrix"]["os"], list(PLATFORMS), dependency)
        for force in (True,):
            changed.return_value = (sorted(app | producer), force)
            selected = event_selection({}, "push")
            self.assertEqual(selected["check_route"], "all")
            self.assertTrue(selected["wayland"])
        for raw in (("./" + next(iter(app))), next(iter(app)) + "/../session.rs", None):
            self.assertEqual(ci_scope.automatic_route([raw]), "all")
        self.assertEqual(event_selection({}, "workflow_dispatch")["check_route"], "all")
        self.assertTrue(event_selection({}, "workflow_dispatch")["wayland"])


    @patch("scripts.ci_scope.changed_paths")
    @patch("scripts.ci_scope.subprocess.run")
    def test_exact_reviewed_control_layout_selects_helpers_and_all_other_inputs_fail_closed(self, run, changed):
        control = {"scripts/ci_scope.py", "scripts/tests/test_ci_scope.py",
                   ".github/workflows/ci.yml", "scripts/tests/test_ci_acceptance.py"}
        self.assertEqual(ci_scope.AUTOMATIC_CONTROL_INPUTS, control)
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml").read_bytes()
        # Bind the production admission independently. Exercise real hash/size
        # comparisons with owned fixture bytes under test-only pins, so a
        # future different workflow can safely fall back all without requiring
        # its execution layout to be admitted by this exemption.
        self.assertEqual(ci_scope.REVIEWED_CONTROL_WORKFLOW_BYTES, 65769)
        self.assertEqual(ci_scope.REVIEWED_CONTROL_WORKFLOW_SHA256,
                         "2b0422f336ee633146a69212a68ab6e87bd666c337c5dab1583b76a82c0f9b2d")
        import hashlib
        with patch.object(ci_scope, "REVIEWED_CONTROL_WORKFLOW_BYTES", len(workflow)), \
                patch.object(ci_scope, "REVIEWED_CONTROL_WORKFLOW_SHA256", hashlib.sha256(workflow).hexdigest()):
            for event_name, event in (("push", {"after": "a" * 40}),
                                      ("pull_request", {"pull_request": {"head": {"sha": "b" * 40}}})):
                for paths in [[path] for path in sorted(control)] + [sorted(control)]:
                    changed.return_value = (paths, False)
                    run.side_effect = [SimpleNamespace(stdout=str(len(workflow)).encode() + b"\n"),
                                       SimpleNamespace(stdout=workflow)]
                    selected = event_selection({**event, "inputs": {"checks": "all", "platform": "windows"}}, event_name)
                    self.assertEqual(selected, {"native": False, "native_matrix": {"os": []}, "config": False,
                                               "wayland": False, "manual": False, "manual_platform": "all",
                                               "check_route": "helpers", "matrix_suite": "full"})
                    expected = event["after"] if event_name == "push" else event["pull_request"]["head"]["sha"]
                    self.assertEqual(run.call_args.args[0][-2:], ["blob", expected + ":.github/workflows/ci.yml"])
                    self.assertNotIn("shell", run.call_args.kwargs)
                    self.assertEqual(run.call_args.kwargs["stderr"], subprocess.DEVNULL)
                    self.assertEqual(run.call_args.kwargs["timeout"], 10)
            event = {"after": "a" * 40}
            for paths in ([], list(control) + ["README.md"], list(control) + ["Cargo.lock"],
                          list(control) + ["scripts/scan_history.py"], list(control) + [".gitleaksignore"],
                          list(control) + ["crates/morpheus-app/src/theme_studio/mod.rs"],
                          list(control) + ["native/webrtc/linux/pipeline-plan.json"],
                          list(control) + ["crates/morpheus-rtc/src/media.rs"],
                          list(control) + [".github/workflows/new.yml"], ["./scripts/ci_scope.py"]):
                run.reset_mock(); self.assertFalse(ci_scope.reviewed_control_only(paths, event, "push")); run.assert_not_called()
            for bad in (None, "", "A" * 40, "a" * 39, "--help", "a" * 40 + ":other", [], {}):
                run.reset_mock(); self.assertFalse(ci_scope.reviewed_control_only(list(control), {"after": bad}, "push")); run.assert_not_called()
            for event_name, malformed in (("push", {}), ("pull_request", {}), ("pull_request", {"pull_request": []}),
                                          ("pull_request", {"pull_request": {"head": None}}), ("workflow_dispatch", event)):
                run.reset_mock(); self.assertFalse(ci_scope.reviewed_control_only(list(control), malformed, event_name)); run.assert_not_called()
            run.reset_mock(); self.assertFalse(ci_scope.reviewed_control_only(list(control), event, "push", True)); run.assert_not_called()
            changed.return_value = (sorted(control), True)
            self.assertEqual(event_selection(event, "push")["check_route"], "all")
            run.assert_not_called()
            for size in (b"", b"999999999999\n", b"-1\n", str(len(workflow) + 1).encode() + b"\n", "not-bytes"):
                run.reset_mock(); run.side_effect = [SimpleNamespace(stdout=size)]
                self.assertFalse(ci_scope.reviewed_control_only(list(control), event, "push"))
                self.assertEqual(run.call_count, 1)
            for content in (workflow + b"#", workflow[:-1], b"x" * len(workflow), "not-bytes"):
                run.reset_mock(); run.side_effect = [SimpleNamespace(stdout=str(len(workflow)).encode() + b"\n"), SimpleNamespace(stdout=content)]
                self.assertFalse(ci_scope.reviewed_control_only(list(control), event, "push"))
            for error in (OSError("opaque"), subprocess.CalledProcessError(1, "git"), subprocess.TimeoutExpired("git", 10)):
                run.reset_mock(); run.side_effect = error
                self.assertFalse(ci_scope.reviewed_control_only(list(control), event, "push"))
            # Missing candidate identity never activates the exception; existing
            # manual/release all and normal appearance/producer rules stay intact.
            run.reset_mock(); run.side_effect = None; changed.return_value = (sorted(control), False)
            self.assertEqual(event_selection({}, "push")["check_route"], "all"); run.assert_not_called()
            self.assertEqual(event_selection({}, "workflow_dispatch")["check_route"], "all")

        # Outside every test-only pin override, a same-size unrelated blob
        # must fail the independently reviewed production SHA admission.
        run.reset_mock()
        run.side_effect = [SimpleNamespace(stdout=b"64676\n"),
                           SimpleNamespace(stdout=b"x" * 64676)]
        self.assertFalse(ci_scope.reviewed_control_only(sorted(control), {"after": "a" * 40}, "push"))


    @patch("scripts.ci_scope.reviewed_workflow_layout", return_value=True)
    @patch("scripts.ci_scope.subprocess.run")
    def test_apply_native_reuse_requires_qualified_exact_inputs_and_owner_main_push(self, run, workflow):
        app = sorted(ci_scope.AUTOMATIC_DESKTOP_INPUTS)
        event = {"after": "a" * 40, "ref": "refs/heads/main", "forced": False, "deleted": False,
                 "repository": {"full_name": "brk275/Morpheus", "private": False, "owner": {"login": "brk275"}},
                 "sender": {"login": "brk275"}}
        qualification = {"source": "ee1bfdf1eff5c234a413c1f147b9ac6dd19f07fa",
                         "tree": "9e81e57e38c37881ff45ae798bdd15dc7ca80bfd",
                         "workflow_sha256": ci_scope.REVIEWED_CONTROL_WORKFLOW_SHA256,
                         "receipt_sha256": "b" * 64, "route": "desktop",
                         "jobs": tuple((platform, i + 1, i + 10) for i, platform in enumerate(PLATFORMS))}
        selectors = (b"100644 blob " + b"c" * 40 + b"\tscripts/ci_scope.py\x00"
                     b"100644 blob " + b"d" * 40 + b"\tscripts/tests/test_ci_scope.py\x00")
        # The disabled state always fails closed; maintainer activation with
        # actual independently qualified jobs is a separate source receipt.
        with patch.object(ci_scope, "NATIVE_REUSE_QUALIFICATION", None):
            self.assertFalse(ci_scope.qualified_apply_main_reuse(app, event, "push"))
        run.assert_not_called(); workflow.assert_not_called()
        with (patch.object(ci_scope, "NATIVE_REUSE_QUALIFICATION", qualification),
              patch.dict(ci_scope.os.environ, {"GITHUB_ACTOR": "brk275", "GITHUB_TRIGGERING_ACTOR": "brk275"})):
            def compare(paths=app, changed_event=event, name="push", force=False, *, listing=selectors, result=0):
                run.reset_mock(); workflow.reset_mock()
                run.side_effect = [SimpleNamespace(stdout=listing), SimpleNamespace(returncode=result)]
                return ci_scope.qualified_apply_main_reuse(paths, changed_event, name, force)
            self.assertTrue(compare())
            arguments = run.call_args.args[0]
            self.assertEqual(arguments[-6:], [ci_scope.NATIVE_REUSE_SOURCE, "a" * 40, "--", ".",
                                            ":(top,exclude)scripts/ci_scope.py", ":(top,exclude)scripts/tests/test_ci_scope.py"])
            self.assertEqual(run.call_args.kwargs["stdout"], subprocess.DEVNULL)
            self.assertEqual(run.call_args.kwargs["stderr"], subprocess.DEVNULL)
            self.assertEqual(run.call_args.kwargs["timeout"], 10)
            self.assertIn("--no-ext-diff", arguments); self.assertIn("--no-textconv", arguments)
            self.assertIn("--ignore-submodules=none", arguments); self.assertIn("-r", arguments)
            for paths in (app + list(ci_scope.NATIVE_REUSE_SELECTORS), [app[0]]):
                self.assertTrue(compare(paths))
            for changed_event in (event | {"ref": "refs/heads/topic"}, event | {"forced": True}, event | {"deleted": True},
                                  event | {"after": "--help"}, event | {"after": None},
                                  event | {"sender": {"login": "other"}}, event | {"repository": {}}, {}):
                self.assertFalse(compare(changed_event=changed_event)); run.assert_not_called()
            for name in ("pull_request", "workflow_dispatch", "workflow_run"):
                self.assertFalse(compare(name=name)); run.assert_not_called()
            self.assertFalse(compare(force=True)); run.assert_not_called()
            for paths in ([], list(ci_scope.NATIVE_REUSE_SELECTORS), app + ["Cargo.lock"],
                          app + ["scripts/run_native_webrtc_linux.py"], app + ["scripts/tests/test_ci_acceptance.py"],
                          app + [".github/workflows/ci.yml"], app + ["scripts/scan_history.py"], app + ["unknown"]):
                self.assertFalse(compare(paths)); run.assert_not_called()
            for actor in ("GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR"):
                with patch.dict(ci_scope.os.environ, {actor: "other"}):
                    self.assertFalse(compare()); run.assert_not_called()
            workflow.return_value = False
            self.assertFalse(compare()); run.assert_not_called()
            workflow.return_value = True
            for listing in (b"", selectors.replace(b"100644", b"040000", 1), selectors.replace(b"blob", b"tree", 1),
                            selectors.replace(b"100644", b"120000", 1), selectors + b"foreign\x00", "not-bytes"):
                self.assertFalse(compare(listing=listing)); self.assertEqual(run.call_count, 1)
            for result in (1, 2, -9, None, "0", False):
                self.assertFalse(compare(result=result))
            run.reset_mock(); run.side_effect = OSError("opaque")
            self.assertFalse(ci_scope.qualified_apply_main_reuse(app, event, "push"))
            for bad in (None, {}, qualification | {"source": "e" * 40}, qualification | {"tree": "e" * 40},
                        qualification | {"receipt_sha256": ""}, qualification | {"route": "all"},
                        qualification | {"jobs": qualification["jobs"][:3]},
                        qualification | {"jobs": (qualification["jobs"][0],) * 4}):
                with patch.object(ci_scope, "NATIVE_REUSE_QUALIFICATION", bad):
                    self.assertFalse(compare()); run.assert_not_called()
            with patch("scripts.ci_scope.changed_paths", return_value=(app, False)):
                run.side_effect = [SimpleNamespace(stdout=selectors), SimpleNamespace(returncode=0)]
                selected = event_selection(event, "push")
                self.assertEqual(selected["check_route"], "helpers")
                self.assertFalse(selected["native"] or selected["config"] or selected["wayland"])
                self.assertFalse(selected["manual"])

    @patch("scripts.ci_scope.reviewed_workflow_layout", return_value=True)
    @patch("scripts.ci_scope.subprocess.run")
    def test_vendor_native_reuse_is_disabled_until_exact_four_job_qualification(self, run, workflow):
        inputs = ("vendor/controlled-native/source.rs", "crates/controlled-native/src/lib.rs")
        event = {"after": "a" * 40, "ref": "refs/heads/main", "forced": False, "deleted": False,
                 "repository": {"full_name": "brk275/Morpheus", "private": False, "owner": {"login": "brk275"}},
                 "sender": {"login": "brk275"}}
        qualification = {"source": "e" * 40, "tree": "f" * 40,
                         "workflow_sha256": ci_scope.REVIEWED_CONTROL_WORKFLOW_SHA256,
                         "receipt_sha256": "b" * 64, "route": "vendor", "inputs": inputs,
                         "jobs": tuple((platform, 42, i + 10) for i, platform in enumerate(PLATFORMS))}
        selectors = (b"100644 blob " + b"c" * 40 + b"\tscripts/ci_scope.py\x00"
                     b"100644 blob " + b"d" * 40 + b"\tscripts/tests/test_ci_scope.py\x00")
        # Fixture IDs/receipts are controlled test DATA, never production qualification.
        with patch.object(ci_scope, "VENDOR_REUSE_QUALIFICATION", None):
            self.assertFalse(ci_scope.qualified_vendor_main_reuse(list(inputs), event, "push"))
            with patch("scripts.ci_scope.changed_paths", return_value=(list(inputs), False)):
                disabled = event_selection(event, "push")
                self.assertEqual(disabled["check_route"], "all")
                self.assertTrue(disabled["native"])
                self.assertEqual(disabled["native_matrix"]["os"], list(PLATFORMS))
        run.assert_not_called(); workflow.assert_not_called()
        with (patch.object(ci_scope, "VENDOR_REUSE_QUALIFICATION", qualification),
              patch.dict(ci_scope.os.environ, {"GITHUB_ACTOR": "brk275", "GITHUB_TRIGGERING_ACTOR": "brk275",
                    "GIT_DIR": "untrusted", "GIT_CONFIG_GLOBAL": "untrusted", "SSH_ASKPASS": "untrusted"})):
            def compare(paths=list(inputs), changed_event=event, name="push", force=False, *,
                        kind=b"commit\n", tree=b"f" * 40 + b"\n", listing=selectors, result=0):
                run.reset_mock(); workflow.reset_mock()
                run.side_effect = [SimpleNamespace(stdout=kind), SimpleNamespace(stdout=tree),
                                   SimpleNamespace(stdout=listing), SimpleNamespace(returncode=result)]
                return ci_scope.qualified_vendor_main_reuse(paths, changed_event, name, force)
            # One real all-platform vendor workflow has one run and four jobs.
            self.assertTrue(compare())
            self.assertEqual(run.call_count, 4)
            calls = run.call_args_list
            self.assertEqual(calls[0].args[0][-3:], ["cat-file", "-t", "e" * 40])
            self.assertEqual(calls[1].args[0][-3:], ["rev-parse", "--verify", "e" * 40 + "^{tree}"])
            self.assertEqual(calls[2].args[0][-7:], ["ls-tree", "--full-tree", "-z", "a" * 40, "--", *ci_scope.NATIVE_REUSE_SELECTORS])
            arguments = calls[-1].args[0]
            self.assertEqual(arguments[-6:], ["e" * 40, "a" * 40, "--", ".",
                ":(top,exclude)scripts/ci_scope.py", ":(top,exclude)scripts/tests/test_ci_scope.py"])
            for option in ("--no-replace-objects", "--no-ext-diff", "--no-textconv", "--ignore-submodules=none", "-r"):
                self.assertIn(option, arguments)
            for call in calls:
                self.assertEqual(call.kwargs["timeout"], 10)
                self.assertEqual(call.kwargs["stderr"], subprocess.DEVNULL)
                environment = call.kwargs["env"]
                self.assertNotIn("GIT_DIR", environment); self.assertNotIn("SSH_ASKPASS", environment)
                self.assertEqual(environment["GIT_CONFIG_GLOBAL"], "/dev/null")
                self.assertEqual(environment["GIT_NO_LAZY_FETCH"], "1")
            self.assertEqual(calls[-1].kwargs["stdout"], subprocess.DEVNULL)
            for paths in ([inputs[0]], list(inputs) + list(ci_scope.NATIVE_REUSE_SELECTORS)):
                self.assertTrue(compare(paths))
            # Separate genuinely qualified runs remain valid as well.
            distinct_runs = qualification | {"jobs": tuple((platform, i + 1, i + 10)
                                                        for i, platform in enumerate(PLATFORMS))}
            with patch.object(ci_scope, "VENDOR_REUSE_QUALIFICATION", distinct_runs):
                self.assertTrue(compare())
            for paths in ([], None, [None], list(ci_scope.NATIVE_REUSE_SELECTORS),
                          list(inputs) + ["Cargo.lock"], list(inputs) + [".github/workflows/ci.yml"],
                          list(inputs) + ["rust-toolchain.toml"], list(inputs) + ["scripts/scan_history.py"],
                          list(inputs) + ["vendor/unrecorded.rs"], list(inputs) + ["unknown"], ["./" + inputs[0]]):
                self.assertFalse(compare(paths)); run.assert_not_called(); workflow.assert_not_called()
            for changed_event in (event | {"ref": "refs/heads/topic"}, event | {"forced": True},
                                  event | {"deleted": True}, event | {"after": "--help"}, event | {"after": None},
                                  event | {"sender": {"login": "other"}}, event | {"repository": {}}, {}):
                self.assertFalse(compare(changed_event=changed_event)); run.assert_not_called()
            for name in ("pull_request", "workflow_dispatch", "workflow_run", "release"):
                self.assertFalse(compare(name=name)); run.assert_not_called()
            self.assertFalse(compare(force=True)); run.assert_not_called()
            for actor in ("GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR"):
                with patch.dict(ci_scope.os.environ, {actor: "other"}):
                    self.assertFalse(compare()); run.assert_not_called()
            for field, values in (("kind", (b"tree\n", b"tag\n", b"", "commit\n")),
                                  ("tree", (b"d" * 40 + b"\n", b"", "f" * 40 + "\n"))):
                for value in values:
                    self.assertFalse(compare(**{field: value}))
                    self.assertEqual(run.call_count, 1 if field == "kind" else 2)
            for listing in (b"", selectors.replace(b"100644 blob", b"040000 tree", 1),
                            selectors.replace(b"blob", b"tree", 1), selectors.replace(b"100644", b"120000", 1),
                            selectors.replace(b"100644 blob", b"160000 commit", 1), selectors.replace(b"100644", b"100755", 1),
                            selectors + b"foreign\x00", "not-bytes"):
                self.assertFalse(compare(listing=listing)); self.assertEqual(run.call_count, 3)
            for result in (1, 2, -9, None, "0", False):
                self.assertFalse(compare(result=result))
            workflow.return_value = False
            self.assertFalse(compare()); run.assert_not_called()
            workflow.return_value = True
            for bad in (None, {}, qualification | {"source": "--help"}, qualification | {"tree": "F" * 40},
                        qualification | {"workflow_sha256": "b" * 64}, qualification | {"receipt_sha256": ""},
                        qualification | {"route": "desktop"}, qualification | {"jobs": qualification["jobs"][:3]},
                        qualification | {"jobs": (qualification["jobs"][0],) * 4},
                        qualification | {"jobs": tuple((platform, 42, 10) for platform in PLATFORMS)},
                        qualification | {"jobs": ((PLATFORMS[0], True, 10),) + qualification["jobs"][1:]},
                        qualification | {"inputs": ()}, qualification | {"inputs": (inputs[0], inputs[0])},
                        qualification | {"inputs": ([],)}, qualification | {"inputs": ("../outside",)},
                        qualification | {"inputs": ("/absolute",)}, qualification | {"inputs": ("a/.git/b",)},
                        qualification | {"inputs": ("a\\b",)}, qualification | {"inputs": ("a\nb",)},
                        qualification | {"inputs": ci_scope.NATIVE_REUSE_SELECTORS}):
                with patch.object(ci_scope, "VENDOR_REUSE_QUALIFICATION", bad):
                    self.assertFalse(compare()); run.assert_not_called(); workflow.assert_not_called()
            for error in (OSError("opaque"), subprocess.CalledProcessError(1, "git"), subprocess.TimeoutExpired("git", 10)):
                run.reset_mock(); run.side_effect = error
                self.assertFalse(ci_scope.qualified_vendor_main_reuse(list(inputs), event, "push"))
            with patch("scripts.ci_scope.changed_paths", return_value=(list(inputs), False)):
                run.side_effect = [SimpleNamespace(stdout=b"commit\n"), SimpleNamespace(stdout=b"f" * 40 + b"\n"),
                                   SimpleNamespace(stdout=selectors), SimpleNamespace(returncode=0)]
                selected = event_selection(event, "push")
                self.assertEqual(selected["check_route"], "helpers")
                self.assertFalse(selected["native"] or selected["config"] or selected["wayland"])
                self.assertFalse(selected["manual"])

    def test_frozen_appearance_reuse_checks_actual_merge_tree_and_independent_inputs(self):
        # Real immutable Git DATA, not candidate code or a native build. The
        # synthetic hashes below are confined to this existing selector test.
        real_run = subprocess.run
        safe_env = {key: value for key, value in os.environ.items() if not key.startswith(("GIT_", "SSH_"))}
        safe_env.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_ATTR_NOSYSTEM="1",
                        GIT_NO_REPLACE_OBJECTS="1", GIT_NO_LAZY_FETCH="1", GIT_OPTIONAL_LOCKS="0",
                        GIT_AUTHOR_NAME="Morpheus fixture", GIT_COMMITTER_NAME="Morpheus fixture",
                        GIT_AUTHOR_EMAIL="fixture@example.invalid", GIT_COMMITTER_EMAIL="fixture@example.invalid",
                        GIT_AUTHOR_DATE="2000-01-01T00:00:00+0000", GIT_COMMITTER_DATE="2000-01-01T00:00:00+0000")
        command = ["git", "--no-pager", "--no-replace-objects", "-c", "core.hooksPath=/dev/null",
                   "-c", "core.fsmonitor=false", "-c", "core.attributesFile=/dev/null", "-c", "gc.auto=0"]
        with tempfile.TemporaryDirectory() as temporary:
            def git(*args, data=None):
                return real_run(command + list(args), input=data, cwd=temporary, env=safe_env,
                                check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout

            git("init", "--quiet")

            def commit(entries, message):
                git("read-tree", "--empty")
                for path, (mode, data) in sorted(entries.items()):
                    blob = git("hash-object", "--no-filters", "-w", "--stdin", data=data).decode().strip()
                    git("update-index", "--add", "--cacheinfo", mode, blob, path)
                tree = git("write-tree").decode().strip()
                source = git("commit-tree", tree, data=(message + "\n").encode()).decode().strip()
                return source, tree

            ui = "crates/morpheus-app/src/main.rs"
            windows = tuple(path for path, _ in ci_scope.APPEARANCE_REUSE_WINDOWS_INPUTS)
            selectors = ci_scope.NATIVE_REUSE_SELECTORS
            workflow = b"name: controlled frozen workflow\n"
            native_entries = {ui: ("100644", b"reviewed UI"), "Cargo.lock": ("100644", b"reviewed lock"),
                              ".github/workflows/ci.yml": ("100644", workflow),
                              **{path: ("100644", b"old selector") for path in selectors},
                              **{path: ("100644", b"old Windows input") for path in windows}}
            composed_entries = native_entries | {path: ("100644", b"qualified " + path.encode()) for path in windows}
            independent_entries = composed_entries | {ui: ("100644", b"independent source has no UI qualification")}
            native, native_tree = commit(native_entries, "native execution")
            source, tree = commit(composed_entries, "reviewed composition")
            independent, independent_tree = commit(independent_entries, "independent Windows execution")
            window_blobs = tuple((path, git("rev-parse", independent + ":" + path).decode().strip()) for path in windows)
            globals_changed = {
                "APPEARANCE_REUSE_SOURCE": source, "APPEARANCE_REUSE_TREE": tree,
                "APPEARANCE_REUSE_NATIVE_SOURCE": native, "APPEARANCE_REUSE_NATIVE_TREE": native_tree,
                "APPEARANCE_REUSE_WINDOWS_SOURCE": independent, "APPEARANCE_REUSE_WINDOWS_TREE": independent_tree,
                "APPEARANCE_REUSE_WINDOWS_INPUTS": window_blobs,
                "APPEARANCE_REUSE_INPUTS": (ui,),
                "REVIEWED_CONTROL_WORKFLOW_SHA256": hashlib.sha256(workflow).hexdigest(),
                "REVIEWED_CONTROL_WORKFLOW_BYTES": len(workflow),
            }
            qualification = ci_scope.APPEARANCE_REUSE_QUALIFICATION | {
                "source": source, "tree": tree, "native_source": native, "native_tree": native_tree,
                "windows_source": independent, "windows_tree": independent_tree,
                "windows_inputs": window_blobs, "workflow_sha256": hashlib.sha256(workflow).hexdigest(),
            }
            repository = {"full_name": "brk275/Morpheus", "private": False, "owner": {"login": "brk275"}}
            trusted_env = {"GITHUB_ACTOR": "brk275", "GITHUB_TRIGGERING_ACTOR": "brk275",
                           "GITHUB_REPOSITORY": "brk275/Morpheus"}
            candidate_entries = composed_entries | {path: ("100644", b"new reviewed selector") for path in selectors}
            calls = []

            def execute(argv, **kwargs):
                calls.append(argv)
                self.assertEqual(kwargs["timeout"], 10)
                self.assertEqual(kwargs["stderr"], subprocess.DEVNULL)
                self.assertEqual(kwargs["env"]["GIT_NO_LAZY_FETCH"], "1")
                self.assertEqual(kwargs["env"]["GIT_CONFIG_GLOBAL"], "/dev/null")
                self.assertNotIn("GIT_DIR", kwargs["env"])
                self.assertNotIn("SSH_ASKPASS", kwargs["env"])
                return real_run(argv, cwd=temporary, **kwargs)

            def compare(entries=None, *, pr=False, changes=None, environment=None, paths=None,
                        name=None, force=False, changed_qualification=None, selection=False):
                candidate, _ = commit(candidate_entries if entries is None else entries, "candidate")
                git("update-ref", "refs/heads/fixture", candidate)
                git("symbolic-ref", "HEAD", "refs/heads/fixture")
                event = {"repository": repository, "sender": {"login": "brk275"}}
                event_name = name or ("pull_request" if pr else "push")
                actual_env = trusted_env | {"GITHUB_SHA": candidate, "GITHUB_EVENT_NAME": event_name,
                    "GITHUB_REF": "refs/pull/7/merge" if pr else "refs/heads/main", "GITHUB_BASE_REF": "main" if pr else "",
                    "GIT_DIR": "untrusted", "SSH_ASKPASS": "untrusted"}
                if pr:
                    event |= {"number": 7, "action": "synchronize", "pull_request": {
                        "number": 7, "state": "open", "merged": False, "user": {"login": "brk275"},
                        "base": {"ref": "main", "sha": native, "repo": repository},
                        "head": {"sha": source, "repo": repository}}}
                else:
                    event |= {"before": native, "after": candidate, "ref": "refs/heads/main",
                              "forced": False, "deleted": False}
                event |= changes or {}
                actual_env |= environment or {}
                selected_paths = [ui] if paths is None else paths
                calls.clear()
                with (patch.multiple(ci_scope, **globals_changed),
                      patch.object(ci_scope, "APPEARANCE_REUSE_QUALIFICATION", qualification if changed_qualification is None else changed_qualification),
                      patch.dict(ci_scope.os.environ, actual_env),
                      patch.object(ci_scope.subprocess, "run", side_effect=execute)):
                    if selection:
                        with patch.object(ci_scope, "changed_paths", return_value=(selected_paths, force)):
                            return event_selection(event, event_name)
                    return ci_scope.qualified_appearance_input_reuse(selected_paths, event, event_name, force)

            self.assertEqual(len({job[1] for job in qualification["jobs"]}), 1)
            self.assertEqual(len({job[2] for job in qualification["jobs"]}), 4)
            self.assertTrue(compare())
            self.assertTrue(compare(pr=True))
            self.assertTrue(compare(paths=[ui] + list(selectors)))
            selected = compare(selection=True)
            self.assertEqual(selected["check_route"], "helpers")
            self.assertFalse(selected["native"] or selected["config"] or selected["wayland"])
            for changes in ({"forced": True}, {"deleted": True}, {"ref": "refs/heads/topic"},
                            {"before": "0" * 40}, {"sender": {"login": "other"}},
                            {"repository": repository | {"private": True}}):
                self.assertFalse(compare(changes=changes)); self.assertEqual(calls, [])
            for key in ("GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR", "GITHUB_REPOSITORY", "GITHUB_REF", "GITHUB_SHA"):
                self.assertFalse(compare(environment={key: "other"})); self.assertEqual(calls, [])
            for name in ("workflow_dispatch", "release", "workflow_run"):
                self.assertFalse(compare(name=name)); self.assertEqual(calls, [])
            self.assertFalse(compare(force=True)); self.assertEqual(calls, [])
            for paths in ([], list(selectors), ["README.md"], [ui, "Cargo.lock"], [ui, "vendor/sdk.rs"],
                          [ui, ".github/workflows/ci.yml"], [ui, "unknown.rs"], [ui, ui]):
                self.assertFalse(compare(paths=paths)); self.assertEqual(calls, [])
            for field in ("source", "tree", "native_source", "native_tree", "windows_source", "windows_tree", "workflow_sha256"):
                self.assertFalse(compare(changed_qualification=qualification | {field: "f" * 40})); self.assertEqual(calls, [])
            jobs = qualification["jobs"]
            for bad in (jobs[:3], (jobs[0],) * 4, (jobs[0][:2] + (jobs[0][2] + 1,),) + jobs[1:]):
                self.assertFalse(compare(changed_qualification=qualification | {"jobs": bad})); self.assertEqual(calls, [])
            for field in (ui, "Cargo.lock", ".github/workflows/ci.yml", windows[0]):
                changed = candidate_entries | {field: ("100644", b"unqualified change")}
                self.assertFalse(compare(changed))
                # A PR head still matching the checkpoint cannot hide an
                # unqualified input inherited by its actual synthetic merge.
                self.assertFalse(compare(changed, pr=True))
            for mode in ("100755", "120000"):
                self.assertFalse(compare(candidate_entries | {selectors[0]: (mode, b"replacement")}))
            directory = dict(candidate_entries)
            del directory[selectors[0]]
            directory[selectors[0] + "/child.py"] = ("100644", b"directory is not an excluded file")
            self.assertFalse(compare(directory))
            selected = compare(candidate_entries | {"Cargo.lock": ("100644", b"new dependency")}, selection=True)
            self.assertEqual(selected["check_route"], "all")
            self.assertEqual(selected["native_matrix"]["os"], list(PLATFORMS))

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


class HostProvisionScopeTests(unittest.TestCase):
    # Independently retain the reviewed workflow text, not the classifier's
    # constant. Command, identity and privilege changes must fail closed.
    ADDITION = b"""      - name: Verify checkout identity before host provisioning
        shell: bash
        env:
          REVIEWED_SOURCE: ${{ github.sha }}
        run: |
          set -euo pipefail
          [[ "$REVIEWED_SOURCE" =~ ^[0-9a-f]{40}$ ]]
          test "$(git rev-parse HEAD)" = "$REVIEWED_SOURCE"
      - name: Tighten hosted notice tree permissions before candidate imports
        timeout-minutes: 5
        shell: bash
        run: |
          set -euo pipefail
          [[ "$GITHUB_SHA" =~ ^[0-9a-f]{40}$ ]]
          # Bootstrap from this immutable reviewed workflow revision, including
          # when the selected package source predates the provisioning helper.
          git show --no-ext-diff --no-textconv "$GITHUB_SHA:scripts/harden_linux_notice_host.py" |
            sudo --preserve-env=GITHUB_ACTIONS,RUNNER_ENVIRONMENT,RUNNER_OS,ImageOS /usr/bin/python3 -I -B -
"""

    def setUp(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml").read_bytes()
        # Exercise the actual workflow both before and after the additive port.
        self.assertLessEqual(workflow.count(self.ADDITION), 1)
        self.before = workflow.replace(self.ADDITION, b"", 1)
        marker = b"      - name: Check scope and packaging helpers\n"
        self.assertEqual(self.before.count(marker), 1)
        self.after = self.before.replace(marker, self.ADDITION + marker, 1)

    def recognized(self, before, after):
        with patch("scripts.ci_scope.subprocess.run", side_effect=[
                SimpleNamespace(stdout=before), SimpleNamespace(stdout=after)]):
            return helpers_host_provisioning_only("a" * 40, "b" * 40)

    def selected(self, after, other_paths=(), event_name="push"):
        paths = [".github/workflows/ci.yml", *other_paths]
        event = {"before": "a" * 40, "after": "b" * 40}
        if event_name == "pull_request":
            event = {"pull_request": {"base": {"sha": "a" * 40}, "head": {"sha": "b" * 40}}}
        # First pair belongs to the unchanged scanner exception; the second
        # belongs to the new exact helper addition recognizer.
        with patch("scripts.ci_scope.subprocess.run", side_effect=[
                SimpleNamespace(stdout=b"\0".join(path.encode() for path in paths) + b"\0"),
                SimpleNamespace(stdout=self.before), SimpleNamespace(stdout=after),
                SimpleNamespace(stdout=self.before), SimpleNamespace(stdout=after)]):
            selected, forced = changed_paths(event, event_name)
        self.assertFalse(forced)
        return selected, classify(selected)

    def test_exact_addition_preserves_helper_and_manual_package_selection(self):
        self.assertTrue(self.recognized(self.before, self.after))
        helpers = ["scripts/harden_linux_notice_host.py", "scripts/tests/test_harden_linux_notice_host.py",
                   ".github/workflows/package-qa.yml", "packaging/linux/appimage_capture_worker.py"]
        for event in ("push", "pull_request"):
            with self.subTest(event=event):
                paths, selected = self.selected(self.after, helpers, event)
                self.assertEqual(paths, helpers)
                self.assertFalse(selected["native"])
                self.assertEqual(classify(paths, force_all=True)["native_matrix"]["os"], list(PLATFORMS))

    def test_partial_reordered_removed_and_repeated_additions_fail_closed(self):
        split = self.ADDITION.index(b"      - name: Tighten hosted notice")
        first, second = self.ADDITION[:split], self.ADDITION[split:]
        for changed in (self.before, self.after.replace(first, b"", 1),
                        self.after.replace(second, b"", 1),
                        self.after.replace(self.ADDITION, second + first, 1),
                        self.after.replace(self.ADDITION, self.ADDITION * 2, 1)):
            with self.subTest(changed=changed[-40:]):
                self.assertFalse(self.recognized(self.before, changed))
        self.assertFalse(self.recognized(self.after, self.before))
        self.assertFalse(self.recognized(self.after, self.after))

    def test_changed_identity_privilege_command_and_timeout_fail_closed(self):
        for old, new in ((b"REVIEWED_SOURCE: ${{ github.sha }}", b"REVIEWED_SOURCE: attacker"),
                         (b"set -euo pipefail", b"set +e"),
                         (b"timeout-minutes: 5", b"timeout-minutes: 6"),
                         (b"git rev-parse HEAD", b"git rev-parse main"),
                         (b"harden_linux_notice_host.py", b"different-helper.py"),
                         (b"sudo --preserve-env=", b"sudo -E --preserve-env="),
                         (b"/usr/bin/python3 -I -B -", b"python3 -")):
            with self.subTest(command=old):
                changed = self.after.replace(self.ADDITION, self.ADDITION.replace(old, new, 1), 1)
                paths, selected = self.selected(changed)
                self.assertEqual(paths, [".github/workflows/ci.yml"])
                self.assertEqual(selected["native_matrix"]["os"], list(PLATFORMS))
                self.assertTrue(selected["wayland"])

    def test_other_workflow_bytes_and_mixed_scanner_edits_remain_native(self):
        for old, new in ((b"cargo fmt --all -- --check", b"cargo fmt --all"),
                         (b"persist-credentials: false", b"persist-credentials: true"),
                         (b"../trusted/scripts/scan_history.py", b"scripts/scan_history.py"),
                         (b"contents: read", b"contents: write")):
            with self.subTest(outside=old):
                self.assertIn(old, self.after)
                _, selected = self.selected(self.after.replace(old, new, 1))
                self.assertEqual(selected["native_matrix"]["os"], list(PLATFORMS))
                self.assertTrue(selected["wayland"])

    def test_unknown_helper_layout_duplicate_job_or_misplaced_steps_fail_closed(self):
        for before, after in (
                (self.before.replace(b"    name: Validation helper tests\n", b"    name: Other helpers\n", 1),
                 self.after.replace(b"    name: Validation helper tests\n", b"    name: Other helpers\n", 1)),
                (self.before + b"  helpers: {}\n", self.after + b"  helpers: {}\n"),
                (self.before, self.ADDITION + self.before),
                (self.before, self.after + b"  unknown-job: {}\n"),
                (self.before, self.after.replace(b"\n", b"\r\n")),
                (self.before, self.after + b"\0")):
            with self.subTest(tail=after[-40:]):
                self.assertFalse(self.recognized(before, after))

    def test_mixed_shared_native_or_unknown_paths_keep_all_platforms(self):
        for path in ("Cargo.lock", "crates/morpheus-matrix/src/lib.rs", "new-build-input.conf"):
            with self.subTest(path=path):
                paths, selected = self.selected(self.after, [path])
                self.assertEqual(paths, [path])
                self.assertEqual(selected["native_matrix"]["os"], list(PLATFORMS))
                self.assertTrue(selected["wayland"])

    def test_scanner_only_edits_keep_the_existing_separate_exception(self):
        changed = self.before.replace(b"../trusted/scripts/scan_history.py",
                                      b"../trusted/scripts/reviewed_scanner.py", 1)
        self.assertNotEqual(changed, self.before)
        with patch("scripts.ci_scope.helpers_host_provisioning_only", side_effect=AssertionError):
            paths, selected = self.selected(changed)
        self.assertEqual(paths, [])
        self.assertFalse(selected["native"])

    def test_oversized_or_unreadable_workflow_fails_closed(self):
        self.assertFalse(self.recognized(self.before + b"#" * (1024 * 1024), self.after))
        self.assertFalse(self.recognized(self.before, self.after + b"#" * (1024 * 1024)))
        for error in (OSError("git unavailable"), subprocess.CalledProcessError(128, "git"),
                      subprocess.TimeoutExpired("git", 10)):
            with self.subTest(error=type(error).__name__):
                with patch("scripts.ci_scope.subprocess.run", side_effect=error):
                    self.assertFalse(helpers_host_provisioning_only("a" * 40, "b" * 40))


class ScannerScopeTests(unittest.TestCase):
    @patch("scripts.ci_scope.subprocess.run")
    def test_only_scanner_step_edits_omit_native_matrix(self, run):
        from types import SimpleNamespace
        start = b"      - name: Scan committed history for credentials\n"
        end = b"      - name: Select checks from changed inputs using reviewed classifier\n"
        original = b"same preamble\n" + start + b"old scan\n" + end + b"same native jobs\n"
        updated = original.replace(b"old scan", b"new isolated scan")
        run.side_effect = [SimpleNamespace(stdout=original), SimpleNamespace(stdout=updated)]
        self.assertTrue(scanner_step_only("a" * 40, "b" * 40))
        run.side_effect = [SimpleNamespace(stdout=original), SimpleNamespace(stdout=updated + b"native change\n")]
        self.assertFalse(scanner_step_only("a" * 40, "b" * 40))
        run.side_effect = [SimpleNamespace(stdout=original), SimpleNamespace(stdout=updated + start)]
        self.assertFalse(scanner_step_only("a" * 40, "b" * 40))

    def test_fixture_manifest_is_security_helper_input(self):
        self.assertFalse(classify(["scripts/public-fixtures.json", "scripts/scan_history.py"])["native"])
        self.assertEqual(classify(["scripts/public-fixtures.json", "Cargo.lock"])["native_matrix"]["os"], list(PLATFORMS))


if __name__ == "__main__":
    unittest.main()
