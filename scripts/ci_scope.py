#!/usr/bin/env python3
"""Select development checks by impact; unknown inputs require every platform."""

import argparse
import hashlib
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
        elif (path in MANUAL_LINUX_PACKAGE_INPUTS or path in MANUAL_NATIVE_VAULT_INPUTS
              or path in {"scripts/ci_scope.py", ".github/workflows/native-webrtc-source.yml",
                          ".github/workflows/native-webrtc-macos-source.yml",
                          ".github/workflows/native-webrtc-windows-source.yml"}
              or path.startswith("native/webrtc/linux/") or path.startswith("native/webrtc/macos/") or path.startswith("native/webrtc/windows/")):
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


# Exact reviewed development inputs only. Desktop still compiles and tests all
# four app targets. The standalone protocol/device/vendor/interop binaries do
# not consume these draft/controller inputs. Keep auth/account scope, pack/font
# I/O, shared main/config/RTC source and dependency/workflow changes conservative.
# app-window.slint is compiled by the app and its compiler-only UI check,
# not standalone protocol/device/config binaries. All four desktop targets
# retain their runtime account/security UI guards. No Slint-prefix exemption.
# Maintainer source review may always require wider checks for semantic impact.
AUTOMATIC_DESKTOP_INPUTS = frozenset((
                'crates/morpheus-app/src/account_access_ui_tests.rs',
                'crates/morpheus-app/ui/app-window.slint',
                'crates/morpheus-app/src/theme_studio/mod.rs',
                'crates/morpheus-app/src/theme_studio/session.rs',
            ))
# These manual DATA producer/consumer assets are exercised by existing helpers;
# ordinary app Cargo jobs do not run them. No prefix or extension exemption.
AUTOMATIC_PRODUCER_INPUTS = frozenset((
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
                'scripts/native_webrtc/windows_preflight.ps1',
                'scripts/run_native_webrtc_linux.py',
                'scripts/run_native_webrtc_macos.py',
                'scripts/tests/test_native_webrtc_unix_inputs.py',
                'scripts/tests/test_native_webrtc_windows_inputs.py',
            ))


# Exact shared sender-index implementation; existing vendor checks exercise the
# native transport, crypto, SDK consumers and FFI on all four platforms. A mixed
# input or any sibling source keeps the conservative full route. Live stock
# interoperability and full release verification remain separate required gates.
AUTOMATIC_VENDOR_INPUTS = frozenset((
    "vendor/matrix-rtc-livekit/src/transport_impl.rs",
))


def automatic_route(paths, force_all=False):
    if (force_all or not paths or any(type(path) is not str for path in paths)):
        return "all"
    changed = set(paths)
    if changed <= AUTOMATIC_VENDOR_INPUTS:
        return "vendor"
    if not changed <= AUTOMATIC_DESKTOP_INPUTS | AUTOMATIC_PRODUCER_INPUTS:
        return "all"
    return "desktop" if changed & AUTOMATIC_DESKTOP_INPUTS else "helpers"


# An exact reviewed execution layout, not a blanket workflow exception. The
# four control inputs are exercised by helpers. Any other workflow bytes or
# mixed native/security/dependency input retain the existing full route.
AUTOMATIC_CONTROL_INPUTS = frozenset((
    "scripts/ci_scope.py", "scripts/tests/test_ci_scope.py",
    ".github/workflows/ci.yml", "scripts/tests/test_ci_acceptance.py",
))
REVIEWED_CONTROL_WORKFLOW_SHA256 = "8098da9e123c94d429d8cee052d923fc5e2b729f19ac92b4f963080ee29e93b3"
REVIEWED_CONTROL_WORKFLOW_BYTES = 65800


def reviewed_control_only(paths, event, event_name, force_all=False):
    if (force_all or not paths or any(type(path) is not str for path in paths)
            or not set(paths) <= AUTOMATIC_CONTROL_INPUTS or type(event) is not dict):
        return False
    return reviewed_workflow_layout(event, event_name)


