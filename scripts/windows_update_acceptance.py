"""Explicit private standard-host Windows installed-update mechanism acceptance.

Two version-only native builds use the existing producer, NSIS template, signer
and retained-handle Windows Job route. No release keys, profiles, vault, accounts,
UI or media are used. Shipping activation and broader beta gates stay closed.
"""
from __future__ import annotations
import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import time
import tomllib

if __package__:
    from . import package as package, native_notices as notices
    from . import native_notice_source as notice_source
    from . import package_revision as revision
    from . import prepare_windows_update as producer
    from . import run_windows_capture_acceptance as capture
else:
    import package
    import native_notices as notices
    import native_notice_source as notice_source
    import package_revision as revision
    import prepare_windows_update as producer
    import run_windows_capture_acceptance as capture

ROOT = Path(__file__).resolve().parents[1]
CASES = ("invalid-signature", "truncated", "discard", "staged-mutation", "call-exclusion",
         "failed-trial", "interrupted-marker", "upgrade")
KEY = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Morpheus"
MANUFACTURER = r"Software\Morpheus\Morpheus"
LIMIT = 512 * 1024 * 1024
PLAN = "MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_PLAN"
PLAN_HASH = PLAN + "_SHA256"
ENTRY_MAGIC = b"MORPHEUS-WINDOWS-UPDATE-QA-V1\n"

class Failure(RuntimeError):
    def __init__(self):
        super().__init__("Private installed Windows update acceptance failed")

def require(value):
    if not value:
        raise Failure()

def boolean(value, expected):
    return value is expected or type(value) is str and value == str(expected).lower()

def guard(environment, platform=sys.platform):
    require(platform == "win32")
    for name, value in {"GITHUB_ACTIONS":"true", "RUNNER_ENVIRONMENT":"github-hosted", "RUNNER_OS":"Windows",
        "GITHUB_REPOSITORY":"brk275/Morpheus", "GITHUB_REF":"refs/heads/main", "GITHUB_EVENT_NAME":"workflow_dispatch",
        "GITHUB_ACTOR":"brk275", "GITHUB_TRIGGERING_ACTOR":"brk275"}.items():
        require(environment.get(name) == value)
    require(environment.get("IMAGEOS") in {"win25", "win25-vs2026"})
    event = json.loads(read(Path(environment["GITHUB_EVENT_PATH"]), 1024 * 1024))
    owner = event.get("repository", {}).get("owner", {}).get("id")
    require(type(owner) is int and owner > 0 and event.get("sender", {}).get("id") == owner
            and event.get("repository", {}).get("private") is False
            and event.get("inputs", {}).get("platform") in {"windows", "all"}
            and boolean(event.get("inputs", {}).get("source_bundle"), False))

def checked(path, *, directory=False):
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path)
    for item in (path, *path.parents):
        info = item.lstat()
        require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400)
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode) and info.st_nlink == 1)
    return path

def read(path, limit):
    path = checked(path)
    require(0 < path.stat().st_size <= limit)
    with path.open("rb") as source:
        data = source.read(limit + 1)
    require(0 < len(data) <= limit)
    return data

def digest(path, limit=LIMIT):
    path = checked(path)
    before = path.stat()
    require(0 < before.st_size <= limit)
    value, count = hashlib.sha256(), 0
    with path.open("rb") as source:
        for data in iter(lambda: source.read(65536), b""):
            value.update(data); count += len(data)
            require(count <= limit)
    after = path.stat()
    require(count == before.st_size and (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns)
            == (after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns))
    return value.hexdigest()

def write_new(path, data):
    with path.open("xb") as stream:
        stream.write(data); stream.flush(); os.fsync(stream.fileno())
    path.chmod(0o600)

