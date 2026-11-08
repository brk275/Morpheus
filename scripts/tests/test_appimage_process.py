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
    @unittest.skipUnless(sys.platform == "linux", "Linux complete private log snapshot")
    def test_owned_log_snapshot_is_complete_independent_and_scans_once(self):
        import fixture_paths as F
        import appimage_update_acceptance as A
        original_mkdtemp = tempfile.mkdtemp
        for action in ("valid", "valid_large", "finding", "warning", "stdout", "malformed", "missing",
                       "source_changed", "source_added", "snapshot_changed", "snapshot_extra",
                       "symlink", "oversize", "process_cleanup", "source_fifo_race", "report_fifo",
                       "coverage_short", "coverage_extra", "coverage_missing", "coverage_duplicate",
                       "coverage_malformed", "source_partial_copy", "partial_copy_unknown_inode"):
            with self.subTest(action=action), tempfile.TemporaryDirectory() as temporary:
                fixture = Path(temporary).resolve()
                root = fixture / "source"
                root.mkdir(mode=0o700)
                bodies = {"a.log":b"first\x00raw\nbody", "b.log":b"second separate body\n"}
                if action == "valid_large":
                    bodies["a.log"] = b"first\x00" + b"A" * 200_000 + b"tail\n"
                for name, value in bodies.items():
                    P.client.write_new(root / name, value)
                foreign = fixture / "foreign"
                P.client.write_new(foreign, b"preserve this unrelated fixture")
                if action == "symlink":
                    (root / "a.log").unlink()
                    (root / "a.log").symlink_to(foreign)
                if action == "oversize":
                    with (root / "a.log").open("wb") as stream:
                        stream.truncate(8 * 1024 * 1024 + 1)
                snapshots = []
                original_open = F.os.open
                original_read = F.os.read
                source_identity = ((root / "a.log").lstat().st_dev, (root / "a.log").lstat().st_ino)
                replaced = {"source":False, "report_opened":False, "partial_reads":0,
                            "unknown_snapshot":False}
                def raced_read(descriptor, maximum):
                    info = F.os.fstat(descriptor)
                    if (action in {"source_partial_copy", "partial_copy_unknown_inode"}
                            and (info.st_dev, info.st_ino) == source_identity):
                        replaced["partial_reads"] += 1
                        if replaced["partial_reads"] == 1:
                            return original_read(descriptor, min(5, maximum))
                        if action == "partial_copy_unknown_inode":
                            path = snapshots[0] / "data/00000000.log"
                            path.unlink()
                            P.client.write_new(path, b"keep unknown replacement")
                            replaced["unknown_snapshot"] = True
                        raise OSError("synthetic owned-copy read refusal")
                    return original_read(descriptor, maximum)
                def raced_open(path, flags, *args, **options):
                    if action == "report_fifo" and path == "report.json":
                        replaced["report_opened"] = True
                        self.fail("Unexpected snapshot FIFO must never be opened")
                    if action == "source_fifo_race" and path == "a.log" and not replaced["source"]:
                        self.assertTrue(flags & F.os.O_NONBLOCK)
                        (root / "a.log").unlink()
                        F.os.mkfifo(root / "a.log", 0o600)
                        replaced["source"] = True
                    return original_open(path, flags, *args, **options)
                def allocate(*args, **options):
                    self.assertEqual(options["dir"], Path("/tmp"))
                    path = original_mkdtemp(prefix=options["prefix"], dir=fixture)
                    snapshots.append(Path(path))
                    return path
                def scanner(arguments, *, cwd, deadline):
                    self.assertEqual(arguments[0:2], ["/scanner", "dir"])
                    self.assertEqual(deadline, 40)
                    data = Path(arguments[2])
                    safe_bodies = ([bodies["b.log"]] if action in {"symlink", "oversize", "source_fifo_race", "source_partial_copy"}
                                   else list(bodies.values()))
                    expected_names = (["00000001.log"] if action in {"source_fifo_race", "source_partial_copy"} else
                                      [str(index).zfill(8) + ".log" for index in range(len(safe_bodies))])
                    self.assertEqual(sorted(p.name for p in data.iterdir()), expected_names)
                    self.assertEqual([p.read_bytes() for p in sorted(data.iterdir())], safe_bodies)
                    self.assertEqual(Path(arguments[arguments.index("--config") + 1]).read_bytes(),
                                     b"[extend]\nuseDefault = true\n")
                    self.assertEqual(Path(arguments[arguments.index("--gitleaks-ignore-path") + 1]).read_bytes(), b"")
                    self.assertEqual(arguments[arguments.index("--max-target-megabytes") + 1], "0")
                    self.assertEqual(arguments[arguments.index("--log-level") + 1], "info")
                    self.assertIn("--no-color", arguments)
                    self.assertEqual(arguments[arguments.index("--max-decode-depth") + 1], "5")
                    self.assertEqual(arguments[arguments.index("--max-archive-depth") + 1], "0")
                    self.assertIn("--ignore-gitleaks-allow", arguments)
                    self.assertEqual(arguments[arguments.index("--report-path") + 1], "-")
                    if action == "process_cleanup":
                        raise F.PrivateLogScanError(process_stopped=False)
                    if action == "report_fifo":
                        F.os.mkfifo(Path(cwd) / "report.json", 0o600)
                    report_body = (b"" if action == "missing" else b"{bad" if action == "malformed" else
                                   b'[{"RuleID":"synthetic"}]' if action == "finding" else b"[]\n")
                    if action == "source_changed": (root / "a.log").write_bytes(b"changed source")
                    if action == "source_added": P.client.write_new(root / "c.log", b"new unscanned original")
                    if action == "snapshot_changed": (data / "00000000.log").write_bytes(b"changed copy")
                    if action == "snapshot_extra": P.client.write_new(data / "extra.log", b"unknown copy")
                    count = sum(len(value) for value in safe_bodies)
                    if action == "coverage_short": count -= 1
                    if action == "coverage_extra": count += 1
                    display = "200.03 KB" if action == "valid_large" else f"{count} bytes"
                    duration = "1µs" if action == "valid" else "1ms"
                    footer = (f"3:04PM INF scanned ~{count} bytes ({display}) in {duration}\n"
                              "3:04PM INF no leaks found\n").encode()
                    if action == "coverage_missing": footer = b""
                    if action == "coverage_duplicate": footer += footer
                    if action == "coverage_malformed": footer = footer.replace(b"scanned ~", b"scanned ?")
                    return (1 if action == "finding" else 0,
                            b"[]" + b" " * 4096 if action == "stdout" else report_body,
                            footer + b"partial scan warning" if action == "warning" else footer)
                with mock.patch.object(F.tempfile, "mkdtemp", side_effect=allocate), \
                        mock.patch.object(F.os, "open", side_effect=raced_open), \
                        mock.patch.object(F.os, "read", side_effect=raced_read), \
                        mock.patch.object(P.time, "monotonic", return_value=10), \
                        mock.patch.object(F, "_run_log_scanner", side_effect=scanner) as invoked:
                    if action in {"valid", "valid_large"}:
                        P.scan_private_logs(root, "/scanner")
                        self.assertEqual(invoked.call_count, 1)
                        # The outer owner also uses explicit independent defaults.
                        A.Commands(root, env={}).scan("/scanner")
                        self.assertEqual(invoked.call_count, 2)
                    else:
                        with self.assertRaises(P.PackageFailure):
                            P.scan_private_logs(root, "/scanner")
                        self.assertEqual(invoked.call_count, 0 if action == "partial_copy_unknown_inode" else 1)
                if action in {"source_partial_copy", "partial_copy_unknown_inode"}:
                    self.assertEqual(replaced["partial_reads"], 2)
                if action == "partial_copy_unknown_inode":
                    self.assertTrue(replaced["unknown_snapshot"])
                    self.assertEqual((snapshots[0] / "data/00000000.log").read_bytes(),
                                     b"keep unknown replacement")
                if action == "report_fifo":
                    self.assertFalse(replaced["report_opened"])
                self.assertEqual(foreign.read_bytes(), b"preserve this unrelated fixture")
                if action in {"snapshot_changed", "snapshot_extra", "process_cleanup", "report_fifo", "partial_copy_unknown_inode"}:
                    self.assertTrue(snapshots[0].exists())
                elif snapshots:
                    self.assertTrue(all(not path.exists() for path in snapshots))
                if action not in {"symlink", "oversize", "source_changed", "source_fifo_race"}:
                    self.assertEqual((root / "a.log").read_bytes(), bodies["a.log"])
                if action == "source_fifo_race":
                    self.assertTrue(replaced["source"])
                    self.assertTrue(P.stat.S_ISFIFO((root / "a.log").lstat().st_mode))
                self.assertEqual((root / "b.log").read_bytes(), bodies["b.log"])
        with tempfile.TemporaryDirectory() as temporary, \
                mock.patch.object(P.time, "monotonic", return_value=10), \
                mock.patch.object(F, "_run_log_scanner") as scanner:
            with self.assertRaises(P.PackageFailure):
                P.scan_private_logs(Path(temporary).resolve(), "/scanner", deadline=10)
            scanner.assert_not_called()


    @unittest.skipUnless(sys.platform == "linux", "Linux private scanner ownership")
    def test_private_scanner_interrupt_carries_original_and_known_stop_receipt(self):
        import fixture_paths as F
        for interruption in (KeyboardInterrupt(), SystemExit(7)):
            for stop_fails in (False, True):
                with self.subTest(kind=type(interruption).__name__, stop_fails=stop_fails):
                    process = mock.Mock(stdout=FakePipe(81), stderr=FakePipe(82))
                    process.poll.return_value = None
                    process.wait.return_value = 0
                    if stop_fails:
                        process.kill.side_effect = OSError("synthetic private stop refusal")
                    with mock.patch("subprocess.Popen", return_value=process), \
                            mock.patch("select.select", side_effect=interruption), \
                            mock.patch("time.monotonic", return_value=10), \
                            self.assertRaises(F.PrivateLogScanError) as caught:
                        F._run_log_scanner(["/unused-scanner"], cwd=Path("/tmp"), deadline=40)
                    self.assertIs(caught.exception.interruption, interruption)
                    self.assertIs(caught.exception.process_stopped, not stop_fails)
                    process.kill.assert_called_once_with()
                    if not stop_fails:
                        self.assertGreaterEqual(process.wait.call_count, 1)
                    self.assertTrue(process.stdout.closed and process.stderr.closed)

    @unittest.skipUnless(sys.platform == "linux", "Linux private snapshot cleanup")
    def test_interrupted_snapshot_cleanup_requires_stopped_scanner_and_exact_inventory(self):
        import fixture_paths as F
        allocate_real = tempfile.mkdtemp
        for kind in (KeyboardInterrupt, SystemExit):
            for mode in ("known_stop", "unknown_stop", "unknown_interrupt", "snapshot_extra", "snapshot_fifo"):
                with self.subTest(kind=kind.__name__, mode=mode), tempfile.TemporaryDirectory() as temporary:
                    fixture = Path(temporary).resolve()
                    root = fixture / "source"
                    root.mkdir(mode=0o700)
                    log = root / "private.log"
                    P.client.write_new(log, b"synthetic private log")
                    snapshots = []
                    interruption = kind()
                    def allocate(*args, **options):
                        path = Path(allocate_real(prefix=options["prefix"], dir=fixture))
                        snapshots.append(path)
                        return str(path)
                    def scanner(arguments, *, cwd, deadline):
                        self.assertEqual(arguments[arguments.index("--report-path") + 1], "-")
                        self.assertEqual(set(Path(cwd).iterdir()),
                                         {Path(cwd) / name for name in ("data", "gitleaks.toml", "empty.ignore")})
                        if mode == "snapshot_extra":
                            P.client.write_new(Path(cwd) / "unknown", b"preserve unknown bytes")
                        if mode == "snapshot_fifo":
                            F.os.mkfifo(Path(cwd) / "unknown", 0o600)
                        if mode == "unknown_interrupt":
                            raise interruption
                        raise F.PrivateLogScanError(process_stopped=mode != "unknown_stop",
                                                    interruption=interruption)
                    with mock.patch.object(F.tempfile, "mkdtemp", side_effect=allocate), \
                            mock.patch.object(F, "_run_log_scanner", side_effect=scanner), \
                            self.assertRaises(kind) as caught:
                        F.scan_owned_logs(root, Path("/scanner"))
                    self.assertIs(caught.exception, interruption)
                    self.assertEqual(len(snapshots), 1)
                    retained = mode != "known_stop"
                    self.assertEqual(snapshots[0].exists(), retained)
                    self.assertEqual(log.read_bytes(), b"synthetic private log")
                    if retained:
                        self.assertEqual((snapshots[0] / "data/00000000.log").read_bytes(),
                                         b"synthetic private log")
                    if mode == "snapshot_extra":
                        self.assertEqual((snapshots[0] / "unknown").read_bytes(), b"preserve unknown bytes")
                    if mode == "snapshot_fifo":
                        self.assertTrue(P.stat.S_ISFIFO((snapshots[0] / "unknown").lstat().st_mode))

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
            "https://api.github.com/repos/linuxdeploy/linuxdeploy/releases/assets/699514588",
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
        self.assertEqual(downloaded.call_args_list[-1].kwargs["url"],
                         "https://api.github.com" + M.PIPEWIRE_PATCH_API_PATH)
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

    def test_only_pinned_pipewire_commit_api_uses_fixed_patch_header(self):
        self.assertEqual(M.PIPEWIRE_PATCH_SHA256, P.client.R.UPSTREAM_PATCH_SHA256)
        self.assertEqual(M.PIPEWIRE_PATCH_API_PATH, "/repos/PipeWire/pipewire/commits/" + P.client.R.COMMIT)
        url = "https://api.github.com" + M.PIPEWIRE_PATCH_API_PATH
        for packaged in (False, True):
            payload = b"wrong patch bytes"
            if packaged:
                payload += b"\nMORPHEUS_INPUT_STATUS:200:0\n"
            runner = SimpleNamespace(run=mock.Mock(return_value=payload))
            download = M.download_package_input if packaged else M.download
            with self.subTest(packaged=packaged), self.assertRaises(M.CommandFailure) as caught:
                download(runner, url=url, maximum=32768, expected_sha256=M.PIPEWIRE_PATCH_SHA256,
                         root=Path("/private"), environment={})
            # Admit only the fixed route; wrong bytes must still fail its pin.
            self.assertEqual(caught.exception.reason, "input")
            runner.run.assert_called_once()
            argv = runner.run.call_args.args[1]
            self.assertEqual(argv[-1], url)
            headers = [argv[i + 1] for i, item in enumerate(argv) if item == "--header"]
            self.assertEqual(headers, ["Accept: application/vnd.github.patch"])
            self.assertEqual(argv[argv.index("--proto") + 1], "=https")
            self.assertEqual(argv[argv.index("--proto-redir") + 1], "=https")
            self.assertEqual(argv[argv.index("--max-filesize") + 1], "32768")
            runner.run.reset_mock()
            for changes in ({"url": url[:-1] + "0"}, {"url": url + "?private=x"},
                            {"url": url + "/extra"}, {"url": url.replace("PipeWire", "other")},
                            {"expected_sha256": "0" * 64}, {"maximum": 32769},
                            {"maximum": 32768.0}, {"expected_size": 2182}):
                arguments = dict(url=url, maximum=32768, expected_sha256=M.PIPEWIRE_PATCH_SHA256,
                                 root=Path("/private"), environment={})
                arguments.update(changes)
                with self.subTest(packaged=packaged, changes=changes), self.assertRaises(M.CommandFailure):
                    download(runner, **arguments)
            runner.run.assert_not_called()

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
        import corresponding_source as S
        import source_bundle_qa as Q
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout, case = root / "checkout", root / "case"
            checkout.mkdir()
            (case / "packages").mkdir(parents=True)
            data = b"synthetic image bytes" * 5
            (case / "packages/candidate.AppImage").write_bytes(data)
            artifact_hash = hashlib.sha256(data).hexdigest()
            receipt = {"installed_capture_passed": True, "appimage_sha256": artifact_hash,
                "corresponding_source": {"schema": 1, "source_revision": "a" * 40, "source_tree": "b" * 40,
                    "bundle_sha256": "c" * 64, "manifest_sha256": "d" * 64,
                    "registry_packages": 1, "git_sources": 1, "locked_packages": 2,
                    "distribution_ready": False, "unresolved_release_requirements": list(S.UNRESOLVED),
                    "native_sources": {"coverage": "receipt-bound-known-native-components-only",
                        "package_artifact_sha256": artifact_hash, "build_receipt_sha256": "e" * 64,
                        "native_sources_complete": False, "actual_native_build_inputs_verified": False,
                        "raw_receipts_included": False, "native_lifecycle_acceptance": False}}}
            S.validate_receipt(receipt["corresponding_source"], "a" * 40, native_artifact=artifact_hash)
            def reporting(*args, flush=False, **kwargs):
                # An unflushed stream may buffer successfully and fail only at
                # interpreter shutdown, outside main's handoff cleanup scope.
                if flush:
                    raise OSError("synthetic reporting failure")
            with mock.patch.object(P, "ROOT", checkout), \
                    mock.patch.object(P, "public_hosted_environment"), \
                    mock.patch.object(P, "controlled_environment", return_value={}), \
                    mock.patch.object(Q, "guard_native"), \
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