def reviewed_workflow_layout(event, event_name):
    if event_name == "push":
        after = event.get("after")
    elif event_name == "pull_request":
        pull = event.get("pull_request")
        head = pull.get("head") if type(pull) is dict else None
        after = head.get("sha") if type(head) is dict else None
    else:
        return False
    if type(after) is not str or re.fullmatch(r"[0-9a-f]{40}", after) is None:
        return False
    # Only fixed-path immutable Git DATA, never candidate Python or shell.
    # Size is checked before the full read; both operations name the same
    # immutable object and the independent SHA256 verifies the complete bytes.
    command = ["git", "--no-pager", "--no-replace-objects", "-c", "core.hooksPath=/dev/null",
               "-c", "core.fsmonitor=false", "cat-file"]
    spec = after + ":.github/workflows/ci.yml"
    try:
        size = subprocess.run(command + ["-s", spec], check=True, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, timeout=10).stdout
        if type(size) is not bytes or size != str(REVIEWED_CONTROL_WORKFLOW_BYTES).encode() + b"\n":
            return False
        content = subprocess.run(command + ["blob", spec], check=True, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL, timeout=10).stdout
        return (type(content) is bytes and len(content) == REVIEWED_CONTROL_WORKFLOW_BYTES
                and hashlib.sha256(content).hexdigest() == REVIEWED_CONTROL_WORKFLOW_SHA256)
    except (OSError, subprocess.SubprocessError):
        return False


# The maintainer qualified all four native desktop jobs for this exact source.
# The digest binds the immutable source/run/job receipt. Helpers/history still
# run, and no binary, cache artifact or active run is treated as qualification.
NATIVE_REUSE_SOURCE = "ee1bfdf1eff5c234a413c1f147b9ac6dd19f07fa"
NATIVE_REUSE_TREE = "9e81e57e38c37881ff45ae798bdd15dc7ca80bfd"
NATIVE_REUSE_QUALIFICATION = {'source': 'ee1bfdf1eff5c234a413c1f147b9ac6dd19f07fa', 'tree': '9e81e57e38c37881ff45ae798bdd15dc7ca80bfd', 'workflow_sha256': '9d557eeb730a8785aaf2d3eb9fbd5224ab202fdc2f2f384a60c73a87b61c9aa0', 'receipt_sha256': 'f994e9dfc5fb136ce6e6c2ffa3dfb06b8da2be8ffa13b14edfccf66cbcab927e', 'route': 'desktop', 'jobs': (('macos-15', 40453533115, 122021806445), ('macos-15-intel', 40455765740, 122028913464), ('ubuntu-24.04', 40453536502, 122021792596), ('windows-2025', 40453247821, 122020886627))}
NATIVE_REUSE_SELECTORS = ("scripts/ci_scope.py", "scripts/tests/test_ci_scope.py")