def private_environment(environment, home):
    # CPython Windows os.environ.items() exposes canonical uppercase keys.
    # Keep the same closed toolchain allowlist when copying that real mapping.
    names = {"PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "PROGRAMFILES", "PROGRAMFILES(X86)",
        "PROGRAMW6432", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS", "INCLUDE", "LIB", "LIBPATH",
        "VCINSTALLDIR", "VSINSTALLDIR", "VCTOOLSINSTALLDIR", "WINDOWSSDKDIR", "WINDOWSSDKVERSION",
        "UNIVERSALCRTSDKDIR", "UCRTVERSION", "CARGO_HOME", "RUSTUP_HOME", "RUNNER_TEMP", "IMAGEOS",
        "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS", "GITHUB_REPOSITORY", "GITHUB_REF",
        "GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_SHA", "GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR",
        "MORPHEUS_PACKAGE_SOURCE_REVISION"}
    result = {k:v for k,v in environment.items() if k in names}
    # Preserve actual fresh-host toolchain locations before replacing HOME.
    original = Path(environment["USERPROFILE"])
    result.setdefault("CARGO_HOME", str(original / ".cargo"))
    result.setdefault("RUSTUP_HOME", str(original / ".rustup"))
    for name in ("home", "roaming", "local", "temp"):
        if os.name == "nt":
            notice_source.private_directory(home / name, parents=True, exist_ok=True)
        else:
            (home / name).mkdir(parents=True, exist_ok=True)
    if os.name == "nt":
        # Cargo/libgit2 reads its own controlled HOME. The notice-fetch HOME
        # is separate; do not inherit ambient Git configuration or credentials.
        config = home / "home" / ".gitconfig"
        expected = b"[core]\n\tlongpaths = true\n"
        notice_source.checked(config.parent, directory=True, private=True)
        if not config.exists():
            write_new(config, expected)
        notice_source.checked(config, private=True)
        require(notices.read_regular(config, 128) == expected)
    result.update(HOME=str(home/"home"), USERPROFILE=str(home/"home"), APPDATA=str(home/"roaming"),
        LOCALAPPDATA=str(home/"local"), TEMP=str(home/"temp"), TMP=str(home/"temp"),
        RUST_LOG="warn", RUST_BACKTRACE="0", CARGO_INCREMENTAL="0", CARGO_BUILD_JOBS="2",
        RUSTUP_TOOLCHAIN="1.99.0", GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
    return result


def msvc_build_environment(environment):
    """Select protected provider x64 tools; never resolve GNU link from PATH.

    Rust's vcvars shortcut can accept the first link.exe when its architecture
    probe is unavailable. Derive the complete fixed native toolchain instead
    of forwarding more ambient vcvars/compiler overrides or running setup hooks.
    """
    result = dict(environment)
    program_files = Path(environment["PROGRAMFILES"])
    installations = [program_files / "Microsoft Visual Studio" / name / "Enterprise"
                     for name in ("18", "2022")]
    installation = next((path for path in installations if path.exists()), None)
    require(installation is not None)
    notice_source.checked(installation, directory=True)
    metadata = installation / "VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt"
    notice_source.checked(metadata)
    version = notices.read_regular(metadata, 64).decode("ascii").strip()
    require(re.fullmatch(r"14\.[0-9]{1,2}\.[0-9]{3,6}", version) is not None)
    tool_root = installation / "VC/Tools/MSVC" / version
    binaries = tool_root / "bin/Hostx64/x64"
    notice_source.checked(binaries, directory=True)
    for name in ("link.exe", "cl.exe", "lib.exe"):
        tool = notice_source.checked(binaries / name)
        require(tool.stat().st_size > 0)
    libraries = [tool_root / "lib/x64"]
    includes = [tool_root / "include"]
    atl = tool_root / "atlmfc"
    if atl.exists():
        notice_source.checked(atl, directory=True)
        libraries.append(atl / "lib/x64")
        includes.append(atl / "include")
    sdk = Path(environment["PROGRAMFILES(X86)"]) / "Windows Kits/10"
    notice_source.checked(sdk / "Lib", directory=True)
    versions = []
    for index, path in enumerate((sdk / "Lib").iterdir(), 1):
        require(index <= 64)
        if re.fullmatch(r"10\.0\.[0-9]{4,6}\.[0-9]{1,5}", path.name):
            notice_source.checked(path, directory=True)
            try:
                for part, library in (("um", "kernel32.lib"), ("ucrt", "ucrt.lib")):
                    parent = notice_source.checked(path / part / "x64", directory=True)
                    notice_source.checked(parent / library)
            except FileNotFoundError:
                continue  # A partial installed SDK is not a complete candidate.
            versions.append(path.name)
    require(versions)
    sdk_version = max(versions, key=lambda value: tuple(map(int, value.split("."))))
    for part, library in (("um", "kernel32.lib"), ("ucrt", "ucrt.lib")):
        path = sdk / "Lib" / sdk_version / part / "x64"
        notice_source.checked(path / library)
        libraries.append(path)
    includes.extend(sdk / "Include" / sdk_version / name for name in ("ucrt", "shared", "um", "winrt", "cppwinrt"))
    for path in libraries + includes:
        notice_source.checked(path, directory=True)
    result.update(CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=str(binaries / "link.exe"),
        PATH=str(binaries) + os.pathsep + environment["PATH"],
        LIB=os.pathsep.join(map(str, libraries)), INCLUDE=os.pathsep.join(map(str, includes)),
        LIBPATH=os.pathsep.join(map(str, libraries)),
        VSINSTALLDIR=str(installation) + os.sep, VCINSTALLDIR=str(installation / "VC") + os.sep,
        VCTOOLSINSTALLDIR=str(tool_root) + os.sep, VCTOOLSVERSION=version,
        VSCMD_ARG_TGT_ARCH="x64", WINDOWSSDKDIR=str(sdk) + os.sep,
        WINDOWSSDKVERSION=sdk_version + os.sep, UNIVERSALCRTSDKDIR=str(sdk) + os.sep,
        UCRTVERSION=sdk_version)
    return result


class Commands:
    def __init__(self, root, environment):
        self.root, self.environment = root, environment
        self.logs = root / "logs"; self.logs.mkdir()
        self.sequence = 0
        self.build_substage = "unavailable"
        self.failed_command = None
        self.last_exit_status = None
        # Closed observations only; never retain command text or private output.
        self.command_phase = "unavailable"
        self.close_state = "unattempted"
        self.close_refusal = None
        self.scan_phase = "unattempted"
        self.scan_counts = dict(success=0, exit_one=0, other_exit=0, timeout=0, refusal=0)
        self.cleanup_failed = False
        self.deadline = time.monotonic() + 6900
    def close_job(self, job):
        self.close_state = "attempted"
        self.close_refusal = None
        try:
            job.close()
            self.close_state = "complete"
        except BaseException as error:
            self.cleanup_failed = True
            try:
                self.close_state = "failed"
                if (type(error) is capture.Failure and type(error.code) is str
                        and error.code in capture.FAILURES and type(error.operation) is str
                        and error.operation in capture.OPERATIONS):
                    self.close_refusal = {"code": error.code, "operation": error.operation}
            except BaseException:
                pass  # Optional observations cannot replace failed Job containment.
            raise
    def start(self, job, args, environment=None, cwd=ROOT):
        # Only the fixed protected system Git may retain provider hardlinks.
        # Recheck its complete path/ACL contract immediately before each spawn;
        # a same-named alias cannot borrow the initial selection's admission.
        if args and Path(args[0]).name.casefold() == "git.exe":
            require(Path(args[0]) == notice_source.system_git())
        checked(cwd, directory=True)
        self.sequence += 1
        log = self.logs / (str(self.sequence).zfill(4) + ".log")
        with log.open("xb") as stream:
            log.chmod(0o600)
            handle = job.spawn(list(map(str,args)), self.environment | (environment or {}), stream, cwd=cwd)
        return handle, log
    def bounded_logs(self):
        total = 0
        logs = list(self.logs.glob("*.log"))
        require(len(logs) <= 128)
        for log in logs:
            size = checked(log).stat().st_size
            require(size <= 8 * 1024 * 1024)
            total += size
        require(total <= 64 * 1024 * 1024)
    def wait(self, job, handle, seconds):
        end = min(self.deadline, time.monotonic() + seconds)
        while True:
            self.command_phase = "wait_log_bounds"
            self.bounded_logs()
            self.command_phase = "process_wait"
            state = job.kernel.WaitForSingleObject(handle, 0)
            if state == 0:
                return job.wait(handle, 0)
            require(state == 258 and time.monotonic() < end)
            time.sleep(.025)
    def empty(self, job, seconds=30):
        end = min(self.deadline, time.monotonic()+seconds)
        self.command_phase = "job_query"
        while job.active():
            self.command_phase = "drain_log_bounds"
            self.bounded_logs()
            self.command_phase = "drain_deadline"
            require(time.monotonic() < end)
            time.sleep(.025)
            self.command_phase = "job_query"
        self.command_phase = "drained_log_bounds"
        self.bounded_logs()
    def run(self, args, *, environment=None, cwd=ROOT, seconds=180):
        self.failed_command = None
        self.last_exit_status = None
        status = None
        self.command_phase = "job_create"
        job = capture.WindowsJob()
        try:
            self.command_phase = "start"
            handle, log = self.start(job,args,environment,cwd)
            status = self.wait(job, handle, seconds)
            try:
                self.last_exit_status = status if type(status) is int and 0 <= status <= 0xffffffff else None
            except BaseException:
                pass  # Optional metadata cannot replace the original lifecycle.
            self.command_phase = "exit_status"
            require(status == 0)
            self.empty(job)
            self.command_phase = "result_log_bounds"
            require(log.stat().st_size <= 8*1024*1024)
            self.command_phase = "result_log_read"
            result = log.read_bytes()
            self.command_phase = "complete"
            return result
        finally:
            self.close_job(job)
            if type(status) is int and status != 0:
                try:
                    self.failed_command = (log, status, log.stat().st_size)
                except OSError:
                    pass  # Optional metadata must not replace the refusal.
    def scan(self, scanner):
        self.scan_phase = "configuration"
        config, ignore = self.root/"default-gitleaks.toml", self.root/"empty.ignore"
        write_new(config,b"[extend]\nuseDefault = true\n"); write_new(ignore,b"")
        failed = False
        self.scan_phase = "enumeration"
        for log in sorted(self.logs.glob("*.log")):
            try:
                self.scan_phase = "owned_log_read"
                require(log.stat().st_size <= 8*1024*1024)
                with checked(log).open("rb") as stream:
                    self.scan_phase = "scanner"
                    result = subprocess.run([str(scanner),"stdin","--config",str(config),"--gitleaks-ignore-path",str(ignore),
                        "--ignore-gitleaks-allow","--redact=100","--no-banner"],stdin=stream,
                        stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,cwd=self.root,
                        env=self.environment,timeout=60,check=False)
                if result.returncode:
                    failed = True
                    self.scan_counts["exit_one" if result.returncode == 1 else "other_exit"] += 1
                else:
                    self.scan_counts["success"] += 1
            except subprocess.TimeoutExpired:
                self.scan_counts["timeout"] += 1
                failed = True
            except (Failure,OSError,subprocess.SubprocessError):
                self.scan_counts["refusal"] += 1
                failed = True
        self.scan_phase = "result"
        require(not failed)
        self.scan_phase = "complete"

# Closed source-location metadata only. Never inspect command output, exception
# messages, arguments, paths, fixture keys or transaction contents here.
_FAILURE_STAGES = frozenset(("source-export", "build-old", "build-new", "signing", *CASES))
_BUILD_SUBSTAGES = frozenset(("unavailable", "source_export", "notices_prepare", "cargo_build",
                             "payload", "config", "producer", "cargo_packager"))



_CLEANUP_PREDICATES = frozenset(("root_identity", "inventory_limit", "inventory_symlink",
    "inventory_reparse", "callback_scope", "callback_root_identity", "inner_removed",
    "workspace_scope", "workspace_identity", "workspace_empty", "workspace_removed"))


def _cleanup_require(value, predicate):
    if not value:
        error = Failure()
        error.cleanup_predicate = predicate
        raise error


def _cleanup_reparse_metadata(root, path, info):
    """Closed metadata from the inventory's existing lstat, never follow/query."""
    relative = path.relative_to(root)
    first = relative.parts[0] if relative.parts else ""
    role = first if first in {"native-target", "tools-home", "tiny-source", "synthetic-git", "logs"} else "other"
    kind = "directory" if stat.S_ISDIR(info.st_mode) else "regular" if stat.S_ISREG(info.st_mode) else "other"
    attributes = getattr(info, "st_file_attributes", None)
    tag = getattr(info, "st_reparse_tag", None)
    subcomponent = "not-tools-home"
    compatibility = "not-tools-home"
    if role == "tools-home":
        component = relative.parts[1].casefold() if len(relative.parts) > 1 else ""
        subcomponent = component if component in {"home", "roaming", "local", "temp"} else "other" if component else "none"
        compatibility = {"application data": "application-data", "local settings": "local-settings",
                         "history": "history", "cookies": "cookies",
                         "temporary internet files": "temporary-internet-files",
                         "my documents": "my-documents", "nethood": "nethood", "printhood": "printhood",
                         "recent": "recent", "sendto": "sendto", "templates": "templates",
                         "start menu": "start-menu"}.get(relative.name.casefold(), "other")
    known_components = {"tools-home", "home", "appdata", "local", "roaming", "temp", "microsoft",
                        "windows", "visualstudio", ".cargo", ".rustup", "toolchains", "registry", "cache",
                        "native-target", "tiny-source", "synthetic-git", "logs"}
    components = [part.casefold() if part.casefold() in known_components else "opaque"
                  for part in relative.parts] if len(relative.parts) <= 64 else None
    return {"role": role, "kind": kind,
            "attributes": attributes if type(attributes) is int and 0 <= attributes <= 0xffffffff else None,
            "tag": tag if type(tag) is int and 0 <= tag <= 0xffffffff else None,
            "subcomponent": subcomponent, "compatibility_name": compatibility,
            "depth": len(relative.parts) if 1 <= len(relative.parts) <= 64 else None, "components": components}


def _valid_cleanup_reparse(value):
    return (type(value) is dict and set(value) == {"role", "kind", "attributes", "tag", "subcomponent", "compatibility_name", "depth", "components"}
            and type(value["role"]) is str and value["role"] in {"native-target", "tools-home", "tiny-source", "synthetic-git", "logs", "other"}
            and type(value["kind"]) is str and value["kind"] in {"directory", "regular", "other"}
            and all(value[k] is None or type(value[k]) is int and 0 <= value[k] <= 0xffffffff for k in ("attributes", "tag"))
            and type(value["subcomponent"]) is str and value["subcomponent"] in {"not-tools-home", "home", "roaming", "local", "temp", "other", "none"}
            and type(value["compatibility_name"]) is str and value["compatibility_name"] in {"not-tools-home", "application-data", "local-settings", "history", "cookies", "temporary-internet-files", "my-documents", "nethood", "printhood", "recent", "sendto", "templates", "start-menu", "other"}
            and (value["depth"] is None or type(value["depth"]) is int and 1 <= value["depth"] <= 64)
            and ((value["depth"] is None and value["components"] is None)
                 or (type(value["components"]) is list and len(value["components"]) == value["depth"]
                     and all(type(part) is str and part in {"tools-home", "home", "appdata", "local", "roaming", "temp", "microsoft", "windows", "visualstudio", ".cargo", ".rustup", "toolchains", "registry", "cache", "native-target", "tiny-source", "synthetic-git", "logs", "opaque"} for part in value["components"])))
            and ((value["role"] == "tools-home" and value["subcomponent"] != "not-tools-home" and value["compatibility_name"] != "not-tools-home")
                 or (value["role"] != "tools-home" and value["subcomponent"] == value["compatibility_name"] == "not-tools-home")))


def _cleanup_refusal_diagnostic(error):
    os_errors = {OSError, PermissionError, FileNotFoundError, FileExistsError,
                 NotADirectoryError, IsADirectoryError, InterruptedError, TimeoutError,
                 BlockingIOError, ChildProcessError, ConnectionError, BrokenPipeError,
                 ConnectionAbortedError, ConnectionRefusedError, ConnectionResetError,
                 ProcessLookupError}
    category = "os_error" if type(error) in os_errors else (
        "guard" if type(error) is Failure else "value_error" if type(error) is ValueError else "other")
    values = {"errno": None, "winerror": None}
    if category == "os_error":
        for name in values:
            value = getattr(error, name, None)
            values[name] = value if type(value) is int and 0 <= value <= 65535 else None
    facts = {"category": category, **values}
    if type(error) is Failure:
        predicate = getattr(error, "cleanup_predicate", None)
        if type(predicate) is str and predicate in _CLEANUP_PREDICATES:
            facts["predicate"] = predicate
            if predicate == "inventory_reparse":
                observed = getattr(error, "cleanup_reparse", None)
                if _valid_cleanup_reparse(observed):
                    facts["reparse"] = observed
    return facts


def gate_failure_diagnostic(commands, stage, cleanup_phase, scanned, cleaned, cleanup_refusal=None):
    """Own fixed observations only; not a log scan or transaction verdict."""
    if scanned is True and cleaned is True:
        return None
    command = getattr(commands, "command_phase", None)
    close = getattr(commands, "close_state", None)
    scan = getattr(commands, "scan_phase", None)
    counts = getattr(commands, "scan_counts", None)
    refusal = getattr(commands, "close_refusal", None)
    if (type(stage) is not str or stage not in _FAILURE_STAGES | {"complete"}
            or type(cleanup_phase) is not str or cleanup_phase not in {
                "scan", "credentials", "job_gate", "registry", "scan_gate", "workspace", "complete"}
            or type(command) is not str or command not in {
                "unavailable", "job_create", "start", "wait_log_bounds", "process_wait",
                "exit_status", "job_query", "drain_log_bounds", "drain_deadline",
                "drained_log_bounds", "result_log_bounds", "result_log_read", "complete"}
            or type(close) is not str or close not in {"unattempted", "attempted", "complete", "failed"}
            or type(scan) is not str or scan not in {
                "unattempted", "configuration", "enumeration", "owned_log_read", "scanner", "result", "complete"}
            or (refusal is not None and (type(refusal) is not dict or set(refusal) != {"code", "operation"}
                or type(refusal["code"]) is not str or refusal["code"] not in capture.FAILURES
                or type(refusal["operation"]) is not str or refusal["operation"] not in capture.OPERATIONS))
            or (cleanup_refusal is not None and (type(cleanup_refusal) is not dict
                or set(cleanup_refusal) not in ({"category", "errno", "winerror"}, {"category", "errno", "winerror", "predicate"}, {"category", "errno", "winerror", "predicate", "reparse"})
                or ("reparse" in cleanup_refusal and (cleanup_refusal.get("predicate") != "inventory_reparse"
                    or not _valid_cleanup_reparse(cleanup_refusal["reparse"])))
                or ("predicate" in cleanup_refusal and (cleanup_refusal.get("category") != "guard"
                    or type(cleanup_refusal["predicate"]) is not str or cleanup_refusal["predicate"] not in _CLEANUP_PREDICATES))
                or type(cleanup_refusal["category"]) is not str
                or cleanup_refusal["category"] not in {"os_error", "guard", "value_error", "other"}
                or any(v is not None and (type(v) is not int or not 0 <= v <= 65535)
                       for v in (cleanup_refusal["errno"], cleanup_refusal["winerror"]))))
            or type(counts) is not dict or set(counts) != {"success", "exit_one", "other_exit", "timeout", "refusal"}
            or any(type(value) is not int or not 0 <= value <= 128 for value in counts.values())
            or sum(counts.values()) > 128 or type(scanned) is not bool or type(cleaned) is not bool
            or type(getattr(commands, "cleanup_failed", None)) is not bool):
        return None
    return {"schema": 1, "check": "windows_update_gate_diagnostic", "stage": stage,
            "command_phase": command, "job_close": close, "job_close_refusal": None if refusal is None else dict(refusal),
            "scan_phase": scan,
            "scan_counts": dict(counts), "cleanup_phase": cleanup_phase,
            "cleanup_refusal": None if cleanup_refusal is None else dict(cleanup_refusal),
            "job_cleanup_failed": commands.cleanup_failed,
            "private_log_scan_passed": scanned, "cleanup_passed": cleaned,
            "passed": False, "distribution_ready": False}

def failure_diagnostic(commands, stage, site, scanned, cleaned):
    if (scanned is not True or cleaned is not True
            or getattr(commands, "cleanup_failed", None) is not False
            or type(stage) is not str or stage not in _FAILURE_STAGES):
        return None
    substage = getattr(commands, "build_substage", None)
    if stage not in {"build-old", "build-new"} or type(substage) is not str or substage not in _BUILD_SUBSTAGES:
        substage = "unavailable"
    if site is not None and (type(site) is not dict or set(site) != {"module", "line"}
            or type(site["module"]) is not str
            or site["module"] not in {"notice_source", "windows_security", "native_notices",
                                       "source_catalogue", "windows_processes"}
            or type(site["line"]) is not int or not 0 < site["line"] <= 99999):
        return None
    return {"schema": 1, "check": "windows_update_failure_diagnostic", "stage": stage,
            "build_substage": substage, "site": site,
            "private_log_scan_passed": True, "cleanup_passed": True,
            "passed": False, "distribution_ready": False}


def cargo_failure_diagnostic(commands, stage, scanned):
    """Fixed build hints, captured after full scan and emitted after cleanup."""
    try:
        if (scanned is not True or getattr(commands, "cleanup_failed", None) is not False
                or stage not in {"build-old", "build-new"}
                or getattr(commands, "build_substage", None) != "cargo_build"):
            return None
        failure = getattr(commands, "failed_command", None)
        if type(failure) is not tuple or len(failure) != 3:
            return None
        path, status, size = failure
        if (type(status) is not int or status == 0 or not 0 <= status <= 0xffffffff
                or type(size) is not int or not 0 <= size <= 8 * 1024 * 1024):
            return None
        path = Path(path)
        if (commands.logs != commands.root / "logs" or path.parent != commands.logs
                or re.fullmatch(r"[0-9]{4}\.log", path.name) is None):
            return None
        notice_source.checked(path, private=True)
        data = notices.read_regular(path, 8 * 1024 * 1024, allow_empty=True)
        if len(data) != size:
            return None
        codes = set()
        for match in re.finditer(rb"\berror\[(E[0-9]{4})\]|\b(?:fatal error|error) (C[0-9]{4}|LNK[0-9]{4})\b", data):
            codes.add((match[1] or match[2]).decode("ascii"))
            if len(codes) > 16:
                return None
        classes = notice_source._cargo_stderr_classes(data)
        lower = data.lower()
        extra = set()
        if any(token in lower for token in (b"native sdk download", b"native sdk archive",
                                           b"no reviewed native sdk", b"cannot publish verified native sdk")):
            extra.add("native_sdk")
        if any(token in lower for token in (b"linking with", b"link.exe failed", b"linker `link.exe` not found")):
            extra.add("linker")
        if any(token in lower for token in (b"could not execute process", b"toolchain is not installed")):
            extra.add("toolchain")
        if extra:
            classes = sorted(set(classes) - {"other"} | extra)
        return {"schema": 1, "check": "windows_update_cargo_failure", "status": status,
                "error_codes": sorted(codes), "stderr_classes": classes,
                "private_log_scan_passed": True, "passed": False, "distribution_ready": False}
    except BaseException:
        # A missing/changed/unsafe log is not a replacement failure or raw text.
        return None


def export_source(commands, source, archive, version, label):
    require(label in {"old", "new"})
    members = revision.source_export_members(archive)
    names = {}
    for member in members:
        notices.portable_source_name(str(PurePosixPath(member.name)), names, windows=True)
    security = notice_source.S.windows_security()
    destination = security.private_directory(commands.root / (label+"-source"))
    with tarfile.open(fileobj=io.BytesIO(archive),mode="r:") as tar:
        for member in members:
            name = str(PurePosixPath(member.name)); path = destination.joinpath(*PurePosixPath(name).parts)
            if member.isdir():
                security.private_directory(path, parents=True, exist_ok=True)
                continue
            security.private_directory(path.parent, parents=True, exist_ok=True)
            with tar.extractfile(member) as stream:
                data = stream.read(member.size + 1)
            require(len(data) == member.size)
            write_new(path, revision.versioned_source_bytes(name, data, version))
            security.path(path, private=True)
    proof = revision.verify_versioned_export(destination,archive,version)
    return destination,proof

def instrumented_entry(data, nonce):
    """Inspect only PE bytes; never probe an unknown CLI by launching it.

    The nonce section is emitted inside main's compile-only FIRST dispatch
    statement. Omitted build/main instrumentation has no such section. The
    ordinary shipping five-field identity and format remain unchanged.
    """
    require(type(nonce) is str and re.fullmatch(r"[0-9a-f]{32}",nonce))
    identity = producer.pe_identity(data)  # Full existing native section/table bounds.
    expected = ENTRY_MAGIC + nonce.encode("ascii")
    pe, = struct.unpack_from("<I",data,60)
    count, = struct.unpack_from("<H",data,pe+6)
    optional, = struct.unpack_from("<H",data,pe+20)
    table = pe+24+optional
    found = 0
    for index in range(count):
        offset = table+index*40
        if data[offset:offset+8] != b".mrphqa\0":
            continue
        used, = struct.unpack_from("<I",data,offset+8)
        size,start = struct.unpack_from("<II",data,offset+16)
        flags, = struct.unpack_from("<I",data,offset+36)
        require(used==len(expected) and used<=size
                and flags & 0xE2000040 == 0x40000040
                and data[start:start+used]==expected and not any(data[start+used:start+size]))
        found += 1
    require(found==1)
    return identity


def build(commands, source, archive, version, nonce, label, cargo, notice_bundle, notice_receipt, reviewed):
    commands.build_substage = "source_export"
    exported,proof = export_source(commands,source,archive,version,label)
    notice_dir=notice_source.S.windows_security().private_directory(commands.root/(label+"-notices"))
    commands.build_substage = "notices_prepare"
    notice_contents=notices.prepare(notice_bundle,notice_receipt,notice_dir,revision=reviewed,
                                    platform="win32",version=version,working=exported)
    target = commands.root/"native-target"
    commands.build_substage = "config"
    environment = msvc_build_environment(commands.environment)
    environment.update(MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_BUILD=nonce, MORPHEUS_PACKAGE_FORMAT="nsis-payload-v2")
    commands.build_substage = "cargo_build"
    commands.run([cargo,"build","--release","--locked","-p","morpheus-app","--features","native-calls",
        "--target-dir",target],environment=environment,cwd=exported,seconds=3600)
    commands.build_substage = "payload"
    require(revision.verify_versioned_export(exported,archive,version) == proof)
    binary = exported/"target/release/morpheus-app.exe"
    binary.parent.mkdir(parents=True); shutil.copyfile(target/"release/morpheus-app.exe",binary)
    identity = instrumented_entry(read(binary,LIMIT),nonce); require(identity["version"] == version)
    output = commands.root/(label+"-package"); output.mkdir()
    commands.build_substage = "config"
    config = package.package_config(exported,"win32")
    config.update(version=version,**{"out-dir":str(output)})
    stage = commands.root/(label+"-producer"); stage.mkdir()
    commands.build_substage = "producer"
    prepared,payload = producer.prepare(config,exported,stage,notice_contents=notice_contents)
    commands.build_substage = "cargo_packager"
    commands.run([cargo,"packager","--release","--formats","nsis","--config",json.dumps(prepared)],
        environment=environment,cwd=exported,seconds=900)
    commands.build_substage = "payload"
    setups = list(output.glob("*.exe")); require(len(setups)==1)
    final = output/payload.name; shutil.copyfile(payload,final)
    return {"setup":checked(setups[0]),"payload":checked(final),"identity":identity,
        "executable_sha256":digest(binary),"setup_sha256":digest(setups[0]),"payload_sha256":digest(final),"source":proof}

def absent_registry():
    import winreg as W
    for hive in (W.HKEY_CURRENT_USER,W.HKEY_LOCAL_MACHINE):
        for view in (W.KEY_WOW64_64KEY,W.KEY_WOW64_32KEY):
            for name in (KEY,MANUFACTURER):
                try:
                    handle = W.OpenKey(hive,name,0,W.KEY_READ|view)
                except FileNotFoundError:
                    continue
                else:
                    W.CloseKey(handle)
                    raise Failure()

def registration(install, versions):
    import winreg as W
    with W.OpenKey(W.HKEY_CURRENT_USER,KEY,0,W.KEY_READ|W.KEY_WOW64_64KEY) as handle:
        values = {}
        count = W.QueryInfoKey(handle)[1]
        require(count == 9)
        for index in range(count):
            name,value,kind = W.EnumValue(handle,index)
            require(name not in values); values[name]=(value,kind)
        expected={"DisplayName":"Morpheus","Publisher":"Morpheus",
            "InstallLocation":'"'+str(install)+'"',"UninstallString":'"'+str(install/"uninstall.exe")+'"',
            "DisplayIcon":'"'+str(install/"morpheus-app.exe")+'"'}
        require(set(values)==set(expected)|{"DisplayVersion","NoModify","NoRepair","EstimatedSize"})
        require(all(values[k]==(v,W.REG_SZ) for k,v in expected.items()))
        require(values["DisplayVersion"][0] in versions and values["DisplayVersion"][1]==W.REG_SZ)
        require(values["NoModify"]==(1,W.REG_DWORD) and values["NoRepair"]==(1,W.REG_DWORD)
                and values["EstimatedSize"][1]==W.REG_DWORD and type(values["EstimatedSize"][0]) is int)
    with W.OpenKey(W.HKEY_CURRENT_USER,MANUFACTURER,0,W.KEY_READ|W.KEY_WOW64_64KEY) as handle:
        require(W.QueryValueEx(handle,"") == (str(install),W.REG_SZ))
    return values

def fact(case, name, phase, identity, nonce):
    value=json.loads(read(case/name,4096))
    require(value=={"schema":1,"nonce":nonce,"case":case.name,"phase":phase,
        "version":identity["version"],"cohort":identity["persistence"]})

def await_file(path, job, seconds=15):
    end=time.monotonic()+seconds
    while not path.exists():
        require(job.active()>0 and time.monotonic()<end)
        time.sleep(.025)
    checked(path)

def actor_verbatim_drive_path(value):
    """Encode only an already-resolved ordinary drive path for the Rust plan.

    Filesystem guards continue to use ordinary paths. The actor compares plan
    fields to GetFinalPathNameByHandleW's verbatim drive representation; no UNC,
    device path or arbitrary prefix is stripped or accepted here.
    """
    require(type(value) is str and 3 < len(value) <= 30000
            and re.fullmatch(r"[A-Za-z]:\\[^\x00]*", value))
    parts = value[3:].split("\\")
    require(all(part not in {"", ".", ".."} and not part.endswith((" ", "."))
                and not any(character in part for character in '<>:"/|?*\0') for part in parts))
    return "\\\\?\\" + value


def actor_plan_windows_paths(plan):
    # Keep the caller's ordinary paths for cleanup and process arguments. Only
    # these three hashed JSON fields are canonicalized for the Windows actor.
    result = dict(plan)
    for name, directory in (("root", True), ("install", True), ("payload", False)):
        require(type(plan.get(name)) is str)
        path = checked(Path(plan[name]), directory=directory)
        result[name] = actor_verbatim_drive_path(str(path.resolve(strict=True)))
    return result


def actor_environment(case, plan, base):
    raw=json.dumps(plan,sort_keys=True,separators=(",",":")).encode()+b"\n"
    write_new(case/"plan.json",raw)
    return private_environment(base,case)|{PLAN:str(case/"plan.json"),PLAN_HASH:hashlib.sha256(raw).hexdigest()}

class ManufacturerFixture:
    """Retained handles to exact synthetic keys on the already admitted runner.

    These four uninstall cases use the same old setup. Foreign named/default
    values and a subkey must survive the real uninstaller; only our exact nonce
    values are later removed, and only count-proven empty fixture keys deleted.
    """
    MODES = {"invalid-signature":"named-values", "truncated":"foreign-default", "discard":"subkey",
             "call-exclusion":"vendor-value"}
    def __init__(self, install, case, nonce):
        import winreg as W
        self.W, self.mode = W, self.MODES[case]
        self.name, self.value = "Morpheus-update-qa-"+nonce, "synthetic-preserved-"+nonce
        self.product, self.vendor, self.child = None, None, None
        try:
            access = W.KEY_READ|W.KEY_SET_VALUE|W.KEY_CREATE_SUB_KEY|W.KEY_WOW64_64KEY
            self.product = W.OpenKey(W.HKEY_CURRENT_USER,MANUFACTURER,0,access)
            self.vendor = W.OpenKey(W.HKEY_CURRENT_USER,r"Software\Morpheus",0,access)
            require(W.QueryInfoKey(self.product)[:2]==(0,1)
                    and W.QueryValueEx(self.product,"")==(str(install),W.REG_SZ)
                    and W.QueryInfoKey(self.vendor)[:2]==(1,0)
                    and W.EnumKey(self.vendor,0)=="Morpheus")
            if self.mode=="vendor-value":
                W.SetValueEx(self.vendor,self.name,0,W.REG_SZ,self.value)
                # Let the genuine uninstaller fully remove the empty product
                # child; the retained vendor key then has a value, no subkeys.
                W.CloseKey(self.product); self.product=None
            elif self.mode=="named-values":
                W.SetValueEx(self.product,self.name,0,W.REG_SZ,self.value)
                W.SetValueEx(self.vendor,self.name,0,W.REG_SZ,self.value)
            elif self.mode=="foreign-default":
                W.SetValueEx(self.product,"",0,W.REG_SZ,self.value)
            else:
                try:
                    existing = W.OpenKey(self.product,self.name,0,W.KEY_READ)
                except FileNotFoundError:
                    pass
                else:
                    W.CloseKey(existing); raise Failure()
                self.child = W.CreateKeyEx(self.product,self.name,0,W.KEY_READ|W.KEY_SET_VALUE)
                require(W.QueryInfoKey(self.child)[:2]==(0,0))
                W.SetValueEx(self.child,self.name,0,W.REG_SZ,self.value)
        except BaseException:
            self.close(); raise
    def exact_value(self, handle, name):
        require(self.W.QueryValueEx(handle,name)==(self.value,self.W.REG_SZ))
    def finish(self):
        W = self.W
        require(W.QueryInfoKey(self.vendor)[:2]==(0 if self.mode=="vendor-value" else 1,
                                                1 if self.mode in {"named-values","vendor-value"} else 0))
        if self.mode=="vendor-value":
            try:
                unexpected = W.OpenKey(W.HKEY_CURRENT_USER,MANUFACTURER,0,W.KEY_READ|W.KEY_WOW64_64KEY)
            except FileNotFoundError:
                pass
            else:
                W.CloseKey(unexpected); raise Failure()
            self.exact_value(self.vendor,self.name)
            W.DeleteValue(self.vendor,self.name)
            require(W.QueryInfoKey(self.vendor)[:2]==(0,0))
            W.CloseKey(self.vendor); self.vendor=None
            W.DeleteKeyEx(W.HKEY_CURRENT_USER,r"Software\Morpheus",W.KEY_WOW64_64KEY,0)
            return
        if self.mode=="foreign-default":
            require(W.QueryInfoKey(self.product)[:2]==(0,1))
            self.exact_value(self.product,"")
            W.DeleteValue(self.product,"")
        else:
            try:
                W.QueryValueEx(self.product,"")
            except FileNotFoundError:
                pass
            else:
                raise Failure()
            if self.mode=="named-values":
                require(W.QueryInfoKey(self.product)[:2]==(0,1))
                self.exact_value(self.product,self.name); self.exact_value(self.vendor,self.name)
                W.DeleteValue(self.product,self.name); W.DeleteValue(self.vendor,self.name)
            else:
                require(W.QueryInfoKey(self.product)[:2]==(1,0) and W.QueryInfoKey(self.child)[:2]==(0,1))
                self.exact_value(self.child,self.name)
                W.DeleteValue(self.child,self.name)
                require(W.QueryInfoKey(self.child)[:2]==(0,0))
                W.CloseKey(self.child); self.child=None
                W.DeleteKeyEx(self.product,self.name,W.KEY_WOW64_64KEY,0)
        require(W.QueryInfoKey(self.product)[:2]==(0,0))
        W.CloseKey(self.product); self.product=None
        W.DeleteKeyEx(W.HKEY_CURRENT_USER,MANUFACTURER,W.KEY_WOW64_64KEY,0)
        require(W.QueryInfoKey(self.vendor)[:2]==(0,0))
        W.CloseKey(self.vendor); self.vendor=None
        W.DeleteKeyEx(W.HKEY_CURRENT_USER,r"Software\Morpheus",W.KEY_WOW64_64KEY,0)
    def close(self):
        failed = False
        for name in ("child","product","vendor"):
            handle = getattr(self,name,None)
            if handle is not None:
                try: self.W.CloseKey(handle)
                except OSError: failed=True
                setattr(self,name,None)
        require(not failed)


def uninstall(commands, case, uninstaller_hash, versions, environment, preservation=None):
    install=case/"installed"
    if not install.exists():
        absent_registry(); return
    registration(install,versions)
    require(digest(install/"uninstall.exe",16*1024*1024)==uninstaller_hash)
    state=case/"installed.morpheus-update"
    require(not (state/"journal.json").exists())
    fixture = ManufacturerFixture(install,case.name,preservation) if preservation is not None else None
    try:
        commands.run([install/"uninstall.exe","/S"],environment=environment,cwd=case,seconds=90)
        require(not install.exists() and not state.exists())
        if fixture is not None: fixture.finish()
        absent_registry()
    finally:
        if fixture is not None: fixture.close()

def cleanup_case(commands, case, old, new, uninstaller_hash, environment):
    """Settle only our authenticated transaction, then use the real uninstaller.

    A single copied-recovery startup may exit and relaunch the restored image;
    the separate terminal probe establishes cleanup before uninstall. Unknown
    identity/registry/journal state stays private and failed on the fresh runner.
    """
    require(not commands.cleanup_failed)
    install = case / "installed"
    if not install.exists():
        absent_registry()
        return
    require(digest(install / "morpheus-app.exe") in {old["executable_sha256"],new["executable_sha256"]})
    registration(install, {old["identity"]["version"],new["identity"]["version"]})
    require(digest(install / "uninstall.exe",16*1024*1024) == uninstaller_hash)
    if (case / "installed.morpheus-update/journal.json").exists():
        commands.run([install / "morpheus-app.exe","--morpheus-windows-qa-recover"],
                     environment=environment,cwd=case,seconds=90)
    commands.run([install / "morpheus-app.exe","--morpheus-windows-qa-cleanup"],
                 environment=environment,cwd=case,seconds=45)
    require(not (case / "installed.morpheus-update/journal.json").exists())
    uninstall(commands,case,uninstaller_hash,{old["identity"]["version"],new["identity"]["version"]},environment)


def run_case(commands, case_name, old, new, nonce, signatures):
    absent_registry()
    case=commands.root/case_name; case.mkdir()
    install=case/"installed"
    base=private_environment(commands.environment,case)
    environment, uninstall_hash = None, None
    completed = False
    try:
        commands.run([old["setup"],"/S","/NS","/D="+str(install)],environment=base,cwd=case,seconds=90)
        require(digest(install/"morpheus-app.exe")==old["executable_sha256"])
        registration(install,{old["identity"]["version"]})
        uninstall_hash=digest(install/"uninstall.exe",16*1024*1024)
        plan={"schema":1,"nonce":nonce,"case":case_name,"root":str(case),"install":str(install),
            "payload":str(new["payload"]),"payload_sha256":new["payload_sha256"],"old":old["identity"],"new":new["identity"],
            "old_executable_sha256":old["executable_sha256"],"new_executable_sha256":new["executable_sha256"],
            "signature":signatures[1],"wrong_signature":signatures[0],"public_key":signatures[2]}
        if os.name == "nt":
            plan = actor_plan_windows_paths(plan)
        environment=actor_environment(case,plan,base)
        job=capture.WindowsJob()
        try:
            handle,_=commands.start(job,[install/"morpheus-app.exe","--morpheus-windows-qa-stage"],environment,cwd=case)
            require(commands.wait(job,handle,30)==0)
            handoff = case_name in {"failed-trial","interrupted-marker","upgrade"}
            fact(case,"stage.json","handed-off" if handoff else "unchanged",old["identity"],nonce)
            if case_name=="interrupted-marker":
                await_file(case/"checkpoint.json",job)
                fact(case,"checkpoint.json","marker-durable-before-exe",old["identity"],nonce)
                commands.close_job(job)  # Deliberate whole-owned-Job interruption, never a PID.
                job=capture.WindowsJob()
                handle,_=commands.start(job,[install/"morpheus-app.exe"],environment,cwd=case)
                require(commands.wait(job,handle,30)==0)
            elif case_name=="upgrade":
                await_file(case/"trial.json",job)
                fact(case,"trial.json","committed-before-store",new["identity"],nonce)
                write_new(case/"release-trial",nonce.encode())
            commands.empty(job,90)
        finally:
            commands.close_job(job)
        if case_name=="failed-trial":
            fact(case,"trial.json","rejected-before-store",new["identity"],nonce)
        if case_name in {"failed-trial","interrupted-marker"}:
            fact(case,"restart.json","restarted",old["identity"],nonce)
        commands.run([install/"morpheus-app.exe","--morpheus-windows-qa-probe"],environment=environment,cwd=case,seconds=45)
        expected=new if case_name=="upgrade" else old
        fact(case,"probe.json","terminal-clean",expected["identity"],nonce)
        require(digest(install/"morpheus-app.exe")==expected["executable_sha256"])
        registration(install,{expected["identity"]["version"]})
        preservation = nonce if case_name in ManufacturerFixture.MODES else None
        uninstall(commands,case,uninstall_hash,{old["identity"]["version"],new["identity"]["version"]},environment,preservation)
        completed = True
        return {"case":case_name,"passed":True,"installed_executable_sha256":expected["executable_sha256"],
                "uninstaller_sha256":uninstall_hash,"registry_contract_verified":True,
                "manufacturer_preservation":ManufacturerFixture.MODES.get(case_name,"empty-owned-key-cleanup")}
    finally:
        if not completed and install.exists():
            require(environment is not None and uninstall_hash is not None)
            cleanup_case(commands,case,old,new,uninstall_hash,environment)

def remove_owned_root(root, identity, workspace, workspace_identity):
    info=root.lstat(); _cleanup_require((info.st_dev,info.st_ino)==identity, "root_identity")
    count=0
    for directory, dirs, files in os.walk(root,followlinks=False):
        for name in dirs+files:
            path=Path(directory)/name; info=path.lstat(); count+=1
            _cleanup_require(count<=300000, "inventory_limit")
            _cleanup_require(not stat.S_ISLNK(info.st_mode), "inventory_symlink")
            try:
                _cleanup_require(not getattr(info,"st_file_attributes",0)&0x400, "inventory_reparse")
            except Failure as error:
                try:
                    error.cleanup_reparse = _cleanup_reparse_metadata(root, path, info)
                except BaseException:
                    pass  # Optional fixed observations cannot replace the refusal.
                raise
    if os.name == "nt":
        def readonly(function, path, error):
            # Only the known readonly-unlink failure can authorize a retry.
            if ((function is not os.unlink and function is not os.remove)
                    or not isinstance(error, PermissionError) or error.errno != 13
                    or getattr(error, "winerror", None) != 5):
                raise error
            _cleanup_require(root.parent == workspace
                    and re.fullmatch(r"morpheus-windows-updateqa-[0-9a-f]{32}", root.name)
                    and Path(path).is_absolute() and Path(path) != root
                    and Path(path).is_relative_to(root), "callback_scope")
            notice_source.S.windows_security().path(root, directory=True, private=True)
            current = root.lstat()
            _cleanup_require((current.st_dev, current.st_ino) == identity, "callback_root_identity")
            # Reuse the exact protected-leaf handle/ACL/link/identity checks.
            # The outer native-source root retains its original inode authority
            # even after rmtree has consumed files in the inner nonce tree.
            notice_source._remove_readonly_owned_leaf(
                workspace, workspace_identity, function, path, error)
        shutil.rmtree(root, onexc=readonly)
    else:
        shutil.rmtree(root)
    _cleanup_require(not root.exists(), "inner_removed")

def remove_unused_workspace(workspace, identity):
    """Before Commands exists, remove only this captured empty private tree.

    There are no child processes or command logs yet. Any unexpected file,
    alias, changed identity or ACL retains the failed tree instead of repair.
    """
    require(workspace.parent==Path.home().resolve(strict=True) and re.fullmatch(r"morpheus-native-source-[0-9a-f]{32}",workspace.name))
    security = notice_source.S.windows_security()
    security.path(workspace, directory=True, private=True)
    info = workspace.lstat(); require((info.st_dev,info.st_ino)==identity)
    directories=[]
    for directory, dirs, files in os.walk(workspace,followlinks=False):
        require(not files and len(directories)<32)
        current=security.path(Path(directory),directory=True,private=True)
        directories.append(current)
        for name in dirs:security.path(current/name,directory=True,private=True)
    for path in reversed(directories):path.rmdir()
    require(not workspace.exists())


def reserve_workspace(nonce):
    require(type(nonce) is str and re.fullmatch(r"[0-9a-f]{32}",nonce))
    security = notice_source.S.windows_security()
    parent = security.path(Path.home().resolve(strict=True),directory=True)
    workspace = security.create_root(parent)
    info=workspace.lstat(); workspace_identity=(info.st_dev,info.st_ino)
    try:
        root=security.private_directory(workspace/("morpheus-windows-updateqa-"+nonce))
        info=root.lstat(); identity=(info.st_dev,info.st_ino)
        return workspace,workspace_identity,root,identity
    except BaseException:
        try:remove_unused_workspace(workspace,workspace_identity)
        except BaseException:pass  # Retain unexpected private evidence; preserve the original refusal.
        raise


def remove_owned_workspace(workspace, workspace_identity, root, identity):
    _cleanup_require(root.parent==workspace and re.fullmatch(r"morpheus-windows-updateqa-[0-9a-f]{32}",root.name), "workspace_scope")
    _cleanup_require(workspace.parent==Path.home().resolve(strict=True) and re.fullmatch(r"morpheus-native-source-[0-9a-f]{32}",workspace.name), "workspace_scope")
    security=notice_source.S.windows_security()
    security.path(workspace,directory=True,private=True)
    info=workspace.lstat();_cleanup_require((info.st_dev,info.st_ino)==workspace_identity, "workspace_identity")
    security.path(root,directory=True,private=True)
    remove_owned_root(root,identity,workspace,workspace_identity)
    security.path(workspace,directory=True,private=True)
    info=workspace.lstat();_cleanup_require((info.st_dev,info.st_ino)==workspace_identity, "workspace_identity")
    _cleanup_require(not any(workspace.iterdir()), "workspace_empty")
    workspace.rmdir();_cleanup_require(not workspace.exists(), "workspace_removed")


def execute(args):
    guard(os.environ)
    require(args.execute_on_free_hosted_runner)
    source=checked(args.source,directory=True)
    reviewed=revision.verify_checkout(source,revision.expected_revision(os.environ))
    scanner=checked(args.gitleaks)
    require(type(args.gitleaks_sha256) is str and re.fullmatch(r"[0-9a-f]{64}",args.gitleaks_sha256)
            and digest(scanner,32*1024*1024)==args.gitleaks_sha256)
    nonce=secrets.token_hex(16)
    workspace,workspace_identity,root,identity=reserve_workspace(nonce)
    try:
        environment=private_environment(os.environ,root/"tools-home")
        # The Rust actor keeps its exact RUNNER_TEMP/nonce-child/case contract;
        # only this synthetic child's temporary parent becomes privately owned.
        environment["RUNNER_TEMP"]=str(workspace)
        commands=Commands(root,environment)
    except BaseException:
        try:remove_unused_workspace(workspace,workspace_identity)
        except BaseException:pass  # No child exists; retain any unexpected private tree.
        raise
    stage,results,scan,clean="source-export",[],False,False
    failed, failure_site = False, None
    cargo_failure = None
    cleanup_refusal = None
    keys = {}
    try:
        # Use the already provisioned exact toolchain binary, not a rustup
        # proxy that could select or auto-install an ambient toolchain.
        cargo=checked(Path(commands.environment["RUSTUP_HOME"]) /
                      "toolchains/1.99.0-x86_64-pc-windows-msvc/bin/cargo.exe")
        # Use the same parameter-free protected provider Git as source notices.
        # PATH Git can be another hardlinked launcher; private inputs still
        # require checked()'s original single-link contract.
        git=notice_source.system_git()
        require(commands.run([cargo,"packager","--version"]).strip()==b"cargo-packager 0.11.8")
        archive_path=root/"source.tar"
        commands.run([git,"archive","--format=tar","--output="+str(archive_path),reviewed],cwd=source)
        archive=read(archive_path,revision.MAX_EXPORT_BYTES)
        config=tomllib.loads((source/"crates/morpheus-app/Cargo.toml").read_text())
        version=config["package"]["version"].split("-",1)[0]
        stage="build-old"; old=build(commands,source,archive,version+"-updateqa.1",nonce,"old",cargo,args.native_source_bundle,args.native_source_receipt,reviewed)
        stage="build-new"; new=build(commands,source,archive,version+"-updateqa.2",nonce,"new",cargo,args.native_source_bundle,args.native_source_receipt,reviewed)
        require({k:v for k,v in old["identity"].items() if k!="version"}
                =={k:v for k,v in new["identity"].items() if k!="version"})
        stage="signing"; key=root/"fixture.key"
        commands.run([cargo,"packager","signer","generate","--ci","--path",key],
            environment={"CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD":""})
        for path in (key,key.with_name(key.name+".pub")):
            checked(path).chmod(0o600)
            info = path.stat()
            keys[path] = ((info.st_dev,info.st_ino),digest(path,4096))
        signatures=[]
        for item in (old,new):
            commands.run([cargo,"packager","signer","sign",item["payload"]],environment={
                "CARGO_PACKAGER_SIGN_PRIVATE_KEY":str(key),"CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD":""})
            signatures.append(read(item["payload"].with_name(item["payload"].name+".sig"),4096).decode().strip())
        signatures.append(read(key.with_name(key.name+".pub"),4096).decode().strip())
        for case in CASES:
            stage=case; results.append(run_case(commands,case,old,new,nonce,signatures))
        # Reinstall/uninstall is exercised by every following genuine NSIS case.
        stage="complete"
    except BaseException as error:
        failed = True
        failure_site = notice_source._notice_failure_site(error)
        raise
    finally:
        cleanup_phase = "scan"
        try:
            try:
                commands.scan(scanner); scan=True
                if failed:
                    cargo_failure = cargo_failure_diagnostic(commands, stage, scan)
            finally:
                cleanup_phase = "credentials"
                # Generated fixture credentials are removed even if a transaction
                # remains pending; never remove a changed/foreign file or registry.
                key_failure = False
                for path, (identity_key, expected_hash) in keys.items():
                    try:
                        info = checked(path).stat()
                        require((info.st_dev,info.st_ino)==identity_key and digest(path,4096)==expected_hash)
                        path.unlink()
                    except (Failure,OSError):
                        key_failure = True
                require(not key_failure)
                # A pending/foreign registration retains this owned tree privately;
                # such a cleanup is a failure, never an installed acceptance pass.
                cleanup_phase = "job_gate"
                require(not commands.cleanup_failed)
                cleanup_phase = "registry"
                absent_registry()
                cleanup_phase = "scan_gate"
                # The reused readonly primitive requires a fresh complete scan.
                # Keep the original scanner exception and the owned tree when
                # that prerequisite failed; key/job/registry teardown ran above.
                if scan is True:
                    cleanup_phase = "workspace"
                    try:
                        remove_owned_workspace(workspace,workspace_identity,root,identity); clean=True
                    except BaseException as error:
                        try:
                            cleanup_refusal = _cleanup_refusal_diagnostic(error)
                        except BaseException:
                            pass  # Closed observation failure never replaces cleanup refusal.
                        raise
                    cleanup_phase = "complete"
            if failed:
                diagnostic = failure_diagnostic(commands, stage, failure_site, scan, clean)
                if diagnostic is not None:
                    print("MORPHEUS_WINDOWS_UPDATE_FAILURE_DIAGNOSTIC " + json.dumps(diagnostic, sort_keys=True),
                          file=sys.stderr, flush=True)
                    if cargo_failure is not None:
                        print("MORPHEUS_WINDOWS_UPDATE_CARGO_DIAGNOSTIC "
                              + json.dumps(cargo_failure, sort_keys=True), file=sys.stderr, flush=True)
        finally:
            # This channel contains no private content and never substitutes
            # for the original full-scan/cleanup gates on source/compiler hints.
            try:
                gate = gate_failure_diagnostic(commands, stage, cleanup_phase, scan, clean, cleanup_refusal)
                if gate is not None:
                    print("MORPHEUS_WINDOWS_UPDATE_GATE_DIAGNOSTIC " + json.dumps(gate, sort_keys=True),
                          file=sys.stderr, flush=True)
            except BaseException:
                pass  # Observation failure must not replace the original refusal.
    require(stage=="complete" and len(results)==len(CASES) and scan and clean)
    print(json.dumps({"check":"windows_installed_update_mechanism","passed":True,"source_revision":reviewed,
        "cohort":old["identity"]["persistence"],"old_source":old["source"],"new_source":new["source"],
        "old_setup_sha256":old["setup_sha256"],
        "new_setup_sha256":new["setup_sha256"],"payload_sha256":new["payload_sha256"],"cases":results,
        "cleanup_passed":True,"private_log_scan_passed":True,"production_activation":False,
        "normal_ui_exit_verified":False,"live_calls_verified":False,"http_download_verified":False,
        "distribution_ready":False},sort_keys=True),flush=True)

def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source",type=Path,required=True)
    parser.add_argument("--native-source-bundle",type=Path,required=True)
    parser.add_argument("--native-source-receipt",type=Path,required=True)
    parser.add_argument("--gitleaks",type=Path,required=True)
    parser.add_argument("--gitleaks-sha256",required=True,help="Exact independently reviewed host scanner SHA256")
    parser.add_argument("--execute-on-free-hosted-runner",action="store_true")
    execute(parser.parse_args(argv))

if __name__=="__main__":
    try:
        main()
    except BaseException:
        print('MORPHEUS_WINDOWS_UPDATE_QA {"passed":false,"diagnostics":"withheld","distribution_ready":false}',file=sys.stderr,flush=True)
        raise SystemExit(1)
