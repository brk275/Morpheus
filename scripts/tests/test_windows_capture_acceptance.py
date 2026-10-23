import contextlib
import ctypes
import io
import json
from pathlib import Path
import subprocess
import struct
import sys
import tempfile
import types
import unittest
from unittest.mock import Mock, patch

from scripts import run_windows_capture_acceptance as capture
from scripts import windows_update_acceptance as update


HOSTED = {"MORPHEUS_WINDOWS_CAPTURE_ACCEPTANCE": "1", "GITHUB_ACTIONS": "true",
          "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Windows"}


class Startup(ctypes.Structure):
    _fields_ = [(name, ctypes.c_size_t) for name in ("cb", "flags", "stdin", "stdout", "stderr")]


class Process(ctypes.Structure):
    _fields_ = [(name, ctypes.c_size_t) for name in ("process", "thread", "pid", "tid")]


class Kernel:
    """Portable Win32-call contract fake; never creates a process or a window."""
    def __init__(self, *, assign=True, resume=1, create=True, duplicate_failure=None):
        self.assign, self.resume, self.create = assign, resume, create
        self.duplicate_failure = duplicate_failure
        self.calls, self.terminated = [], set()
        self.duplicates = 0
        self.sources = []
        self.directories = []

    def GetCurrentProcess(self):
        return -1

    def DuplicateHandle(self, current, source, destination, result, access, inherit, options):
        self.duplicates += 1
        self.sources.append(source)
        self.calls.append(("duplicate", inherit, options))
        if self.duplicate_failure == self.duplicates:
            return False
        result._obj.value = 100 + self.duplicates
        return True

    def CreateProcessW(self, executable, command, security, thread_security, inherit, flags,
                       environment, directory, startup, process):
        self.directories.append(directory)
        self.calls.append(("create", executable, command.value, inherit, flags,
                           environment[len(environment)-2:], startup._obj.stdin,
                           startup._obj.stdout, startup._obj.stderr))
        if self.create:
            process._obj.process, process._obj.thread = 20, 21
        return self.create

    def AssignProcessToJobObject(self, job, process):
        self.calls.append(("assign", job, process))
        return self.assign

    def ResumeThread(self, thread):
        self.calls.append(("resume", thread))
        return self.resume

    def WaitForSingleObject(self, handle, milliseconds):
        self.calls.append(("wait", handle, milliseconds))
        return 0 if handle in self.terminated else 258

    def TerminateProcess(self, handle, code):
        self.calls.append(("terminate", handle))
        self.terminated.add(handle)
        return True

    def TerminateJobObject(self, job, code):
        self.calls.append(("terminate_job", job))
        return True

    def CloseHandle(self, handle):
        self.calls.append(("close", handle))
        return True


def job_with(kernel):
    job = capture.WindowsJob.__new__(capture.WindowsJob)
    job.kernel, job.Startup, job.Process = kernel, Startup, Process
    job.job, job.handles = 10, []
    job.active = Mock(return_value=0)
    return job