def qualified_apply_main_reuse(paths, event, event_name, force_all=False):
    qualified = NATIVE_REUSE_QUALIFICATION
    if (force_all or event_name != "push" or type(qualified) is not dict
            or set(qualified) != {"source", "tree", "workflow_sha256", "receipt_sha256", "route", "jobs"}
            or qualified["source"] != NATIVE_REUSE_SOURCE or qualified["tree"] != NATIVE_REUSE_TREE
            or qualified["workflow_sha256"] != REVIEWED_CONTROL_WORKFLOW_SHA256 or qualified["route"] != "desktop"
            or type(qualified["receipt_sha256"]) is not str
            or re.fullmatch(r"[0-9a-f]{64}", qualified["receipt_sha256"]) is None):
        return False
    jobs = qualified["jobs"]
    if (type(jobs) is not tuple or len(jobs) != len(PLATFORMS)
            or any(type(job) is not tuple or len(job) != 3 or job[0] not in PLATFORMS
                   or type(job[1]) is not int or job[1] <= 0 or type(job[2]) is not int or job[2] <= 0 for job in jobs)
            or {job[0] for job in jobs} != set(PLATFORMS)
            or len({job[1] for job in jobs}) != len(jobs) or len({job[2] for job in jobs}) != len(jobs)):
        return False
    if (not paths or any(type(path) is not str for path in paths)
            or not set(paths) <= AUTOMATIC_DESKTOP_INPUTS | set(NATIVE_REUSE_SELECTORS)
            or not set(paths) & AUTOMATIC_DESKTOP_INPUTS or type(event) is not dict):
        return False
    repository, sender = event.get("repository"), event.get("sender")
    if (type(repository) is not dict or type(sender) is not dict
            or repository.get("full_name") != "brk275/Morpheus" or repository.get("private") is not False
            or type(repository.get("owner")) is not dict or repository["owner"].get("login") != "brk275"
            or sender.get("login") != "brk275" or event.get("ref") != "refs/heads/main"
            or event.get("forced") is not False or event.get("deleted") is not False
            or os.environ.get("GITHUB_ACTOR") != "brk275" or os.environ.get("GITHUB_TRIGGERING_ACTOR") != "brk275"):
        return False
    after = event.get("after")
    if type(after) is not str or re.fullmatch(r"[0-9a-f]{40}", after) is None:
        return False
    if not reviewed_workflow_layout(event, event_name):
        return False
    command = ["git", "--no-pager", "--no-replace-objects", "-c", "core.hooksPath=/dev/null",
               "-c", "core.fsmonitor=false"]
    try:
        # Exclude only two regular selector FILES, never a replacement directory
        # or symlink whose descendants a pathspec could otherwise also exclude.
        selectors = subprocess.run(command + ["ls-tree", "--full-tree", "-z", after, "--", *NATIVE_REUSE_SELECTORS],
                                   check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=10).stdout
        pattern = (rb"100644 blob [0-9a-f]{40}\tscripts/ci_scope\.py\x00"
                   rb"100644 blob [0-9a-f]{40}\tscripts/tests/test_ci_scope\.py\x00")
        if type(selectors) is not bytes or re.fullmatch(pattern, selectors) is None:
            return False
        # Quiet object/mode comparison covers the ENTIRE remaining tree,
        # including locks, toolchains, build configuration and all other tests.
        # No recursive output allocation, candidate imports or external diff.
        compared = subprocess.run(command + ["diff-tree", "--quiet", "--exit-code", "--no-ext-diff",
            "--no-textconv", "--no-renames", "--ignore-submodules=none", "-r", NATIVE_REUSE_SOURCE, after, "--", ".",
            ":(top,exclude)scripts/ci_scope.py", ":(top,exclude)scripts/tests/test_ci_scope.py"],
            check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
        return type(compared.returncode) is int and compared.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


# Exact native vendor inputs were qualified on all four supported platforms.
# Maintainer activation binds the
# exact tested commit/tree/workflow, receipt, four jobs and complete reviewed
# changed-input tuple. Source review and a separate helper run precede merge;
# no candidate data or event can populate this trusted literal.
VENDOR_REUSE_QUALIFICATION = {
    'source': '1abc7a82f5791af5a9b076794be07904e0931f47',
    'tree': '4edadf42f8389381995bd33b6c81d3815dd53ed2',
    'workflow_sha256': '9d557eeb730a8785aaf2d3eb9fbd5224ab202fdc2f2f384a60c73a87b61c9aa0',
    'receipt_sha256': '40563aa005e5223cfea1bdeaa33b4c272fef3da70db03dd5b8260858a27adef7',
    'route': 'vendor',
    'jobs': (
        ('ubuntu-24.04', 40497711784, 122167728535),
        ('windows-2025', 40497711784, 122167728475),
        ('macos-15', 40497711784, 122167728330),
        ('macos-15-intel', 40497711784, 122167728468),
    ),
    'inputs': (
        'vendor/livekit/MORPHEUS_PATCH.md',
        'vendor/livekit/MORPHEUS_PROVENANCE.json',
        'vendor/livekit/src/room/e2ee/manager.rs',
        'vendor/livekit/src/room/e2ee/manager.rs.upstream',
        'vendor/livekit/src/room/e2ee/remote_track_admission.rs',
        'vendor/livekit/src/room/mod.rs',
        'vendor/livekit/src/room/mod.rs.upstream',
        'vendor/livekit/src/room/track/remote_audio_track.rs',
        'vendor/livekit/src/room/track/remote_audio_track.rs.upstream',
        'vendor/livekit/src/room/track/remote_track.rs',
        'vendor/livekit/src/room/track/remote_track.rs.upstream',
        'vendor/livekit/src/room/track/remote_video_track.rs',
        'vendor/livekit/src/room/track/remote_video_track.rs.upstream',
        'vendor/matrix-rtc-core/MORPHEUS_PATCH.md',
        'vendor/matrix-rtc-core/MORPHEUS_PROVENANCE.json',
        'vendor/matrix-rtc-core/src/encryption/mod.rs',
        'vendor/matrix-rtc-core/tests/key_rotation.rs',
        'vendor/matrix-rtc-core/tests/key_rotation.rs.upstream',
        'vendor/matrix-rtc-livekit/src/transport_impl.rs',
    ),
}


def qualified_vendor_main_reuse(paths, event, event_name, force_all=False):
    qualified = VENDOR_REUSE_QUALIFICATION
    if (force_all or event_name != "push" or type(qualified) is not dict
            or set(qualified) != {"source", "tree", "workflow_sha256", "receipt_sha256", "route", "jobs", "inputs"}
            or any(type(qualified[name]) is not str or re.fullmatch(r"[0-9a-f]{40}", qualified[name]) is None
                   for name in ("source", "tree"))
            or qualified["workflow_sha256"] != REVIEWED_CONTROL_WORKFLOW_SHA256 or qualified["route"] != "vendor"
            or type(qualified["receipt_sha256"]) is not str
            or re.fullmatch(r"[0-9a-f]{64}", qualified["receipt_sha256"]) is None):
        return False
    jobs, inputs = qualified["jobs"], qualified["inputs"]
    if (type(jobs) is not tuple or len(jobs) != len(PLATFORMS)
            or any(type(job) is not tuple or len(job) != 3 or job[0] not in PLATFORMS
                   or type(job[1]) is not int or job[1] <= 0 or type(job[2]) is not int or job[2] <= 0 for job in jobs)
            or {job[0] for job in jobs} != set(PLATFORMS)
            or len({job[2] for job in jobs}) != len(jobs)):
        return False
    if (type(inputs) is not tuple or not inputs
            or any(type(path) is not str or not path or str(PurePosixPath(path)) != path
                   or PurePosixPath(path).is_absolute() or ".." in PurePosixPath(path).parts
                   or ".git" in PurePosixPath(path).parts or "\\" in path
                   or any(ord(char) < 32 or ord(char) == 127 for char in path)
                   or path in NATIVE_REUSE_SELECTORS for path in inputs)
            or len(set(inputs)) != len(inputs)):
        return False
    if (type(paths) not in (list, tuple) or not paths or any(type(path) is not str for path in paths)
            or not set(paths) <= set(inputs) | set(NATIVE_REUSE_SELECTORS)
            or not set(paths) & set(inputs) or type(event) is not dict):
        return False
    repository, sender = event.get("repository"), event.get("sender")
    if (type(repository) is not dict or type(sender) is not dict
            or repository.get("full_name") != "brk275/Morpheus" or repository.get("private") is not False
            or type(repository.get("owner")) is not dict or repository["owner"].get("login") != "brk275"
            or sender.get("login") != "brk275" or event.get("ref") != "refs/heads/main"
            or event.get("forced") is not False or event.get("deleted") is not False
            or os.environ.get("GITHUB_ACTOR") != "brk275" or os.environ.get("GITHUB_TRIGGERING_ACTOR") != "brk275"):
        return False
    after = event.get("after")
    if type(after) is not str or re.fullmatch(r"[0-9a-f]{40}", after) is None:
        return False
    if not reviewed_workflow_layout(event, event_name):
        return False
    command = ["git", "--no-pager", "--no-replace-objects", "-c", "core.hooksPath=/dev/null",
               "-c", "core.fsmonitor=false"]
    environment = {key: value for key, value in os.environ.items() if not key.startswith(("GIT_", "SSH_"))}
    environment.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_NO_REPLACE_OBJECTS="1",
                       GIT_OPTIONAL_LOCKS="0", GIT_NO_LAZY_FETCH="1")
    options = {"stdout": subprocess.PIPE, "stderr": subprocess.DEVNULL, "timeout": 10, "env": environment}
    try:
        # Verify the recorded immutable source is a commit with the exact tree;
        # missing/promisor objects fail rather than triggering a network fetch.
        kind = subprocess.run(command + ["cat-file", "-t", qualified["source"]], check=True, **options).stdout
        if kind != b"commit\n":
            return False
        tree = subprocess.run(command + ["rev-parse", "--verify", qualified["source"] + "^{tree}"],
                              check=True, **options).stdout
        if tree != qualified["tree"].encode() + b"\n":
            return False
        # Exclude only these two regular files, never directories/symlinks or
        # submodules. No path prefix, binary artifact or cached job is reused.
        selectors = subprocess.run(command + ["ls-tree", "--full-tree", "-z", after, "--", *NATIVE_REUSE_SELECTORS],
                                   check=True, **options).stdout
        pattern = (rb"100644 blob [0-9a-f]{40}\tscripts/ci_scope\.py\x00"
                   rb"100644 blob [0-9a-f]{40}\tscripts/tests/test_ci_scope\.py\x00")
        if type(selectors) is not bytes or re.fullmatch(pattern, selectors) is None:
            return False
        compared = subprocess.run(command + ["diff-tree", "--quiet", "--exit-code", "--no-ext-diff",
            "--no-textconv", "--no-renames", "--ignore-submodules=none", "-r", qualified["source"], after, "--", ".",
            ":(top,exclude)scripts/ci_scope.py", ":(top,exclude)scripts/tests/test_ci_scope.py"],
            check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10, env=environment)
        return type(compared.returncode) is int and compared.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


