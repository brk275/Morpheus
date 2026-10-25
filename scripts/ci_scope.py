#!/usr/bin/env python3
"""Select development checks by impact; unknown inputs require every platform."""

import argparse
import json
import os
import re
import subprocess
from pathlib import PurePosixPath


PLATFORMS = ("macos-15", "macos-15-intel", "ubuntu-24.04", "windows-2025")
MACOS = PLATFORMS[:2]
# The standalone morpheus-rtc Wayland target does not compile the app or
# configuration crate. These reviewed presentation inputs still select all
# four native targets; only the unrelated system Wayland fixture is omitted.
# Keep this list exact: mixed dependency/build/unknown changes select Wayland.
WAYLAND_UNRELATED_NATIVE_INPUTS = {
    "crates/morpheus-app/src/account_access_ui_tests.rs",
    "crates/morpheus-app/src/appearance.rs",
    "crates/morpheus-app/src/live.rs",
    "crates/morpheus-app/src/main.rs",
    "crates/morpheus-app/src/timeline_model.rs",
    "crates/morpheus-app/ui/app-window.slint",
    "crates/morpheus-config/src/lib.rs",
}
# The standalone portal target and its driver do not compile this separate
# Matrix test binary or import the Matrix acceptance coordinator. Keep their
# existing native/helper selections; omit only the unrelated Wayland fixture.
# Shared containment, SDK/core source, dependencies and unknown paths remain
# conservative, and manual full verification still selects Wayland.
WAYLAND_UNRELATED_MATRIX_DIAGNOSTIC_INPUTS = {
    "crates/morpheus-matrix/tests/drafts_local.rs",
    "scripts/run_matrix_acceptance.py",
    "scripts/tests/test_matrix_acceptance.py",
}
# These validated appearance packs are consumed by the app/configuration
# crate, not the standalone RTC portal fixture. Keep this an exact allowlist:
# unknown theme inputs and mixed build/dependency changes remain conservative.
WAYLAND_UNRELATED_THEME_INPUTS = {
    "themes/aurora.toml",
    "themes/nocturne.toml",
    "themes/paper.toml",
    "themes/terminal.toml",
}
# These Python drivers and the static-runtime source manifest run only in
# explicitly selected Linux package/source QA.
# Ordinary native CI cannot exercise them; its always-on helper suite covers
# their pure/mocked contracts. Unknown package inputs still select Linux.
MANUAL_LINUX_PACKAGE_INPUTS = {
    "packaging/linux/package_appimage.py",
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
    "packaging/linux/source_bundle_qa.py",
}
# The isolated OS-vault fixture is ignored by ordinary CI. Its exact manual
# workflow runs all contracts and native phases on selected Mac/Windows runners.
# Production core changes, unknown fixtures and shared inputs still fail closed.
MANUAL_NATIVE_VAULT_INPUTS = {
    "crates/morpheus-matrix/tests/native_vault.rs",
    ".github/workflows/native-vault-qa.yml",
}
WINDOWS_ACCEPTANCE_INPUTS = {
    "scripts/run_windows_capture_acceptance.py",
    "scripts/windows_capture_window.ps1",
    "scripts/tests/test_windows_capture_acceptance.py",
    "crates/morpheus-rtc/tests/windows_capture.rs",
}
LINUX_ACCEPTANCE_INPUTS = {
    # The fixed native client is an ordinary Linux build dependency now.
    "scripts/build_linux_native.py",
    "scripts/pipewire_startup_probe.py",
    "crates/linux_native_pipewire.rs",
    "scripts/encode_preview_pngs.py",
    "scripts/run_matrix_acceptance.py",
    "scripts/local_matrix_env.py",
    "scripts/local_matrix_rtc.py",
    "scripts/local_matrix_auth.py",
    "scripts/matrix_fixture_diagnostics.py",
    "scripts/tests/test_matrix_fixture_diagnostics.py",
    "scripts/run_element_acceptance.py",
    "scripts/element_call_stock_peer.mjs",
    "scripts/element_peer_fixture.py",
    "scripts/element_process_namespace.py",
    "scripts/tests/test_element_process_namespace.py",
    "scripts/element-peer/package.json",
    "scripts/element-peer/package-lock.json",
    "crates/morpheus-rtc/tests/element_call_peer.rs",
    "crates/morpheus-matrix/tests/oauth_local.rs",
    "scripts/run_wayland_acceptance.py",
    "scripts/wayland_fixture_backend.py",
    "scripts/prepare_appimage_update.py",
    "crates/morpheus-rtc/src/linux_portal.rs",
    "crates/morpheus-rtc/tests/wayland_capture.rs",
    "crates/morpheus-rtc/tests/support/pipewire_probe.rs",
    "vendor/webrtc-sys/pipewire_isolation.rs",
    "vendor/webrtc-sys/pipewire-symbols.txt",
    "crates/morpheus-app/src/updater/linux_transaction.rs",
    "crates/morpheus-app/src/updater/linux_transaction_tests.rs",
}


