"""Pure/mocked command ownership checks; no child process or service is run."""

import importlib.util
import base64
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
               timeout=10, status=0, cleanup_error=None, interrupt=False, progress=None,
               stdin_bytes=None, stderr_limit=8 * 1024 * 1024, expected_status=(0,)):
        process = SimpleNamespace(pid=42, returncode=None, stdout=FakePipe(81), stderr=FakePipe(82))
        chunks = {81: [stdout, b""], 82: [stderr, b""]}
        runner = M.OwnedRunner(root / "logs", progress=progress)
        selector = FakeSelector()
        received_input = []
        def spawn(*args, **kwargs):
            if stdin_bytes is not None:
                received_input.append(kwargs["stdin"].read())
            return process
        if interrupt:
            selector.select = mock.Mock(side_effect=KeyboardInterrupt)
        with mock.patch.object(M.containment, "assert_contained"), \
                mock.patch.object(M.containment, "status", return_value=status) as observed, \
                mock.patch.object(M.containment, "stop_owned", side_effect=cleanup_error) as stopped, \
                mock.patch.object(M.selectors, "DefaultSelector", return_value=selector), \
                mock.patch.object(M.subprocess, "Popen", side_effect=spawn) as spawned, \
                mock.patch.object(M.os, "set_blocking"), \
                mock.patch.object(M.os, "read", side_effect=lambda fd, size: chunks[fd].pop(0)):
            try:
                return runner.run("fixture", ["/usr/bin/unused"], cwd=root, env={}, timeout=timeout,
                                  output_limit=limit, binary_output=binary,
                                  stdin_bytes=stdin_bytes, stderr_limit=stderr_limit, expected_status=expected_status)
            finally:
                self.assertEqual(stopped.call_count, 1)
                self.assertIsNone(process.returncode)
                self.assertTrue(process.stdout.closed and process.stderr.closed)
                self.assertTrue(spawned.call_args.kwargs["start_new_session"])
                self.assertEqual(spawned.call_args.kwargs["umask"], 0o077)
                if stdin_bytes is not None:
                    self.assertEqual(received_input, [stdin_bytes])
                    self.assertTrue(spawned.call_args.kwargs["stdin"].closed)

    def test_private_bounded_input_and_source_stderr_limit_preserve_owned_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(self.invoke(Path(directory), stdin_bytes=b"a" * 40 + b"\n"), b"okay")
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CommandFailure) as caught:
            self.invoke(Path(directory), stdin_bytes=b"fixture", stderr_limit=1)
        self.assertEqual(caught.exception.reason, "output")

    def test_admitted_curl_exit_drains_before_status_parsing_without_false_failure(self):
        progress = SimpleNamespace(command=mock.Mock(), failed=mock.Mock(), cleanup=mock.Mock())
        frame = b"\nMORPHEUS_INPUT_STATUS:503:22\n"
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(self.invoke(Path(directory), stdout=frame, binary=True, status=22,
                                         expected_status=(0, 22), progress=progress), frame)
        progress.failed.assert_not_called()
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CommandFailure) as caught:
            self.invoke(Path(directory), stdout=frame, binary=True, status=22, expected_status=(0, 22),
                        cleanup_error=RuntimeError("private cleanup failure"))
        self.assertEqual(caught.exception.reason, "cleanup")

    def test_invalid_input_is_rejected_before_process_creation(self):
        for value in ("not bytes", b"x" * 65537):
            with tempfile.TemporaryDirectory() as directory, \
                    mock.patch.object(M.containment, "assert_contained"), \
                    mock.patch.object(M.subprocess, "Popen") as spawned, self.assertRaises(M.CommandFailure):
                M.OwnedRunner(Path(directory) / "logs").run("source-objects", ["/usr/bin/git"],
                    cwd=directory, env={}, timeout=1, output_limit=1, stdin_bytes=value)
            spawned.assert_not_called()

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
        self.assertNotIn("--header", args)
        api_urls = (
            "https://api.github.com/repos/linuxdeploy/linuxdeploy/releases/assets/602438599",
            "https://api.github.com/repos/linuxdeploy/linuxdeploy-plugin-appimage/releases/assets/602435573",
            "https://api.github.com/repos/AppImage/type2-runtime/releases/assets/657374641",
        )
        for api_url in api_urls:
            with self.subTest(api_url=api_url):
                result = M.download(runner, url=api_url, maximum=5, expected_size=5,
                                    expected_sha256=hashlib.sha256(b"asset").hexdigest(),
                                    root=Path("/private"), environment={})
                self.assertEqual(result, b"asset")
                args = runner.run.call_args.args[1]
                self.assertEqual(args[-1], api_url)
                self.assertEqual(args[args.index("--header") + 1], "Accept: application/octet-stream")
                self.assertFalse(any(arg.lower().startswith("authorization:") for arg in args))
                self.assertEqual(args[args.index("--proto") + 1], "=https")
                self.assertEqual(args[args.index("--proto-redir") + 1], "=https")
                self.assertEqual(args[args.index("--max-redirs") + 1], "3")
                self.assertEqual(args[args.index("--max-filesize") + 1], "5")
                self.assertEqual(runner.run.call_args.kwargs["timeout"], 125)
                self.assertEqual(runner.run.call_args.kwargs["output_limit"], 5)
                self.assertTrue(runner.run.call_args.kwargs["binary_output"])
        source_url = "https://zlib.net/fossils/zlib-1.3.2.tar.gz"
        self.assertEqual(M.download(runner, url=source_url, maximum=5, expected_size=5,
                                   expected_sha256=hashlib.sha256(b"asset").hexdigest(),
                                   root=Path("/private"), environment={}), b"asset")
        self.assertNotIn("--header", runner.run.call_args.args[1])
        with mock.patch.object(P, "download", return_value=b"asset") as downloaded:
            P.download_inputs(runner, Path("/private"), {})
        self.assertEqual(len(downloaded.call_args_list), 6)
        self.assertEqual([call.kwargs["url"] for call in downloaded.call_args_list[:2]], list(api_urls[:2]))
        self.assertFalse(any("/AppImage/type2-runtime/releases/assets/" in call.kwargs["url"]
                             for call in downloaded.call_args_list))
        for pin, call in zip(P.client.pinned_tools(), downloaded.call_args_list[:2]):
            self.assertEqual(call.args, (runner,))
            self.assertEqual(call.kwargs, {
                "url": P.client.tool_download_url(pin), "maximum": pin["size"], "expected_size": pin["size"],
                "expected_sha256": pin["sha256"], "root": Path("/private"), "environment": {},
                "stage": "download-linuxdeploy" if pin["cache_name"] == "linuxdeploy-x86_64.AppImage" else "download-appimage-plugin",
            })

    def test_invalid_origin_size_or_digest_never_accepted(self):
        runner = SimpleNamespace(run=mock.Mock(return_value=b"asset"))
        for url in ("http://github.com/x", "https://user:pass@github.com/x", "https://localhost/x",
                    "https://zlib.net/", "https://zlib.net/fossils/zlib-latest.tar.gz",
                    "https://zlib.net/fossils/zlib-1.3.2.tar.gz/extra",
                    "https://zlib.net/fossils/zlib-1.3.2.tar.gz?private=x",
                    "https://api.github.com/user",
                    "https://api.github.com/repos/other/project/releases/assets/1",
                    "https://api.github.com/repos/AppImage/type2-runtime/releases/latest",
                    "https://api.github.com/repos/AppImage/type2-runtime/releases/assets/0",
                    "https://api.github.com/repos/AppImage/type2-runtime/releases/assets/123/extra",
                    "https://api.github.com/repos/AppImage/type2-runtime/releases/assets/123?private=x",
                    "https://api.github.com/repos/AppImage/type2-runtime/releases/assets/123#private",
                    "https://api.github.com/repos/AppImage/type2-runtime/releases/assets/100000000000000000000"):
            with self.subTest(url=url), self.assertRaises(M.CommandFailure):
                M.download(runner, url=url, maximum=5, root=Path("/private"), environment={})
        runner.run.assert_not_called()
        with self.assertRaises(M.CommandFailure):
            M.download(runner, url="https://github.com/x", maximum=5, expected_sha256="0" * 64,
                       root=Path("/private"), environment={})
        for pin in ({"expected_size": 4}, {"expected_size": 5, "expected_sha256": "0" * 64}):
            with self.subTest(pin=pin), self.assertRaises(M.CommandFailure):
                M.download(runner, url="https://api.github.com/repos/AppImage/type2-runtime/releases/assets/657374641",
                           maximum=5, root=Path("/private"), environment={}, **pin)

    def test_environment_preserves_home_and_removes_secrets_loader_and_signer(self):
        result = P.controlled_environment({"HOME": "/home/runner", "GITHUB_SHA": "a" * 40,
                   "MORPHEUS_PACKAGE_SOURCE_REVISION": "b" * 40,
                   "LD_LIBRARY_PATH": "private", "RUSTFLAGS": "private", "LDAI_SIGN": "private",
                   "GITHUB_TOKEN": "private", "CARGO_REGISTRY_TOKEN": "private", "RUSTC_WRAPPER": "private"})
        self.assertEqual(result["HOME"], "/home/runner")
        self.assertEqual(result["GITHUB_SHA"], "a" * 40)
        self.assertEqual(result["MORPHEUS_PACKAGE_SOURCE_REVISION"], "b" * 40)
        self.assertEqual(result["RUSTUP_TOOLCHAIN"], "1.99.0")
        self.assertNotIn("private", result.values())

    def test_update_compile_inputs_are_only_bounded_loopback_public_metadata(self):
        public = base64.b64encode(b"Ed" + bytes(40)).decode()
        key = base64.b64encode(("untrusted comment: minisign public key 0000000000000000\n" + public + "\n").encode()).decode()
        valid = {"version": "0.1.0-updateqa.1", "feed": "https://127.0.0.1:12345/feed.json", "public_key": key,
                 "source_proof": {"variant": "updateqa-version-only", "version": "0.1.0-updateqa.1", "inventory_sha256": "a" * 64}}
        P.validate_update_parameters(valid)
        alternate = base64.b64encode(b"ED" + bytes(40)).decode()
        P.validate_update_parameters(valid | {"public_key": base64.b64encode(("untrusted comment: minisign public key 0000000000000000\n" + alternate + "\n").encode()).decode()})
        for change in ({"feed": "http://127.0.0.1:12345/feed.json"}, {"feed": "https://remote.invalid/feed.json"},
                       {"feed": "https://user@127.0.0.1:12345/feed.json"}, {"feed": "https://127.0.0.1:12345/feed.json?x=1"},
                       {"version": "0.1.0"}, {"public_key": "private-value"},
                       {"public_key": base64.b64encode(b"untrusted comment: minisign encrypted secret key\nwrong\n").decode()},
                       {"source_proof": {"variant": "arbitrary-export"}}):
            with self.subTest(change=list(change)), self.assertRaises(P.PackageFailure):
                P.validate_update_parameters(valid | change)

    def test_update_build_flag_does_not_authorize_a_nonpublic_context(self):
        with mock.patch.dict(P.os.environ, {"MORPHEUS_FREE_HOSTED_UPDATE_QA": "1"}, clear=True), \
                mock.patch.object(P, "OwnedRunner") as runner, \
                mock.patch.object(P.containment, "assert_contained") as contained, \
                self.assertRaises(P.PackageFailure):
            P.update_build(Path("unused"), "/scanner")
        runner.assert_not_called()
        contained.assert_not_called()

    def test_generic_qa_output_is_private_exact_and_requires_capture_identity(self):
        import hashlib
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root / "checkout"
            checkout.mkdir()
            case = root / "case"
            (case / "packages").mkdir(parents=True)
            data = b"synthetic image bytes" * 5
            (case / "packages/candidate.AppImage").write_bytes(data)
            with mock.patch.object(P, "ROOT", checkout):
                with self.assertRaises(P.PackageFailure):
                    P.qa_output_path(root / "arbitrary-output")
                output = P.qa_output_path(checkout / "dist")
                self.assertEqual(output.stat().st_mode & 0o777, 0o700)
                for receipt in ({"installed_capture_passed": False, "appimage_sha256": hashlib.sha256(data).hexdigest()},
                                {"installed_capture_passed": True, "appimage_sha256": "0" * 64}):
                    with self.assertRaises(P.PackageFailure):
                        P.qa_image_bytes(case, receipt)
                    self.assertEqual(list(output.iterdir()), [])
                receipt = {"installed_capture_passed": True, "appimage_sha256": hashlib.sha256(data).hexdigest()}
                self.assertEqual(P.qa_image_bytes(case, receipt), data)
                identity = P.retain_qa_image(output, data, receipt)
                self.assertEqual((output / "morpheus-qa.AppImage").read_bytes(), data)
                with self.assertRaises(P.PackageFailure):
                    P.retain_qa_image(output, data, receipt)
                P.remove_qa_image(output, identity)
                self.assertEqual(list(output.iterdir()), [])
                (output / "existing").write_bytes(b"unrelated")
                with self.assertRaises(P.PackageFailure):
                    P.qa_output_path(output)

    def test_generic_report_flush_failure_removes_the_private_handoff(self):
        import hashlib
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout, case = root / "checkout", root / "case"
            checkout.mkdir()
            (case / "packages").mkdir(parents=True)
            data = b"synthetic image bytes" * 5
            (case / "packages/candidate.AppImage").write_bytes(data)
            receipt = {"installed_capture_passed": True, "appimage_sha256": hashlib.sha256(data).hexdigest()}
            def reporting(*args, flush=False, **kwargs):
                # An unflushed stream may buffer successfully and fail only at
                # interpreter shutdown, outside main's handoff cleanup scope.
                if flush:
                    raise OSError("synthetic reporting failure")
            with mock.patch.object(P, "ROOT", checkout), \
                    mock.patch.object(P, "public_hosted_environment"), \
                    mock.patch.object(P, "controlled_environment", return_value={}), \
                    mock.patch.object(P, "expected_revision", return_value="a" * 40), \
                    mock.patch.object(P, "verify_checkout"), \
                    mock.patch.object(P, "command_tools", return_value={}), \
                    mock.patch.object(P, "owned_tool", return_value="/reviewed/scanner"), \
                    mock.patch.object(P.containment, "containment_check"), \
                    mock.patch.object(P, "contained", return_value=receipt), \
                    mock.patch.object(P.tempfile, "TemporaryDirectory") as temporary, \
                    mock.patch("builtins.print", side_effect=reporting), self.assertRaises(OSError):
                temporary.return_value.__enter__.return_value = str(case)
                temporary.return_value.__exit__.return_value = False
                P.main(["--gitleaks", "/reviewed/scanner", "--qa-output", str(checkout / "dist")])
            self.assertEqual(list((checkout / "dist").iterdir()), [])

    def test_source_mismatch_stops_before_tool_selection_or_build(self):
        with mock.patch.object(P.sys, "argv", ["package_appimage", "--gitleaks", "/scanner"]), \
                mock.patch.object(P, "public_hosted_environment"), \
                mock.patch.object(P, "expected_revision", return_value="b" * 40), \
                mock.patch.object(P, "verify_checkout", side_effect=P.SourceRevisionError("checkout")), \
                mock.patch.object(P, "command_tools") as tools, \
                mock.patch.object(P, "contained") as contained, \
                self.assertRaises(P.PackageFailure) as error:
            P.main()
        self.assertEqual(error.exception.reason, "identity")
        tools.assert_not_called()
        contained.assert_not_called()

    def test_source_failure_cannot_block_guarded_owned_cleanup(self):
        cleanup = mock.Mock(return_value="cleaned")
        with mock.patch.object(P.sys, "argv", ["package_appimage", "--cleanup", "/private/case"]), \
                mock.patch.object(P, "public_hosted_environment") as hosted, \
                mock.patch.object(P, "controlled_environment", return_value={}), \
                mock.patch.object(P.containment, "assert_contained") as contained, \
                mock.patch.object(P.client, "private_directory") as private, \
                mock.patch.object(P, "verify_checkout", side_effect=P.SourceRevisionError("dirty")) as verify, \
                mock.patch.object(P, "OwnedRunner"), \
                mock.patch.dict(sys.modules, {"appimage_capture": SimpleNamespace(cleanup=cleanup)}):
            self.assertEqual(P.main(), "cleaned")
        hosted.assert_called_once()
        contained.assert_called_once()
        private.assert_called_once_with(Path("/private/case"))
        verify.assert_not_called()
        cleanup.assert_called_once()

    def test_readiness_cannot_be_enabled_by_environment(self):
        self.assertTrue(P.INSTALLED_CAPTURE_READY)  # Source review only.
        with mock.patch.object(P, "INSTALLED_CAPTURE_READY", False), \
                mock.patch.dict(P.os.environ, {"MORPHEUS_APPIMAGE_READY": "1"}), self.assertRaises(P.PackageFailure) as error:
            P.require_readiness()
        self.assertEqual(error.exception.reason, "not_ready")


