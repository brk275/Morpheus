#!/usr/bin/env python3
"""Explicit hosted Windows capture QA. No pixels or raw child logs are emitted."""
from __future__ import annotations

import ctypes
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

REPO = Path(__file__).resolve().parents[1]
TARGET = "windows_selected_sources_change_and_stop"
STAGES = {"before_test", "window_ready", "window_enumeration", "window_identity",
          "window_application", "source_enumeration", "source_identity", "display_enumeration",
          "display_identity", "display_geometry", "window_frames", "display_frames", "changed_pixels",
          "reopen", "retitle_rejection", "closed_rejection", "complete"}
FAILURES = {"guard", "job_setup", "job_assignment", "process_start", "process_wait",
            "deadline", "cleanup", "containment", "build", "discovery", "capture", "internal"}
OPERATIONS = {"job_create", "job_configure", "handle_duplicate", "process_create",
              "job_assign", "thread_resume", "process_observe", "process_terminate",
              "process_completion", "process_wait", "process_exit_code", "job_query",
              "job_terminate", "job_drain"}

FIXTURE_STAGES = {"load_forms", "load_drawing", "interactive_check", "display_check",
                  "create_window", "show_window", "shown", "painted", "ready"}
MAX_TEST_LOG_BYTES = 1024 * 1024
# Only reviewed static panic prefixes become public codes. Appended OS errors,
# paths, titles, identifiers and panic bodies are never emitted.
PANIC_REASONS = {
    "inspect fixture process": "fixture_process_observation",
    "fixture window exited early": "fixture_exited",
    "fixture acknowledgement timed out": "fixture_ack_timeout",
    "launch synthetic fixture window": "fixture_launch",
    "enumerate synthetic window": "window_enumeration",
    "owned nonce window is missing or ambiguous": "window_identity",
    "identify fixture application": "window_application",
    "enumerate capture sources": "source_enumeration",
    "fixture source key is missing or ambiguous": "source_identity",
    "enumerate fixture monitors": "display_enumeration",
    "fixture requires one isolated display": "display_count",
    "resolve owned window monitor": "display_identity",
    "fixture horizontal position": "window_geometry",
    "fixture vertical position": "window_geometry",
    "fixture width": "window_geometry",
    "fixture height": "window_geometry",
    "display horizontal position": "display_geometry",
    "display vertical position": "display_geometry",
    "display width": "display_geometry",
    "display height": "display_geometry",
    "owned fixture does not cover the full selected display": "display_coverage",
    "capture owned synthetic source": "capture_source",
    "synthetic source pixels did not match": "pixel_mismatch",
}


class Failure(Exception):
    def __init__(self, code, status=None, operation=None):
        self.code = code if code in FAILURES else "internal"
        self.status = status if type(status) is int and 0 <= status <= 0xFFFFFFFF else None
        self.operation = operation if operation in OPERATIONS else None
        self.previous = []
        super().__init__(self.code)

    def retain(self, error):
        # Store only fixed facts, never another exception's arbitrary payload.
        for item in failure_chain(error):
            if item not in self.previous and len(self.previous) < 5:
                self.previous.append(item)


def failure_chain(error):
    """Preserve primary/cleanup distinctions without exposing exception text."""
    result, seen = [], set()
    while error is not None and id(error) not in seen and len(seen) < 6 and len(result) < 6:
        seen.add(id(error))
        item = {"failure": "internal"}
        if isinstance(error, Failure):
            item["failure"] = error.code
            if error.status is not None:
                item["status"] = error.status
            if error.operation is not None:
                item["operation"] = error.operation
        if item not in result:
            result.append(item)
        if isinstance(error, Failure):
            for previous in error.previous:
                if previous not in result and len(result) < 6:
                    result.append(previous)
        error = error.__cause__ if error.__cause__ is not None else error.__context__
    return result


