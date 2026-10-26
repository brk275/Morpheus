#!/usr/bin/env python3
"""Export exact reviewed desktop WebRTC Git source; never run DEPS or hooks.

The SHA-bound plan is an external review anchor, not a self-approval token.
Run only after independent source/tool review in a fresh standard hosted job.
CIPD/GCS artifacts, compiler/runtime tools and generated hook outputs remain
explicitly unresolved. No native build or release acceptance occurs here.
"""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import signal
import stat
import subprocess
import sys
import threading
import time
from urllib.parse import urlsplit
import warnings

NATIVE_REV = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
NATIVE_TREE = "fecdf55fdd1e9cafc5531abab7ff14e35a1159f5"
NATIVE_URL = "https://github.com/webrtc-sdk/webrtc.git"
OUTER_REV = "b52fe87d04af61df3dd07748ff2c2c0b649dca09"
OUTER_TREE = "5c857570433c574a02614b3aab9be1f34493095d"
DEPOT_URL = "https://chromium.googlesource.com/chromium/tools/depot_tools.git"
NESTED_REV = "8efa575d754b8703d99b0f827528e45aeaa167aa"
SECURITY_PLAN_SHA = "a9f2220dfb0e527690ace518911e3e92c12124ab8c37e6cb391a47c89f19c080"
SECURITY_PATCHES = [
    "4f5ba9a827220170080ae4d1183e01b437eb7014c391bda46cd258ae0c5f5169",
    "793e25908a56d8993cb082e40728ad1bd39919a0abf988b1921384d15dd601fd",
    "6f10c7bf1f0396e5a6e287343a93049d3b7e9387f1cffc9fe7f70a580d0bf508",
    "ad5f64e55a6c4187b242535f84149c46590565d0e1017f1340856a8fcbd06835",
    "3e9934378b8c164314907ba7161f9b223554201fda47dad50744a1ae15c61b1d",
    "df26ab1d8a31ca5cab1f39fda912e0c3e73a726c59cfcde4457e79415094edfd",
    "1de7bf5402089c13f93f108f3008aaa8c025e2bdbd7b2375694417c186c86a17",
    "dbefa1b81cd8b24eb0bcd41ec996e6b14265d66ffdad38cf1112e646821d4e09",
]
PLATFORM_PATCHES = {
    "add_licenses": ("3744d07bce56a3cc281f70196ad52513b6395d353a73d2de97f2de616bc27625", "src"),
    "add_deps": ("da222e44f306137ed96b626879948325dc21857dcf054482b1e3df83e59fe5f4", "src"),
    "windows_silence_warnings": ("50dc2fa0db35abe9a4dd2d6849f70d12aa45c82af3371fcf8d6771fed9046887", "src"),
    "ssl_verify_callback_with_native_handle": ("55042fe99107439e88e56cddec4f5caf8c8a92e72f2e035ac8e37d52aaecb20c", "src"),
    "external_audio_source": ("83bfe8101270cc60be25991ec07bbabf51c071f6529b50895e4b1ef07314b81f", "src"),
    "fix_license_json_parsing": ("602275f0ebe9b5122496eaf3ecf624068316c87fdbcfee9d8fa5495463ca471b", "src"),
    "fix_desktop_capture_compile": ("e768451621cffb76232bac65b926e197dc49947fd8cb31e5f976bd480a659c85", "src"),
    "disable_crel": ("9c1c89c4f488209bf3397900754a5980b23e30702df579cca02ba96c164e8d2c", "src/build"),
    "david_disable_gun_source_macro": ("a4789ab8a70cd1d17f15a486caf02736c5ea350facdd4718b7504aa3bf8f66f1", "src/third_party"),
    "disable_sme_for_libyuv": ("e19d3de930350f9257e8ebbd50b7a94677152f999ef94df70434258fcec37a2d", "src/third_party/libyuv"),
}
PLATFORM_ORDER = {
    "win": ["add_licenses", "add_deps", "windows_silence_warnings",
            "ssl_verify_callback_with_native_handle", "external_audio_source"],
    "mac": ["add_licenses", "fix_license_json_parsing", "ssl_verify_callback_with_native_handle",
            "add_deps", "external_audio_source"],
    "linux": ["add_licenses", "fix_license_json_parsing", "ssl_verify_callback_with_native_handle",
              "add_deps", "fix_desktop_capture_compile", "external_audio_source", "disable_crel",
              "david_disable_gun_source_macro", "disable_sme_for_libyuv"],
}
TARGETS = {"linux-x64": ("Linux", "X64", "linux", "x64"),
           "win-x64": ("Windows", "X64", "win", "x64"),
           "mac-x64": ("macOS", "X64", "mac", "x64"),
           "mac-arm64": ("macOS", "ARM64", "mac", "arm64")}
OID = re.compile(r"[0-9a-f]{40}\Z")
SHA = re.compile(r"[0-9a-f]{64}\Z")
MAX_PLAN = 128 * 1024 * 1024
MAX_DEPS = 4 * 1024 * 1024
MAX_PATCH = 16 * 1024 * 1024
MAX_BLOB = 1024 * 1024 * 1024
MAX_METADATA = 256 * 1024 * 1024
MAX_FILES = 1_000_000
MAX_BYTES = 256 * 1024 * 1024 * 1024
TOP_FIELDS = {"vars", "deps", "recursedeps", "hooks", "include_rules",
              "specific_include_rules", "gclient_gn_args_file", "gclient_gn_args",
              "use_relative_paths", "allowed_hosts", "skip_child_includes"}


class Refusal(Exception):
    def __init__(self, category: str, dependency: str = ""):
        self.category = category
        self.dependency = dependency


def need(value: bool, category: str = "input", dependency: str = "") -> None:
    if not value:
        raise Refusal(category, dependency)


def rel(value: str) -> str:
    need(isinstance(value, str) and 0 < len(value) <= 1024)
    need("\\" not in value and ":" not in value)
    path = PurePosixPath(value)
    need(not path.is_absolute() and path.as_posix() == value)
    for part in path.parts:
        need(part not in (".", "..") and part.casefold() != ".git"
             and not part.endswith((" ", ".")))
        need(not any(char in part for char in '<>"|?*\r\n\x00'))
        need(all(ord(char) >= 32 and ord(char) != 127 for char in part))
        need(not re.fullmatch(r"(?i)(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?", part))
    return value


def no_link(path: Path) -> os.stat_result:
    info = path.lstat()
    need(not stat.S_ISLNK(info.st_mode))
    need(not (getattr(info, "st_file_attributes", 0)
              & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0)))
    return info


def absolute(value: str) -> Path:
    need(isinstance(value, str) and 0 < len(value) <= 4096)
    need(not any(char in value for char in "\r\n\x00&|<>^"))
    path = Path(value)
    need(path.is_absolute() and not str(path).startswith("\\\\"))
    current = Path(path.anchor)
    for part in path.parts[1:]:
        need(part not in (".", ".."))
        current /= part
        no_link(current)
    return path


def read(path: Path, cap: int) -> bytes:
    info = no_link(path)
    need(stat.S_ISREG(info.st_mode) and 0 <= info.st_size <= cap)
    with path.open("rb") as stream:
        data = stream.read(cap + 1)
        opened = os.fstat(stream.fileno())
    current = no_link(path)
    identity = lambda item: (item.st_dev, item.st_ino, item.st_size, item.st_mtime_ns)
    need(len(data) <= cap and identity(info) == identity(opened) == identity(current))
    return data


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def pairs(rows: list) -> dict:
    result = {}
    for key, value in rows:
        need(key not in result)
        result[key] = value
    return result


def bound_json(path: Path, expected: str) -> tuple[dict, bytes]:
    need(isinstance(expected, str) and SHA.fullmatch(expected))
    data = read(path, MAX_PLAN)
    need(sha(data) == expected)
    result = json.loads(data, object_pairs_hook=pairs)
    need(type(result) is dict)
    return result, data


