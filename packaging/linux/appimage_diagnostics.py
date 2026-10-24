"""Bounded fixed facts for private AppImage QA; never serialize exception text.

Progress is written privately before waits so a killed namespace remains
observable. Publication is permitted only after the outer owner scans every raw
log. Invalid/missing records or a failed scan disclose only a withheld marker.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import stat
import tempfile

COMMANDS = frozenset((
    "none", "unknown", "cargo-version", "packager-version", "download", "native-build", "package", "extract", "update-metadata",
    "runtime-musl-build", "runtime-musl-install",
    "runtime-zlib-configure", "runtime-zlib-build", "runtime-zlib-install", "runtime-zstd-build",
    "runtime-mimalloc-build", "runtime-mimalloc-archive", "runtime-fuse-configure", "runtime-fuse-install",
    "runtime-squashfuse-bootstrap", "runtime-squashfuse-configure", "runtime-squashfuse-build",
    "runtime-malformed-build", "runtime-malformed-verify", "runtime-malformed-test", "runtime-squashfuse-install",
    "runtime-compile", "runtime-link", "runtime-strip", "runtime-headers", "runtime-zstd-stage",
    "runtime-native-config", "runtime-squashfuse-headers", "runtime-verify", "runtime-notices",
    "runtime-path-regression-build", "runtime-path-regression-verify", "runtime-path-regression-test",
    "download-linuxdeploy", "download-appimage-plugin",
    "download-pipewire-dsc", "download-pipewire-original", "download-pipewire-debian", "download-pipewire-patch",
    "download-runtime-fuse", "download-runtime-mimalloc", "download-runtime-musl", "download-runtime-runtime",
    "download-runtime-squashfuse", "download-runtime-zlib", "download-runtime-zstd",
    "tool-version", "distro-patch", "configure", "client-build", "inventory-elf", "inventory-owner", "inventory-package",
    "inventory-elf-client", "inventory-elf-dependency", "inventory-elf-deployed",
    "source-head", "source-clean", "source-archive",
    "source-cargo-version", "source-rustc-version", "source-cargo-fetch", "cache-git-commit",
    "source-commit", "source-tree", "source-objects",
    "capture-case", "capture-accessibility", "capture-peer-build", "capture-peer-list", "capture-dbus-env", "capture-portal-owner", "capture-focus", "capture-window",
    "capture-yellow", "capture-green", "capture-magenta", "capture-remove-source", "capture-close-window", "capture-imports",
    "capture-inventory", "capture-env-up", "capture-env-down", "capture-rtc-up", "capture-rtc-probe", "capture-rtc-rtc-fixture",
    "capture-rtc-down", "capture-backend-configure", "capture-backend-compile", "cleanup-rtc", "cleanup-env", "cleanup-inventory"))
WRAPPER = frozenset(("namespace", "startup", "build", "capture", "source", "complete"))
MODES = frozenset(("none", "ordinary", "eager_bindings"))
PHASES = frozenset(("none", "setup", "bus", "keyring", "display", "portal", "peer", "launch", "identity", "login", "join",
                    "static_frame", "changed_frame", "screen_stopped", "reopened_frame", "changed_after_reopen", "source_loss",
                    "app_left", "app_close", "bindings", "complete"))
PEER_FACTS = frozenset(("unavailable_audio_verified", "peer_ready", "exact_sender_joined", "static_frame", "changed_frame",
                       "screen_stopped", "reopened_frame", "changed_after_reopen", "source_loss_stopped_track", "app_left", "cleanup", "complete"))
# Exact built-in types only. No exception text, args, errno, filenames or foreign
# properties are consulted; subclasses and unlisted errors remain unknown.
BUILTIN_REASONS = {
    FileNotFoundError: "filesystem_missing", FileExistsError: "filesystem_exists",
    PermissionError: "filesystem_denied", NotADirectoryError: "filesystem_not_directory",
    IsADirectoryError: "filesystem_is_directory", OSError: "filesystem_io",
    UnicodeDecodeError: "python_decode", KeyError: "python_key",
    TypeError: "python_type", ValueError: "python_value",
}
REASONS = frozenset((*BUILTIN_REASONS.values(), "command_deadline", "command_output", "command_exit", "command_pipe", "command_cleanup", "command_input",
                     "command_http_forbidden", "command_http_not_found", "command_http_rate_limited", "command_http_unavailable",
                     "command_http_rejected", "command_download_transport", "command_download_tls",
                     "interrupted", "capture", "identity", "binding", "package", "inventory", "unknown", "namespace_exit",
                     "namespace_deadline", "namespace_output", "cleanup",
                     "inventory_input", "inventory_tool", "inventory_elf", "inventory_dependency",
                     "inventory_package", "inventory_notice", "inventory_deployment", "inventory_limit",
                     "inventory_elf_build_id", "inventory_elf_needed", "inventory_elf_soname",
                     "inventory_elf_search", "inventory_search",
                     "source_input", "source_path", "source_bounds", "source_identity", "source_lock", "source_missing",
                     "source_archive", "source_link", "source_submodule", "source_git", "source_manifest", "source_output"))
CLEANUP = frozenset(("not_started", "passed", "failed"))
POSITIONS = ("wrapper_phase", "command_stage", "capture_mode", "capture_phase")
FIELDS = frozenset((*POSITIONS, "schema", "peer_facts", "failure", "inner_cleanup"))


def reason(error):
    from appimage_process import CommandFailure
    if isinstance(error, CommandFailure):
        return "command_" + error.reason if "command_" + error.reason in REASONS else "unknown"
    if isinstance(error, (KeyboardInterrupt, SystemExit)):
        return "interrupted"
    for kind, token in BUILTIN_REASONS.items():
        if type(error) is kind:
            return token
    from appimage_notices import InventoryFailure
    # Only our exact exception type has a bounded sub-reason. Never inspect
    # a foreign exception's message, properties or payload for publication.
    if type(error) is InventoryFailure:
        value = error.reason
        token = "inventory_" + value if type(value) is str else "inventory"
        return token if token in REASONS else "inventory"
    from corresponding_source import SourceFailure
    if type(error) is SourceFailure:
        value = error.reason
        token = "source_" + value if type(value) is str else "unknown"
        return token if token in REASONS else "unknown"
    # Only the enumerated category is emitted. No repr/str/args/attributes from
    # a foreign exception or server response are inspected or serialized.
    return {"CaptureFailure": "capture", "IdentityFailure": "identity", "BindingFailure": "binding",
            "PackageFailure": "package", "InventoryFailure": "inventory"}.get(type(error).__name__, "unknown")


def valid_record(value):
    if (type(value) is not dict or set(value) != FIELDS or type(value["schema"]) is not int or value["schema"] != 1
            or value["wrapper_phase"] not in WRAPPER or value["command_stage"] not in COMMANDS
            or value["capture_mode"] not in MODES or value["capture_phase"] not in PHASES
            or value["inner_cleanup"] not in CLEANUP or type(value["peer_facts"]) is not list
            or any(type(x) is not str or x not in PEER_FACTS for x in value["peer_facts"])
            or value["peer_facts"] != sorted(set(value["peer_facts"]))):
        return False
    failure = value["failure"]
    if failure is None:
        return True
    return (type(failure) is dict and set(failure) == {*POSITIONS, "reason"}
            and failure["wrapper_phase"] in WRAPPER and failure["command_stage"] in COMMANDS
            and failure["capture_mode"] in MODES and failure["capture_phase"] in PHASES and failure["reason"] in REASONS)


class Progress:
    def __init__(self, root, *, outer=False):
        self.path = Path(root) / "fixed-progress.json"
        self.value = {"schema": 1, "wrapper_phase": "namespace" if outer else "startup", "command_stage": "none",
                      "capture_mode": "none", "capture_phase": "none", "peer_facts": [], "failure": None,
                      "inner_cleanup": "not_started"}
        self.save()

    def save(self):
        if not valid_record(self.value):
            raise ValueError("Invalid fixed progress record")
        encoded = json.dumps(self.value, sort_keys=True).encode() + b"\n"
        descriptor, name = tempfile.mkstemp(prefix=".progress-", dir=self.path.parent)
        try:
            with os.fdopen(descriptor, "wb") as output:
                output.write(encoded)
                output.flush()
                os.fsync(output.fileno())
            os.replace(name, self.path)
        finally:
            Path(name).unlink(missing_ok=True)

    def wrapper(self, phase):
        self.value["wrapper_phase"] = phase
        self.save()

    def command(self, stage):
        self.value["command_stage"] = stage if stage in COMMANDS else "unknown"
        self.save()

    def capture(self, mode, phase, control=None):
        self.value["capture_mode"], self.value["capture_phase"] = mode, phase
        self.peer(control)
        self.save()

    def peer(self, control):
        if control is None:
            return
        from appimage_client import read_regular
        self.value["peer_facts"] = []
        for name in sorted(PEER_FACTS):
            try:
                if read_regular(Path(control) / (name + ".json"), 64) == b'{"passed":true}\n':
                    self.value["peer_facts"].append(name)
            except (OSError, RuntimeError):
                pass

    def failed(self, error, control=None):
        self.peer(control)
        if self.value["failure"] is None:
            self.value["failure"] = {**{key: self.value[key] for key in POSITIONS}, "reason": reason(error)}
        self.save()

    def cleanup(self, passed):
        if self.value["inner_cleanup"] != "failed":
            self.value["inner_cleanup"] = "passed" if passed else "failed"
        self.save()


# These are diagnostic markers from authenticated Cargo Packager 0.11.8 source
# (crate SHA256 9aa0f5056c4191007702931dce47054c98fe78cbbebbaff8c7c39d5ebeed123d),
# src/error.rs and src/package/appimage/appimage. They are NOT acceptance facts
# or proof of a root cause. Only fixed tokens leave this post-scan reducer;
# arbitrary paths, captured child output and foreign exception properties do not.
PACKAGER_ERROR_MARKERS = {
    "appimage_script": b"Error running bundle_appimage.sh script:",
    "square_icon": b"Could not find a square icon to use as AppImage icon",
    "main_binary": b"Could not find the main binary in list of provided binaries",
    "copy": b"Failed to copy file from ",
    "rename": b"Failed to rename file from ",
    "symlink": b"Failed to symlink file from ",
    "io": b"I/O Error (",
    "architecture": b"Unable to determine target-architecture",
    "target_os": b"Unable to determine target-os",
    "target_environment": b"Unable to determine target-environment",
    "invalid_icons": b"Could not find a valid icon",
    "download_hash": b"Hash mismatch of downloaded file",
}
# Restrict shell tracing matches to the exact commands/arguments in the pinned
# generated script. Unknown lines are ignored, never echoed or called. Paths
# may be arbitrary bytes in private logs but only the matching command's token
# is retained. Markers attest only that this trace was observed, not completion.
PACKAGER_SCRIPT_MARKERS = {
    "copy_data": re.compile(rb"^\+ cp -r \.\./appimage_deb/data/"),
    "copy_apprun": re.compile(rb"^\+ cp (?:'[^'\r\n]{1,4096}/AppRun-x86_64'|[^ \r\n]{1,4096}/AppRun-x86_64) AppRun$"),
    "copy_icon": re.compile(rb"^\+ cp [^\r\n]{1,4096} \.DirIcon$"),
    "link_icon": re.compile(rb"^\+ ln -sf [^\r\n]{1,4096} morpheus-app\.png$"),
    "link_desktop": re.compile(rb"^\+ ln -sf usr/share/applications/morpheus-app\.desktop morpheus-app\.desktop$"),
    "linuxdeploy_header": re.compile(rb"^\+ dd if=/dev/zero bs=1 count=3 seek=8 conv=notrunc of=[^\r\n]{1,4096}/linuxdeploy-x86_64\.AppImage$"),
    "linuxdeploy_invocation": re.compile(rb"^\+ (?:'[^'\r\n]{1,4096}/linuxdeploy-x86_64\.AppImage'|[^ \r\n]{1,4096}/linuxdeploy-x86_64\.AppImage) --appimage-extract-and-run --appdir morpheus-app\.AppDir(?: |$)"),
}

def package_failure_markers(root, *, scan_passed):
    """Read one private bounded package log pair only AFTER the outer scan.

    The owned namespace and fixture groups are already stopped. No second
    command, retry, artifact, log retention, schema override or child observer
    is introduced. Missing logs disclose only an unavailable marker; unsafe or
    ambiguous logs withhold the complete public record.
    """
    if scan_passed is not True:
        return {"status": "withheld", "error_markers": [], "script_markers": []}
    from appimage_client import read_regular
    directory = Path(root) / "command-logs"
    try:
        info = directory.lstat()
    except FileNotFoundError:
        return {"status": "unavailable", "error_markers": [], "script_markers": []}
    if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid()
            or info.st_mode & 0o077):
        raise ValueError("Unsafe private package log directory")
    selected = {}
    sequence = None
    for count, path in enumerate(directory.iterdir()):
        if count >= 4096:
            raise ValueError("Private package log directory exceeds bound")
        match = re.fullmatch(r"([0-9]{4})-package\.(stdout|stderr)\.log", path.name)
        if match is None:
            continue
        if sequence is not None and sequence != match[1]:
            raise ValueError("Ambiguous private package log pair")
        sequence = match[1]
        selected[match[2]] = path
    if set(selected) != {"stdout", "stderr"}:
        return {"status": "unavailable", "error_markers": [], "script_markers": []}
    errors, scripts = set(), set()
    for path in selected.values():
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077 or info.st_nlink != 1
                or not 0 <= info.st_size <= 8 * 1024 * 1024):
            raise ValueError("Unsafe private package command log")
        data = read_regular(path, 8 * 1024 * 1024)
        errors.update(token for token, literal in PACKAGER_ERROR_MARKERS.items() if literal in data)
        # The entire file is already bounded. Avoid unbounded line/regex work
        # for arbitrary server data or a single captured compiler-output line.
        for line in data.split(b"\n", 65536):
            if not 0 < len(line) <= 8192 or not line.startswith(b"+ "):
                continue
            scripts.update(token for token, pattern in PACKAGER_SCRIPT_MARKERS.items() if pattern.search(line))
    return {"status": "scanned_private_pair", "error_markers": sorted(errors), "script_markers": sorted(scripts)}


def public_failure(root, *, scan_passed, outer_cleanup, outer_reason):
    """Only a validated fixed record can leave the private workspace."""
    withheld = {"diagnostics": "withheld", "passed": False}
    if scan_passed is not True:
        return withheld
    try:
        from appimage_client import read_regular
        path = Path(root) / "fixed-progress.json"
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077
                or info.st_nlink != 1 or not 0 < info.st_size <= 8192):
            return withheld
        value = json.loads(read_regular(path, 8192))
        if not valid_record(value) or outer_cleanup not in CLEANUP or outer_reason not in REASONS:
            return withheld
        result = {"diagnostics": "scanned_fixed_facts", "passed": False, **value,
                  "outer_cleanup": outer_cleanup, "outer_reason": outer_reason}
        if (value["failure"] is not None and value["failure"]["command_stage"] == "package"
                and value["failure"]["reason"] == "command_exit"):
            result["package_failure_markers"] = package_failure_markers(root, scan_passed=scan_passed)
        return result
    except (OSError, RuntimeError, ValueError, TypeError, KeyError):
        return withheld