def guard(environment=None, platform=None):
    environment = os.environ if environment is None else environment
    if ((sys.platform if platform is None else platform) != "win32"
            or environment.get("MORPHEUS_WINDOWS_CAPTURE_ACCEPTANCE") != "1"
            or environment.get("GITHUB_ACTIONS") != "true"
            or environment.get("RUNNER_ENVIRONMENT") != "github-hosted"
            or environment.get("RUNNER_OS") != "Windows"):
        raise Failure("guard")


def fixed_file_fact(path, allowed):
    try:
        with path.open("rb") as stream:
            value = stream.read(65).decode("ascii")
    except (OSError, UnicodeError):
        return "unavailable"
    return value if value in allowed else "unknown"


def stage_fact(path):
    return fixed_file_fact(path, STAGES)


def bounded_test_output(path):
    try:
        with path.open("rb") as stream:
            value = stream.read(MAX_TEST_LOG_BYTES + 1)
    except FileNotFoundError:
        return "missing", ""
    except OSError:
        return "read_failed", ""
    if len(value) > MAX_TEST_LOG_BYTES:
        return "oversized", ""
    return "complete", value.decode("utf-8", errors="replace").replace("\r\n", "\n")


def panic_reason(output):
    header = re.compile(rf"^thread '{TARGET}'(?: \([0-9]+\))? panicked at "
                        r"[^\n]{0,1024}windows_capture\.rs:[0-9]+:[0-9]+:$", re.M)
    matches = list(header.finditer(output))
    if not matches:
        return "none"
    if len(matches) != 1:
        return "ambiguous"
    message = output[matches[0].end() + 1:].split("\n", 1)[0]
    if len(message) > 2048:
        return "unknown"
    for prefix, reason in PANIC_REASONS.items():
        if message == prefix or message.startswith(prefix + ": "):
            return reason
    return "unknown"


def diagnostic_facts(root):
    log_state, output = bounded_test_output(root / "test.log")
    exit_code = None
    try:
        with (root / "fixture-exit-code").open("rb") as stream:
            raw = stream.read(11)
        if re.fullmatch(rb"[0-9]{1,10}", raw) and int(raw) <= 0xFFFFFFFF:
            exit_code = int(raw)
    except OSError:
        pass
    return {
        "test_log": log_state,
        "named_test_failed": re.search(rf"^test {TARGET} \.\.\. FAILED$", output, re.M) is not None,
        "panic_reason": panic_reason(output),
        "fixture_stage": fixed_file_fact(root / "fixture-stage", FIXTURE_STAGES),
        "fixture_exit_code": exit_code,
    }


def passed(output):
    return (re.search(rf"^test {TARGET} \.\.\. ok$", output, re.M) is not None
            and re.search(r"^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out;", output, re.M) is not None)


def executable_from_output(output):
    matches = []
    for line in output.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if not isinstance(item, dict):
            continue
        target, profile = item.get("target"), item.get("profile")
        if (item.get("reason") == "compiler-artifact"
                and isinstance(target, dict) and isinstance(profile, dict)
                and target.get("name") == "windows_capture"
                and target.get("kind") == ["test"]
                and profile.get("test") is True
                and isinstance(item.get("executable"), str)):
            matches.append(Path(item["executable"]))
    if (len(matches) != 1 or not matches[0].is_absolute()
            or matches[0].suffix.lower() != ".exe" or not matches[0].is_file()):
        raise Failure("discovery")
    return matches[0].resolve()