def save(path: Path, value: object) -> dict:
    data = (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()
    need(len(data) <= MAX_METADATA, "resource")
    with private_file(path) as stream:
        stream.write(data)
    return {"bytes": len(data), "sha256": sha(data)}


def source_url(value: str) -> str:
    need(isinstance(value, str) and len(value) <= 1024)
    parsed = urlsplit(value)
    need(parsed.scheme == "https" and not parsed.username and not parsed.password)
    need(parsed.port is None and not parsed.query and not parsed.fragment)
    need(parsed.hostname in {"github.com", "chromium.googlesource.com",
                             "boringssl.googlesource.com", "aomedia.googlesource.com"})
    need(parsed.netloc == parsed.hostname)
    need(re.fullmatch(r"/[A-Za-z0-9_./-]+", parsed.path))
    need(all(part not in (".", "..") for part in parsed.path.split("/")))
    return value


class Deps:
    """Restricted literal/Var/condition DATA interpreter; no eval/exec/import."""

    def __init__(self, data: bytes, context: dict, location: str):
        self.location = location
        need(len(data) <= MAX_DEPS, "deps_size", location)
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", SyntaxWarning)
                module = ast.parse(data, filename="DEPS-DATA", mode="exec")
        except (SyntaxError, ValueError, RecursionError):
            raise Refusal("deps_grammar", location) from None
        need(sum(1 for _ in ast.walk(module)) <= 200_000, "deps_size", location)
        self.nodes = {}
        for statement in module.body:
            need(isinstance(statement, ast.Assign) and len(statement.targets) == 1,
                 "deps_grammar", location)
            name = statement.targets[0]
            need(isinstance(name, ast.Name) and name.id in TOP_FIELDS
                 and name.id not in self.nodes, "deps_grammar", location)
            self.nodes[name.id] = statement.value
        self.context = context
        self.raw_vars = {}
        raw = self.nodes.get("vars", ast.Dict(keys=[], values=[]))
        need(isinstance(raw, ast.Dict), "deps_grammar", location)
        for key, value in zip(raw.keys, raw.values):
            need(isinstance(key, ast.Constant) and isinstance(key.value, str),
                 "deps_grammar", location)
            need(key.value not in self.raw_vars and key.value not in context,
                 "deps_variable", location)
            self.raw_vars[key.value] = value
        self.stack = set()
        self.values = {}
        # Validate every assigned literal, even data which will never execute.
        self.data = {name: self.literal(node) for name, node in self.nodes.items()}
        self.condition_fields(self.data)

    def condition_fields(self, value: object, depth: int = 0) -> None:
        need(depth <= 64, "deps_size", self.location)
        if type(value) is dict:
            for key, child in value.items():
                if key == "condition":
                    self.condition(child)
                else:
                    self.condition_fields(child, depth + 1)
        elif type(value) is list:
            for child in value:
                self.condition_fields(child, depth + 1)

    def variable(self, name: str) -> object:
        if name in self.context:
            return self.context[name]
        need(name in self.raw_vars and name not in self.stack,
             "deps_variable", self.location)
        if name not in self.values:
            self.stack.add(name)
            value = self.literal(self.raw_vars[name])
            need(type(value) in (str, bool, int, type(None)), "deps_grammar", self.location)
            need(type(value) is not str or len(value) <= MAX_DEPS, "deps_size", self.location)
            self.values[name] = value
            self.stack.remove(name)
        return self.values[name]

    def literal(self, node: ast.AST, depth: int = 0) -> object:
        need(depth <= 128, "deps_size", self.location)
        if isinstance(node, ast.Constant):
            need(type(node.value) in (str, bool, int, type(None)),
                 "deps_grammar", self.location)
            return node.value
        if isinstance(node, (ast.List, ast.Tuple)):
            return [self.literal(item, depth + 1) for item in node.elts]
        if isinstance(node, ast.Dict):
            result = {}
            for key, value in zip(node.keys, node.values):
                name = self.literal(key, depth + 1)
                need(type(name) is str and name not in result, "deps_grammar", self.location)
                result[name] = self.literal(value, depth + 1)
            return result
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            left = self.literal(node.left, depth + 1)
            right = self.literal(node.right, depth + 1)
            need(type(left) is type(right) and type(left) in (str, list),
                 "deps_grammar", self.location)
            need(len(left) + len(right) <= MAX_DEPS, "deps_size", self.location)
            return left + right
        if isinstance(node, ast.Call):
            need(isinstance(node.func, ast.Name) and node.func.id == "Var"
                 and len(node.args) == 1 and not node.keywords, "deps_grammar", self.location)
            name = self.literal(node.args[0], depth + 1)
            need(type(name) is str, "deps_grammar", self.location)
            return self.variable(name)
        raise Refusal("deps_grammar", self.location)

    def condition(self, value: object, active: tuple = ()) -> bool:
        need(type(value) is str and len(value) <= 4096 and len(active) <= 64,
             "deps_condition", self.location)
        try:
            expression = ast.parse(value, mode="eval").body
        except (SyntaxError, ValueError, RecursionError):
            raise Refusal("deps_condition", self.location) from None

        need(sum(1 for _ in ast.walk(expression)) <= 4096, "deps_size", self.location)

        def item(node: ast.AST, boolean: bool = False, depth: int = 0) -> object:
            need(depth <= 64, "deps_size", self.location)
            if isinstance(node, ast.Constant):
                need(type(node.value) in (str, bool, int), "deps_condition", self.location)
                result = node.value
            elif isinstance(node, ast.Name):
                need(node.id not in active, "deps_variable", self.location)
                result = self.variable(node.id)
                if boolean and type(result) is str:
                    return self.condition(result, (*active, node.id))
            elif isinstance(node, ast.BoolOp) and isinstance(node.op, (ast.And, ast.Or)):
                # Evaluate all clauses to reject unknown variables even in false branches.
                values = [item(child, True, depth + 1) for child in node.values]
                result = all(values) if isinstance(node.op, ast.And) else any(values)
            elif isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
                result = not item(node.operand, True, depth + 1)
            elif isinstance(node, ast.Compare):
                values = [item(node.left, False, depth + 1)] + [
                    item(child, False, depth + 1) for child in node.comparators]
                results = []
                for left, op, right in zip(values, node.ops, values[1:]):
                    need(type(left) is type(right), "deps_condition", self.location)
                    if isinstance(op, ast.Eq):
                        results.append(left == right)
                    elif isinstance(op, ast.NotEq):
                        results.append(left != right)
                    else:
                        raise Refusal("deps_condition", self.location)
                result = all(results)
            else:
                raise Refusal("deps_condition", self.location)
            if boolean:
                need(type(result) is bool, "deps_condition", self.location)
            return result

        result = item(expression, True)
        need(type(result) is bool, "deps_condition", self.location)
        return result

    def dependencies(self, mount: str) -> tuple[list, list, list]:
        deps = self.data.get("deps", {})
        need(type(deps) is dict, "deps_grammar", self.location)
        use_relative = self.data.get("use_relative_paths", False)
        need(type(use_relative) is bool, "deps_grammar", self.location)
        selected, records = [], []
        for name, raw in deps.items():
            path = rel(mount + "/" + name if use_relative else name)
            need(path == "src" or path.startswith("src/"), "deps_path", path)
            if raw is None:
                records.append({"path": path, "selected": False, "kind": "disabled"})
                continue
            record = {"url": raw} if type(raw) is str else raw
            need(type(record) is dict, "deps_grammar", self.location)
            condition = record.get("condition", "True")
            enabled = self.condition(condition)
            kind = record.get("dep_type", "git")
            need(kind in ("git", "cipd", "gcs"), "deps_kind", path)
            if kind == "git":
                need(set(record) <= {"url", "condition", "dep_type"}, "deps_grammar", path)
                value = record.get("url")
                need(type(value) is str and "@" in value, "deps_revision", path)
                url, revision = value.rsplit("@", 1)
                source_url(url)
                need(OID.fullmatch(revision), "deps_revision", path)
                if enabled:
                    selected.append({"mount": path, "url": url, "revision": revision})
                records.append({"path": path, "selected": enabled, "kind": kind,
                                "url": url, "revision": revision})
            else:
                allowed = ({"dep_type", "condition", "packages"} if kind == "cipd"
                           else {"dep_type", "condition", "bucket", "objects"})
                need(set(record) <= allowed, "deps_grammar", path)
                # Do not download/resolve these artifacts or hide their outstanding closure.
                records.append({"path": path, "selected": enabled, "kind": kind,
                                "resolved": False, "declaration": record})
        recursive = self.data.get("recursedeps", [])
        need(type(recursive) is list, "deps_grammar", self.location)
        queue = []
        for entry in recursive:
            if type(entry) is str:
                path, filename = entry, "DEPS"
            else:
                need(type(entry) is list and len(entry) == 2, "deps_grammar", self.location)
                path, filename = entry
            path = rel(mount + "/" + path if use_relative else path)
            declared = [item for item in records if item["path"] == path]
            need(len(declared) == 1 and declared[0]["kind"] == "git",
                 "missing_recursive_dependency", path)
            queue.append((path, rel(filename)))
        return selected, records, queue


# One active trusted-Git command; retained on an unproved cleanup failure.
# No further command is admitted after that fatal error.
UNRETIRED_COMMAND = None
IO_CHUNK = 64 * 1024
COMMAND_CLEANUP_SECONDS = 10


def private_file(path: Path):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0), 0o600)
    try:
        if os.name != "nt":
            os.fchmod(fd, 0o600)
            need(stat.S_IMODE(os.fstat(fd).st_mode) == 0o600, "private_mode")
        return os.fdopen(fd, "wb")
    except BaseException:
        os.close(fd)
        raise


def private_directory(path: Path) -> None:
    path.mkdir(mode=0o700)
    if os.name != "nt":
        os.chmod(path, 0o700)
        need(stat.S_IMODE(no_link(path).st_mode) == 0o700, "private_mode")


def private_parents(path: Path, root: Path) -> None:
    need(path.is_relative_to(root), "source_path")
    current = root
    for part in path.relative_to(root).parts:
        current /= part
        if not current.exists():
            private_directory(current)
        info = no_link(current)
        need(stat.S_ISDIR(info.st_mode), "source_path")
        if os.name != "nt":
            need(stat.S_IMODE(info.st_mode) == 0o700, "private_mode")


