import contextlib
import ctypes
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import Mock, patch

from scripts import run_windows_capture_acceptance as capture


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

    def GetCurrentProcess(self):
        return -1

    def DuplicateHandle(self, current, source, destination, result, access, inherit, options):
        self.duplicates += 1
        self.calls.append(("duplicate", inherit, options))
        if self.duplicate_failure == self.duplicates:
            return False
        result._obj.value = 100 + self.duplicates
        return True

    def CreateProcessW(self, executable, command, security, thread_security, inherit, flags,
                       environment, directory, startup, process):
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
        self.assertEqual(facts, {"passed": False, "stage": "window_frames", "failure": "capture", "status": 0xC0000005})
        self.assertNotIn("PRIVATE", output.getvalue())

    def test_unexpected_errors_are_static_and_not_passed(self):
        output = io.StringIO()
        with patch.object(capture, "guard"), patch.object(capture, "execute", side_effect=RuntimeError("PRIVATE")), contextlib.redirect_stdout(output):
            self.assertEqual(capture.main(), 1)
        self.assertNotIn("PRIVATE", output.getvalue())
        self.assertEqual(json.loads(output.getvalue().split(": ", 1)[1])["failure"], "internal")

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
            self.assertEqual(facts, {"passed": False, "stage": "complete", "failure": "internal"})
            self.assertNotIn("PRIVATE", output.getvalue())

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


if __name__ == "__main__":
    unittest.main()