class PackageInputTests(unittest.TestCase):
    payload = b"pinned asset"
    url = "https://api.github.com/repos/linuxdeploy/linuxdeploy/releases/assets/602438599"

    def frame(self, payload=b"", *, http=200, code=0):
        return payload + ("\nMORPHEUS_INPUT_STATUS:%03d:%d\n" % (http, code)).encode()

    def invoke(self, replies, **changes):
        import hashlib
        runner = SimpleNamespace(run=mock.Mock(side_effect=replies))
        arguments = dict(url=self.url, maximum=64, expected_size=len(self.payload),
                         expected_sha256=hashlib.sha256(self.payload).hexdigest(),
                         root=Path("/private"), environment={}, stage="download-linuxdeploy")
        arguments.update(changes)
        with mock.patch.object(M.time, "sleep") as delayed:
            try:
                result = M.download_package_input(runner, **arguments)
                return result, runner, delayed
            finally:
                self.last_runner, self.last_delay = runner, delayed

    def test_numeric_envelope_is_removed_before_original_pin_verification(self):
        result, runner, delayed = self.invoke([self.frame(self.payload)])
        self.assertEqual(result, self.payload)
        argv = runner.run.call_args.args[1]
        self.assertEqual(runner.run.call_args.args[0], "download-linuxdeploy")
        self.assertEqual(argv[-1], self.url)
        for option, value in (("--proto", "=https"), ("--proto-redir", "=https"),
                              ("--noproxy", "*"), ("--max-redirs", "3"), ("--max-filesize", "64"),
                              ("--write-out", M.PACKAGE_INPUT_FRAME)):
            self.assertEqual(argv[argv.index(option) + 1], value)
        headers = [argv[index + 1] for index, item in enumerate(argv) if item == "--header"]
        self.assertEqual(headers, ["Accept: application/octet-stream", "Cache-Control: no-cache"])
        self.assertTrue(runner.run.call_args.kwargs["binary_output"])
        self.assertEqual(runner.run.call_args.kwargs["output_limit"], 64 + M.PACKAGE_INPUT_FRAME_LIMIT)
        self.assertGreater(runner.run.call_args.kwargs["timeout"], 0)
        self.assertLessEqual(runner.run.call_args.kwargs["timeout"], 125)
        self.assertIn(22, runner.run.call_args.kwargs["expected_status"])
        delayed.assert_not_called()

    def test_partial_transfers_restart_with_fresh_bounded_payload(self):
        for code in (18, 28, 52, 55, 56):
            with self.subTest(code=code):
                result, runner, delayed = self.invoke([
                    self.frame(b"discard partial", http=200, code=code), self.frame(self.payload)])
                self.assertEqual(result, self.payload)
                self.assertEqual(runner.run.call_count, 2)
                delayed.assert_called_once_with(1.0)
                self.assertTrue(all(call.kwargs["output_limit"] == 128 for call in runner.run.call_args_list))

    def test_transient_http_has_at_most_three_attempts_with_one_total_budget(self):
        for http in (408, 500, 502, 503, 504):
            with self.subTest(http=http):
                result, runner, delayed = self.invoke([
                    self.frame(http=http, code=22), self.frame(http=http, code=22), self.frame(self.payload)])
                self.assertEqual(result, self.payload)
                self.assertEqual(runner.run.call_count, 3)
                self.assertEqual(delayed.call_args_list, [mock.call(1.0), mock.call(2.0)])
        with self.assertRaises(M.PackageDownloadFailure) as caught:
            self.invoke([self.frame(http=503, code=22)] * 3)
        self.assertEqual(caught.exception.reason, "http_unavailable")
        self.assertEqual(self.last_runner.run.call_count, 3)
        with mock.patch.object(M.time, "monotonic", side_effect=[0, 0, 0, 125]), \
                self.assertRaises(M.CommandFailure) as caught:
            self.invoke([self.frame(http=503, code=22)])
        self.assertEqual(caught.exception.reason, "deadline")
        self.assertEqual(self.last_runner.run.call_count, 1)
        self.last_delay.assert_not_called()

    def test_permanent_http_tls_and_rate_limits_never_retry_or_echo_body(self):
        for http, code, reason in ((403, 22, "http_forbidden"), (404, 22, "http_not_found"),
                                   (429, 22, "http_rate_limited"), (401, 22, "http_rejected"),
                                   (0, 35, "download_tls"), (0, 51, "download_tls"), (0, 60, "download_tls")):
            with self.subTest(http=http, code=code), self.assertRaises(M.PackageDownloadFailure) as caught:
                self.invoke([self.frame(b"private reflected server body", http=http, code=code)])
            self.assertEqual(caught.exception.reason, reason)
            self.assertNotIn("private", str(caught.exception))
            self.assertEqual(self.last_runner.run.call_count, 1)
            self.last_delay.assert_not_called()

    def test_bad_frame_hash_size_or_misleading_status_is_never_retried(self):
        for output in (self.payload, self.frame(self.payload)[:-1], self.frame(b"wrong"),
                       self.frame(self.payload, http=403), self.frame(self.payload, http=0),
                       self.frame(self.payload, http=999), self.frame(self.payload, code=99),
                       self.frame(self.frame(self.payload))):
            with self.subTest(output=output[-40:]), self.assertRaises(M.CommandFailure):
                self.invoke([output])
            self.assertEqual(self.last_runner.run.call_count, 1)
            self.last_delay.assert_not_called()
        with self.assertRaises(M.CommandFailure):
            self.invoke([self.frame(self.payload)], expected_size=len(self.payload) - 1)
        self.assertEqual(self.last_runner.run.call_count, 1)
        self.last_delay.assert_not_called()

    def test_invalid_contract_starts_no_command(self):
        for changes in ({"url": "http://github.com/x"}, {"url": "https://user:private@github.com/x"},
                        {"url": "https://github.com/x?private=x"}, {"url": "https://github.com:bad/x"},
                        {"url": "https://localhost/x"}, {"url": "https://api.github.com/user"},
                        {"stage": "private-token"}, {"maximum": True}, {"maximum": 0},
                        {"maximum": 64 * 1024 * 1024}, {"expected_size": True}, {"expected_size": 65},
                        {"expected_sha256": None}, {"expected_sha256": "private"}):
            with self.subTest(changes=changes), self.assertRaises(M.CommandFailure):
                self.invoke([], **changes)
            self.last_runner.run.assert_not_called()

    def test_owned_failures_and_interrupts_are_never_retried(self):
        for error in [M.CommandFailure(reason) for reason in ("cleanup", "output", "deadline", "pipe", "input", "exit")]:
            with self.subTest(reason=error.reason), self.assertRaises(M.CommandFailure) as caught:
                self.invoke([error])
            self.assertIs(caught.exception, error)
            self.assertEqual(self.last_runner.run.call_count, 1)
            self.last_delay.assert_not_called()
        with self.assertRaises(KeyboardInterrupt):
            self.invoke([KeyboardInterrupt()])
        self.assertEqual(self.last_runner.run.call_count, 1)


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