class PosixGitProcess:
    """Fresh session for reviewed Git/its ordinary inherited-group helpers.

    This is not a hostile-executable/setsid-escape sandbox. WNOWAIT retains the
    actual leader PID until the last group signal; no post-reap PGID operation.
    """

    def __init__(self):
        need(all(hasattr(os, name) for name in ("waitid", "P_PID", "WNOWAIT", "WEXITED")), "process_api")
        need(signal.getsignal(signal.SIGCHLD) == signal.SIG_DFL, "process_api")
        self.process = None
        self.cancelled = False
        self.result = None
        self.start_returned = False

    def start(self, argv, cwd, environment, incoming, output, error):
        # Publish the actual Popen owner before its initializer can create a child.
        self.process = subprocess.Popen.__new__(subprocess.Popen)
        self.process.__init__(argv, cwd=cwd, env=environment, stdin=incoming,
                              stdout=output, stderr=error, shell=False,
                              close_fds=True, start_new_session=True, umask=0o077)
        self.start_returned = True

    def observe(self):
        if self.process is None:
            return 0
        if self.result is not None:
            return self.result
        value = os.waitid(os.P_PID, self.process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        if value is None or value.si_pid == 0:
            return None
        need(value.si_pid == self.process.pid, "process_wait")
        self.result = value.si_status if value.si_code == os.CLD_EXITED else -value.si_status
        return self.result

    def cancel(self):
        if self.process is not None and not self.cancelled:
            # A failed/interrupted initializer may already have reaped its child.
            # Never signal that uncertain numeric identity; retain/fail closed.
            need(self.start_returned, "process_cleanup")
            need(getattr(self.process, "pid", None) is not None, "process_cleanup")
            # The original child is still live or unreaped: its PID cannot be
            # recycled to an unrelated process group before this only signal.
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.cancelled = True

    def join(self, deadline):
        if self.process is None:
            return 0
        need(self.cancelled, "process_cleanup")
        # No poll()/wait() occurred before cancel(). Never signal this PGID again.
        result = self.process.wait(timeout=max(0.001, deadline - time.monotonic()))
        need(self.start_returned, "process_cleanup")
        return result

    def close(self):
        pass


class WindowsGitProcess:
    """Create suspended in an unnamed no-breakaway Job, then resume.

    Only stdin/stdout/stderr handles are inherited. A Job assignment is part of
    CreateProcess, so failure never executes an unassigned Git process.
    """

    def __init__(self):
        import ctypes
        import msvcrt
        self.c, self.msvcrt = ctypes, msvcrt
        C = ctypes
        P, D, S, B = C.c_void_p, C.c_uint32, C.c_size_t, C.c_int32
        class BasicLimits(C.Structure):
            _fields_ = [("process_time", C.c_int64), ("job_time", C.c_int64),
                        ("flags", D), ("min_working_set", S), ("max_working_set", S),
                        ("active_limit", D), ("affinity", S), ("priority", D), ("scheduling", D)]
        class ExtendedLimits(C.Structure):
            _fields_ = [("basic", BasicLimits), ("io", C.c_uint64 * 6),
                        ("process_memory", S), ("job_memory", S), ("peak_process", S), ("peak_job", S)]
        class Accounting(C.Structure):
            _fields_ = [("times", C.c_int64 * 4), ("page_faults", D), ("total", D),
                        ("active", D), ("terminated", D)]
        class Startup(C.Structure):
            _fields_ = [("cb", D), ("reserved", C.c_wchar_p), ("desktop", C.c_wchar_p),
                        ("title", C.c_wchar_p), ("x", D), ("y", D), ("x_size", D), ("y_size", D),
                        ("x_chars", D), ("y_chars", D), ("fill", D), ("flags", D),
                        ("show", C.c_uint16), ("reserved_count", C.c_uint16), ("reserved_bytes", P),
                        ("incoming", P), ("output", P), ("error", P)]
        class StartupEx(C.Structure):
            _fields_ = [("startup", Startup), ("attributes", P)]
        class ProcessInfo(C.Structure):
            _fields_ = [("process", P), ("thread", P), ("pid", D), ("tid", D)]
        need(os.name == "nt" and C.sizeof(P) == 8 and C.sizeof(ExtendedLimits) == 144
             and C.sizeof(Accounting) == 48 and C.sizeof(StartupEx) == 112
             and C.sizeof(ProcessInfo) == 24, "process_api")
        self.Accounting, self.StartupEx, self.ProcessInfo = Accounting, StartupEx, ProcessInfo
        self.api = C.WinDLL(str(absolute(os.environ["SystemRoot"]) / "System32/kernel32.dll"), use_last_error=True)
        signatures = [
            ("GetCurrentProcess", [], P), ("CloseHandle", [P], B),
            ("DuplicateHandle", [P, P, P, C.POINTER(P), D, B, D], B),
            ("CreateJobObjectW", [P, C.c_wchar_p], P),
            ("SetInformationJobObject", [P, C.c_int, P, D], B),
            ("QueryInformationJobObject", [P, C.c_int, P, D, C.POINTER(D)], B),
            ("TerminateJobObject", [P, D], B), ("IsProcessInJob", [P, P, C.POINTER(B)], B),
            ("InitializeProcThreadAttributeList", [P, D, D, C.POINTER(S)], B),
            ("UpdateProcThreadAttribute", [P, D, S, P, S, P, P], B),
            ("DeleteProcThreadAttributeList", [P], None),
            ("CreateProcessW", [C.c_wchar_p, C.c_wchar_p, P, P, B, D, P, C.c_wchar_p, P, P], B),
            ("ResumeThread", [P], D), ("WaitForSingleObject", [P, D], D),
            ("GetExitCodeProcess", [P, C.POINTER(D)], B),
        ]
        for name, arguments, result in signatures:
            function = getattr(self.api, name)
            function.argtypes, function.restype = arguments, result
        self.info = ProcessInfo()
        self.job = None
        self.parent_duplicates = []
        self.cancelled = False
        job = self.api.CreateJobObjectW(None, None)
        need(job, "process_job")
        self.job = job
        limits = ExtendedLimits()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE; no breakaway flags.
        try:
            need(self.api.SetInformationJobObject(job, 9, C.byref(limits), C.sizeof(limits)), "process_job")
        except BaseException:
            self.close()
            raise

    def start(self, argv, cwd, environment, incoming, output, error):
        C, api = self.c, self.api
        P, D = C.c_void_p, C.c_uint32
        size = C.c_size_t()
        first = api.InitializeProcThreadAttributeList(None, 2, 0, C.byref(size))
        need(not first and C.get_last_error() == 122 and 0 < size.value <= 1024 * 1024, "process_attributes")
        buffer = C.create_string_buffer(size.value)
        attributes = C.cast(buffer, P)
        initialized = False
        try:
            need(api.InitializeProcThreadAttributeList(attributes, 2, 0, C.byref(size)), "process_attributes")
            initialized = True
            current = api.GetCurrentProcess()
            for fd in (incoming.fileno(), output, error):
                duplicate = P()
                need(api.DuplicateHandle(current, self.msvcrt.get_osfhandle(fd), current,
                                         C.byref(duplicate), 0, 1, 2), "process_handles")
                need(duplicate.value is not None, "process_handles")
                self.parent_duplicates.append(duplicate.value)
            handles = (P * 3)(*self.parent_duplicates)
            jobs = (P * 1)(self.job)
            need(api.UpdateProcThreadAttribute(attributes, 0, 0x20002, handles, C.sizeof(handles), None, None), "process_attributes")
            need(api.UpdateProcThreadAttribute(attributes, 0, 0x2000D, jobs, C.sizeof(jobs), None, None), "process_attributes")
            startup = self.StartupEx()
            startup.startup.cb = C.sizeof(startup)
            startup.startup.flags = 0x100  # STARTF_USESTDHANDLES
            startup.startup.incoming, startup.startup.output, startup.startup.error = self.parent_duplicates
            startup.attributes = attributes
            command = subprocess.list2cmdline(argv)
            need(len(command) < 32767 and "\0" not in command, "process_arguments")
            block = "\0".join(key + "=" + value for key, value in sorted(environment.items(), key=lambda item: item[0].upper())) + "\0\0"
            need(len(block) < 32767, "process_arguments")
            command_buffer, environment_buffer = C.create_unicode_buffer(command), C.create_unicode_buffer(block)
            # CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT.
            # Native output handles are written directly into the retained owner.
            need(api.CreateProcessW(argv[0], command_buffer, None, None, 1, 0x80404,
                                    C.cast(environment_buffer, P), str(cwd), C.byref(startup), C.byref(self.info)), "process_start")
            assigned = C.c_int32()
            need(api.IsProcessInJob(self.info.process, self.job, C.byref(assigned)) and assigned.value, "process_job")
            need(api.ResumeThread(self.info.thread) == 1, "process_resume")
        finally:
            if initialized:
                api.DeleteProcThreadAttributeList(attributes)
            self.close_duplicates()

    def close_duplicates(self):
        while self.parent_duplicates:
            need(self.api.CloseHandle(self.parent_duplicates[-1]), "process_cleanup")
            self.parent_duplicates.pop()

    def observe(self):
        if not self.info.process:
            return 0
        value = self.api.WaitForSingleObject(self.info.process, 0)
        if value == 258:  # WAIT_TIMEOUT
            return None
        need(value == 0, "process_wait")
        code = self.c.c_uint32()
        need(self.api.GetExitCodeProcess(self.info.process, self.c.byref(code)), "process_wait")
        return code.value

    def active(self):
        value, length = self.Accounting(), self.c.c_uint32()
        need(self.api.QueryInformationJobObject(self.job, 1, self.c.byref(value), self.c.sizeof(value),
                                                self.c.byref(length)) and length.value == self.c.sizeof(value), "process_cleanup")
        return value.active

    def cancel(self):
        if self.job and not self.cancelled:
            need(self.api.TerminateJobObject(self.job, 1), "process_cleanup")
            self.cancelled = True
        self.close_duplicates()

    def join(self, deadline):
        need(self.cancelled, "process_cleanup")
        while True:
            code = self.observe()
            if code is not None and self.active() == 0:
                return code
            need(time.monotonic() < deadline, "process_cleanup")
            time.sleep(0.01)

    def close(self):
        self.close_duplicates()
        for container, name in ((self.info, "thread"), (self.info, "process"), (self, "job")):
            handle = getattr(container, name)
            if handle:
                need(self.api.CloseHandle(handle), "process_cleanup")
                setattr(container, name, None)


def run_git_owned(argv, cwd, environment, timeout, output_path, error_path, stdout_limit, incoming_path):
    """Private binary pipes with pre-write individual and combined budgets."""
    global UNRETIRED_COMMAND
    need(UNRETIRED_COMMAND is None, "process_cleanup")
    owner = None
    threads, streams, pipes = [], [], []
    state = {"failure": None, "counts": [0, 0]}
    lock, stop = threading.Lock(), threading.Event()
    limits, combined = [stdout_limit, MAX_METADATA], max(stdout_limit, MAX_METADATA)
    code = None
    thread_start_unproved = False

    def refuse(category):
        with lock:
            if state["failure"] is None:
                state["failure"] = category
        stop.set()

    def copy_stream(fd, target, index):
        try:
            while not stop.is_set():
                block = os.read(fd, IO_CHUNK)
                if not block:
                    break
                with lock:
                    if (state["counts"][index] + len(block) > limits[index]
                            or sum(state["counts"]) + len(block) > combined):
                        state["failure"] = "command_output"
                        stop.set()
                        break
                    state["counts"][index] += len(block)
                # Reserve the bounded bytes under the lock, then release it
                # before disk I/O so timeout/cancel is never blocked by this lock.
                view = memoryview(block)
                while view:
                    written = target.write(view)
                    need(type(written) is int and 0 < written <= len(view), "command_io")
                    view = view[written:]
        except BaseException:
            refuse("command_io")
        finally:
            try:
                os.close(fd)
            except BaseException:
                refuse("command_io")

    try:
        deadline = time.monotonic() + timeout
        for path in (output_path, error_path):
            streams.append(private_file(path))
        incoming = incoming_path.open("rb") if incoming_path is not None else open(os.devnull, "rb")
        streams.append(incoming)
        for _ in range(2):
            pipes.append(list(os.pipe()))
        owner = WindowsGitProcess() if os.name == "nt" else PosixGitProcess()
        UNRETIRED_COMMAND = (owner, threads, streams, pipes)
        try:
            owner.start(argv, cwd, environment, incoming, pipes[0][1], pipes[1][1])
        finally:
            for pair in pipes:
                os.close(pair[1])
                pair[1] = None
        for index in range(2):
            thread = threading.Thread(target=copy_stream, args=(pipes[index][0], streams[index], index), daemon=False)
            threads.append(thread)
            started = False
            try:
                thread.start()
                started = True
            finally:
                if not started:
                    thread_start_unproved = True
        while not stop.is_set():
            code = owner.observe()
            if code is not None:
                break
            if time.monotonic() >= deadline:
                refuse("command_timeout")
                break
            stop.wait(0.01)
    finally:
        # Stop the original group/Job BEFORE reaping/releasing original authority.
        # Failure to prove cleanup is fatal and retains this one owner/resources.
        cleanup_deadline = time.monotonic() + COMMAND_CLEANUP_SECONDS
        cleanup_ok = False
        try:
            if owner is not None:
                owner.cancel()
                actual_code = owner.join(cleanup_deadline)
                if code is None:
                    code = actual_code
            need(not thread_start_unproved, "process_cleanup")
            for index, pair in enumerate(pipes):
                if pair[1] is not None:
                    os.close(pair[1])
                    pair[1] = None
                if index >= len(threads) or threads[index].ident is None:
                    os.close(pair[0])
            for thread in threads:
                if thread.ident is not None:
                    thread.join(max(0, cleanup_deadline - time.monotonic()))
                    need(not thread.is_alive(), "process_cleanup")
            if owner is not None:
                owner.close()
            for stream in streams:
                stream.close()
            UNRETIRED_COMMAND = None
            cleanup_ok = True
        finally:
            need(cleanup_ok, "process_cleanup")
    need(state["failure"] is None, state["failure"] or "command_io")
    need(code == 0, "git_command")
    return {"stdout_bytes": state["counts"][0], "stderr_bytes": state["counts"][1],
            "stdout_limit_bytes": limits[0], "stderr_limit_bytes": limits[1],
            "combined_limit_bytes": combined, "original_joined": True,
            "process_contract": "windows_job" if os.name == "nt" else "trusted_git_fresh_process_group"}


def gitlink_policy(rows: list, specs: dict) -> dict:
    # Only exact externally reviewed original gitlinks may remain uninitialized.
    # They are directories, never blobs, submodule checkouts or source-completeness proof.
    need(type(rows) is list and len(rows) <= 1024, "gitlink_plan")
    need(len({name.casefold() for name in specs}) == len(specs), "gitlink_graft")
    entries = {}
    for row in rows:
        need(type(row) is dict and set(row) == {"mount", "path", "mode", "type", "oid", "action"},
             "gitlink_plan")
        mount, path = rel(row["mount"]), rel(row["path"])
        name = rel(mount + "/" + path)
        need(mount in specs and len(PurePosixPath(name).parts) <= 64, "gitlink_plan", name)
        need(row["mode"] == "160000" and row["type"] == "commit"
             and type(row["oid"]) is str and OID.fullmatch(row["oid"])
             and row["action"] == "uninitialized-directory", "gitlink_plan", name)
        need(name.casefold() not in {key.casefold() for key in entries}, "gitlink_collision", name)
        entries[name] = dict(row)
    roots = {name.casefold() for name in entries}
    prefixes = set(roots)
    for name, row in entries.items():
        parents = {str(path) for path in PurePosixPath(name.casefold()).parents if str(path) != "."}
        need(not (parents & roots), "gitlink_collision", name)
        prefixes.update(parents)
        owner = row["mount"].casefold()
        for mount in specs:
            other = mount.casefold()
            # The owning repository and its pre-existing ancestor grafts are
            # legitimate. No other selected graft may claim the marker's scope.
            if other == owner or owner.startswith(other + "/"):
                continue
            marker = name.casefold()
            need(not (other == marker or other.startswith(marker + "/")
                      or marker.startswith(other + "/")), "gitlink_graft", name)
    return {"entries": entries, "roots": roots, "prefixes": prefixes}


def outside_gitlinks(name: str, policy: dict) -> None:
    path = PurePosixPath(name.casefold())
    need(str(path) not in policy["prefixes"]
         and not any(str(parent) in policy["roots"] for parent in path.parents),
         "gitlink_file_conflict", name)


def verify_gitlinks(checkout: Path, entries: dict) -> None:
    for name in entries:
        path = absolute(str(checkout / name))
        info = no_link(path)
        need(stat.S_ISDIR(info.st_mode), "gitlink_materialization", name)
        if os.name != "nt":
            need(stat.S_IMODE(info.st_mode) == 0o700, "private_mode", name)
        with os.scandir(path) as listing:
            need(next(listing, None) is None, "gitlink_not_empty", name)


class Git:
    def __init__(self, executable: Path, root: Path, environment: dict, timeout: int):
        self.executable, self.root, self.environment, self.timeout = executable, root, environment, timeout
        self.counter = 0
        self.commands = []
        self.options = ["-c", "core.hooksPath=" + str(root / "empty-hooks"),
                        "-c", "core.fsmonitor=false", "-c", "core.attributesFile=" + str(root / "empty"),
                        "-c", "core.autocrlf=false", "-c", "gc.auto=0", "-c", "maintenance.auto=false",
                        "-c", "protocol.allow=never", "-c", "protocol.https.allow=always",
                        "-c", "protocol.file.allow=never", "-c", "protocol.ext.allow=never",
                        "-c", "http.followRedirects=false", "-c", "http.sslVerify=true",
                        "-c", "credential.helper=", "-c", "credential.interactive=false",
                        "-c", "submodule.recurse=false", "-c", "fetch.recurseSubmodules=false",
                        "-c", "transfer.fsckObjects=true", "-c", "fetch.fsckObjects=true"]

    def argv(self, args: list[str]) -> list[str]:
        return [str(self.executable), "--no-pager", *self.options, *args]

    def command_file(self, args: list[str], cap: int, cwd: Path | None = None,
                     input_bytes: bytes | None = None) -> Path:
        self.counter += 1
        output = self.root / "private-logs" / f"{self.counter:04d}.stdout"
        error = self.root / "private-logs" / f"{self.counter:04d}.stderr"
        self.commands.append({"argv": self.argv(args), "cwd": str(cwd or self.root),
                              "stdin_sha256": sha(input_bytes) if input_bytes is not None else None,
                              "stdin_bytes": len(input_bytes) if input_bytes is not None else 0,
                              "timeout_seconds": self.timeout,
                              "stdout": output.relative_to(self.root).as_posix(),
                              "stderr": error.relative_to(self.root).as_posix()})
        incoming = None
        if input_bytes is not None:
            need(len(input_bytes) <= MAX_METADATA, "resource")
            incoming = self.root / "private-logs" / f"{self.counter:04d}.stdin"
            with private_file(incoming) as stream:
                stream.write(input_bytes)
        result = run_git_owned(self.argv(args), cwd or self.root, self.environment, self.timeout,
                               output, error, cap, incoming)
        self.commands[-1]["execution"] = result
        return output

    def command(self, args: list[str], cap: int = MAX_METADATA, cwd: Path | None = None) -> bytes:
        return read(self.command_file(args, cap, cwd), cap)

    def repository(self, spec: dict, number: int, checkout: Path, policy: dict) -> tuple[dict, dict, list]:
        store = self.root / "objects" / f"{number:03d}.git"
        self.command(["init", "--bare", "--template=" + str(self.root / "empty-template"), str(store)])
        prefix = ["--git-dir=" + str(store)]
        self.command([*prefix, "fetch", "--no-tags", "--no-recurse-submodules",
                      "--no-write-fetch-head", "--depth=1", spec["url"], spec["revision"]])
        revision = spec["revision"]
        need(self.command([*prefix, "cat-file", "-t", revision], 128) == b"commit\n", "git_identity")
        tree = self.command([*prefix, "rev-parse", "--verify", revision + "^{tree}"], 128).decode().strip()
        need(tree == spec["tree"], "git_identity", spec["mount"])
        self.command([*prefix, "fsck", "--full", "--strict", "--no-reflogs"])
        listing = self.command([*prefix, "ls-tree", "-rz", "--full-tree", revision])
        rows, links = [], []
        for line in listing.split(b"\0"):
            if not line:
                continue
            metadata, name = line.split(b"\t", 1)
            mode, kind, oid = metadata.decode("ascii").split()
            path = rel(name.decode("utf-8"))
            full_path = rel(spec["mount"] + "/" + path)
            need(OID.fullmatch(oid), "git_identity")
            if mode == "160000" and kind == "commit":
                declaration = {"mount": spec["mount"], "path": path, "mode": mode,
                               "type": kind, "oid": oid, "action": "uninitialized-directory"}
                need(policy["entries"].get(full_path) == declaration, "undeclared_gitlink", full_path)
                links.append({**declaration, "repository_revision": revision,
                              "repository_tree": tree, "exported_path": full_path,
                              "materialized_type": "directory", "materialized_empty": True,
                              "source_present": False, "submodule_initialized": False})
                continue
            need(kind == "blob" and mode in ("100644", "100755"), "unsupported_tree_mode", full_path)
            outside_gitlinks(full_path, policy)
            rows.append({"path": path, "mode": mode, "blob": oid})
        need(0 < len(rows) and len(rows) + len(links) <= MAX_FILES, "resource")
        names = [row["path"].casefold() for row in [*rows, *links]]
        need(len(set(names)) == len(names), "path_collision", spec["mount"])
        need({row["exported_path"] for row in links}
             == {name for name, row in policy["entries"].items() if row["mount"] == spec["mount"]},
             "unmatched_gitlink_plan", spec["mount"])
        files = {}
        # One original bounded command completes before any batch decoding. No
        # pipe read can wait indefinitely before the command's timeout applies.
        queries = b"".join((row["blob"] + "\n").encode("ascii") for row in rows)
        batch = self.command_file([*prefix, "cat-file", "--batch"],
                                  MAX_BYTES + len(rows) * 256, input_bytes=queries)
        total = 0
        with batch.open("rb") as source:
            for row in rows:
                header = source.readline(160)
                parts = header.decode("ascii").rstrip("\n").split()
                need(len(parts) == 3 and parts[:2] == [row["blob"], "blob"], "git_blob")
                size = int(parts[2])
                total += size
                need(0 <= size <= MAX_BLOB and total <= MAX_BYTES, "resource")
                name = spec["mount"] + "/" + row["path"]
                destination = checkout / name
                private_parents(destination.parent, checkout)
                absolute(str(destination.parent))
                need(not destination.exists(), "overlapping_repository_file", name)
                git_hash = hashlib.sha1(b"blob " + str(size).encode() + b"\0")
                value = hashlib.sha256()
                remaining = size
                with destination.open("xb") as target:
                    while remaining:
                        block = source.read(min(remaining, 1024 * 1024))
                        need(block, "git_blob")
                        remaining -= len(block)
                        git_hash.update(block)
                        value.update(block)
                        target.write(block)
                need(source.read(1) == b"\n" and git_hash.hexdigest() == row["blob"], "git_blob")
                os.chmod(destination, int(row["mode"], 8) & 0o777)
                files[name] = {**row, "bytes": size, "sha256": value.hexdigest()}
            need(source.read(1) == b"", "git_blob")
        need(sum(row["bytes"] for row in files.values()) <= MAX_BYTES, "resource")
        own_entries = {}
        for row in links:
            name = row["exported_path"]
            destination = checkout / name
            private_parents(destination.parent, checkout)
            absolute(str(destination.parent))
            need(not destination.exists(), "gitlink_materialization", name)
            private_directory(destination)
            own_entries[name] = policy["entries"][name]
        verify_gitlinks(checkout, own_entries)
        return {**spec, "object_store": store.relative_to(self.root).as_posix(),
                "files": list(files.values()), "gitlinks": links}, files, links


def snapshot(root: Path, expected: dict, policy: dict) -> dict:
    verify_gitlinks(root, policy["entries"])
    actual = {}
    entries = 0
    for directory, subdirs, names in os.walk(root, followlinks=False):
        for name in [*subdirs, *names]:
            entries += 1
            need(entries <= MAX_FILES, "resource")
            info = no_link(Path(directory) / name)
            if os.name != "nt" and stat.S_ISDIR(info.st_mode):
                need(stat.S_IMODE(info.st_mode) == 0o700, "private_mode")
        for name in names:
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            need(relative in expected, "unbound_change", relative)
            data = read(path, MAX_BLOB)
            fact = expected[relative]
            mode = fact["mode"]
            if os.name != "nt":
                need(stat.S_IMODE(path.stat().st_mode) == (int(mode, 8) & 0o777), "unbound_change", relative)
            actual[relative] = {**fact, "bytes": len(data), "sha256": sha(data)}
    need(actual == expected, "unbound_change")
    verify_gitlinks(root, policy["entries"])
    return actual


def patch_paths(data: bytes) -> list[str]:
    need(len(data) <= MAX_PATCH and b"\x00" not in data, "patch_format")
    # These two exact reviewed backports are canonical plain unified diffs.
    # Their digests bind every byte and these independently reviewed paths.
    # Keep the patch artifacts unchanged; do not admit arbitrary plain diffs.
    canonical_plain = {
        "1de7bf5402089c13f93f108f3008aaa8c025e2bdbd7b2375694417c186c86a17": (
            "api/crypto/frame_crypto_transformer.cc",
            "api/crypto/sif_payload_unittest.cc",
        ),
        "dbefa1b81cd8b24eb0bcd41ec996e6b14265d66ffdad38cf1112e646821d4e09": (
            "api/crypto/frame_crypto_transformer.cc",
            "api/crypto/sif_payload_unittest.cc",
        ),
    }
    known_paths = canonical_plain.get(sha(data))
    if known_paths is not None:
        return list(known_paths)
    paths = []
    old = None
    new = None
    hunk = False
    for line in data.decode("utf-8").splitlines():
        if line.startswith("diff --git "):
            if old is not None:
                need(new is not None and hunk, "patch_format")
            match = re.fullmatch(r"diff --git a/([^ ]+) b/([^ ]+)", line)
            need(match and match[1] == match[2], "patch_format")
            path = rel(match[1])
            need(path not in paths, "patch_format")
            paths.append(path)
            old, new, hunk = path, None, False
        elif line.startswith("--- "):
            need(old is not None and line[4:] in ("a/" + old, "/dev/null"), "patch_format")
        elif line.startswith("+++ "):
            need(old is not None and line[4:] == "b/" + old, "patch_format")
            new = old
        elif line.startswith("@@ "):
            need(new is not None, "patch_format")
            hunk = True
        elif line.startswith("new file mode "):
            need(line in ("new file mode 100644", "new file mode 100755"), "patch_format")
        elif line.startswith(("GIT binary patch", "Binary files ", "rename ", "copy ",
                              "deleted file mode", "old mode", "new mode")):
            raise Refusal("patch_format")
    need(old is not None and new is not None and hunk, "patch_format")
    return paths


def validate_patch_set(manifest: dict, source_root: Path, output_root: Path, host_os: str) -> None:
    need(set(manifest) == {"schema", "patches"} and manifest["schema"] == "morpheus-native-source-patches-v1")
    patches = manifest["patches"]
    need(type(patches) is list and 8 <= len(patches) <= 32)
    need(all(type(row) is dict for row in patches), "patch_set")
    native = [row.get("sha256") for row in patches if row.get("kind") == "native-security"]
    need(native == SECURITY_PATCHES, "patch_set")
    platform = [(row.get("sha256"), row.get("root")) for row in patches
                if row.get("kind") == "upstream-platform"]
    need(platform == [PLATFORM_PATCHES[name] for name in PLATFORM_ORDER[host_os]], "patch_set")
    names = set()
    for row in patches:
        need(set(row) == {"name", "kind", "file", "bytes", "sha256", "root", "changes"})
        need(row["kind"] in ("native-security", "upstream-platform"), "patch_set")
        need(type(row["name"]) is str and len(row["name"]) <= 128 and row["name"] not in names)
        names.add(rel(row["name"]))
        need(type(row["sha256"]) is str and SHA.fullmatch(row["sha256"])
             and type(row["bytes"]) is int and 0 < row["bytes"] <= MAX_PATCH, "patch_identity")
        need(row["kind"] != "native-security" or row["root"] == "src", "patch_set")
        scope = rel(row["root"])
        need(scope == "src" or scope.startswith("src/"), "patch_path")
        path = absolute(row["file"])
        need(path.is_relative_to(source_root) and not path.is_relative_to(output_root), "patch_path")
        data = read(path, MAX_PATCH)
        need(len(data) == row["bytes"] and sha(data) == row["sha256"], "patch_identity")
        need(type(row["changes"]) is list and 0 < len(row["changes"]) <= 128, "patch_format")
        need(sorted(patch_paths(data)) == sorted(rel(item["path"]) for item in row["changes"]), "patch_path")


def apply_patches(git: Git, checkout: Path, manifest: dict, expected: dict,
                  source_root: Path, policy: dict) -> tuple[list, dict]:
    patches = manifest["patches"]
    receipts = []
    for number, row in enumerate(patches):
        need(set(row) == {"name", "kind", "file", "bytes", "sha256", "root", "changes"})
        need(row["kind"] in ("native-security", "upstream-platform"), "patch_set")
        need(row["kind"] != "native-security" or row["root"] == "src", "patch_set")
        scope = rel(row["root"])
        need(scope == "src" or scope.startswith("src/"), "patch_path")
        path = absolute(row["file"])
        need(path.is_relative_to(source_root) and not path.is_relative_to(git.root), "patch_path")
        data = read(path, MAX_PATCH)
        need(len(data) == row["bytes"] and sha(data) == row["sha256"], "patch_identity")
        declared = row["changes"]
        need(type(declared) is list and 0 < len(declared) <= 128, "patch_format")
        names = patch_paths(data)
        need(sorted(names) == sorted(item["path"] for item in declared), "patch_path")
        updates = {}
        for change in declared:
            need(set(change) == {"path", "before", "after"}, "patch_format")
            name = scope + "/" + rel(change["path"])
            outside_gitlinks(name, policy)
            before, after = change["before"], change["after"]
            need(after is not None and set(after) == {"sha256", "bytes", "mode"}, "patch_format")
            need(SHA.fullmatch(after["sha256"]) and type(after["bytes"]) is int
                 and 0 <= after["bytes"] <= MAX_BLOB and after["mode"] in ("100644", "100755"), "patch_format")
            if before is None:
                need(name not in expected and not (checkout / name).exists(), "patch_preimage", name)
            else:
                need(type(before) is dict and set(before) == {"sha256", "bytes", "mode"}
                     and name in expected, "patch_preimage", name)
                need(type(before["sha256"]) is str and SHA.fullmatch(before["sha256"])
                     and type(before["bytes"]) is int and 0 <= before["bytes"] <= MAX_BLOB
                     and before["mode"] in ("100644", "100755"), "patch_preimage", name)
                need(all(expected[name][key] == before[key] for key in before), "patch_preimage", name)
                need(sha(read(checkout / name, MAX_BLOB)) == before["sha256"], "patch_preimage", name)
            updates[name] = {"path": change["path"], **after}
        snapshot(checkout, expected, policy)
        # Git apply uses complete context, no fuzz/3way/reject/ignore-whitespace.
        # Exact whole preimage/postimage hashes bind any accepted context offset.
        command = ["apply", "--whitespace=nowarn", "-p1", "--directory=" + scope]
        git.command([*command, "--check", str(path)], cwd=checkout)
        git.command([*command, str(path)], cwd=checkout)
        # The child uses private umask077; restore ONLY declared approved source
        # modes before verifying their complete postimages and the whole tree.
        if os.name != "nt":
            for name, fact in updates.items():
                need(stat.S_ISREG(no_link(checkout / name).st_mode), "patch_postimage", name)
                os.chmod(checkout / name, int(fact["mode"], 8) & 0o777)
        next_expected = {**expected, **updates}
        snapshot(checkout, next_expected, policy)
        expected = next_expected
        copied = git.root / "materials" / f"patch-{number:02d}.patch"
        with private_file(copied) as stream:
            stream.write(data)
        receipts.append({"name": row["name"], "kind": row["kind"], "sha256": sha(data),
                         "bytes": len(data), "root": scope, "changes": declared,
                         "artifact": copied.relative_to(git.root).as_posix()})
    return receipts, expected


def provision(args: argparse.Namespace) -> None:
    need(os.environ.get("GITHUB_ACTIONS") == "true"
         and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted", "runner")
    runner = absolute(os.environ["RUNNER_TEMP"])
    plan_path = absolute(args.plan)
    need(plan_path.is_relative_to(runner), "runner")
    plan, plan_bytes = bound_json(plan_path, args.plan_sha256)
    need(set(plan) == {"schema", "target", "git", "repositories", "deps_files",
                       "condition_values", "patch_manifest", "security_plan", "command_timeout_seconds", "gitlinks"})
    need(plan["schema"] == "morpheus-native-webrtc-sources-v2" and plan["target"] in TARGETS)
    os_name, arch, host_os, host_cpu = TARGETS[plan["target"]]
    need(os.environ.get("RUNNER_OS") == os_name and os.environ.get("RUNNER_ARCH") == arch, "runner")
    need(type(plan["command_timeout_seconds"]) is int
         and 60 <= plan["command_timeout_seconds"] <= 1800)
    root = Path(args.output_root)
    need(root.is_absolute() and root.name not in (".", ".."))
    rel(root.name)
    parent = absolute(str(root.parent))
    need(parent.is_relative_to(runner) and root != runner and not root.exists(), "runner")
    tool = plan["git"]
    need(set(tool) == {"path", "sha256", "bytes", "version"})
    executable = absolute(tool["path"])
    data = read(executable, MAX_BLOB)
    need(sha(data) == tool["sha256"] and len(data) == tool["bytes"], "git_tool")
    need(isinstance(tool["version"], str) and len(tool["version"]) <= 128)
    repositories = plan["repositories"]
    need(type(repositories) is list and 2 <= len(repositories) <= 256)
    specs = {}
    for spec in repositories:
        need(set(spec) == {"mount", "url", "revision", "tree"})
        mount = rel(spec["mount"])
        need(mount not in specs and OID.fullmatch(spec["revision"]) and OID.fullmatch(spec["tree"]))
        source_url(spec["url"])
        need(mount in ("src", "outer-depot") or mount.startswith("src/"), "repository_path")
        specs[mount] = spec
    need(specs.get("src") == {"mount": "src", "url": NATIVE_URL,
                              "revision": NATIVE_REV, "tree": NATIVE_TREE}, "native_identity")
    need(specs.get("outer-depot") == {"mount": "outer-depot", "url": DEPOT_URL,
                                      "revision": OUTER_REV, "tree": OUTER_TREE}, "depot_identity")
    need(specs.get("src/third_party/depot_tools", {}).get("revision") == NESTED_REV, "depot_identity")
    policy = gitlink_policy(plan["gitlinks"], specs)
    deps_rows = {}
    need(type(plan["deps_files"]) is list and 0 < len(plan["deps_files"]) <= 64)
    for row in plan["deps_files"]:
        need(set(row) == {"mount", "path", "sha256", "bytes"})
        key = (rel(row["mount"]), rel(row["path"]))
        need(key not in deps_rows and SHA.fullmatch(row["sha256"]))
        deps_rows[key] = row
    custom = plan["condition_values"]
    need(type(custom) is dict and len(custom) <= 64)
    context = {"host_os": host_os, "host_cpu": host_cpu,
               "checkout_android": False, "checkout_ios": False,
               "checkout_linux": host_os == "linux", "checkout_mac": host_os == "mac",
               "checkout_win": host_os == "win"}
    for key, value in custom.items():
        need(re.fullmatch(r"[A-Za-z][A-Za-z0-9_]*", key) and key not in context)
        need(type(value) in (str, bool) and (type(value) is bool or len(value) <= 4096))
        context[key] = value
    descriptor = plan["patch_manifest"]
    need(type(descriptor) is dict and set(descriptor) == {"path", "sha256"})
    patch_path = absolute(descriptor["path"])
    need(patch_path.is_relative_to(runner), "patch_path")
    patch_manifest, patch_bytes = bound_json(patch_path, descriptor["sha256"])
    security = plan["security_plan"]
    need(type(security) is dict and set(security) == {"path", "sha256"}
         and security["sha256"] == SECURITY_PLAN_SHA, "security_plan")
    security_path = absolute(security["path"])
    need(security_path.is_relative_to(runner), "security_plan")
    security_plan, security_bytes = bound_json(security_path, SECURITY_PLAN_SHA)
    need(security_plan["schema"] == "morpheus-native-security-source-plan-v1"
         and security_plan["baseline_commit"] == NATIVE_REV
         and security_plan["baseline_tree"] == NATIVE_TREE
         and [row["sha256"] for row in security_plan["patches"]] == SECURITY_PATCHES,
         "security_plan")
    validate_patch_set(patch_manifest, runner, root, host_os)
    private_directory(root)
    for directory in ("objects", "checkout", "private-logs", "materials", "empty-hooks", "empty-template", "empty-home"):
        private_directory(root / directory)
    with private_file(root / "empty"):
        pass
    environment = {"PATH": str(executable.parent),
                   "HOME": str(root / "empty-home"), "USERPROFILE": str(root / "empty-home"),
                   "XDG_CONFIG_HOME": str(root / "empty-home"), "CURL_HOME": str(root / "empty-home"),
                   "GIT_CONFIG_GLOBAL": os.devnull,
                   "GIT_CONFIG_SYSTEM": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
                   "GIT_ATTR_NOSYSTEM": "1", "GIT_NO_REPLACE_OBJECTS": "1",
                   "GIT_NO_LAZY_FETCH": "1", "GIT_OPTIONAL_LOCKS": "0", "GIT_ALLOW_PROTOCOL": "https",
                   "GIT_PROTOCOL_FROM_USER": "0", "GIT_TERMINAL_PROMPT": "0",
                   "GCM_INTERACTIVE": "Never", "GIT_CEILING_DIRECTORIES": str(root),
                   "TEMP": str(runner), "TMP": str(runner)}
    if os.name == "nt":
        environment["SystemRoot"] = str(absolute(os.environ["SystemRoot"]))
    git = Git(executable, root, environment, plan["command_timeout_seconds"])
    need(git.command(["--version"], 1024).decode().strip() == tool["version"], "git_tool")
    checkout = root / "checkout"
    fetched, files, source_receipts, gitlink_receipts = set(), {}, [], []

    def fetch(mount: str) -> None:
        nonlocal files
        if mount in fetched:
            return
        need(mount in specs, "missing_dependency", mount)
        receipt, exported, links = git.repository(specs[mount], len(fetched), checkout, policy)
        need(not (set(exported) & set(files)), "overlapping_repository_file", mount)
        files.update(exported)
        need(len({name.casefold() for name in files}) == len(files), "path_collision", mount)
        need(len(files) <= MAX_FILES and sum(row["bytes"] for row in files.values()) <= MAX_BYTES, "resource")
        fetched.add(mount)
        source_receipts.append(receipt)
        gitlink_receipts.extend(links)

    fetch("src")
    fetch("outer-depot")
    queue = [("src", "DEPS")]
    parsed = set()
    selections, deferred = [], []
    while queue:
        mount, filename = queue.pop(0)
        key = (mount, filename)
        need(key not in parsed and key in deps_rows, "missing_deps", mount + "/" + filename)
        parsed.add(key)
        data = read(checkout / mount / filename, MAX_DEPS)
        bound = deps_rows[key]
        need(len(data) == bound["bytes"] and sha(data) == bound["sha256"], "deps_identity", mount + "/" + filename)
        parser = Deps(data, context, mount + "/" + filename)
        selected, records, recursive = parser.dependencies(mount)
        selections.append({"file": mount + "/" + filename, "sha256": sha(data), "entries": records})
        deferred.append({"file": mount + "/" + filename,
                         "hooks": parser.data.get("hooks", []),
                         "gclient_gn_args_file": parser.data.get("gclient_gn_args_file"),
                         "gclient_gn_args": parser.data.get("gclient_gn_args", []),
                         "recursive_deps": recursive,
                         "executed": False})
        active = set()
        for item in sorted(selected, key=lambda row: (row["mount"].count("/"), row["mount"])):
            need(item["mount"] in specs, "missing_dependency", item["mount"])
            planned = specs[item["mount"]]
            need(all(planned[name] == item[name] for name in item), "dependency_identity", item["mount"])
            fetch(item["mount"])
            active.add(item["mount"])
        queue.extend((path, name) for path, name in recursive if path in active)
    need(parsed == set(deps_rows) and fetched == set(specs), "plan_closure")
    snapshot(checkout, files, policy)
    patches, files = apply_patches(git, checkout, patch_manifest, files, runner, policy)
    for fact in security_plan["final_security_paths"]:
        name = "src/" + rel(fact["path"])
        need(name in files and all(files[name][key] == fact[key] for key in ("bytes", "sha256", "mode")),
             "security_postimage", name)
    security_receipt = save(root / "materials/security-source-receipt.json", {
        "source_plan_sha256": sha(security_bytes),
        "baseline_commit": NATIVE_REV, "baseline_tree": NATIVE_TREE,
        "security_only_tree": security_plan["security_only_tree"],
        "patches": [{key: row[key] for key in ("name", "order", "bytes", "sha256")}
                    for row in security_plan["patches"]],
        "final_security_paths": [{key: row[key] for key in ("path", "bytes", "sha256", "mode")}
                                 for row in security_plan["final_security_paths"]]})
    with private_file(root / "materials/plan.json") as stream:
        stream.write(plan_bytes)
    with private_file(root / "materials/patch-manifest.json") as stream:
        stream.write(patch_bytes)
    manifests = {"security": security_receipt}
    manifests["repositories"] = save(root / "materials/repositories.json", source_receipts)
    manifests["gitlinks"] = save(root / "materials/gitlinks.json", sorted(gitlink_receipts, key=lambda row: row["exported_path"]))
    manifests["commands"] = save(root / "materials/commands.json", git.commands)
    manifests["deps"] = save(root / "materials/deps-selection.json", selections)
    manifests["deferred"] = save(root / "materials/deferred-hooks.json", deferred)
    manifests["patches"] = save(root / "materials/applied-patches.json", patches)
    source_files = [{"path": name.removeprefix("src/"), "bytes": fact["bytes"], "sha256": fact["sha256"]}
                    for name, fact in sorted(files.items()) if name.startswith("src/")]
    outer_files = [{"path": name.removeprefix("outer-depot/"), "bytes": fact["bytes"], "sha256": fact["sha256"]}
                   for name, fact in sorted(files.items()) if name.startswith("outer-depot/")]
    typed_source_files = [{**row, "type": "file", "mode": files["src/" + row["path"]]["mode"]}
                          for row in source_files]
    typed_outer_files = [{**row, "type": "file", "mode": files["outer-depot/" + row["path"]]["mode"]}
                         for row in outer_files]
    manifests["typed_source_files"] = save(root / "materials/post-patch-source-inventory.json", typed_source_files)
    manifests["typed_outer_files"] = save(root / "materials/outer-depot-inventory.json", typed_outer_files)
    manifests["source_files"] = save(root / "materials/post-patch-source-files.json", source_files)
    manifests["outer_files"] = save(root / "materials/outer-depot-files.json", outer_files)
    manifests["all_files"] = save(root / "materials/all-file-provenance.json", files)
    save(root / "materials/source-materials.json", {
        "schema": "morpheus-native-webrtc-git-materials-v1", "target": plan["target"],
        "plan_sha256": args.plan_sha256, "patch_manifest_sha256": descriptor["sha256"],
        "security_plan_sha256": SECURITY_PLAN_SHA,
        "native_revision": NATIVE_REV, "native_tree": NATIVE_TREE,
        "outer_depot_revision": OUTER_REV, "nested_depot_revision": NESTED_REV,
        "git": {key: tool[key] for key in ("sha256", "bytes", "version")},
        "manifests": manifests, "selected_git_source_set_verified": True,
        "native_sources_complete": False, "compiler_cipd_runtime_closure": False,
        "hooks_executed": False, "build_ready": False, "distribution_ready": False,
        "uninitialized_gitlinks": len(gitlink_receipts), "submodule_sources_materialized": False})
    verify_gitlinks(checkout, policy["entries"])
    save(root / "source-provision-receipt.json", {
        "schema": "morpheus-native-webrtc-source-provision-v1", "plan_sha256": args.plan_sha256,
        "materials_sha256": sha(read(root / "materials/source-materials.json", MAX_METADATA)),
        "native_sources_complete": False, "build_ready": False, "distribution_ready": False,
        "uninitialized_gitlinks": len(gitlink_receipts), "submodule_sources_materialized": False})
    print('{"native_source_provision":"git_source_set_complete"}', flush=True)


ASSEMBLY_SOURCE_SHA = "5debe4d363c339de898b965dee15660867b9a0bc4a96380f434bf19deab20c7b"
ASSEMBLY_SOURCE_BINDING_SHA = "33d7a49ef10d0c3f6acc2a8466c762e8fbabf6212c1ae0038b81839ded4fcb1b"
ASSEMBLY_INSTRUMENTED_SHA = "927a95029b9ddc8f4b4cce20f07975505b343260d913b9c3488bc6f6182ad0f9"
ASSEMBLY_INSTRUMENTED_BINDING_SHA = "343c10863e454774b759b078b4e56be87774578eba68e05831560844b5e6b913"
ASSEMBLY_INSTRUMENTED_STAGER_SHA = "8903aef2de2ea5a7778ad4ba21047909ad7595d3e1359f151829e489910fc84f"
ASSEMBLY_CONSUMER_BINDING_SHA = "e1d439d60a9ab6f923a2e5f5a4bd23f411a34803617d5a49d69a0a9769008a5c"
ASSEMBLY_PREFIX = "third_party/instrumented_libs/binaries"


def assembly_layout(files: dict, directories: set) -> None:
    need(type(files) is dict and type(directories) is set and "." in directories,
         "assembly_inventory")
    need(0 < len(files) and len(files) + len(directories) <= MAX_FILES, "resource")
    need(sum(row["bytes"] for row in files.values()) <= MAX_BYTES, "resource")
    names = {}
    for name in [*files, *sorted(directories - {"."})]:
        rel(name)
        need(name.casefold() not in names, "assembly_collision")
        names[name.casefold()] = name
    for name, row in files.items():
        need(set(row) == {"path", "bytes", "sha256", "mode"} and row["path"] == name
             and type(row["bytes"]) is int and 0 <= row["bytes"] <= MAX_BYTES
             and type(row["sha256"]) is str and SHA.fullmatch(row["sha256"])
             and type(row["mode"]) is int
             and row["mode"] in (0o400, 0o444, 0o500, 0o555, 0o600, 0o644, 0o700, 0o755),
             "assembly_inventory")
    for name in names.values():
        need(all(str(parent) in directories for parent in PurePosixPath(name).parents),
             "assembly_inventory")


def assembly_tree(root: Path, files: dict, directories: set, destination: Path | None = None) -> None:
    # Only original regular files and exact private empty directories are copied.
    # Never follow links, infer a checkout, or execute any source/tool.
    assembly_layout(files, directories)
    seen_files, seen_dirs = set(), set()
    def walk_error(_error: OSError) -> None:
        # Missing topology cannot be treated as a successfully verified tree.
        raise Refusal("assembly_walk") from None
    for directory, children, names in os.walk(root, followlinks=False, onerror=walk_error):
        current = Path(directory)
        info = no_link(current)
        need(stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid()
             and stat.S_IMODE(info.st_mode) == 0o700, "assembly_private")
        relative = current.relative_to(root).as_posix()
        need(relative in directories, "assembly_unbound")
        seen_dirs.add(relative)
        for child in children:
            child_path = current / child
            child_info = no_link(child_path)
            need(child_path.relative_to(root).as_posix() in directories, "assembly_unbound")
            need(stat.S_ISDIR(child_info.st_mode) and child_info.st_uid == os.geteuid()
                 and stat.S_IMODE(child_info.st_mode) == 0o700, "assembly_private")
        for name in names:
            relative = (current / name).relative_to(root).as_posix()
            need(relative in files, "assembly_unbound")
            seen_files.add(relative)
    need(seen_files == set(files) and seen_dirs == directories, "assembly_unbound")
    if destination is not None:
        for name in sorted(directories - {"."}, key=lambda name: (name.count("/"), name)):
            private_directory(destination / name)
    identity = lambda st: (st.st_dev, st.st_ino, st.st_size, st.st_mode, st.st_uid,
                           st.st_nlink, st.st_mtime_ns, st.st_ctime_ns)
    for name, row in sorted(files.items()):
        path = root / name
        before = no_link(path)
        need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1
             and before.st_uid == os.geteuid() and before.st_size == row["bytes"]
             and stat.S_IMODE(before.st_mode) == row["mode"], "assembly_identity")
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
        output = None
        try:
            need(identity(os.fstat(fd)) == identity(before), "assembly_identity")
            if destination is not None:
                output = private_file(destination / name)
            value, count = hashlib.sha256(), 0
            while True:
                block = os.read(fd, IO_CHUNK)
                if not block:
                    break
                count += len(block)
                need(count <= row["bytes"], "assembly_identity")
                value.update(block)
                if output is not None:
                    output.write(block)
            need(count == row["bytes"] and value.hexdigest() == row["sha256"]
                 and identity(before) == identity(os.fstat(fd)) == identity(no_link(path)),
                 "assembly_identity")
            if output is not None:
                output.flush()
                os.fchmod(output.fileno(), row["mode"])
                os.fsync(output.fileno())
        finally:
            if output is not None:
                output.close()
            os.close(fd)


def assembly_instrumented(spec: dict, runner: Path) -> tuple[Path, dict, set, list, dict]:
    need(set(spec) == {"root", "plan", "materials", "core_manifest"}, "assembly_profile")
    root = absolute(spec["root"])
    need(root.parent == runner and re.fullmatch(r"morpheus-webrtc-tools-[0-9a-f]{16}", root.name),
         "assembly_profile")
    fixed = {"plan": "selected-instrumented-inputs.json", "materials": "tool-materials.json",
             "core_manifest": "selected-tool-manifest.json"}
    for key, name in fixed.items():
        need(set(spec[key]) == {"path", "sha256"} and spec[key]["path"] == name,
             "assembly_profile")
    need(spec["plan"]["sha256"] == ASSEMBLY_INSTRUMENTED_SHA, "assembly_profile")
    plan, _ = bound_json(root / fixed["plan"], ASSEMBLY_INSTRUMENTED_SHA)
    material, _ = bound_json(root / fixed["materials"], spec["materials"]["sha256"])
    core = read(root / fixed["core_manifest"], MAX_PLAN)
    need(sha(core) == spec["core_manifest"]["sha256"] == material["manifest_sha256"],
         "assembly_profile")
    need(plan["schema"] == "morpheus-native-linux-instrumented-inputs-v1"
         and plan["baseline"] == NATIVE_REV and plan["tree"] == NATIVE_TREE
         and plan["target"] == "Linuxx64"
         and plan["root_eight_security_plan_sha256"] == SECURITY_PLAN_SHA,
         "assembly_profile")
    tools = plan["materials"]
    need([tool["name"] for tool in tools] == ["msan-chained-origins-noble-lib",
                                            "msan-no-origins-noble-lib"], "assembly_profile")
    need(material["schema"] == 4 and material["status"] == "instrumented_library_data_staged"
         and material["instrumented_inputs_sha256"] == ASSEMBLY_INSTRUMENTED_SHA
         and material["target"] == "Linuxx64" and material["baseline"] == NATIVE_REV
         and material["materials"] == tools and material["executed_tools"] == []
         and material["normalization"] == "exact_gzip_library_alias_regular_copies_no_filesystem_links"
         and material["permission_model"] == "unix_private_directories_0700_files_0600_or_0700"
         and "python_inputs_sha256" not in material and "linux_inputs_sha256" not in material,
         "assembly_profile")
    need(all(material[key] is False for key in ("environment_complete", "native_sources_complete",
         "compiler_cipd_runtime_closure", "build_ready", "distribution_ready", "hooks_executed")),
         "assembly_profile")
    files, directories, overlay, archives = {}, {"."}, [], []
    def entry(name: str, size: int, digest: str, mode: int) -> None:
        rel(name)
        need(name not in files, "assembly_collision")
        files[name] = {"path": name, "bytes": size, "sha256": digest, "mode": mode}
        directories.update(str(parent) for parent in PurePosixPath(name).parents)
    for tool in tools:
        entry("archives/" + tool["name"] + ".tar.gz", tool["bytes"], tool["sha256"], 0o600)
        for name in tool["implicit_directories"]:
            directories.add("instrumented-inputs/" + rel(name))
        for member in tool["members"]:
            name = "instrumented-inputs/" + rel(member["raw_path"])
            if member["kind"] == "directory":
                directories.add(name)
                directories.update(str(parent) for parent in PurePosixPath(name).parents)
            elif member["kind"] == "regular":
                mode = 0o700 if member["mode"] & 0o111 else 0o600
                entry(name, member["bytes"], member["sha256"], mode)
                overlay.append({"path": ASSEMBLY_PREFIX + "/" + member["raw_path"], "type": "file",
                                "mode": "100755" if mode == 0o700 else "100644",
                                "bytes": member["bytes"], "sha256": member["sha256"]})
            else:
                need(member["kind"] == "symlink", "assembly_profile")
        for alias in tool["alias_copies"]:
            entry("instrumented-inputs/" + alias["destination"], alias["bytes"], alias["sha256"], alias["output_mode"])
            overlay.append({"path": ASSEMBLY_PREFIX + "/" + alias["destination"], "type": "file",
                            "mode": "100755" if alias["output_mode"] == 0o700 else "100644",
                            "bytes": alias["bytes"], "sha256": alias["sha256"]})
        archives.append({key: tool[key] for key in ("name", "sha256", "bytes", "members",
                         "alias_copies", "omitted_aliases", "omission_evidence")})
    need(material["archives"] == archives and len(overlay) == 2584, "assembly_profile")
    for name in fixed.values():
        data = read(root / name, MAX_PLAN)
        entry(name, len(data), sha(data), 0o600)
    assembly_tree(root, files, directories)
    return root, files, directories, sorted(overlay, key=lambda row: row["path"]), {
        "producer_binding_sha256": ASSEMBLY_INSTRUMENTED_BINDING_SHA,
        "producer_source_sha256": ASSEMBLY_INSTRUMENTED_STAGER_SHA,
        "plan_sha256": ASSEMBLY_INSTRUMENTED_SHA,
        "materials_sha256": spec["materials"]["sha256"],
        "core_manifest_sha256": spec["core_manifest"]["sha256"]}


def assemble_instrumented(args: argparse.Namespace) -> None:
    need(os.name == "posix" and sys.platform == "linux" and os.uname().machine == "x86_64"
         and os.environ.get("GITHUB_ACTIONS") == "true"
         and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
         and os.environ.get("RUNNER_OS") == "Linux" and os.environ.get("RUNNER_ARCH") == "X64",
         "runner")
    runner = absolute(os.environ["RUNNER_TEMP"])
    path = absolute(args.plan)
    need(path.is_relative_to(runner), "runner")
    plan, _ = bound_json(path, args.plan_sha256)
    need(set(plan) == {"schema", "purpose", "target", "recipe_sha256", "public_repository",
         "standard_hosted_runner", "repository", "run_id", "run_attempt", "job", "source", "instrumented"}
         and plan["schema"] == "morpheus-native-instrumented-assembly-plan-v1"
         and plan["purpose"] == "assemble_verified_native_instrumented_source_only"
         and plan["target"] == "linux-x64" and plan["public_repository"] is True
         and plan["standard_hosted_runner"] is True
         and plan["repository"] == os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus",
         "assembly_plan")
    for key, env in (("run_id", "GITHUB_RUN_ID"), ("run_attempt", "GITHUB_RUN_ATTEMPT"), ("job", "GITHUB_JOB")):
        need(type(plan[key]) is str and bool(plan[key]) and plan[key] == os.environ.get(env), "runner")
    need(sha(read(absolute(__file__), MAX_BLOB)) == plan["recipe_sha256"], "assembly_plan")
    source = plan["source"]
    need(set(source) == {"root", "inventory", "producer_source_sha256", "producer_binding_sha256"}
         and source["producer_source_sha256"] == ASSEMBLY_SOURCE_SHA
         and source["producer_binding_sha256"] == ASSEMBLY_SOURCE_BINDING_SHA, "assembly_plan")
    original = absolute(source["root"])
    inventory_ref = source["inventory"]
    need(set(inventory_ref) == {"path", "sha256"}, "assembly_plan")
    original_inventory = absolute(inventory_ref["path"])
    need(original.is_relative_to(runner) and original != runner
         and original_inventory.is_relative_to(runner) and not original_inventory.is_relative_to(original),
         "assembly_plan")
    inputs, _ = bound_json(original_inventory, inventory_ref["sha256"])
    need(set(inputs) == {"schema", "files", "directories"}
         and inputs["schema"] == "morpheus-native-provision-tree-inventory-v1"
         and type(inputs["files"]) is list and type(inputs["directories"]) is list,
         "assembly_inventory")
    files = {row["path"]: row for row in inputs["files"]}
    directories = set(inputs["directories"])
    need(len(files) == len(inputs["files"]) and len(directories) == len(inputs["directories"]),
         "assembly_inventory")
    assembly_layout(files, directories)
    data_root, data_files, data_dirs, overlay, material = assembly_instrumented(plan["instrumented"], runner)
    root = Path(args.output_root)
    need(root.is_absolute() and absolute(str(root.parent)) == runner
         and re.fullmatch(r"morpheus-webrtc-assembled-[0-9a-f]{16}", root.name)
         and not os.path.lexists(root), "runner")
    for left, right in ((original, data_root), (original, root), (data_root, root)):
        need(not left.is_relative_to(right) and not right.is_relative_to(left), "assembly_roots")
    assembly_tree(original, files, directories)
    def original_json(name: str) -> object:
        need(name in files, "assembly_original")
        data = read(original / name, MAX_PLAN)
        need(sha(data) == files[name]["sha256"], "assembly_original")
        return json.loads(data, object_pairs_hook=pairs)
    receipt = original_json("source-provision-receipt.json")
    materials = original_json("materials/source-materials.json")
    source_plan = original_json("materials/plan.json")
    need(receipt["schema"] == "morpheus-native-webrtc-source-provision-v1"
         and materials["schema"] == "morpheus-native-webrtc-git-materials-v1"
         and materials["target"] == source_plan["target"] == "linux-x64"
         and source_plan["schema"] == "morpheus-native-webrtc-sources-v2"
         and receipt["materials_sha256"] == files["materials/source-materials.json"]["sha256"]
         and receipt["plan_sha256"] == materials["plan_sha256"] == files["materials/plan.json"]["sha256"]
         and materials["security_plan_sha256"] == SECURITY_PLAN_SHA
         and materials["native_revision"] == NATIVE_REV and materials["native_tree"] == NATIVE_TREE
         and materials["outer_depot_revision"] == OUTER_REV and materials["nested_depot_revision"] == NESTED_REV
         and materials["selected_git_source_set_verified"] is True, "assembly_original")
    need(all(receipt[key] is False and materials[key] is False for key in
             ("native_sources_complete", "build_ready", "distribution_ready"))
         and materials["compiler_cipd_runtime_closure"] is False and materials["hooks_executed"] is False,
         "assembly_original")
    names = {"typed_source_files": "materials/post-patch-source-inventory.json",
             "typed_outer_files": "materials/outer-depot-inventory.json",
             "repositories": "materials/repositories.json", "gitlinks": "materials/gitlinks.json"}
    values = {}
    for key, name in names.items():
        fact = materials["manifests"][key]
        need(fact == {"bytes": files[name]["bytes"], "sha256": files[name]["sha256"]}, "assembly_original")
        values[key] = original_json(name)
    specs = {row["mount"]: row for row in source_plan["repositories"]}
    need(len(specs) == len(source_plan["repositories"])
         and {row["mount"] for row in values["repositories"]} == set(specs), "assembly_original")
    need(specs.get("src") == {"mount": "src", "url": NATIVE_URL, "revision": NATIVE_REV, "tree": NATIVE_TREE}
         and specs.get("outer-depot") == {"mount": "outer-depot", "url": DEPOT_URL,
                                         "revision": OUTER_REV, "tree": OUTER_TREE}
         and specs.get("src/third_party/depot_tools", {}).get("revision") == NESTED_REV,
         "assembly_original")
    policy = gitlink_policy(source_plan["gitlinks"], specs)
    links, link_paths = [], set()
    for repo in values["repositories"]:
        need(all(repo[key] == specs[repo["mount"]][key] for key in ("mount", "url", "revision", "tree")),
             "assembly_original")
        for link in repo["gitlinks"]:
            declaration = policy["entries"].get(link["exported_path"])
            need(declaration is not None and declaration["mount"] == repo["mount"]
                 and link["exported_path"] not in link_paths and link == {**declaration,
                 "repository_revision": repo["revision"], "repository_tree": repo["tree"],
                 "exported_path": declaration["mount"] + "/" + declaration["path"],
                 "materialized_type": "directory", "materialized_empty": True,
                 "source_present": False, "submodule_initialized": False}, "assembly_original")
            links.append(link)
            link_paths.add(link["exported_path"])
    need(link_paths == set(policy["entries"])
         and sorted(links, key=lambda row: row["exported_path"]) == values["gitlinks"]
         and len(links) == len(policy["entries"]) == materials["uninitialized_gitlinks"]
         == receipt["uninitialized_gitlinks"]
         and materials["submodule_sources_materialized"] is False
         and receipt["submodule_sources_materialized"] is False, "assembly_original")
    verify_gitlinks(original / "checkout", policy["entries"])
    checkout_files = {}
    for key, prefix in (("typed_source_files", "checkout/src/"), ("typed_outer_files", "checkout/outer-depot/")):
        for row in values[key]:
            name = prefix + rel(row["path"])
            need(set(row) == {"path", "type", "mode", "bytes", "sha256"} and row["type"] == "file"
                 and row["mode"] in ("100644", "100755") and name not in checkout_files
                 and files[name] == {"path": name, "bytes": row["bytes"], "sha256": row["sha256"],
                                      "mode": int(row["mode"], 8) & 0o777}, "assembly_original")
            outside_gitlinks(name.removeprefix("checkout/"), policy)
            checkout_files[name] = files[name]
    need(set(checkout_files) == {name for name in files if name.startswith("checkout/")}, "assembly_original")
    combined, combined_dirs = dict(files), set(directories)
    for row in overlay:
        name = "checkout/src/" + rel(row["path"])
        outside_gitlinks("src/" + row["path"], policy)
        need(name not in combined, "assembly_overwrite")
        combined[name] = {"path": name, "bytes": row["bytes"], "sha256": row["sha256"],
                          "mode": int(row["mode"], 8) & 0o777}
        combined_dirs.update(str(parent) for parent in PurePosixPath(name).parents)
    assembly_layout(combined, combined_dirs)
    for name in ("materials/instrumented-source-overlay.json", "materials/instrumented-assembly-receipt.json"):
        need(name not in combined and name not in combined_dirs, "assembly_overwrite")
    private_directory(root)
    private_directory(root / "provision")
    private_directory(root / "handoff")
    output = root / "provision"
    assembly_tree(original, files, directories, output)
    for name in sorted(combined_dirs - directories, key=lambda name: (name.count("/"), name)):
        private_directory(output / name)
    for row in overlay:
        raw = row["path"].removeprefix(ASSEMBLY_PREFIX + "/")
        origin = "instrumented-inputs/" + raw
        target = "checkout/src/" + row["path"]
        need(data_files[origin]["bytes"] == row["bytes"] and data_files[origin]["sha256"] == row["sha256"],
             "assembly_profile")
        # Reuse the same bounded verified regular-file copying path for one
        # approved material. Its final source mode differs only by read bits.
        stage = output / target
        fd = os.open(data_root / origin, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
        try:
            before = os.fstat(fd)
            need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and before.st_uid == os.geteuid()
                 and before.st_size == row["bytes"]
                 and stat.S_IMODE(before.st_mode) == data_files[origin]["mode"], "assembly_identity")
            value, count = hashlib.sha256(), 0
            with private_file(stage) as stream:
                while True:
                    block = os.read(fd, IO_CHUNK)
                    if not block:
                        break
                    count += len(block)
                    need(count <= row["bytes"], "assembly_identity")
                    value.update(block)
                    stream.write(block)
                after = os.fstat(fd)
                identity = lambda st: (st.st_dev, st.st_ino, st.st_size, st.st_mode, st.st_uid,
                                      st.st_nlink, st.st_mtime_ns, st.st_ctime_ns)
                need(count == row["bytes"] and value.hexdigest() == row["sha256"]
                     and identity(before) == identity(after) == identity(no_link(data_root / origin)),
                     "assembly_identity")
                stream.flush()
                os.fchmod(stream.fileno(), int(row["mode"], 8) & 0o777)
                os.fsync(stream.fileno())
        finally:
            os.close(fd)
    assembly_tree(output, combined, combined_dirs)
    verify_gitlinks(output / "checkout", policy["entries"])
    assembly_tree(original, files, directories)
    assembly_tree(data_root, data_files, data_dirs)
    overlay_fact = save(output / "materials/instrumented-source-overlay.json", overlay)
    assembled = {"schema": "morpheus-native-instrumented-source-assembly-v1", "target": "linux-x64",
        "assembly_plan_sha256": args.plan_sha256, "assembly_recipe_sha256": plan["recipe_sha256"],
        "original_tree_inventory_sha256": inventory_ref["sha256"],
        "original_producer_binding_sha256": ASSEMBLY_SOURCE_BINDING_SHA,
        "original_producer_source_sha256": ASSEMBLY_SOURCE_SHA,
        "original_source_receipt_sha256": files["source-provision-receipt.json"]["sha256"],
        "instrumented_data": material, "source_overlay": overlay_fact,
        "source_projection_files": len(overlay), "source_projection_bytes": sum(row["bytes"] for row in overlay),
        "gitlink_count": len(links), "submodule_sources_materialized": False,
        "required_consumer_binding_sha256": ASSEMBLY_CONSUMER_BINDING_SHA,
        "python_data_bound_here": False, "python_runtime_realized_here": False,
        "source_git_administrative_metadata_created": False,
        "hooks_executed": False, "tools_executed": [], "native_sources_complete": False,
        "compiler_cipd_runtime_closure": False, "build_ready": False, "distribution_ready": False}
    assembly_fact = save(output / "materials/instrumented-assembly-receipt.json", assembled)
    for name, fact in (("materials/instrumented-source-overlay.json", overlay_fact),
                       ("materials/instrumented-assembly-receipt.json", assembly_fact)):
        combined[name] = {"path": name, **fact, "mode": 0o600}
    assembly_tree(output, combined, combined_dirs)
    verify_gitlinks(output / "checkout", policy["entries"])
    source_rows = sorted(values["typed_source_files"] + overlay, key=lambda row: row["path"])
    source_fact = save(root / "handoff/assembled-source-inventory.json", source_rows)
    whole = [{"path": name, "type": "file", "bytes": row["bytes"], "sha256": row["sha256"],
              **({"mode": "100755" if row["mode"] == 0o755 else "100644"}
                 if row["mode"] in (0o644, 0o755) else {})} for name, row in sorted(combined.items())]
    whole_fact = save(root / "handoff/provenance-inventory.json", whole)
    need(source_fact["bytes"] <= 128 * 1024 * 1024 and whole_fact["bytes"] <= 128 * 1024 * 1024,
         "assembly_consumer_inventory")
    verify_gitlinks(output / "checkout", policy["entries"])
    save(root / "handoff/assembly-result.json", {"schema": "morpheus-native-source-assembly-result-v1",
        "status": "instrumented_source_assembled_data_only", "provenance_root": "provision",
        "source_root": "provision/checkout/src", "source_inventory": source_fact,
        "provenance_inventory": whole_fact, "assembly_receipt": assembly_fact,
        "source_overlay": overlay_fact, "original42bd_inventories_preserved": True,
        "native_sources_complete": False, "build_ready": False, "distribution_ready": False,
        "execution_admission_granted": False})
    print('{"native_source_assembly":"assembled_data_only","build_ready":false}', flush=True)


class FixedParser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        # argparse's default error can echo arbitrary argv; never publish it.
        raise Refusal("arguments")


def main() -> int:
    parser = FixedParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--plan", required=True)
    parser.add_argument("--plan-sha256", required=True)
    parser.add_argument("--output-root", required=True)
    parser.add_argument("--assemble-instrumented", action="store_true")
    try:
        args = parser.parse_args()
        if args.assemble_instrumented:
            assemble_instrumented(args)
        else:
            provision(args)
    except Refusal as error:
        record = {"native_source_provision": "refused", "category": error.category}
        if error.dependency and len(error.dependency) <= 1024:
            record["dependency"] = error.dependency
        print(json.dumps(record, sort_keys=True), file=sys.stderr, flush=True)
        return 1
    except SystemExit as status:
        return 0 if status.code == 0 else 1
    except BaseException:
        print('{"native_source_provision":"failed"}', file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