class WindowsJob:
    """Kernel-owned process tree. Children enter this job before first execution."""
    def __init__(self):
        if sys.platform != "win32":
            raise Failure("guard")
        from ctypes import wintypes as w
        class IO(ctypes.Structure):
            _fields_ = [(name, ctypes.c_ulonglong) for name in ("read_ops", "write_ops", "other_ops", "read_bytes", "write_bytes", "other_bytes")]
        class Limits(ctypes.Structure):
            _fields_ = [("process_time", ctypes.c_longlong), ("job_time", ctypes.c_longlong),
                        ("flags", w.DWORD), ("minimum", ctypes.c_size_t), ("maximum", ctypes.c_size_t),
                        ("active", w.DWORD), ("affinity", ctypes.c_size_t), ("priority", w.DWORD), ("scheduling", w.DWORD)]
        class Extended(ctypes.Structure):
            _fields_ = [("limits", Limits), ("io", IO), ("process_memory", ctypes.c_size_t),
                        ("job_memory", ctypes.c_size_t), ("peak_process", ctypes.c_size_t), ("peak_job", ctypes.c_size_t)]
        class Accounting(ctypes.Structure):
            _fields_ = [(name, ctypes.c_longlong) for name in ("user", "kernel", "period_user", "period_kernel")] + [
                (name, w.DWORD) for name in ("faults", "total", "active", "terminated")]
        class Startup(ctypes.Structure):
            _fields_ = [("cb", w.DWORD), ("reserved", w.LPWSTR), ("desktop", w.LPWSTR), ("title", w.LPWSTR)] + [
                (name, w.DWORD) for name in ("x", "y", "width", "height", "chars_x", "chars_y", "fill", "flags")] + [
                ("show", w.WORD), ("reserved_size", w.WORD), ("reserved_bytes", ctypes.c_void_p),
                ("stdin", w.HANDLE), ("stdout", w.HANDLE), ("stderr", w.HANDLE)]
        class Process(ctypes.Structure):
            _fields_ = [("process", w.HANDLE), ("thread", w.HANDLE), ("pid", w.DWORD), ("tid", w.DWORD)]
        # This fixture targets the supported x64 runner only. Reject an ABI
        # mismatch before passing any structure to kernel32.
        if tuple(map(ctypes.sizeof, (IO, Limits, Extended, Accounting, Startup, Process))) != (48, 64, 144, 48, 104, 24):
            raise Failure("job_setup")
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        signatures = {
            "CreateJobObjectW": ([ctypes.c_void_p, w.LPCWSTR], w.HANDLE),
            "SetInformationJobObject": ([w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD], w.BOOL),
            "QueryInformationJobObject": ([w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD, ctypes.c_void_p], w.BOOL),
            "AssignProcessToJobObject": ([w.HANDLE, w.HANDLE], w.BOOL),
            "TerminateJobObject": ([w.HANDLE, w.UINT], w.BOOL),
            "CloseHandle": ([w.HANDLE], w.BOOL),
            "CreateProcessW": ([w.LPCWSTR, w.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, w.BOOL, w.DWORD,
                                ctypes.c_void_p, w.LPCWSTR, ctypes.POINTER(Startup), ctypes.POINTER(Process)], w.BOOL),
            "ResumeThread": ([w.HANDLE], w.DWORD),
            "WaitForSingleObject": ([w.HANDLE, w.DWORD], w.DWORD),
            "GetExitCodeProcess": ([w.HANDLE, ctypes.POINTER(w.DWORD)], w.BOOL),
            "TerminateProcess": ([w.HANDLE, w.UINT], w.BOOL),
            "GetCurrentProcess": ([], w.HANDLE),
            "DuplicateHandle": ([w.HANDLE, w.HANDLE, w.HANDLE, ctypes.POINTER(w.HANDLE), w.DWORD, w.BOOL, w.DWORD], w.BOOL),
        }
        for name, (args, result) in signatures.items():
            function = getattr(kernel, name); function.argtypes = args; function.restype = result
        self.kernel, self.Startup, self.Process, self.Accounting = kernel, Startup, Process, Accounting
        self.handles = []
        self.job = kernel.CreateJobObjectW(None, None)  # Noninheritable, unnamed.
        if not self.job:
            raise Failure("job_setup", ctypes.get_last_error(), "job_create")
        limits = Extended(); limits.limits.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not kernel.SetInformationJobObject(self.job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            code = ctypes.get_last_error(); kernel.CloseHandle(self.job); self.job = None
            raise Failure("job_setup", code, "job_configure")

    def spawn(self, command, environment, log):
        import msvcrt
        from ctypes import wintypes as w
        executable = str(Path(command[0]).resolve())
        command_line = ctypes.create_unicode_buffer(subprocess.list2cmdline([executable, *map(str, command[1:])]))
        env_block = ctypes.create_unicode_buffer("\0".join(f"{key}={value}" for key, value in sorted(environment.items(), key=lambda item: item[0].upper())) + "\0")
        startup = self.Startup(); startup.cb = ctypes.sizeof(startup); startup.flags = 0x100  # STARTF_USESTDHANDLES
        process = self.Process()
        duplicates = []
        created = False
        try:
            with open(os.devnull, "rb") as null:
                current = self.kernel.GetCurrentProcess()
                for stream in (null, log):
                    duplicate = w.HANDLE()
                    if not self.kernel.DuplicateHandle(current, msvcrt.get_osfhandle(stream.fileno()),
                            current, ctypes.byref(duplicate), 0, True, 2):  # DUPLICATE_SAME_ACCESS
                        raise Failure("process_start", ctypes.get_last_error(), "handle_duplicate")
                    duplicates.append(duplicate.value)
                startup.stdin, startup.stdout, startup.stderr = duplicates[0], duplicates[1], duplicates[1]
                created = self.kernel.CreateProcessW(executable, command_line, None, None, True,
                    0x4 | 0x400 | 0x08000000, env_block, str(REPO), ctypes.byref(startup), ctypes.byref(process))
                if not created:
                    raise Failure("process_start", ctypes.get_last_error(), "process_create")
                # Retain ownership before assignment/cleanup can fail. close()
                # also terminates unassigned suspended children by handle.
                self.handles.append(process.process)
            self.assign_and_resume(process.process, process.thread)
            return process.process
        finally:
            if created:
                self.kernel.CloseHandle(process.thread)
            for handle in duplicates:
                self.kernel.CloseHandle(handle)

    def terminate_handle(self, handle):
        # A retained handle cannot be redirected by PID reuse. A failed job
        # assignment must kill the still-suspended child before closing it.
        result = self.kernel.WaitForSingleObject(handle, 0)
        if result == 0:
            return
        if result != 258:
            raise Failure("cleanup", ctypes.get_last_error(), "process_observe")
        if not self.kernel.TerminateProcess(handle, 1):
            code = ctypes.get_last_error()
            # Job termination is asynchronous. The kernel can reject another
            # terminate with ERROR_ACCESS_DENIED while completion is pending.
            # Only a signaled retained handle proves that cleanup completed.
            result = self.kernel.WaitForSingleObject(handle, 10000)
            if result == 0:
                return
            error = Failure("cleanup", code, "process_terminate")
            error.retain(Failure("cleanup", ctypes.get_last_error() if result == 0xFFFFFFFF else None,
                                 "process_completion"))
            raise error
        result = self.kernel.WaitForSingleObject(handle, 10000)
        if result != 0:
            raise Failure("cleanup", ctypes.get_last_error() if result == 0xFFFFFFFF else None,
                          "process_completion")

    def assign_and_resume(self, process, thread):
        try:
            if not self.kernel.AssignProcessToJobObject(self.job, process):
                raise Failure("job_assignment", ctypes.get_last_error(), "job_assign")
            # CREATE_SUSPENDED establishes exactly one suspend count.
            result = self.kernel.ResumeThread(thread)
            if result != 1:
                raise Failure("process_start", ctypes.get_last_error() if result == 0xFFFFFFFF else None,
                              "thread_resume")
        except BaseException:
            self.terminate_handle(process)
            raise

    def wait(self, handle, seconds):
        from ctypes import wintypes as w
        result = self.kernel.WaitForSingleObject(handle, int(seconds * 1000))
        if result == 258:
            raise Failure("deadline", operation="process_wait")
        if result != 0:
            raise Failure("process_wait", ctypes.get_last_error(), "process_wait")
        code = w.DWORD()
        if not self.kernel.GetExitCodeProcess(handle, ctypes.byref(code)):
            raise Failure("process_wait", ctypes.get_last_error(), "process_exit_code")
        return code.value

    def active(self):
        value = self.Accounting()
        if not self.kernel.QueryInformationJobObject(self.job, 1, ctypes.byref(value), ctypes.sizeof(value), None):
            raise Failure("cleanup", ctypes.get_last_error(), "job_query")
        return value.active

    def close(self):
        if not self.job:
            return
        failure = None
        try:
            if not self.kernel.TerminateJobObject(self.job, 1):
                raise Failure("cleanup", ctypes.get_last_error(), "job_terminate")
            deadline = time.monotonic() + 10
            while self.active():
                if time.monotonic() >= deadline:
                    raise Failure("cleanup", operation="job_drain")
                time.sleep(.05)
        except Failure as error:
            failure = error
        finally:
            # Also covers the child whose job assignment failed. Attempt every
            # retained handle even if an earlier termination failed.
            for handle in self.handles:
                try:
                    self.terminate_handle(handle)
                except Failure as error:
                    if failure is None:
                        failure = error
                    else:
                        failure.retain(error)
                finally:
                    self.kernel.CloseHandle(handle)
            self.handles.clear()
            # Kernel kill-on-close remains the fallback if observation failed.
            self.kernel.CloseHandle(self.job); self.job = None
        if failure:
            raise failure


def owned_run(job, command, environment, path, seconds):
    with path.open("wb") as log:
        os.chmod(path, 0o600)
        handle = job.spawn(command, environment, log)
        return job.wait(handle, seconds)


def containment_probe(root, environment):
    job = WindowsJob()
    try:
        program = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c','import time; time.sleep(300)'],creationflags=0x8); time.sleep(300)"
        with (root / "containment.log").open("wb") as log:
            job.spawn([sys.executable, "-c", program], environment, log)
        deadline = time.monotonic() + 15
        while job.active() < 2:
            if time.monotonic() >= deadline:
                raise Failure("containment")
            time.sleep(.05)
    finally:
        job.close()  # Requires active==0, including the detached descendant.


def execute(root, environment):
    containment_probe(root, environment)
    cargo = shutil.which("cargo")
    if not cargo:
        raise Failure("build")
    job = WindowsJob()
    try:
        build_log = root / "build.log"
        code = owned_run(job, [cargo, "test", "--locked", "-p", "morpheus-rtc", "--features", "native-devices",
            "--test", "windows_capture", "--no-run", "--message-format=json"], environment, build_log, 600)
        if code:
            raise Failure("build", code)
        executable = executable_from_output(build_log.read_text(errors="replace"))
        test_log = root / "test.log"
        code = owned_run(job, [str(executable), "--ignored", "--exact", TARGET, "--test-threads=1"], environment, test_log, 120)
        log_state, output = bounded_test_output(test_log)
        if code or log_state != "complete" or not passed(output) or stage_fact(root / "stage") != "complete":
            raise Failure("capture", code)
    finally:
        job.close()


def main():
    facts = {"passed": False, "stage": "not_started"}
    try:
        guard()
        with tempfile.TemporaryDirectory(prefix="morpheus-windows-capture-") as directory:
            root = Path(directory)
            root.chmod(0o700)
            if root.resolve().is_relative_to(REPO):
                raise Failure("guard")
            environment = os.environ.copy()
            environment.update(MORPHEUS_WINDOWS_CAPTURE_ROOT=str(root), MORPHEUS_WINDOWS_CAPTURE_NONCE=uuid.uuid4().hex)
            (root / "command").write_text("")
            (root / "stage").write_text("before_test")
            try:
                execute(root, environment)
                facts["passed"] = True
            finally:
                facts["stage"] = stage_fact(root / "stage")
                facts["diagnostics"] = diagnostic_facts(root)
    except BaseException as error:
        facts["passed"] = False
        details = failure_chain(error)
        facts.update(details[0])
        if len(details) > 1:
            facts["prior_failures"] = details[1:]
    print("Windows synthetic capture facts: " + json.dumps(facts, sort_keys=True))
    return 0 if facts["passed"] and "failure" not in facts else 1


if __name__ == "__main__":
    raise SystemExit(main())