class PackageConfigPreflightTests(unittest.TestCase):
    def test_real_cli_parse_preflight_precedes_native_build_and_keeps_actual_formats(self):
        import copy
        import json

        class NativeBuildReached(Exception):
            pass

        for rejected in (False, True):
            with self.subTest(rejected=rejected), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                stage = root / "client"
                stage.mkdir()
                runtime_notices = root / "runtime-notices"
                runtime_notices.mkdir()
                base = {"name": "morpheus-app", "formats": ["appimage"],
                        "before-packaging-command": "must-not-run", "deb": {"files": {}}}
                saved = copy.deepcopy(base)
                generated = []
                actual_config = P.client.appimage_config

                def config(value, directory):
                    result = actual_config(value, directory)
                    generated.append(result)
                    return result

                calls = []

                def run(label, argv, **options):
                    calls.append(label)
                    if label == "native-build":
                        raise NativeBuildReached()
                    self.assertEqual(label, "package-config")
                    self.assertEqual(argv[:4], ["/tools/cargo", "packager", "--release", "--config"])
                    self.assertEqual(len(argv), 5)
                    probe = json.loads(argv[4])
                    self.assertEqual(probe["formats"], [])
                    self.assertEqual(probe["appimage"]["excludedLibs"], ["libpipewire-0.3.so*"])
                    self.assertNotIn("before-packaging-command", probe)
                    self.assertNotIn("--formats", argv)
                    self.assertEqual(options["cwd"], root)
                    self.assertEqual(options["timeout"], 30)
                    self.assertEqual(options["output_limit"], 65536)
                    self.assertEqual(options["stderr_limit"], 65536)
                    if rejected:
                        raise M.CommandFailure("exit")
                    return b""

                runner = SimpleNamespace(run=run)
                with mock.patch.object(P, "ROOT", root), \
                        mock.patch.object(P, "check_versions"), \
                        mock.patch.object(P, "download_inputs", return_value=({}, {}, b"")), \
                        mock.patch.object(P.runtime_builder, "prepare", return_value=(b"runtime", {}, runtime_notices)), \
                        mock.patch.object(P.client, "seed_tools", return_value={}), \
                        mock.patch.object(P.client, "prepare_client", return_value=(stage, {})), \
                        mock.patch.object(P.client, "build_receipt", return_value=root / "receipt.json"), \
                        mock.patch.object(P.client, "appimage_config", side_effect=config), \
                        mock.patch.object(P, "package_config", return_value=base), \
                        mock.patch.object(P.client, "read_regular") as binary_read:
                    expected = M.CommandFailure if rejected else NativeBuildReached
                    with self.assertRaises(expected):
                        P.build_unsigned(runner, root, tools={"cargo": "/tools/cargo"},
                                         environment={}, notices=SimpleNamespace())
                self.assertEqual(calls, ["package-config"] if rejected else ["package-config", "native-build"])
                binary_read.assert_not_called()
                self.assertEqual(base, saved)
                self.assertEqual(len(generated), 1)
                self.assertEqual(generated[0]["formats"], ["appimage"])
                self.assertEqual(set(generated[0]["appimage"]), {"files", "excludedLibs"})


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

    def test_permanent_http_status_is_fatal_even_with_a_transport_exit(self):
        for http, reason in ((403, "http_forbidden"), (404, "http_not_found"), (429, "http_rate_limited")):
            for code in (18, 28, 52, 55, 56):
                with self.subTest(http=http, code=code), self.assertRaises(M.PackageDownloadFailure) as caught:
                    self.invoke([self.frame(b"private reflected server body", http=http, code=code)])
                self.assertEqual(caught.exception.reason, reason)
                self.assertEqual(self.last_runner.run.call_count, 1)
                self.last_delay.assert_not_called()
                self.assertNotIn("private", str(caught.exception))
            for code in (35, 51, 60):
                with self.subTest(http=http, code=code), self.assertRaises(M.PackageDownloadFailure) as caught:
                    self.invoke([self.frame(http=http, code=code)])
                self.assertEqual(caught.exception.reason, "download_tls")
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