# One exact reviewed UI checkpoint: all four native desktop jobs passed on F.
# C differs from F only in the three separately checked Windows helper blobs.
# Helpers and complete-history scanning remain on. The frozen receipt records
# actual source/job/log identities; no artifact or release acceptance is reused.
APPEARANCE_REUSE_SOURCE = "129770747c8a7f5c58ce9f7e83e0274eb29fd690"
APPEARANCE_REUSE_TREE = "ee1c64a1d02e124f9828a4919136b2d9c549abff"
APPEARANCE_REUSE_NATIVE_SOURCE = "51afad97a59ed148cef1e0604bdcca46614de771"
APPEARANCE_REUSE_NATIVE_TREE = "d96584a69de8d2dfc0786164dfbc6bbf13dd8fef"
APPEARANCE_REUSE_WINDOWS_SOURCE = "4bc8f829ed8d096cdb9c7e7629438ed5bf5fdb1b"
APPEARANCE_REUSE_WINDOWS_TREE = "550006fc419ec10b6a268b74ec6f374bd55a410f"
APPEARANCE_REUSE_RECEIPT = "758b64e61103890a89214e74d5b2163667be6f3396e69b1f67b2a433f55e1a83"
APPEARANCE_REUSE_JOBS = (
    ("ubuntu-24.04", 40795707711, 123191954281),
    ("macos-15", 40795707711, 123191954274),
    ("macos-15-intel", 40795707711, 123191954327),
    ("windows-2025", 40795707711, 123191954286),
)
APPEARANCE_REUSE_WINDOWS_INPUTS = (
    ('.github/workflows/native-webrtc-windows-source.yml', '612e561da24ae578583f4006405a64e783d7b319'),
    ('scripts/native_webrtc/windows_preflight.ps1', 'ea9762ca83a5047502c696e2d2504716f453f158'),
    ('scripts/tests/test_native_webrtc_windows_inputs.py', '4091e940ebfffcae8644f0a1bb405fd94c813199'),
)
# Only the two exact PR61 compiled UI inputs. Whole remaining-tree equality
# and all existing fixed owner/event/workflow/source/Windows gates still decide.
APPEARANCE_REUSE_INPUTS = (
    "crates/morpheus-app/src/account_access_ui_tests.rs",
    "crates/morpheus-app/ui/app-window.slint",
)
APPEARANCE_REUSE_QUALIFICATION = {
    "source": APPEARANCE_REUSE_SOURCE, "tree": APPEARANCE_REUSE_TREE,
    "native_source": APPEARANCE_REUSE_NATIVE_SOURCE, "native_tree": APPEARANCE_REUSE_NATIVE_TREE,
    "windows_source": APPEARANCE_REUSE_WINDOWS_SOURCE, "windows_tree": APPEARANCE_REUSE_WINDOWS_TREE,
    "workflow_sha256": REVIEWED_CONTROL_WORKFLOW_SHA256,
    "receipt_sha256": APPEARANCE_REUSE_RECEIPT, "route": "desktop",
    "jobs": APPEARANCE_REUSE_JOBS, "windows_inputs": APPEARANCE_REUSE_WINDOWS_INPUTS,
}


