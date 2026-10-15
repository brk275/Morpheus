"""Pure/mocked command ownership checks; no child process or service is run."""

import importlib.util
import io
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "packaging/linux"))
import appimage_process as M
import package_appimage as P


class FakePipe:
    def __init__(self, number):
        self.number = number
        self.closed = False

    def fileno(self):
        return self.number

    def close(self):
        self.closed = True


class FakeSelector:
    def __init__(self):
        self.entries = {}

    def register(self, stream, event, kind):
        self.entries[stream] = kind

    def unregister(self, stream):
        del self.entries[stream]

    def get_map(self):
        return self.entries

    def select(self, timeout):
        return [(SimpleNamespace(fileobj=stream, data=kind), 1) for stream, kind in list(self.entries.items())]

    def close(self):
        self.entries.clear()


class ProcessTests(unittest.TestCase):
    def invoke(self, root, *, stdout=b"okay", stderr=b"private", limit=4096, binary=False,
               timeout=10, status=0, cleanup_error=None, interrupt=False, progress=None):
        process = SimpleNamespace(pid=42, returncode=None, stdout=FakePipe(81), stderr=FakePipe(82))
        chunks = {81: [stdout, b""], 82: [stderr, b""]}
        runner = M.OwnedRunner(root / "logs", progress=progress)
        selector = FakeSelector()
        if interrupt:
            selector.select = mock.Mock(side_effect=KeyboardInterrupt)
        with mock.patch.object(M.containment, "assert_contained"), \
                mock.patch.object(M.containment, "status", return_value=status) as observed, \
                mock.patch.object(M.containment, "stop_owned", side_effect=cleanup_error) as stopped, \
                mock.patch.object(M.selectors, "DefaultSelector", return_value=selector), \
                mock.patch.object(M.subprocess, "Popen", return_value=process) as spawned, \
                mock.patch.object(M.os, "set_blocking"), \
                mock.patch.object(M.os, "read", side_effect=lambda fd, size: chunks[fd].pop(0)):
            try:
                return runner.run("fixture", ["/usr/bin/unused"], cwd=root, env={}, timeout=timeout,
                                  output_limit=limit, binary_output=binary)
            finally:
                self.assertEqual(stopped.call_count, 1)
                self.assertIsNone(process.returncode)
                self.assertTrue(process.stdout.closed and process.stderr.closed)
                self.assertTrue(spawned.call_args.kwargs["start_new_session"])

    def test_success_private_output_and_group_cleanup_before_return(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(self.invoke(root), b"okay")
            self.assertEqual((root / "logs/0001-fixture.stderr.log").read_bytes(), b"private")
            self.assertEqual((root / "logs/0001-fixture.stdout.log").stat().st_mode & 0o777, 0o600)

    def test_binary_download_not_written_to_text_log(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(self.invoke(root, binary=True), b"okay")
            self.assertEqual((root / "logs/0001-fixture.stdout.log").read_bytes(), b"")

    def test_output_limit_kills_owned_group(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CommandFailure) as error:
            self.invoke(Path(directory), limit=1)
        self.assertEqual(error.exception.reason, "output")

    def test_interrupt_retained_after_group_cleanup(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(KeyboardInterrupt):
            self.invoke(Path(directory), interrupt=True)

    def test_nonzero_and_cleanup_errors_fail_closed(self):
        for status, cleanup, reason in ((1, None, "exit"), (0, RuntimeError("private cleanup"), "cleanup")):
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CommandFailure) as error:
                self.invoke(Path(directory), status=status, cleanup_error=cleanup)
            self.assertEqual(error.exception.reason, reason)

    def test_supervisor_records_cleanup_failure_even_when_command_exit_is_preserved(self):
        from appimage_capture_worker import WorkerSupervision
        progress = WorkerSupervision()
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CommandFailure) as caught:
            self.invoke(Path(directory), status=1, cleanup_error=RuntimeError("private cleanup"), progress=progress)
        self.assertEqual(caught.exception.reason, "exit")
        self.assertTrue(progress.cleanup_failed)

    def test_full_write_handles_partial_and_zero_writes(self):
        class Partial(io.BytesIO):
            def write(self, data):
                return super().write(data[:2])
        output = Partial()
        M.full_write(output, b"abcdef")
        self.assertEqual(output.getvalue(), b"abcdef")
        with self.assertRaises(M.CommandFailure):
            M.full_write(SimpleNamespace(write=lambda value: 0), b"abcdef")

    def test_diagnostic_failure_cannot_skip_owned_child_cleanup(self):
        progress = SimpleNamespace(command=mock.Mock(), failed=mock.Mock(side_effect=OSError("private")),
                                   cleanup=mock.Mock(side_effect=OSError("private")))
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CommandFailure) as error:
            self.invoke(Path(directory), status=1, cleanup_error=RuntimeError("private"), progress=progress)
        self.assertEqual(error.exception.reason, "exit")
        progress.failed.assert_called_once()
        progress.cleanup.assert_called_once_with(False)


