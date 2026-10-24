import ast
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
        self.assertEqual(MANUAL_CHECKS, {"all", "models", "desktop", "devices", "vendor", "sdk",
                                        "matrix", "wayland", "config", "helpers"})
        self.assertEqual(MATRIX_SUITES, {"full", "draft-recovery", "drafts-only", "timeline-reads",
                                         "timeline-failures", "media-only", "app-timeline",
                                         "outgoing-ring", "native-call", "oauth-only", "rtc-recovery"})
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
