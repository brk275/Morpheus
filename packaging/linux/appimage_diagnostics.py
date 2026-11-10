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
import sys
from types import ModuleType
import tempfile

COMMANDS = frozenset((
    "none", "unknown", "cargo-version", "packager-version", "download", "native-build", "package-config", "package", "extract", "update-metadata",
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
PHASES = frozenset(("none", "setup", "bus", "keyring", "display", "display_start", "display_wayland_wait", "display_ipc_wait", "portal", "peer", "launch", "identity", "login", "join",
                    "login_homeserver", "login_username", "login_password", "login_submit",
                    "login_room_wait", "login_room_open", "login_call_stage",
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
SOURCE_REASONS = frozenset(("source_input", "source_path", "source_bounds", "source_identity", "source_lock", "source_missing",
                            "source_archive", "source_link", "source_submodule", "source_git", "source_manifest", "source_output"))
REASONS = frozenset((*BUILTIN_REASONS.values(), "command_deadline", "command_output", "command_exit", "command_pipe", "command_cleanup", "command_input",
                     "command_http_forbidden", "command_http_not_found", "command_http_rate_limited", "command_http_unavailable",
                     "command_http_rejected", "command_download_transport", "command_download_tls",
                     "ui_wait_timeout", "ui_app_exited", "ui_tree_bound",
                     "capture_process_exit", "capture_log_failure", "capture_log_limit", "capture_wait_timeout",
                     "interrupted", "capture", "identity", "binding", "package", "inventory", "unknown", "namespace_exit",
                     "namespace_deadline", "namespace_output", "cleanup",
                     "inventory_input", "inventory_tool", "inventory_elf", "inventory_dependency",
                     "inventory_package", "inventory_notice", "inventory_deployment", "inventory_limit",
                     "inventory_elf_build_id", "inventory_elf_needed", "inventory_elf_soname",
                     "inventory_elf_search", "inventory_search",
                     *SOURCE_REASONS))
CLEANUP = frozenset(("not_started", "passed", "failed"))
POSITIONS = ("wrapper_phase", "command_stage", "capture_mode", "capture_phase")
FIELDS = frozenset((*POSITIONS, "schema", "peer_facts", "failure", "inner_cleanup"))


def reason(error):
    from appimage_process import CommandFailure
    if isinstance(error, CommandFailure):
        return "command_" + error.reason if "command_" + error.reason in REASONS else "unknown"
    if isinstance(error, (KeyboardInterrupt, SystemExit)):
        return "interrupted"
    # The driver already owns any instance of its exact exception type.
    # Do not import the driver or inspect a foreign exception's properties.
    capture_module = sys.modules.get("appimage_capture")
    capture_type = (vars(capture_module).get("CaptureFailure")
                    if type(capture_module) is ModuleType else None)
    if type(error) is capture_type:
        value = error.reason
        if type(value) is str and value in {"capture", "process_exit", "log_failure", "log_limit", "wait_timeout"}:
            return "capture" if value == "capture" else "capture_" + value
        return "unknown"
    for kind, token in BUILTIN_REASONS.items():
        if type(error) is kind:
            return token
    from linux_package_ui import DiagnosticFailure
    # This owned UI driver exposes only these fixed failure codes here. Never
    # publish a selector, entered value, foreign property or exception text.
    if type(error) is DiagnosticFailure:
        value = error.code
        return "ui_" + value if type(value) is str and value in {
            "wait_timeout", "app_exited", "tree_bound"} else "unknown"
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


def source_phase(error):
    from corresponding_source import SOURCE_PHASES, SourceFailure
    if type(error) is SourceFailure:
        value = error.source_phase
        if type(value) is str and value in SOURCE_PHASES:
            return value
    return None


PROCESS_EXIT_FIELDS = frozenset(("role", "status", "app_stdout", "app_stderr", "peer_stdout", "peer_stderr"))
PROCESS_EXIT_ROLES = frozenset(("app", "peer", "other", "unknown"))
PROCESS_LOG_STATES = frozenset(("clear", "failed", "truncated", "failed_truncated", "unknown"))


def valid_process_exit(value):
    if (type(value) is not dict or any(type(key) is not str for key in value)
            or set(value) != PROCESS_EXIT_FIELDS
            or type(value["role"]) is not str or value["role"] not in PROCESS_EXIT_ROLES
            or value["status"] is not None and (type(value["status"]) is not int
                                                or not -128 <= value["status"] <= 255)):
        return False
    return all(type(value[key]) is str and value[key] in PROCESS_LOG_STATES
               for key in ("app_stdout", "app_stderr", "peer_stdout", "peer_stderr"))


def process_exit(error):
    module = sys.modules.get("appimage_capture")
    owner = vars(module).get("CaptureFailure") if type(module) is ModuleType else None
    if type(error) is owner:
        values = vars(error)
        reason, value = values.get("reason"), values.get("process_exit")
        if type(reason) is str and reason == "process_exit" and valid_process_exit(value):
            return dict(value)
    return None


SHARE_CONTROL_FIELDS = frozenset(("observation", "state", "enabled", "checked"))
SHARE_CONTROL_STATES = frozenset(("share", "cancel", "stop", "missing", "ambiguous", "unreadable"))


def valid_share_control(value):
    if (type(value) is not dict or set(value) != SHARE_CONTROL_FIELDS
            or type(value["observation"]) is not str or value["observation"] != "after_timeout"
            or type(value["state"]) is not str
            or value["state"] not in SHARE_CONTROL_STATES
            or any(value[key] is not None and type(value[key]) is not bool
                   for key in ("enabled", "checked"))):
        return False
    return (value["state"] in {"share", "cancel", "stop"}
            or value["enabled"] is None and value["checked"] is None)


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
    if (type(failure) is not dict or set(failure) not in
            ({*POSITIONS, "reason"}, {*POSITIONS, "reason", "share_control"},
             {*POSITIONS, "reason", "source_phase"}, {*POSITIONS, "reason", "process_exit"})
            or failure["wrapper_phase"] not in WRAPPER or failure["command_stage"] not in COMMANDS
            or failure["capture_mode"] not in MODES or failure["capture_phase"] not in PHASES
            or failure["reason"] not in REASONS):
        return False
    if "process_exit" in failure:
        return failure["reason"] == "capture_process_exit" and valid_process_exit(failure["process_exit"])
    if "source_phase" in failure:
        from corresponding_source import SOURCE_PHASES
        return (type(failure["source_phase"]) is str and failure["source_phase"] in SOURCE_PHASES
                and type(failure["reason"]) is str and failure["reason"] in SOURCE_REASONS)
    if "share_control" not in failure:
        return True
    return (failure["wrapper_phase"] == "capture" and failure["command_stage"] == "capture-yellow"
            and failure["capture_mode"] in {"ordinary", "eager_bindings"}
            and failure["capture_phase"] == "reopened_frame" and failure["reason"] == "ui_wait_timeout"
            and valid_share_control(failure["share_control"]))


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

    def failed(self, error, control=None, *, share_control=None):
        self.peer(control)
        if self.value["failure"] is None:
            failure = {**{key: self.value[key] for key in POSITIONS}, "reason": reason(error)}
            phase = source_phase(error)
            if phase is not None:
                failure["source_phase"] = phase
            exited = process_exit(error)
            if exited is not None:
                failure["process_exit"] = exited
            if share_control is not None:
                failure["share_control"] = share_control
                if not valid_record({**self.value, "failure": failure}):
                    raise ValueError("Invalid share-control observation")
            self.value["failure"] = failure
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
    # CLI errors are distinct from packaging errors; each phrase is fixed in
    # the authenticated 0.11.8 CLI source, never captured exception text.
    "cli_metadata": b"Failed to read cargo metadata:",
    "cli_config": b"Failed to parse config",
    "cli_manifest": b"Failed to parse Cargo.toml:",
    "cli_package_json": b"Failed to parse package.json:",
    "cli_workspace_config": b"Failed to deserialize config from `package.metadata.packager` in Cargo.toml:",
    "cli_no_config": b"Couldn't detect a valid configuration file or all configurations are disabled! Nothing to do here.",
    # Transparent std::io errors otherwise have no crate-specific prefix.
    # Publish only these fixed numeric OS classes, never the private path.
    "filesystem_missing": b"(os error 2)",
    "filesystem_denied": b"(os error 13)",
    "filesystem_not_directory": b"(os error 20)",
    "filesystem_readonly": b"(os error 30)",
    "filesystem_no_space": b"(os error 28)",
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
        # Only normalize bounded SGR color escapes AFTER the complete raw logs
        # passed the independent default scan. No terminal text is published.
        # Do not interpret OSC/control strings or alter the raw scan input.
        data = re.sub(rb"\x1b\[[0-9;]{0,64}m", b"", data)
        errors.update(token for token, literal in PACKAGER_ERROR_MARKERS.items() if literal in data)
        # The entire file is already bounded. Avoid unbounded line/regex work
        # for arbitrary server data or a single captured compiler-output line.
        for line in data.split(b"\n", 65536):
            if not 0 < len(line) <= 8192 or not line.startswith(b"+ "):
                continue
            scripts.update(token for token, pattern in PACKAGER_SCRIPT_MARKERS.items() if pattern.search(line))
    return {"status": "scanned_private_pair", "error_markers": sorted(errors), "script_markers": sorted(scripts)}


# Only exact static panic labels in the authenticated receiving peer source.
# Error suffixes, thread names, paths and source lines never leave the reducer.
PEER_PANIC_LABELS = {
    b"Installed app sender disappeared": "sender_missing",
    b"Installed app sender changed": "sender_changed",
    b"Installed app participant changed": "participant_changed",
    b"Screen track has no video frames": "screen_frames",
    b"Installed screen track ended before matching pixels": "screen_stream_ended",
    b"Installed screen pixels were not delivered": "screen_pixel_deadline",
    b"Installed screen track was not observed before deadline": "screen_track_deadline",
    b"Installed screen stream delivered no frame before deadline": "screen_frame_deadline",
    b"Installed screen frames did not match expected source before deadline": "screen_match_deadline",
    b"Installed app did not reach the next peer phase": "command_deadline",
    b"Call ended instead of screen share": "call_ended_during_stop",
    b"Installed screen track did not stop": "screen_stop_deadline",
    b"Peer cleanup failed": "cleanup",
    b"Installed capture phase deadline exceeded": "work_deadline",
    b"Cannot open peer store": "open_store",
    b"Peer login failed": "login",
    b"Peer sticky room sync did not become ready": "sticky_sync",
    b"Cannot read peer rooms": "sticky_sync",
    b"Peer sync closed": "sticky_sync",
    b"Peer client missing": "client",
    b"Cannot open fixture slot": "slot_open",
    b"Peer join failed": "join",
    **{label: "cross_signing" for label in (
        b"Signed-in client has no user ID",
        b"Cannot refresh own cross-signing identity before bootstrap",
        b"Cannot read own device before bootstrap",
        b"Cross-signing bootstrap challenge timed out",
        b"Cross-signing bootstrap failed without a password challenge",
        b"Cross-signing bootstrap timed out", b"Cross-signing bootstrap failed",
        b"Refreshing own cross-signing identity timed out",
        b"Cannot refresh own cross-signing identity",
        b"Own cross-signing identity missing after bootstrap",
        b"Own cross-signing identity is not verified",
        b"Cannot read own device after cross-signing bootstrap",
        b"Own device missing after cross-signing bootstrap",
        b"Signing own device timed out", b"Cannot sign own device",
        b"Refreshing signed own device timed out", b"Cannot refresh signed own device",
        b"Cannot read signed own device", b"Signed own device missing",
        b"Own device is not signed by the cross-signing identity")},
}
PEER_PANIC_FRAME = re.compile(
    rb"thread '[^\r\n]{1,128}'(?: \([0-9]{1,10}\))? panicked at "
    rb"(?:[^\x00\r\n]{1,4096}/)?"
    rb"packaging/linux/appimage_peer\.rs:[0-9]{1,6}:[0-9]{1,4}:")


def peer_failure_marker(root, mode, *, scan_passed, include_sites=False):
    """Reduce one completed, scanned private stderr; never establish acceptance."""
    if type(include_sites) is not bool:
        raise ValueError("Invalid peer marker selection")
    def record(status, site="unknown", sites=()):
        value = {"status": status, "panic_site": site}
        if include_sites:
            value["panic_sites"] = sorted(set(sites))
        return value
    if scan_passed is not True:
        return record("withheld")
    from appimage_client import private_directory
    case = {"ordinary": "ordinary", "eager_bindings": "bindings"}.get(mode)
    if case is None:
        raise ValueError("Invalid private peer mode")
    directory = Path(root)
    for part in (None, case, "session"):
        if part is not None:
            directory = directory / part
        try:
            private_directory(directory)
        except FileNotFoundError:
            return record("unavailable")
    path = directory / "peer.stderr.log"
    try:
        before = path.lstat()
    except FileNotFoundError:
        return record("unavailable")
    if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid()
            or before.st_mode & 0o077 or before.st_nlink != 1
            or not 0 <= before.st_size <= 8 * 1024 * 1024):
        raise ValueError("Unsafe private peer log")
    def identity(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_nlink,
                info.st_size, info.st_mtime_ns, info.st_ctime_ns)
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        opened = os.fstat(stream.fileno())
        if identity(before) != identity(opened):
            raise ValueError("Changed private peer log")
        data = stream.read(8 * 1024 * 1024 + 1)
        if (identity(opened) != identity(os.fstat(stream.fileno()))
                or len(data) != before.st_size):
            raise ValueError("Changed private peer log")
    if identity(before) != identity(path.lstat()):
        raise ValueError("Changed private peer log")
    # Exact peer panic frames and their immediately following static labels
    # are observed context, not proof that an exception is causal.
    if data.count(b"\n") > 65536:
        return record("scanned_private_peer_stderr")
    lines = data.split(b"\n", 65536)
    sites = []
    for index, line in enumerate(lines[:-1]):
        if len(line) <= 4608 and PEER_PANIC_FRAME.fullmatch(line.rstrip(b"\r")):
            message = lines[index + 1].rstrip(b"\r")
            site = next((token for label, token in PEER_PANIC_LABELS.items()
                         if message == label or message.startswith(label + b": ")), "unknown")
            sites.append(site)
    return record("scanned_private_peer_stderr",
                  sites[0] if len(sites) == 1 else "ambiguous" if sites else "unknown", sites)


CAPTURE_FAILURE_PREFIX = b"MORPHEUS_NATIVE_CAPTURE_FAILURE "
CAPTURE_FAILURE_CODES = frozenset((
    b"portal_setup",
    b"portal_create",
    b"portal_select",
    b"portal_start",
    b"portal_remote",
    b"portal_selection",
    b"pipewire_setup",
    b"portal_session_closed",
    b"pipewire_unconnected",
    b"pipewire_error",
    b"pipewire_paused",
    b"format_rejected",
    b"buffer_plane_missing",
    b"frame_chunk_rejected",
    b"pipeline_unknown",
    b"first_format",
    b"first_buffer",
    b"first_streaming",
    b"first_processing",
    b"first_dequeue",
    b"first_frame",
    b"capture_worker",
    b"capture_start_timeout",
    b"capture_select_timeout",
    b"capture_publish",
    b"capture_publish_timeout",
    b"capture_frame_push",
    b"capture_unpublish",
    b"capture_worker_shutdown",
    b"capture_previous_worker",
    b"capture_thread_spawn",
    b"capture_start_rejected",
))


def _capture_marker_codes(data):
    """Fixed observations only; never infer which attempt caused a marker."""
    rejected = {"status": "invalid", "codes": []}
    if (type(data) is not bytes or len(data) > 8 * 1024 * 1024
            or data.count(b"\n") > 65536):
        return rejected
    observed = []
    for raw in data.split(b"\n", 65536):
        line = raw.removesuffix(b"\r")
        if not line.startswith(b"MORPHEUS_NATIVE_CAPTURE_FAILURE"):
            continue
        if (len(line) > 128 or not line.startswith(CAPTURE_FAILURE_PREFIX)
                or line[len(CAPTURE_FAILURE_PREFIX):] not in CAPTURE_FAILURE_CODES
                or len(observed) >= 16):
            return rejected
        observed.append(line[len(CAPTURE_FAILURE_PREFIX):].decode("ascii"))
    return {"status": "known_codes", "codes": observed}


def _runtime_wait_observation(data):
    """Admit one exact complete runtime record, never arbitrary stderr text."""
    if (type(data) is not bytes or len(data) > 8 * 1024 * 1024
            or data.count(b"\n") > 65536):
        return {"status": "invalid"}
    prefix = b"MORPHEUS_APPIMAGE_WAIT_V1"
    selected = None
    lines = data.split(b"\n", 65536)
    for index, line in enumerate(lines):
        if not line.startswith(prefix):
            continue
        # The producer emits a leading separator and a final LF atomically.
        # A partial, duplicate, oversized or decorated reserved record is not a fact.
        if (selected is not None or index == 0 or index == len(lines) - 1 or len(line) >= 96
                or b"\x00" in line or b"\r" in line):
            return {"status": "invalid"}
        selected = line
    if selected is None:
        return {"status": "absent"}
    parts = selected.split(b" ")
    if (len(parts) != 3 or parts[0] != prefix
            or parts[2] not in {b"cleanup=passed", b"cleanup=failed", b"cleanup=skipped"}):
        return {"status": "invalid"}
    field = parts[1].split(b"=")
    if len(field) != 2:
        return {"status": "invalid"}
    kind, value = field
    result = {"status": "observed", "cleanup": parts[2][len(b"cleanup="):].decode("ascii")}
    if kind in {b"exit", b"signal"}:
        if (not 1 <= len(value) <= 3 or not value.isdigit()
                or (len(value) > 1 and value.startswith(b"0"))):
            return {"status": "invalid"}
        number = int(value)
        if kind == b"exit" and 0 <= number <= 255:
            result.update(outcome="exited", exit_code=number)
        elif kind == b"signal" and 1 <= number <= 64:
            result.update(outcome="signaled", signal=number)
        else:
            return {"status": "invalid"}
    elif kind == b"wait" and value in {b"failed", b"other"}:
        result["outcome"] = "wait_failed" if value == b"failed" else "wait_other"
    else:
        return {"status": "invalid"}
    return result


def _rust_panic_framing(line):
    """Recognize the pinned Rust1.99 hook frame without exporting its fields."""
    if (type(line) is not bytes or len(line) > 8192 or b"\x00" in line
            or b"\r" in line or b"\n" in line or not line.startswith(b"thread '")):
        return False
    name, separator, rest = line[len(b"thread '"):].partition(b"' (")
    if not separator or len(name) > 256:
        return False
    thread_id, separator, location = rest.partition(b") panicked at ")
    if (not separator or not 1 <= len(thread_id) <= 20 or not thread_id.isdigit()
            or thread_id.startswith(b"0")):
        return False
    fields = location.rsplit(b":", 3)
    return (len(fields) == 4 and 1 <= len(fields[0]) <= 2048 and fields[3] == b""
            and all(1 <= len(value) <= 10 and value.isdigit()
                    and (len(value) == 1 or not value.startswith(b"0")) for value in fields[1:3]))


def _app_process_marker_codes(data):
    """Observe fixed stderr labels; neither identify a child signal nor a cause."""
    if (type(data) is not bytes or len(data) > 8 * 1024 * 1024
            or data.count(b"\n") > 65536):
        return {"status": "invalid", "codes": []}
    observed = set()
    # Paired text context only: never emitting-thread or callback provenance.
    refcell_occurrences = refcell_pairs = 0
    refcell_context = previous_frame_context = None
    lines = data.split(b"\n", 65536)
    for index, raw in enumerate(lines):
        line = raw.removesuffix(b"\r")
        # Never export the executable/library path, symbol or private payload.
        # Unknown, localized, malformed or oversized lines stay unclassified.
        if len(line) > 8192 or b"\x00" in line:
            previous_frame_context = None
            continue
        if b": symbol lookup error: " in line and b": undefined symbol: " in line:
            observed.add("loader_undefined_symbol")
        if b": error while loading shared libraries: " in line:
            observed.add("loader_shared_libraries")
        if line == b"Failed to clean up cache directory":
            observed.add("runtime_cleanup")
        if line.startswith(b"Failed to run ") and b": " in line[len(b"Failed to run "):]:
            observed.add("runtime_exec")
        framed = _rust_panic_framing(line)
        if framed:
            observed.add("rust_panic_framing")
        if line in {b"panic in a function that cannot unwind", b"thread caused non-unwinding panic. aborting."}:
            observed.add("rust_non_unwinding_panic")
        if line in {b"RefCell already borrowed", b"RefCell already mutably borrowed"}:
            observed.add("rust_refcell_borrow_panic")
            # Count every occurrence accepted by the old generic comparison,
            # including CR-trimmed or final fragments, so it cannot look unique.
            refcell_occurrences = min(refcell_occurrences + 1, 2)
            if previous_frame_context is not None and index < len(lines) - 1 and raw == line:
                refcell_pairs = min(refcell_pairs + 1, 2)
                refcell_context = previous_frame_context if refcell_pairs == 1 else None
        previous_frame_context = None
        if (framed and index < len(lines) - 1 and raw == line
                and line.count(b") panicked at ") == 1):
            if line.startswith(b"thread 'morpheus-screen' ("):
                previous_frame_context = "rust_refcell_thread_screen"
            elif line.startswith(b"thread 'main' ("):
                previous_frame_context = "rust_refcell_thread_main"
            else:
                previous_frame_context = "rust_refcell_thread_unknown"
    if refcell_occurrences == 1 and refcell_pairs == 1 and refcell_context is not None:
        observed.add(refcell_context)
    result = {"status": "observed_fixed_labels" if observed else "unknown", "codes": sorted(observed)}
    wait = _runtime_wait_observation(data)
    if wait["status"] != "absent":
        result["runtime_wait"] = wait
        if not observed and wait["status"] == "observed":
            result["status"] = "observed_runtime_wait"
    return result


def capture_failure_markers(root, mode, *, scan_passed, include_process=False):
    """Read only the stopped, independently scanned owned app stderr."""
    if type(include_process) is not bool:
        raise ValueError("Invalid private process marker selection")
    if scan_passed is not True:
        return {"status": "withheld", "codes": []}
    from appimage_client import private_directory
    case = {"ordinary": "ordinary", "eager_bindings": "bindings"}.get(mode)
    if case is None:
        raise ValueError("Invalid private capture mode")
    directory = Path(root)
    for part in (None, case, "session"):
        if part is not None:
            directory = directory / part
        try:
            private_directory(directory)
        except FileNotFoundError:
            return {"status": "unavailable", "codes": []}
    path = directory / "app.stderr.log"
    try:
        before = path.lstat()
    except FileNotFoundError:
        return {"status": "unavailable", "codes": []}
    if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid()
            or before.st_mode & 0o077 or before.st_nlink != 1
            or not 0 <= before.st_size <= 8 * 1024 * 1024):
        raise ValueError("Unsafe private capture log")
    def identity(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_nlink,
                info.st_size, info.st_mtime_ns, info.st_ctime_ns)
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        opened = os.fstat(stream.fileno())
        if identity(before) != identity(opened):
            raise ValueError("Changed private capture log")
        data = stream.read(8 * 1024 * 1024 + 1)
        if (identity(opened) != identity(os.fstat(stream.fileno()))
                or len(data) != before.st_size):
            raise ValueError("Changed private capture log")
    if identity(before) != identity(path.lstat()):
        raise ValueError("Changed private capture log")
    result = _capture_marker_codes(data)
    if result["status"] == "known_codes":
        result["status"] = "scanned_private_app_stderr"
    if include_process:
        result["process_markers"] = _app_process_marker_codes(data)
    return result


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
        if (value["wrapper_phase"] == "capture" and value["capture_phase"] == "peer"
                and "peer_ready" not in value["peer_facts"]
                and value["inner_cleanup"] == "passed" and outer_cleanup == "passed"):
            result["peer_failure_marker"] = peer_failure_marker(
                root, value["capture_mode"], scan_passed=scan_passed)
        failure = value["failure"]
        if (failure is not None and failure["wrapper_phase"] == "capture"
                and failure["capture_phase"] == "reopened_frame"
                and failure["reason"] == "capture_process_exit"
                and value["inner_cleanup"] == "passed" and outer_cleanup == "passed"):
            # Observed static markers are diagnostic context only. They do not
            # establish which process failed, causality or package acceptance.
            result["peer_failure_marker"] = peer_failure_marker(
                root, failure["capture_mode"], scan_passed=scan_passed, include_sites=True)
        owned_app_exit = (failure is not None and failure["reason"] == "capture_process_exit"
                          and valid_process_exit(failure.get("process_exit"))
                          and failure["process_exit"]["role"] == "app")
        if (failure is not None and failure["wrapper_phase"] == "capture"
                and failure["reason"] == "capture_process_exit"
                and (failure["capture_phase"] == "reopened_frame" or owned_app_exit)
                and value["inner_cleanup"] == "passed" and outer_cleanup == "passed"):
            # The exact owned app can exit during any closed capture phase.
            # Keep the original first failure; reduce the same stopped/scanned
            # bounded stderr, without changing watch order or acceptance.
            result["native_capture_failure_markers"] = capture_failure_markers(
                root, failure["capture_mode"], scan_passed=scan_passed,
                include_process=owned_app_exit)
        if (failure is not None and failure["wrapper_phase"] == "capture"
                and failure["capture_phase"] == "reopened_frame"
                and failure["command_stage"] == "capture-yellow"
                and failure["reason"] == "ui_wait_timeout"
                and value["inner_cleanup"] == "passed" and outer_cleanup == "passed"):
            result["native_capture_failure_markers"] = capture_failure_markers(
                root, failure["capture_mode"], scan_passed=scan_passed)
        return result
    except (OSError, RuntimeError, ValueError, TypeError, KeyError):
        return withheld