def classify(paths, force_all=False):
    native = set(PLATFORMS if force_all else ())
    config = False
    wayland_affected = force_all
    for raw in paths:
        path = str(PurePosixPath(raw))
        if (path.endswith(".md")
                or (path.startswith("docs/screenshots/") and path.endswith(".png"))
                or path in {"LICENSE", ".gitignore", ".gitleaksignore", "scripts/public-fixtures.json"}):
            continue
        if path not in (WAYLAND_UNRELATED_NATIVE_INPUTS | WAYLAND_UNRELATED_THEME_INPUTS
                        | WAYLAND_UNRELATED_MATRIX_DIAGNOSTIC_INPUTS):
            wayland_affected = True
        if path == "Cargo.lock" or path.endswith("Cargo.toml"):
            native.update(PLATFORMS)
        elif path in MANUAL_LINUX_PACKAGE_INPUTS or path in MANUAL_NATIVE_VAULT_INPUTS or path == "scripts/ci_scope.py":
            # The classifier itself is exercised by the always-on helper
            # tests. Mixed changes still select their affected native inputs;
            # manual full verification remains an unconditional full matrix.
            continue
        elif path in LINUX_ACCEPTANCE_INPUTS or path.startswith("packaging/linux/") or path == "packaging/linux.toml":
            native.add("ubuntu-24.04")
        elif path in WINDOWS_ACCEPTANCE_INPUTS:
            native.add("windows-2025")
        elif path.startswith(("packaging/macos/", "vendor/nokhwa-bindings-macos/")):
            native.update(MACOS)
        elif path.startswith("packaging/windows/"):
            native.add("windows-2025")
        elif path.startswith("themes/"):
            config = True
        elif path.startswith(("scripts/", "tests/")):
            # The always-on helpers cover Python/JavaScript and package.py
            # --check; native Cargo jobs do not exercise the packaging CLI.
            # Actual package/install checks remain explicit platform QA gates.
            continue
        elif path in {".github/workflows/package-qa.yml", ".github/workflows/wayland-startup-probe.yml", ".github/workflows/history-security.yml"}:
            # This workflow is manual-only. CI compiles do not exercise its
            # package/install/diagnostic steps; explicitly dispatch the affected
            # platforms after review instead of rebuilding every desktop here.
            continue
        elif path in {".github/CODEOWNERS", ".github/dependabot.yml"} or path.startswith(".github/ISSUE_TEMPLATE/"):
            continue
        else:
            # Includes shared native source, workflow/build configuration,
            # dependency locks, assets, and previously unknown inputs.
            native.update(PLATFORMS)
    return {
        "native": bool(native),
        "native_matrix": {"os": [p for p in PLATFORMS if p in native]},
        "config": config and not native,
        "wayland": "ubuntu-24.04" in native and wayland_affected,
    }



# Manual choices are fixed reviewed routes, never shell fragments, runner labels
# or refs. All/default retains the full supported matrix and every existing gate.
MANUAL_PLATFORMS = {
    "all": PLATFORMS,
    "linux": ("ubuntu-24.04",),
    "windows": ("windows-2025",),
    "macos-arm": ("macos-15",),
    "macos-intel": ("macos-15-intel",),
}
MANUAL_CHECKS = frozenset({"all", "models", "desktop", "desktop-video", "devices", "vendor", "sdk",
                           "matrix", "wayland", "config", "helpers"})
# Exact existing suites from run_matrix_acceptance.py; no new test composition.
MATRIX_SUITES = frozenset({"full", "draft-recovery", "drafts-only", "timeline-reads",
                           "timeline-failures", "media-only", "app-timeline",
                           "outgoing-ring", "native-call", "oauth-only", "rtc-recovery", "element-interop"})


def event_selection(event, event_name):
    if event_name != "workflow_dispatch":
        paths, force_all = changed_paths(event, event_name)
        return classify(paths, force_all) | {
            "manual": False, "manual_platform": "all",
            "check_route": "all", "matrix_suite": "full",
        }
    inputs = event.get("inputs", {})
    if not isinstance(inputs, dict) or set(inputs) - {"platform", "checks", "matrix_suite"}:
        raise ValueError("Unknown manual CI inputs")
    platform, route, suite = (inputs.get("platform", "all"), inputs.get("checks", "all"),
                              inputs.get("matrix_suite", "full"))
    if (type(platform) is not str or platform not in MANUAL_PLATFORMS
            or type(route) is not str or route not in MANUAL_CHECKS
            or type(suite) is not str or suite not in MATRIX_SUITES):
        raise ValueError("Invalid manual CI selection")
    if route in {"matrix", "wayland"} and platform != "linux":
        raise ValueError("Focused Matrix and Wayland routes require explicit Linux")
    if suite != "full" and route != "matrix":
        raise ValueError("Focused Matrix suites require the Matrix route")
    if route == "helpers" and platform != "all":
        raise ValueError("Helpers run once on their fixed Linux host")
    if route == "config" and platform not in {"all", "linux"}:
        raise ValueError("Configuration runs on its fixed Linux host")
    platforms = () if route in {"helpers", "config"} else MANUAL_PLATFORMS[platform]
    return {
        "native": bool(platforms), "native_matrix": {"os": list(platforms)},
        "config": route == "config",
        "wayland": "ubuntu-24.04" in platforms and route in {"all", "wayland"},
        "manual": True, "manual_platform": platform,
        "check_route": route, "matrix_suite": suite,
    }