class WindowsCaptureTests(unittest.TestCase):
    def test_guard_requires_explicit_hosted_windows(self):
        capture.guard(HOSTED, "win32")
        for key in HOSTED:
            environment = HOSTED.copy()
            environment.pop(key)
            with self.subTest(key=key), self.assertRaises(capture.Failure):
                capture.guard(environment, "win32")
        for platform in ("linux", "darwin"):
            with self.assertRaises(capture.Failure):
                capture.guard(HOSTED, platform)
        with self.assertRaises(capture.Failure):
            capture.guard(dict(HOSTED, RUNNER_ENVIRONMENT="self-hosted"), "win32")

    def test_pass_requires_exact_named_nonempty_test_result(self):
        line = f"test {capture.TARGET} ... ok\n"
        summary = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2s\n"
        self.assertTrue(capture.passed(line + summary))
        for invalid in (line, summary, line + summary.replace("1 passed", "0 passed"),
                        line.replace(capture.TARGET, "different_test") + summary,
                        line + summary.replace("0 ignored", "1 ignored"),
                        line + summary.replace("0 failed", "1 failed")):
            self.assertFalse(capture.passed(invalid))

    def test_stage_and_failure_values_are_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stage"
            self.assertEqual(capture.stage_fact(path), "unavailable")
            for value in capture.STAGES:
                path.write_text(value)
                self.assertEqual(capture.stage_fact(path), value)
            for value in ("private/path", "complete\nprivate", "x" * 200):
                path.write_text(value)
                self.assertEqual(capture.stage_fact(path), "unknown")
        self.assertEqual(capture.Failure("private/error").code, "internal")
        for status in (-1, True, "private/error", 1 << 32):
            self.assertIsNone(capture.Failure("capture", status).status)
        self.assertEqual(capture.Failure("capture", 0xC0000005).status, 0xC0000005)

    def test_executable_requires_one_actual_test_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "windows_capture-fixture.exe"
            executable.touch()
            artifact = {"reason": "compiler-artifact", "target": {"name": "windows_capture", "kind": ["test"]},
                        "profile": {"test": True}, "executable": str(executable)}
            output = json.dumps(artifact)
            self.assertEqual(capture.executable_from_output(output), executable.resolve())
            for invalid in (output + "\n" + output, "{}", "null", "unstructured compiler error",
                            json.dumps(dict(artifact, target=None)), json.dumps(dict(artifact, profile=[])),
                            json.dumps(dict(artifact, executable="relative.exe"))):
                with self.subTest(invalid=invalid), self.assertRaises(capture.Failure):
                    capture.executable_from_output(invalid)

    def invoke_spawn(self, kernel):
        job = job_with(kernel)
        with tempfile.TemporaryFile() as log, patch.dict(sys.modules, {"msvcrt": types.SimpleNamespace(get_osfhandle=lambda fd: fd)}), \
                patch.object(ctypes, "get_last_error", return_value=5, create=True):
            command = [str(Path("folder with spaces/tool.exe").resolve()), 'quoted"argument', "two words", ""]
            result = job.spawn(command, {"FIXTURE": "value"}, log)
        return job, command, result

    def test_child_is_suspended_then_assigned_before_resume(self):
        kernel = Kernel()
        job, command, result = self.invoke_spawn(kernel)
        self.assertEqual(result, 20)
        self.assertEqual(job.handles, [20])
        creation = next(call for call in kernel.calls if call[0] == "create")
        self.assertEqual(creation[1:3], (command[0], subprocess.list2cmdline(command)))
        self.assertTrue(creation[4] & 4)  # CREATE_SUSPENDED
        self.assertTrue(creation[4] & 0x400)  # CREATE_UNICODE_ENVIRONMENT
        self.assertEqual(creation[5], "\0\0")
        self.assertEqual(creation[6:], (101, 102, 102))
        events = [call[0] for call in kernel.calls]
        self.assertLess(events.index("assign"), events.index("resume"))
        self.assertIn(("close", 21), kernel.calls)
        self.assertIn(("close", 101), kernel.calls)
        self.assertIn(("close", 102), kernel.calls)
        self.assertNotIn(("close", 20), kernel.calls)

    def test_explicit_export_cwd_keeps_suspended_job_ownership_and_refuses_injection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            kernel = Kernel()
            job = job_with(kernel)
            with tempfile.TemporaryFile() as log, \
                    patch.dict(sys.modules, {"msvcrt": types.SimpleNamespace(get_osfhandle=lambda fd: fd)}), \
                    patch.object(ctypes, "get_last_error", return_value=5, create=True):
                job.spawn([str(root / "fixture.exe")], {"FIXTURE": "value"}, log, cwd=root)
                self.assertEqual(kernel.directories, [str(root)])
                events = [call[0] for call in kernel.calls]
                self.assertLess(events.index("assign"), events.index("resume"))
                for cwd, environment in (("C:relative", {}), ("relative", {}), (str(root) + "\0", {}),
                                         (root, {"BAD=KEY": "value"}), (root, {"BAD": "injected\0value"})):
                    before = len(kernel.calls)
                    with self.subTest(cwd=cwd), self.assertRaises(capture.Failure):
                        job.spawn([str(root / "fixture.exe")], environment, log, cwd=cwd)
                    self.assertEqual(len(kernel.calls), before)

    def test_private_stdin_is_duplicated_without_closing_the_callers_stream(self):
        kernel = Kernel()
        job = job_with(kernel)
        with tempfile.TemporaryFile() as source, tempfile.TemporaryFile() as log, \
                patch.dict(sys.modules, {"msvcrt": types.SimpleNamespace(get_osfhandle=lambda fd: fd)}), \
                patch.object(ctypes, "get_last_error", return_value=5, create=True), \
                patch("builtins.open", side_effect=AssertionError("provided stdin must not open devnull")):
            result = job.spawn([str(Path("fixture.exe").resolve())], {}, log, stdin_stream=source)
            self.assertEqual(result, 20)
            self.assertEqual(kernel.sources, [source.fileno(), log.fileno()])
            self.assertFalse(source.closed)
            self.assertFalse(log.closed)
        events = [call[0] for call in kernel.calls]
        self.assertLess(events.index("assign"), events.index("resume"))
        self.assertEqual([call for call in kernel.calls if call[0] == "close"],
                         [("close", 21), ("close", 101), ("close", 102)])

    def test_private_stdin_failure_paths_close_only_owned_duplicates(self):
        for options in ({"duplicate_failure": 1}, {"duplicate_failure": 2},
                        {"create": False}, {"assign": False}, {"resume": 0}):
            with self.subTest(options=options):
                kernel = Kernel(**options)
                job = job_with(kernel)
                with tempfile.TemporaryFile() as source, tempfile.TemporaryFile() as log, \
                        patch.dict(sys.modules, {"msvcrt": types.SimpleNamespace(get_osfhandle=lambda fd: fd)}), \
                        patch.object(ctypes, "get_last_error", return_value=5, create=True):
                    with self.assertRaises(capture.Failure):
                        job.spawn([str(Path("fixture.exe").resolve())], {}, log, stdin_stream=source)
                    self.assertFalse(source.closed)
                    self.assertFalse(log.closed)
                    expected = [] if options.get("duplicate_failure") == 1 else [101]
                    if options.get("duplicate_failure") is None:
                        expected.append(102)
                    closed = [call[1] for call in kernel.calls if call[0] == "close" and call[1] in (101, 102)]
                    self.assertEqual(closed, expected)
                if options.get("assign") is False or "resume" in options:
                    self.assertIn(("terminate", 20), kernel.calls)

    def test_assignment_failure_kills_suspended_child_and_never_resumes(self):
        kernel = Kernel(assign=False)
        with self.assertRaises(capture.Failure) as raised:
            self.invoke_spawn(kernel)
        self.assertEqual(raised.exception.code, "job_assignment")
        self.assertNotIn("resume", [call[0] for call in kernel.calls])
        self.assertIn(("terminate", 20), kernel.calls)
        self.assertLess(kernel.calls.index(("terminate", 20)), kernel.calls.index(("close", 21)))
        self.assertIn(("wait", 20, 10000), kernel.calls)

    def test_resume_failure_terminates_retained_child(self):
        for resume in (0, 2, 0xFFFFFFFF):
            kernel = Kernel(resume=resume)
            with self.subTest(resume=resume), self.assertRaises(capture.Failure) as raised:
                self.invoke_spawn(kernel)
            self.assertEqual(raised.exception.code, "process_start")
            self.assertIn(("terminate", 20), kernel.calls)

    def test_creation_and_duplication_failure_close_owned_handles(self):
        kernel = Kernel(create=False)
        with self.assertRaises(capture.Failure):
            self.invoke_spawn(kernel)
        self.assertEqual([call for call in kernel.calls if call[0] == "close"], [("close", 101), ("close", 102)])
        self.assertNotIn("assign", [call[0] for call in kernel.calls])
        kernel = Kernel(duplicate_failure=2)
        with self.assertRaises(capture.Failure):
            self.invoke_spawn(kernel)
        self.assertEqual([call for call in kernel.calls if call[0] == "close"], [("close", 101)])
        self.assertNotIn("create", [call[0] for call in kernel.calls])

    def test_cleanup_attempts_every_handle_even_after_failure(self):
        kernel = Kernel()
        job = job_with(kernel)
        job.handles = [20, 30]
        job.terminate_handle = Mock(side_effect=[capture.Failure("cleanup"), None])
        with self.assertRaises(capture.Failure):
            job.close()
        self.assertEqual([call.args[0] for call in job.terminate_handle.call_args_list], [20, 30])
        self.assertEqual([call for call in kernel.calls if call[0] == "close"], [("close", 20), ("close", 30), ("close", 10)])
        self.assertIsNone(job.job)
        self.assertEqual(job.handles, [])

    def test_failed_terminate_waits_for_delayed_completion_on_retained_handle(self):
        kernel = Kernel()
        kernel.WaitForSingleObject = Mock(side_effect=[258, 0])
        kernel.TerminateProcess = Mock(return_value=False)
        job = job_with(kernel)
        with patch.object(ctypes, "get_last_error", return_value=5, create=True):
            job.terminate_handle(20)
        self.assertEqual(kernel.WaitForSingleObject.call_args_list,
                         [unittest.mock.call(20, 0), unittest.mock.call(20, 10000)])
        kernel.TerminateProcess.assert_called_once_with(20, 1)

    def test_failed_terminate_timeout_or_wait_failure_never_passes(self):
        for wait_result in (258, 0xFFFFFFFF, 128):
            kernel = Kernel()
            kernel.WaitForSingleObject = Mock(side_effect=[258, wait_result])
            kernel.TerminateProcess = Mock(return_value=False)
            job = job_with(kernel)
            with (patch.object(ctypes, "get_last_error", side_effect=[5, 6], create=True),
                  self.subTest(wait_result=wait_result), self.assertRaises(capture.Failure) as raised):
                job.terminate_handle(20)
            facts = capture.failure_chain(raised.exception)
            self.assertEqual(facts[0], {"failure": "cleanup", "status": 5, "operation": "process_terminate"})
            self.assertEqual(facts[1]["operation"], "process_completion")
            self.assertEqual(facts[1].get("status"), 6 if wait_result == 0xFFFFFFFF else None)
            self.assertEqual(kernel.WaitForSingleObject.call_args.args, (20, 10000))

    def test_assignment_failure_is_retained_when_child_cleanup_also_fails(self):
        kernel = Kernel(assign=False)
        job = job_with(kernel)
        job.terminate_handle = Mock(side_effect=capture.Failure("cleanup", 5, "process_terminate"))
        with (patch.object(ctypes, "get_last_error", return_value=5, create=True),
              self.assertRaises(capture.Failure) as raised):
            job.assign_and_resume(20, 21)
        self.assertEqual(capture.failure_chain(raised.exception), [
            {"failure": "cleanup", "status": 5, "operation": "process_terminate"},
            {"failure": "job_assignment", "status": 5, "operation": "job_assign"},
        ])
        self.assertNotIn("resume", [call[0] for call in kernel.calls])

    def test_job_cleanup_preserves_each_distinct_operation_and_closes_all_handles(self):
        kernel = Kernel()
        kernel.TerminateJobObject = Mock(return_value=False)
        job = job_with(kernel)
        job.handles = [20, 30]
        job.terminate_handle = Mock(side_effect=[capture.Failure("cleanup", 5, "process_terminate"),
                                                capture.Failure("cleanup", 6, "process_completion")])
        with (patch.object(ctypes, "get_last_error", return_value=5, create=True),
              self.assertRaises(capture.Failure) as raised):
            job.close()
        self.assertEqual([item["operation"] for item in capture.failure_chain(raised.exception)],
                         ["job_terminate", "process_terminate", "process_completion"])
        self.assertEqual([call.args[0] for call in job.terminate_handle.call_args_list], [20, 30])
        self.assertEqual([call for call in kernel.calls if call[0] == "close"], [("close", 20), ("close", 30), ("close", 10)])

    def test_timeout_is_fixed_and_owner_always_closes(self):
        kernel = Kernel()
        job = job_with(kernel)
        with self.assertRaises(capture.Failure) as raised:
            job.wait(20, 2)
        self.assertEqual(raised.exception.code, "deadline")
        job.handles.append(20)
        job.close()
        self.assertIn(("terminate", 20), kernel.calls)

    def test_public_failure_withholds_raw_logs_and_paths(self):
        def fail(root, environment):
            (root / "stage").write_text("window_frames")
            (root / "test.log").write_text("PRIVATE-DESKTOP-PATH-AND-PANIC")
            raise capture.Failure("capture", 0xC0000005)
        output = io.StringIO()
        with patch.object(capture, "guard"), patch.object(capture, "execute", side_effect=fail), contextlib.redirect_stdout(output):
            self.assertEqual(capture.main(), 1)
        facts = json.loads(output.getvalue().split(": ", 1)[1])
        self.assertEqual(facts, {"passed": False, "stage": "window_frames", "failure": "capture", "status": 0xC0000005,
                                 "diagnostics": {"test_log": "complete", "named_test_failed": False,
                                                 "panic_reason": "none", "fixture_stage": "unavailable",
                                                 "fixture_exit_code": None}})
        self.assertNotIn("PRIVATE", output.getvalue())

    def test_unexpected_errors_are_static_and_not_passed(self):
        output = io.StringIO()
        with patch.object(capture, "guard"), patch.object(capture, "execute", side_effect=RuntimeError("PRIVATE")), contextlib.redirect_stdout(output):
            self.assertEqual(capture.main(), 1)
        self.assertNotIn("PRIVATE", output.getvalue())
        self.assertEqual(json.loads(output.getvalue().split(": ", 1)[1])["failure"], "internal")

    def test_public_cleanup_facts_retain_primary_failure_without_private_text(self):
        def fail(root, environment):
            try:
                raise capture.Failure("job_assignment", 5, "job_assign")
            finally:
                raise capture.Failure("cleanup", 5, "process_terminate")
        output = io.StringIO()
        with patch.object(capture, "guard"), patch.object(capture, "execute", side_effect=fail), contextlib.redirect_stdout(output):
            self.assertEqual(capture.main(), 1)
        facts = json.loads(output.getvalue().split(": ", 1)[1])
        self.assertEqual(facts["operation"], "process_terminate")
        self.assertEqual(facts["prior_failures"], [{"failure": "job_assignment", "status": 5, "operation": "job_assign"}])
        private = capture.Failure("private-value", "private-status", "private-operation")
        self.assertEqual(capture.failure_chain(private), [{"failure": "internal"}])
        with self.assertRaises(RuntimeError) as raised:
            try:
                raise private
            finally:
                raise RuntimeError("private-exception-body")
        self.assertEqual(capture.failure_chain(raised.exception), [{"failure": "internal"}])

    def test_cleanup_failure_revokes_prior_success(self):
        with tempfile.TemporaryDirectory() as directory:
            class UnsuccessfulCleanup:
                def __enter__(self):
                    return directory

                def __exit__(self, *args):
                    raise OSError("PRIVATE-CLEANUP-PATH")
            def success(root, environment):
                (root / "stage").write_text("complete")
            output = io.StringIO()
            with patch.object(capture, "guard"), patch.object(capture, "execute", side_effect=success), \
                    patch.object(capture.tempfile, "TemporaryDirectory", return_value=UnsuccessfulCleanup()), contextlib.redirect_stdout(output):
                self.assertEqual(capture.main(), 1)
            facts = json.loads(output.getvalue().split(": ", 1)[1])
            self.assertEqual(facts, {"passed": False, "stage": "complete", "failure": "internal",
                                     "diagnostics": {"test_log": "missing", "named_test_failed": False,
                                                     "panic_reason": "none", "fixture_stage": "unavailable",
                                                     "fixture_exit_code": None}})
            self.assertNotIn("PRIVATE", output.getvalue())

    def test_panic_diagnostics_accept_only_reviewed_prefixes_and_named_test(self):
        for thread_suffix in ("", " (1234)"):
            header = (f"thread '{capture.TARGET}'{thread_suffix} panicked at "
                      "PRIVATE\\tests\\windows_capture.rs:42:9:\n")
            for message, reason in capture.PANIC_REASONS.items():
                with self.subTest(message=message, suffix=thread_suffix):
                    self.assertEqual(capture.panic_reason(header + message), reason)
                    self.assertEqual(capture.panic_reason(header + message + ": PRIVATE-OS-ERROR"), reason)
            self.assertEqual(capture.panic_reason(header + "PRIVATE-UNKNOWN-PANIC"), "unknown")
            self.assertEqual(capture.panic_reason(header + "fixture window exited early-extra"), "unknown")
            self.assertEqual(capture.panic_reason(header + "x" * 2049), "unknown")
            self.assertEqual(capture.panic_reason(header + "fixture window exited early\n" + header), "ambiguous")
            self.assertEqual(capture.panic_reason(header.replace(capture.TARGET, "another_test") +
                                                  "fixture window exited early"), "none")
            self.assertEqual(capture.panic_reason(header.replace("windows_capture.rs", "other.rs") +
                                                  "fixture window exited early"), "none")

    def test_bounded_test_log_never_accepts_partial_oversized_or_missing_data(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "test.log"
            self.assertEqual(capture.bounded_test_output(path), ("missing", ""))
            path.write_bytes(b"a\r\nb\xff")
            self.assertEqual(capture.bounded_test_output(path), ("complete", "a\nb\ufffd"))
            path.write_bytes(b"x" * capture.MAX_TEST_LOG_BYTES)
            self.assertEqual(capture.bounded_test_output(path)[0], "complete")
            path.write_bytes(b"x" * (capture.MAX_TEST_LOG_BYTES + 1))
            self.assertEqual(capture.bounded_test_output(path), ("oversized", ""))
            self.assertFalse(capture.passed(capture.bounded_test_output(path)[1]))
            with patch.object(Path, "open", side_effect=PermissionError("PRIVATE-PATH")):
                self.assertEqual(capture.bounded_test_output(path), ("read_failed", ""))

    def test_fixture_diagnostics_reject_unknown_stages_and_non_u32_exit_codes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for stage in capture.FIXTURE_STAGES:
                (root / "fixture-stage").write_text(stage)
                self.assertEqual(capture.diagnostic_facts(root)["fixture_stage"], stage)
            for invalid in ("PRIVATE", "ready\nPRIVATE", "x" * 200):
                (root / "fixture-stage").write_text(invalid)
                self.assertEqual(capture.diagnostic_facts(root)["fixture_stage"], "unknown")
            (root / "fixture-stage").write_bytes(b"\xff")
            self.assertEqual(capture.diagnostic_facts(root)["fixture_stage"], "unavailable")
            for code in (0, 3, 0xC0000005, 0xFFFFFFFF):
                (root / "fixture-exit-code").write_text(str(code))
                self.assertEqual(capture.diagnostic_facts(root)["fixture_exit_code"], code)
            for invalid in ("-1", "true", "4294967296", "3\nPRIVATE", "x" * 200, "0" * 11):
                (root / "fixture-exit-code").write_text(invalid)
                self.assertIsNone(capture.diagnostic_facts(root)["fixture_exit_code"])

    def test_named_failure_diagnostics_survive_cleanup_without_private_payload(self):
        paths = []
        def fail(root, environment):
            paths.append(root)
            (root / "stage").write_text("window_ready")
            (root / "fixture-stage").write_text("interactive_check")
            (root / "fixture-exit-code").write_text("3")
            (root / "test.log").write_bytes(
                (f"test {capture.TARGET} ... FAILED\r\n"
                 f"thread '{capture.TARGET}' (42) panicked at PRIVATE\\windows_capture.rs:31:9:\r\n"
                 "fixture window exited early: PRIVATE-ERROR\r\n").encode())
            raise capture.Failure("capture", 101)
        output = io.StringIO()
        with patch.object(capture, "guard"), patch.object(capture, "execute", side_effect=fail), contextlib.redirect_stdout(output):
            self.assertEqual(capture.main(), 1)
        facts = json.loads(output.getvalue().split(": ", 1)[1])
        self.assertEqual(facts["diagnostics"], {"test_log": "complete", "named_test_failed": True,
                         "panic_reason": "fixture_exited", "fixture_stage": "interactive_check", "fixture_exit_code": 3})
        self.assertFalse(paths[0].exists())
        self.assertNotIn("PRIVATE", output.getvalue())

    def test_fixture_phases_and_panic_prefixes_match_reviewed_source(self):
        rust = (capture.REPO / "crates/morpheus-rtc/tests/windows_capture.rs").read_text()
        powershell = (capture.REPO / "scripts/windows_capture_window.ps1").read_text()
        import re
        self.assertEqual(set(re.findall(r'fixture.stage\("([a-z_]+)"\)', rust)), capture.STAGES - {"before_test"})
        self.assertEqual(set(re.findall(r"Write-FixtureStage '([a-z_]+)'", powershell)), capture.FIXTURE_STAGES)
        for prefix in capture.PANIC_REASONS:
            self.assertIn('"' + prefix + '"', rust)
        self.assertIn("Duration::from_secs(15)", rust)
        self.assertIn("Duration::from_secs(10)", rust)
        self.assertIn(".stdout(Stdio::null())", rust)
        self.assertIn(".stderr(Stdio::null())", rust)

    def test_fixture_uses_only_fixed_commands_and_guarded_child_identity(self):
        rust = (capture.REPO / "crates/morpheus-rtc/tests/windows_capture.rs").read_text()
        powershell = (capture.REPO / "scripts/windows_capture_window.ps1").read_text()
        self.assertIn("Some(fixture.child.id())", rust)
        self.assertIn("Some(title.as_str())", rust)
        self.assertIn("fixture requires one isolated display", rust)
        self.assertIn("owned fixture does not cover the full selected display", rust)
        self.assertNotIn("-Command", rust)
        self.assertIn("AllScreens.Count -ne 1", powershell)
        self.assertNotIn("Invoke-Expression", powershell)


class InstalledWindowsUpdateTests(unittest.TestCase):
    def hosted(self, event):
        return {"GITHUB_ACTIONS":"true", "RUNNER_ENVIRONMENT":"github-hosted", "RUNNER_OS":"Windows",
                "GITHUB_REPOSITORY":"brk275/Morpheus", "GITHUB_REF":"refs/heads/main",
                "GITHUB_EVENT_NAME":"workflow_dispatch", "GITHUB_ACTOR":"brk275",
                "GITHUB_TRIGGERING_ACTOR":"brk275", "ImageOS":"win25", "GITHUB_EVENT_PATH":str(event)}

    def test_installed_update_requires_public_owner_manual_standard_windows_selection(self):
        with tempfile.TemporaryDirectory() as temporary:
            event = Path(temporary) / "event.json"
            value = {"repository":{"owner":{"id":7},"private":False}, "sender":{"id":7},
                     "inputs":{"platform":"windows","source_bundle":False}}
            event.write_text(json.dumps(value))
            environment = self.hosted(event)
            update.guard(environment,"win32")
            for name in environment:
                bad = dict(environment); bad.pop(name)
                with self.subTest(missing=name), self.assertRaises((update.Failure,KeyError)):
                    update.guard(bad,"win32")
            for change in ({"repository":{"owner":{"id":7},"private":True}}, {"sender":{"id":8}},
                           {"inputs":{"platform":"linux","source_bundle":False}},
                           {"inputs":{"platform":"windows","source_bundle":True}}):
                event.write_text(json.dumps(value | change))
                with self.subTest(change=change), self.assertRaises(update.Failure):
                    update.guard(environment,"win32")
            event.write_text(json.dumps(value))
            for platform in ("darwin","linux"):
                with self.assertRaises(update.Failure): update.guard(environment,platform)
            with self.assertRaises(update.Failure):
                update.guard(environment | {"RUNNER_ENVIRONMENT":"self-hosted"},"win32")

    def test_compiled_nonce_proof_is_inert_and_rejects_missing_or_changed_dispatch_instrumentation(self):
        nonce = "a"*32
        identity = {"protocol":2,"product":"io.github.brk275.morpheus","version":"0.1.0-updateqa.1",
                    "arch":"x86_64","persistence":"b"*64}
        image = bytearray(12288)
        image[:2]=b"MZ"; struct.pack_into("<I",image,60,128); image[128:132]=b"PE\0\0"
        struct.pack_into("<HH",image,132,0x8664,2); struct.pack_into("<HH",image,148,240,2)
        struct.pack_into("<H",image,152,0x20B); struct.pack_into("<H",image,220,2)
        metadata = json.dumps(identity,separators=(",",":")).encode()
        proof = update.ENTRY_MAGIC+nonce.encode()
        for offset,name,start,data in ((392,b".mrphupd",4096,metadata),(432,b".mrphqa\0",8192,proof)):
            image[offset:offset+8]=name
            struct.pack_into("<I",image,offset+8,len(data)); struct.pack_into("<II",image,offset+16,4096,start)
            struct.pack_into("<I",image,offset+36,0x40000040); image[start:start+len(data)]=data
        self.assertEqual(update.instrumented_entry(bytes(image),nonce),identity)
        for offset,data in ((432,b".omitted"),(8192,b"changed"),(468,struct.pack("<I",0xC0000040)),
                            (8192+len(proof),b"x")):
            bad = image.copy(); bad[offset:offset+len(data)]=data
            with self.subTest(offset=offset), self.assertRaises((update.Failure,ValueError)):
                update.instrumented_entry(bytes(bad),nonce)
        with self.assertRaises(update.Failure): update.instrumented_entry(bytes(image),"c"*32)

    def test_command_wait_uses_retained_handle_and_checks_private_log_bound_before_observing(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            commands = update.Commands(root,{})
            job = Mock()
            job.kernel.WaitForSingleObject.side_effect = [258,0]
            job.wait.return_value = 9
            with patch.object(update.time,"sleep"):
                self.assertEqual(commands.wait(job,123,1),9)
            self.assertEqual(job.kernel.WaitForSingleObject.call_args_list,
                             [unittest.mock.call(123,0),unittest.mock.call(123,0)])
            job.wait.assert_called_once_with(123,0)
            log = commands.logs / "0001.log"
            with log.open("wb") as stream: stream.truncate(8*1024*1024+1)
            job.reset_mock()
            with self.assertRaises(update.Failure): commands.wait(job,123,1)
            job.kernel.WaitForSingleObject.assert_not_called()

    def test_registry_preflight_observes_both_hives_views_and_never_deletes_foreign_keys(self):
        calls = []
        def absent(hive,name,reserved,access):
            calls.append((hive,name,access)); raise FileNotFoundError()
        registry = types.SimpleNamespace(HKEY_CURRENT_USER=1,HKEY_LOCAL_MACHINE=2,KEY_WOW64_64KEY=0x100,
                                        KEY_WOW64_32KEY=0x200,KEY_READ=0x19,OpenKey=absent)
        with patch.dict(sys.modules,{"winreg":registry}):
            update.absent_registry()
        self.assertEqual(len(calls),8)
        self.assertEqual({(hive,access) for hive,_,access in calls},
                         {(1,0x119),(1,0x219),(2,0x119),(2,0x219)})
        registry.OpenKey = Mock(return_value=object()); registry.CloseKey = Mock()
        with patch.dict(sys.modules,{"winreg":registry}), self.assertRaises(update.Failure):
            update.absent_registry()
        registry.CloseKey.assert_called_once()
        self.assertFalse(hasattr(registry,"DeleteKey"))

    def test_failure_cleanup_uses_actual_recovery_then_terminal_probe_before_genuine_uninstall(self):
        with tempfile.TemporaryDirectory() as temporary:
            case = Path(temporary).resolve(); (case / "installed").mkdir()
            state = case / "installed.morpheus-update"; state.mkdir()
            journal = state / "journal.json"; journal.write_text("synthetic journal")
            old = {"executable_sha256":"a"*64,"identity":{"version":"0.1.0-updateqa.1"}}
            new = {"executable_sha256":"b"*64,"identity":{"version":"0.1.0-updateqa.2"}}
            commands = Mock(cleanup_failed=False); order = []
            def run(args,**kwargs):
                order.append(args[-1])
                if args[-1]=="--morpheus-windows-qa-recover": journal.unlink()
            commands.run.side_effect = run
            with (patch.object(update,"digest",side_effect=lambda path,*args: "c"*64 if path.name=="uninstall.exe" else "a"*64),
                  patch.object(update,"registration"),
                  patch.object(update,"uninstall",side_effect=lambda *args:order.append("uninstall"))):
                update.cleanup_case(commands,case,old,new,"c"*64,{})
            self.assertEqual(order,["--morpheus-windows-qa-recover","--morpheus-windows-qa-cleanup","uninstall"])
            journal.write_text("synthetic journal")
            commands.reset_mock(); commands.run.side_effect=update.Failure()
            with (patch.object(update,"digest",return_value="a"*64), patch.object(update,"registration"),
                  patch.object(update,"uninstall") as uninstall, self.assertRaises(update.Failure)):
                update.cleanup_case(commands,case,old,new,"a"*64,{})
            self.assertEqual(commands.run.call_count,1)
            self.assertTrue(journal.exists())
            uninstall.assert_not_called()
            commands.reset_mock()
            with (patch.object(update,"digest",return_value="foreign"), patch.object(update,"registration"),
                  patch.object(update,"uninstall") as uninstall, self.assertRaises(update.Failure)):
                update.cleanup_case(commands,case,old,new,"a"*64,{})
            commands.run.assert_not_called(); uninstall.assert_not_called()

    def test_failed_job_drain_latches_cleanup_failure_and_prevents_recovery_launch(self):
        with tempfile.TemporaryDirectory() as temporary:
            commands = update.Commands(Path(temporary).resolve(),{})
            job = Mock(); job.close.side_effect = RuntimeError("private cleanup failure")
            with self.assertRaises(RuntimeError): commands.close_job(job)
            self.assertTrue(commands.cleanup_failed)
            with patch.object(update,"digest") as digest, self.assertRaises(update.Failure):
                update.cleanup_case(commands,Path(temporary),{}, {},"uninstaller",{})
            digest.assert_not_called()

    def test_foreign_registry_fixture_requires_exact_retained_values_before_cleanup(self):
        fixture = update.ManufacturerFixture.__new__(update.ManufacturerFixture)
        fixture.W = Mock(REG_SZ=1,KEY_WOW64_64KEY=0x100,HKEY_CURRENT_USER=1)
        fixture.mode, fixture.name, fixture.value = "foreign-default", "fixed-name", "exact-nonce-value"
        fixture.product, fixture.vendor, fixture.child = 2,3,None
        fixture.W.QueryInfoKey.side_effect=[(1,0,0),(0,1,0)]
        fixture.W.QueryValueEx.return_value=("foreign-unexpected-value",1)
        with self.assertRaises(update.Failure): fixture.finish()
        fixture.W.DeleteValue.assert_not_called(); fixture.W.DeleteKeyEx.assert_not_called()
        self.assertEqual(update.ManufacturerFixture.MODES,
                         {"invalid-signature":"named-values","truncated":"foreign-default","discard":"subkey",
                          "call-exclusion":"vendor-value"})

    def test_plan_is_create_new_hash_bound_and_keeps_signer_credentials_out_of_actor_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            case = Path(temporary).resolve()
            # No source or fixture credentials are read. The supplied key value
            # is deliberately removed by the closed private environment.
            base = {"USERPROFILE":str(case / "original"),"RUNNER_TEMP":str(case),
                    "CARGO_PACKAGER_SIGN_PRIVATE_KEY":"private fixture key","GITHUB_TOKEN":"private token"}
            plan = {"schema":1,"nonce":"a"*32,"case":"discard"}
            environment = update.actor_environment(case,plan,base)
            self.assertEqual(environment[update.PLAN_HASH],update.hashlib.sha256((case/"plan.json").read_bytes()).hexdigest())
            self.assertNotIn("CARGO_PACKAGER_SIGN_PRIVATE_KEY",environment)
            self.assertNotIn("GITHUB_TOKEN",environment)
            with self.assertRaises(FileExistsError): update.actor_environment(case,plan,base)


if __name__ == "__main__":
    unittest.main()
