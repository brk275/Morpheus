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
WINDOWS_ACCEPTANCE_INPUTS = {
    "scripts/run_windows_capture_acceptance.py",
    "scripts/windows_capture_window.ps1",
    "scripts/tests/test_windows_capture_acceptance.py",
    "crates/morpheus-rtc/tests/windows_capture.rs",
}
LINUX_ACCEPTANCE_INPUTS = {
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
    for raw in paths:
        path = str(PurePosixPath(raw))
        if (path.endswith(".md")
                or (path.startswith("docs/screenshots/") and path.endswith(".png"))
                or path in {"LICENSE", ".gitignore", ".gitleaksignore"}):
            continue
        if path == "Cargo.lock" or path.endswith("Cargo.toml"):
            native.update(PLATFORMS)
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
        elif path in {"scripts/ci_scope.py", "scripts/package.py"}:
            # These scripts control native scope or shared package inputs.
            native.update(PLATFORMS)
        elif path.startswith(("scripts/", "tests/")):
            # Scope job checks Python and JavaScript without external accounts.
            continue
        elif path in {".github/workflows/package-qa.yml", ".github/workflows/wayland-startup-probe.yml"}:
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
    }


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
    return [p.decode("utf-8", errors="surrogateescape") for p in result.stdout.split(b"\0") if p], False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--github-event", action="store_true")
    args = parser.parse_args()
    paths, force_all = args.paths, args.all
    if args.github_event:
        with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as event_file:
            paths, force_all = changed_paths(json.load(event_file), os.environ["GITHUB_EVENT_NAME"])
    result = classify(paths, force_all)
    print(json.dumps(result, sort_keys=True))
    if args.github_event:
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            for key, value in result.items():
                output.write(f"{key}={json.dumps(value, separators=(',', ':'))}\n")


if __name__ == "__main__":
    main()