def scanner_step_only(before, after):
    """Exclude CI scanner-only edits from native builds; compare all other bytes.

    Unknown layouts and any change outside the single bounded scanner step keep
    the full native matrix. Source/security helper checks still run normally.
    """
    start = b"      - name: Scan committed history for credentials\n"
    end = b"      - name: Select checks from changed inputs using reviewed classifier\n"
    def outside(revision):
        content = subprocess.run(
            ["git", "show", revision + ":.github/workflows/ci.yml"],
            check=True, capture_output=True,
        ).stdout
        if len(content) > 1024 * 1024 or content.count(start) != 1 or content.count(end) != 1:
            raise ValueError("unrecognized scanner step boundaries")
        a, z = content.index(start), content.index(end)
        if z <= a:
            raise ValueError("invalid scanner step order")
        return content[:a], content[z:]
    try:
        return outside(before) == outside(after)
    except (ValueError, subprocess.CalledProcessError):
        return False


def helpers_host_provisioning_only(before, after):
    """Recognize only the reviewed two-step addition to the existing helper job.

    Every other workflow byte must match, including the scanner and native
    jobs. Unknown helper layouts, removals and partial/repeated additions keep
    the workflow in the normal fail-closed native classification.
    """
    original_job = b"""  helpers:
    name: Validation helper tests
    needs: scope
    if: github.event.repository.private == false
    runs-on: ubuntu-24.04
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4
        with:
          persist-credentials: false
      - name: Check scope and packaging helpers
        run: |
          python3 -m unittest discover -s scripts/tests -v
          python3 -m unittest discover -s scripts -p 'test_*.py' -v
          python3 -m compileall -q scripts
          node --check scripts/element_call_peer.mjs
          node --check scripts/element_call_stock_peer.mjs
          python3 scripts/package.py --check

"""
    addition = b"""      - name: Verify checkout identity before host provisioning
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
    marker = b"      - name: Check scope and packaging helpers\n"
    try:
        contents = [subprocess.run(
            ["git", "show", revision + ":.github/workflows/ci.yml"],
            check=True, capture_output=True, timeout=10,
        ).stdout for revision in (before, after)]
        if any(len(content) > 1024 * 1024 for content in contents):
            return False
        original, updated = contents
        # Do not normalize YAML or remove all matching names: duplicates and
        # altered commands must never disappear behind this exact exception.
        names = (b"Verify checkout identity before host provisioning",
                 b"Tighten hosted notice tree permissions before candidate imports")
        if (original.count(original_job) != 1
                or re.findall(rb"^  helpers:[^\n]*\n", original, re.MULTILINE) != [b"  helpers:\n"]
                or any(name in original or updated.count(name) != 1 for name in names)):
            return False
        updated_job = original_job.replace(marker, addition + marker, 1)
        return updated == original.replace(original_job, updated_job, 1)
    except (OSError, subprocess.SubprocessError):
        return False


def changed_paths(event, event_name):
    if event_name == "workflow_dispatch":
        return [], True
    if event_name == "pull_request":
        before = event["pull_request"]["base"]["sha"]
        after = event["pull_request"]["head"]["sha"]
        separator = "..."
    elif event_name == "push":
        before, after = event["before"], event["after"]
        separator = ".."
    else:
        return [], True
    if not all(re.fullmatch(r"[0-9a-f]{40}", value) for value in (before, after)):
        raise ValueError("GitHub event contains an invalid commit SHA")
    if before == "0" * 40:
        return [], True
    try:
        result = subprocess.run(
            ["git", "diff", "--no-renames", "--name-only", "-z", before + separator + after],
            check=True, capture_output=True,
        )
    except subprocess.CalledProcessError:
        # Missing history must widen verification, never silently skip it.
        return [], True
    paths = [p.decode("utf-8", errors="surrogateescape") for p in result.stdout.split(b"\0") if p]
    if ".github/workflows/ci.yml" in paths and (scanner_step_only(before, after)
                                               or helpers_host_provisioning_only(before, after)):
        paths.remove(".github/workflows/ci.yml")
    return paths, False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--github-event", action="store_true")
    args = parser.parse_args()
    paths, force_all = args.paths, args.all
    result = classify(paths, force_all)
    if args.github_event:
        with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as event_file:
            result = event_selection(json.load(event_file), os.environ["GITHUB_EVENT_NAME"])
    print(json.dumps(result, sort_keys=True))
    if args.github_event:
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            for key, value in result.items():
                encoded = value if type(value) is str else json.dumps(value, separators=(",", ":"))
                output.write(f"{key}={encoded}\n")


if __name__ == "__main__":
    main()