def qualified_appearance_input_reuse(paths, event, event_name, force_all=False):
    qualified = APPEARANCE_REUSE_QUALIFICATION
    if (force_all is not False or event_name not in ("push", "pull_request")
            or type(qualified) is not dict or set(qualified) != {
                "source", "tree", "native_source", "native_tree", "windows_source", "windows_tree",
                "workflow_sha256", "receipt_sha256", "route", "jobs", "windows_inputs"}):
        return False
    frozen = (APPEARANCE_REUSE_SOURCE, APPEARANCE_REUSE_TREE,
              APPEARANCE_REUSE_NATIVE_SOURCE, APPEARANCE_REUSE_NATIVE_TREE,
              APPEARANCE_REUSE_WINDOWS_SOURCE, APPEARANCE_REUSE_WINDOWS_TREE)
    revisions = tuple(qualified[key] for key in (
        "source", "tree", "native_source", "native_tree", "windows_source", "windows_tree"))
    if (revisions != frozen or any(type(value) is not str or re.fullmatch(r"[0-9a-f]{40}", value) is None
                                   for value in revisions)
            or qualified["workflow_sha256"] != REVIEWED_CONTROL_WORKFLOW_SHA256
            or qualified["receipt_sha256"] != APPEARANCE_REUSE_RECEIPT or qualified["route"] != "desktop"
            or qualified["jobs"] != APPEARANCE_REUSE_JOBS
            or qualified["windows_inputs"] != APPEARANCE_REUSE_WINDOWS_INPUTS):
        return False
    jobs = qualified["jobs"]
    if (type(jobs) is not tuple or len(jobs) != 4
            or any(type(job) is not tuple or len(job) != 3 or job[0] not in PLATFORMS
                   or type(job[1]) is not int or job[1] <= 0 or type(job[2]) is not int or job[2] <= 0 for job in jobs)
            or {job[0] for job in jobs} != set(PLATFORMS) or len({job[2] for job in jobs}) != 4):
        return False
    if (type(paths) not in (list, tuple) or not paths or any(type(path) is not str for path in paths)
            or len(set(paths)) != len(paths)
            or not set(paths) <= set(APPEARANCE_REUSE_INPUTS) | set(NATIVE_REUSE_SELECTORS)
            or not any(path in APPEARANCE_REUSE_INPUTS and path.endswith((".rs", ".slint")) for path in paths)
            or type(event) is not dict):
        return False

    def owned_repository(value):
        return (type(value) is dict and value.get("full_name") == "brk275/Morpheus"
                and value.get("private") is False and type(value.get("owner")) is dict
                and value["owner"].get("login") == "brk275")

    if (not owned_repository(event.get("repository")) or type(event.get("sender")) is not dict
            or event["sender"].get("login") != "brk275"
            or any(os.environ.get(key) != "brk275" for key in ("GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR"))
            or os.environ.get("GITHUB_REPOSITORY") != "brk275/Morpheus"
            or os.environ.get("GITHUB_EVENT_NAME") != event_name
            or event.get("forced", False) is not False or event.get("deleted", False) is not False):
        return False
    candidate = os.environ.get("GITHUB_SHA")
    if type(candidate) is not str or re.fullmatch(r"[0-9a-f]{40}", candidate) is None:
        return False
    if event_name == "push":
        if (event.get("ref") != "refs/heads/main" or os.environ.get("GITHUB_REF") != "refs/heads/main"
                or event.get("after") != candidate or event.get("forced") is not False
                or event.get("deleted") is not False or type(event.get("before")) is not str
                or re.fullmatch(r"[0-9a-f]{40}", event["before"]) is None or event["before"] == "0" * 40):
            return False
    else:
        pull = event.get("pull_request")
        if (type(pull) is not dict or pull.get("state") != "open" or pull.get("merged") is not False
                or type(pull.get("user")) is not dict or pull["user"].get("login") != "brk275"
                or type(event.get("number")) is not int or event["number"] <= 0
                or pull.get("number") != event["number"]
                or event.get("action") not in ("opened", "synchronize", "reopened")
                or os.environ.get("GITHUB_BASE_REF") != "main"
                or os.environ.get("GITHUB_REF") != f"refs/pull/{event['number']}/merge"):
            return False
        for field in ("base", "head"):
            value = pull.get(field)
            if (type(value) is not dict or not owned_repository(value.get("repo"))
                    or type(value.get("sha")) is not str or re.fullmatch(r"[0-9a-f]{40}", value["sha"]) is None):
                return False
        if pull["base"].get("ref") != "main":
            return False
    command = ["git", "--no-pager", "--no-replace-objects", "-c", "core.hooksPath=/dev/null",
               "-c", "core.fsmonitor=false", "-c", "core.attributesFile=/dev/null"]
    environment = {key: value for key, value in os.environ.items() if not key.startswith(("GIT_", "SSH_"))}
    environment.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_NO_REPLACE_OBJECTS="1",
                       GIT_OPTIONAL_LOCKS="0", GIT_NO_LAZY_FETCH="1", GIT_ATTR_NOSYSTEM="1")
    options = {"stdout": subprocess.PIPE, "stderr": subprocess.DEVNULL, "timeout": 10, "env": environment}
    try:
        def output(args):
            return subprocess.run(command + args, check=True, **options).stdout

        # The PR checkout is a synthetic merge. Checking only head.sha would
        # miss unqualified dependency/workflow changes inherited from its base.
        if output(["rev-parse", "--verify", "HEAD^{commit}"]) != candidate.encode() + b"\n":
            return False
        for source, tree in ((qualified["source"], qualified["tree"]),
                             (qualified["native_source"], qualified["native_tree"]),
                             (qualified["windows_source"], qualified["windows_tree"])):
            if (output(["cat-file", "-t", source]) != b"commit\n"
                    or output(["rev-parse", "--verify", source + "^{tree}"]) != tree.encode() + b"\n"):
                return False
        selectors = output(["ls-tree", "--full-tree", "-z", candidate, "--", *NATIVE_REUSE_SELECTORS])
        if (type(selectors) is not bytes or re.fullmatch(
                rb"100644 blob [0-9a-f]{40}\tscripts/ci_scope\.py\x00"
                rb"100644 blob [0-9a-f]{40}\tscripts/tests/test_ci_scope\.py\x00", selectors) is None):
            return False
        windows_paths = tuple(path for path, _ in APPEARANCE_REUSE_WINDOWS_INPUTS)
        expected = b"".join(b"100644 blob " + blob.encode() + b"\t" + path.encode() + b"\0"
                            for path, blob in APPEARANCE_REUSE_WINDOWS_INPUTS)
        for source in (qualified["source"], qualified["windows_source"]):
            if output(["ls-tree", "--full-tree", "-z", source, "--", *windows_paths]) != expected:
                return False
        differences = output(["diff-tree", "--no-commit-id", "--name-only", "-z", "--no-ext-diff",
            "--no-textconv", "--no-renames", "--ignore-submodules=none", "-r",
            qualified["native_source"], qualified["source"]])
        if differences != b"".join(path.encode() + b"\0" for path in windows_paths):
            return False
        spec = qualified["source"] + ":.github/workflows/ci.yml"
        if output(["cat-file", "-s", spec]) != str(REVIEWED_CONTROL_WORKFLOW_BYTES).encode() + b"\n":
            return False
        workflow = output(["cat-file", "blob", spec])
        if (type(workflow) is not bytes or len(workflow) != REVIEWED_CONTROL_WORKFLOW_BYTES
                or hashlib.sha256(workflow).hexdigest() != REVIEWED_CONTROL_WORKFLOW_SHA256):
            return False
        compared = subprocess.run(command + ["diff-tree", "--quiet", "--exit-code", "--no-ext-diff",
            "--no-textconv", "--no-renames", "--ignore-submodules=none", "-r", qualified["source"], candidate,
            "--", ".", ":(top,exclude)scripts/ci_scope.py", ":(top,exclude)scripts/tests/test_ci_scope.py"],
            check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10, env=environment)
        return type(compared.returncode) is int and compared.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def event_selection(event, event_name):
    if event_name != "workflow_dispatch":
        paths, force_all = changed_paths(event, event_name)
        selected = classify(paths, force_all)
        route = automatic_route(paths, force_all)
        if route == "all" and reviewed_control_only(paths, event, event_name, force_all):
            route = "helpers"
        if qualified_apply_main_reuse(paths, event, event_name, force_all):
            route = "helpers"
        if qualified_vendor_main_reuse(paths, event, event_name, force_all):
            route = "helpers"
        if route != "helpers" and qualified_appearance_input_reuse(paths, event, event_name, force_all):
            route = "helpers"
        if route in ("desktop", "vendor"):
            selected = {"native": True, "native_matrix": {"os": list(PLATFORMS)},
                        "config": False, "wayland": False}
        elif route == "helpers":
            selected = {"native": False, "native_matrix": {"os": []},
                        "config": False, "wayland": False}
        return selected | {
            "manual": False, "manual_platform": "all",
            "check_route": route, "matrix_suite": "full",
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