class DownloadAndEnvironmentTests(unittest.TestCase):
    def test_exact_digest_and_https_redirect_flags(self):
        import hashlib
        runner = SimpleNamespace(run=mock.Mock(return_value=b"asset"))
        result = M.download(runner, url="https://github.com/project/release/asset", maximum=5,
                            expected_size=5, expected_sha256=hashlib.sha256(b"asset").hexdigest(),
                            root=Path("/private"), environment={})
        self.assertEqual(result, b"asset")
        args = runner.run.call_args.args[1]
        self.assertIn("--disable", args)
        self.assertEqual(args[args.index("--proto-redir") + 1], "=https")
        self.assertEqual(args[args.index("--noproxy") + 1], "*")
        self.assertTrue(runner.run.call_args.kwargs["binary_output"])

    def test_invalid_origin_size_or_digest_never_accepted(self):
        runner = SimpleNamespace(run=mock.Mock(return_value=b"asset"))
        for url in ("http://github.com/x", "https://user:pass@github.com/x", "https://localhost/x"):
            with self.subTest(url=url), self.assertRaises(M.CommandFailure):
                M.download(runner, url=url, maximum=5, root=Path("/private"), environment={})
        runner.run.assert_not_called()
        with self.assertRaises(M.CommandFailure):
            M.download(runner, url="https://github.com/x", maximum=5, expected_sha256="0" * 64,
                       root=Path("/private"), environment={})

    def test_environment_preserves_home_and_removes_secrets_loader_and_signer(self):
        result = P.controlled_environment({"HOME": "/home/runner", "GITHUB_SHA": "a" * 40,
                   "LD_LIBRARY_PATH": "private", "RUSTFLAGS": "private", "LDAI_SIGN": "private",
                   "GITHUB_TOKEN": "private", "CARGO_REGISTRY_TOKEN": "private", "RUSTC_WRAPPER": "private"})
        self.assertEqual(result["HOME"], "/home/runner")
        self.assertEqual(result["RUSTUP_TOOLCHAIN"], "1.99.0")
        self.assertNotIn("private", result.values())

    def test_readiness_cannot_be_enabled_by_environment(self):
        self.assertTrue(P.INSTALLED_CAPTURE_READY)  # Source review only.
        with mock.patch.object(P, "INSTALLED_CAPTURE_READY", False), \
                mock.patch.dict(P.os.environ, {"MORPHEUS_APPIMAGE_READY": "1"}), self.assertRaises(P.PackageFailure) as error:
            P.require_readiness()
        self.assertEqual(error.exception.reason, "not_ready")


class OuterNamespaceTests(unittest.TestCase):
    def test_failed_namespace_is_stopped_then_host_fixture_cleaned_then_scanned(self):
        with tempfile.TemporaryDirectory() as directory:
            order = []
            child = SimpleNamespace(pid=42)
            with mock.patch.object(P.containment, "namespace_command", return_value=["/usr/bin/unused"]), \
                    mock.patch.object(P.subprocess, "Popen", return_value=child), \
                    mock.patch.object(P.containment, "status", return_value=1), \
                    mock.patch.object(P.containment, "stop_owned", side_effect=lambda p: order.append("stop")), \
                    mock.patch.object(P, "cleanup_owned_fixture", side_effect=lambda *a: order.append("fixture")), \
                    mock.patch.object(P, "scan_private_logs", side_effect=lambda *a: order.append("scan")), \
                    self.assertRaises(P.PackageFailure):
                P.contained(Path(directory), "/private/scanner", {"cargo": "/private/cargo"}, {})
            self.assertEqual(order, ["stop", "fixture", "scan"])

    def test_interrupt_is_preserved_and_cleanup_scan_are_still_attempted(self):
        with tempfile.TemporaryDirectory() as directory:
            order = []
            child = SimpleNamespace(pid=42)
            def broken_cleanup(*args):
                order.append("fixture")
                raise RuntimeError("private child body")
            with mock.patch.object(P.containment, "namespace_command", return_value=["/usr/bin/unused"]), \
                    mock.patch.object(P.subprocess, "Popen", return_value=child), \
                    mock.patch.object(P.containment, "status", side_effect=KeyboardInterrupt), \
                    mock.patch.object(P.containment, "stop_owned", side_effect=lambda p: order.append("stop")), \
                    mock.patch.object(P, "cleanup_owned_fixture", side_effect=broken_cleanup), \
                    mock.patch.object(P, "scan_private_logs", side_effect=lambda *a: order.append("scan")), \
                    self.assertRaises(KeyboardInterrupt):
                P.contained(Path(directory), "/private/scanner", {"cargo": "/private/cargo"}, {})
            self.assertEqual(order, ["stop", "fixture", "scan"])


if __name__ == "__main__":
    unittest.main()
