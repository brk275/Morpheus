"""Mocked command checks; explicit hosted opt-in also checks the real sealed-log provider."""

import importlib.util
import base64
import io
import json
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


def _temporary_real_seal_provider(test):
    """Opt-in hosted micro: the real namespace, owner, scanner and public encoder."""
    import hashlib
    import os
    import re
    sys.path.insert(0, str(ROOT / "scripts"))
    import appimage_update_acceptance as A

    env = dict(os.environ)
    expected = {"GITHUB_ACTIONS":"true", "RUNNER_ENVIRONMENT":"github-hosted",
                "RUNNER_OS":"Linux", "RUNNER_ARCH":"X64", "GITHUB_REPOSITORY":"brk275/Morpheus",
                "GITHUB_EVENT_NAME":"workflow_dispatch", "GITHUB_ACTOR":"brk275",
                "GITHUB_TRIGGERING_ACTOR":"brk275",
                "GITHUB_REF":"refs/heads/codex/temp-linux-byte-coverage-regression-20261105"}
    test.assertTrue(sys.platform == "linux" and all(env.get(k) == v for k, v in expected.items()))
    event = json.loads(P.client.read_regular(Path(env["GITHUB_EVENT_PATH"]), 1024 * 1024))
    test.assertIs(event["repository"]["private"], False)
    owner, sender = event["repository"]["owner"], event["sender"]
    test.assertTrue(owner["login"] == sender["login"] == "brk275"
                    and type(owner["id"]) is int and 0 < owner["id"] < 2**63
                    and type(sender["id"]) is int and owner["id"] == sender["id"]
                    and env["GITHUB_ACTOR_ID"] == str(owner["id"]))
    revision = env["MORPHEUS_PACKAGE_SOURCE_REVISION"]
    test.assertEqual(P.verify_checkout(ROOT, P.expected_revision(env)), revision)
    run = env["GITHUB_RUN_ID"]
    test.assertTrue(re.fullmatch(r"[1-9][0-9]{0,18}", run) is not None and 0 < int(run) < 2**63)
    scanner = Path(env["MORPHEUS_SEAL_PROVIDER_SCANNER"])
    test.assertEqual(scanner.resolve(strict=True), scanner)
    test.assertEqual(hashlib.sha256(P.client.read_regular(scanner, 32 * 1024 * 1024)).hexdigest(),
                     "88f91962aa2f93ac6ab281d553b9e125f5197bbbce38f9f2437f7299c32e5509")
    P.owned_tool(scanner)
    phases = {"provider_admitted":False, "encoder_entered":False, "encoder_returned":False,
              "fresh_scan_entered":False, "fresh_scan_passed":False, "handoff_created":False}
    maximum = {"calls":0, "chunks":0, "passed":False}
    admission = None
    calls = {"count":0}
    if env.get("MORPHEUS_SEAL_PROVIDER_INNER") == "1":
        P.containment.assert_contained()
        owner_root = P.client.private_directory(Path(env["MORPHEUS_SEAL_PROVIDER_OWNER_ROOT"]))
        root = P.client.private_directory(Path(env["MORPHEUS_SEAL_PROVIDER_BUILD_ROOT"]))
        test.assertEqual(root, owner_root / "artifacts" / "0.1.0-updateqa.1")
        test.assertTrue(owner_root.parent == Path(env["RUNNER_TEMP"]).resolve(strict=True)
                        and owner_root.name.startswith("morpheus-seal-provider-"))
        plan_path = root / "update-plan.json"
        plan = json.loads(P.client.read_regular(plan_path, 32768))
        test.assertEqual(plan["revision"], revision)
        progress = P.diagnostics.Progress(root)
        progress.wrapper("build")
        runner = M.OwnedRunner(root / "command-logs", progress=progress)
        refused = None
        try:
            runner.run("package", ["/usr/bin/python3", "-I", "-c",
                       "import sys;sys.stderr.write('synthetic package failure\\n');sys.exit(1)"],
                       cwd=root, env={"PATH":"/usr/bin:/bin"}, timeout=10, output_limit=4096)
        except M.CommandFailure as error:
            test.assertEqual(error.reason, "exit")
            refused = error
            progress.failed(error)
        test.assertIs(type(refused), M.CommandFailure)
        test.assertEqual(runner.last_stage, "package")
        test.assertNotEqual(progress.value["inner_cleanup"], "failed")
        # Real pinned scanner: NUL and a line larger than its fragment boundary
        # must still contribute every exact byte to the accepted raw counter.
        P.client.write_new(root / "command-logs" / "coverage-boundary.log",
                           b"first\x00" + b"A" * 200_000 + b"tail\n")
        # A known application signature is silently skipped by the default
        # fragment source; exact-byte coverage must refuse its clean report.
        negative = root / "coverage-refusal"
        negative.mkdir(mode=0o700)
        P.client.private_directory(negative)
        binary = negative / "binary.log"
        payload = b"\x00asm\x01\x00\x00\x00synthetic private fixture"
        P.client.write_new(binary, payload)
        info = binary.lstat()
        identity = (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_gid,
                    info.st_nlink, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
        try:
            with test.assertRaises(P.PackageFailure):
                P.scan_private_logs(negative, str(scanner))
        finally:
            info = binary.lstat()
            test.assertEqual((info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_gid,
                              info.st_nlink, info.st_size, info.st_mtime_ns, info.st_ctime_ns), identity)
            test.assertEqual(P.client.read_regular(binary, 4096), payload)
            binary.unlink()
            negative.rmdir()
        P.scan_private_logs(root, str(scanner))
        program = Path("/usr/bin/openssl")
        opt = Path("/opt").lstat()
        chain = [item.lstat() for item in (program, *program.parents)]
        admission = {"openssl_canonical":program.resolve(strict=True) == program,
                     "openssl_root_owned":all(info.st_uid == 0 for info in chain),
                     "openssl_nonwritable":all(info.st_mode & 0o022 == 0 for info in chain),
                     "opt_root_owned":opt.st_uid == 0, "opt_nonwritable":opt.st_mode & 0o022 == 0}
        actual_tool, actual_run, actual_scan = P.owned_tool, M.OwnedRunner.run, P.scan_private_logs
        def tool(value, **options):
            if str(value) == "/usr/bin/openssl": phases["provider_admitted"] = False
            result = actual_tool(value, **options)
            if str(value) == "/usr/bin/openssl": phases["provider_admitted"] = True
            return result
        def child(owner, *args, **options):
            phases["encoder_entered"] = True
            phases["encoder_returned"] = False
            calls["count"] += 1
            result = actual_run(owner, *args, **options)
            phases["encoder_returned"] = True
            return result
        def scan(*args, **options):
            phases["fresh_scan_entered"] = True
            phases["fresh_scan_passed"] = False
            result = actual_scan(*args, **options)
            phases["fresh_scan_passed"] = True
            return result
        scanned = False
        try:
            with mock.patch.object(P, "owned_tool", side_effect=tool), \
                    mock.patch.object(M.OwnedRunner, "run", child), \
                    mock.patch.object(P, "scan_private_logs", side_effect=scan):
                max_root = root / "maximum-payload"
                max_root.mkdir(mode=0o700)
                P.client.private_directory(max_root)
                max_scanned, max_blocks = P._temporary_openssl_blocks(
                    max_root, b"X" * 65536, str(scanner), P.time.monotonic() + 120)
                maximum["calls"] = calls["count"]
                maximum["chunks"] = len(max_blocks) if type(max_blocks) is list else 0
                maximum["passed"] = max_scanned is True and maximum["calls"] == maximum["chunks"] == 207
                del max_blocks
                if maximum["passed"]:
                    scanned = P._temporary_seal_package(root, plan_path, plan, runner, progress,
                                                        refused, True, str(scanner))
            phases["handoff_created"] = (root / "sealed-package.json").is_file()
        finally:
            # Even failed observation leaves no new unscanned child output.
            P.scan_private_logs(root, str(scanner))
            P.client.write_new(root / "provider-observation.json", json.dumps(
                {"observations":phases, "maximum":maximum, "admission":admission}).encode("ascii"))
        test.assertTrue(scanned and all(phases.values()) and maximum["passed"], "real_provider_boundary_failed")
        return

    # The parent observes the REAL stopped namespace owner before using the
    # unchanged production handoff reader. No flag is assigned by the fixture.
    test.assertNotEqual(P.os.getpid(), 1)
    observations = None
    namespace_passed = False
    handoff_passed = False
    with tempfile.TemporaryDirectory(prefix="morpheus-seal-provider-", dir=env["RUNNER_TEMP"]) as directory:
        root = Path(directory).resolve(strict=True)
        commands = A.Commands(root)
        commands.reviewed_revision = revision
        (root / "artifacts").mkdir(mode=0o700)
        build_root = A.private_directory(root / "artifacts" / "0.1.0-updateqa.1")
        plan_path = build_root / "update-plan.json"
        A.write_private(plan_path, json.dumps({"schema":1, "reviewed_checkout":str(ROOT),
                        "revision":revision, "target":str(root / "target"), "version":"0.1.0-updateqa.1",
                        "feed":"https://127.0.0.1:1/feed.json", "public_key":"synthetic public fixture"}), create=True)
        inner = A.package_build_environment(env)
        inner.update({"MORPHEUS_LINUX_SEAL_REAL_PROVIDER":"1", "MORPHEUS_SEAL_PROVIDER_INNER":"1",
                      "MORPHEUS_SEAL_PROVIDER_BUILD_ROOT":str(build_root),
                      "MORPHEUS_SEAL_PROVIDER_OWNER_ROOT":str(root),
                      "MORPHEUS_SEAL_PROVIDER_SCANNER":str(scanner)})
        invocation = A.namespace_command(["/usr/bin/python3", "-I", "-B", str(Path(__file__).resolve()),
                     "ProcessTests.test_temporary_inner_package_seal_owns_one_child_scans_all_output_and_refuses_failed_gates"],
                     build_root / "namespace", inner)
        try:
            commands.run(invocation, cwd=ROOT, seconds=180, env=inner, operation="package_build")
            namespace_passed = True
        except A.QAError:
            pass  # Only the fixed post-scan status may leave this private log.
        finally:
            commands.scan(str(scanner))
        test.assertIs(commands._temporary_package_stopped, True)
        status = build_root / "provider-observation.json"
        if status.exists():
            value = json.loads(P.client.read_regular(status, 2048))
            test.assertTrue(type(value) is dict and set(value) == {"observations", "maximum", "admission"})
            observations, maximum, admission = value["observations"], value["maximum"], value["admission"]
            test.assertTrue(type(observations) is dict and set(observations) == set(phases)
                            and all(type(v) is bool for v in observations.values()))
            test.assertTrue(type(maximum) is dict and set(maximum) == {"calls", "chunks", "passed"}
                            and type(maximum["passed"]) is bool
                            and all(type(maximum[k]) is int and 0 <= maximum[k] <= 207 for k in ("calls", "chunks")))
            test.assertTrue(type(admission) is dict and set(admission) == {"openssl_canonical",
                            "openssl_root_owned", "openssl_nonwritable", "opt_root_owned", "opt_nonwritable"}
                            and all(type(v) is bool for v in admission.values()))
        if namespace_passed:
            try:
                value = A._temporary_read_sealed_package(commands, build_root, plan_path)
                handoff_passed = type(value) is dict and value["source_revision"] == revision
                del value  # Never publish or decrypt synthetic cipher frames.
            except (A.QAError, ValueError, OSError):
                pass  # Failure stays false; only the fixed parent_handoff reason leaves.
    test.assertFalse(root.exists())
    reason = "passed"
    if observations is None:
        reason = "inner_preflight"
    elif not observations["provider_admitted"]:
        reason = "provider_canonical_or_admission"
    elif not observations["encoder_entered"]:
        reason = "private_log_or_auxiliary_setup"
    elif not observations["encoder_returned"]:
        reason = "encoder_exit_or_cleanup"
    elif not observations["fresh_scan_passed"]:
        reason = "fresh_scan"
    elif not observations["handoff_created"]:
        reason = "cipher_protocol_or_handoff"
    elif not namespace_passed:
        reason = "namespace_owner"
    elif not handoff_passed:
        reason = "parent_handoff"
    facts = {"reason":reason, "schema":1, "check":"real_linux_seal_provider", "namespace_passed":namespace_passed,
             "parent_handoff_passed":handoff_passed, "cleanup_passed":True,
             "observations":observations, "maximum":maximum, "admission":admission}
    print("MORPHEUS_LINUX_SEAL_PROVIDER " + json.dumps(facts, sort_keys=True), flush=True)
    test.assertTrue(namespace_passed and handoff_passed and observations is not None
                    and all(observations.values()) and maximum["passed"]
                    and all(admission[k] for k in ("openssl_canonical", "openssl_root_owned", "openssl_nonwritable")),
                    "real_provider_micro_failed")


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
                replaced = {"source":False, "report_nonblocking":False, "partial_reads":0,
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
                        replaced["report_nonblocking"] = bool(flags & F.os.O_NONBLOCK)
                        self.assertTrue(replaced["report_nonblocking"])
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
                    report = Path(arguments[arguments.index("--report-path") + 1])
                    if action == "process_cleanup":
                        raise F.PrivateLogScanError(process_stopped=False)
                    if action == "report_fifo":
                        report.unlink()
                        F.os.mkfifo(report, 0o600)
                    if action not in {"missing", "report_fifo"}:
                        report.write_bytes(b"{bad" if action == "malformed" else
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
                            b"unexpected output" if action == "stdout" else b"",
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
                    self.assertTrue(replaced["report_nonblocking"])
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


    @unittest.skipUnless(sys.platform == "linux", "Linux full-log aggregate scan deadline")
    def test_temporary_scan_deadline_only_tightens_default_and_never_accepts_partial_scan(self):
        import fixture_paths as F
        with tempfile.TemporaryDirectory() as directory:
            root = P.client.private_directory(Path(directory).resolve())
            for name in ("a.log", "b.log"):
                P.client.write_new(root / name, b"synthetic log")
            clock = {"value":10}
            def scanned(arguments, *, cwd, deadline):
                self.assertIn(deadline, (11, 40))
                Path(arguments[arguments.index("--report-path") + 1]).write_bytes(b"[]\n")
                if deadline == 11: clock["value"] = 12
                return 0, b"", b"3:04PM INF scanned ~26 bytes (26 bytes) in 1ms\n3:04PM INF no leaks found\n"
            with mock.patch.object(P.time, "monotonic", side_effect=lambda: clock["value"]), \
                    mock.patch.object(F, "_run_log_scanner", side_effect=scanned) as scanner:
                with self.assertRaises(P.PackageFailure):
                    P.scan_private_logs(root, "/scanner", deadline=10)
                scanner.assert_not_called()
                P.scan_private_logs(root, "/scanner")
                self.assertEqual(scanner.call_count, 1)
                with self.assertRaises(P.PackageFailure):
                    P.scan_private_logs(root, "/scanner", deadline=11)
                self.assertEqual(scanner.call_count, 2)


    @unittest.skipUnless(P.os.name == "posix", "Linux private package log capture")
    def test_temporary_inner_package_seal_owns_one_child_scans_all_output_and_refuses_failed_gates(self):
        if P.os.environ.get("MORPHEUS_LINUX_SEAL_REAL_PROVIDER") == "1":
            return _temporary_real_seal_provider(self)
        import hashlib
        self.assertEqual(hashlib.sha256(P._TEMP_SEALED_PUBLIC_PEM).hexdigest(), P._TEMP_SEALED_RECIPIENT)
        for action in ("valid", "scan_refusal", "aux_cleanup", "malformed", "child_exit",
                       "preexisting_aux", "linked_aux", "stderr_budget", "encode_deadline"):
            with self.subTest(action=action), tempfile.TemporaryDirectory() as directory:
                root = P.client.private_directory(Path(directory).resolve())
                logs = root / "command-logs"
                logs.mkdir(mode=0o700)
                P.client.private_directory(logs)
                sentinel = None
                auxiliary = root / "sealed-package"
                if action in {"preexisting_aux", "linked_aux"}:
                    foreign = auxiliary if action == "preexisting_aux" else root / "untracked-directory"
                    foreign.mkdir(mode=0o700)
                    sentinel = foreign / "existing-value"
                    sentinel.write_bytes(b"preserve this existing directory")
                    if action == "linked_aux":
                        auxiliary.symlink_to(foreign, target_is_directory=True)
                    foreign_mode = foreign.lstat().st_mode
                stdout = b"synthetic-private-stdout" * 1000
                stderr = b"synthetic-private-stderr" * 1000
                for kind, body in (("stdout", stdout), ("stderr", stderr)):
                    P.client.write_new(logs / ("0007-package." + kind + ".log"), body)
                plan = {"revision": "b" * 40}
                plan_path = root / "update-plan.json"
                P.client.write_new(plan_path, json.dumps(plan).encode())
                progress = P.diagnostics.Progress(root)
                progress.wrapper("build"); progress.command("package")
                refusal = M.CommandFailure("exit")
                progress.failed(refusal)
                original_failure = dict(progress.value["failure"])
                runner = SimpleNamespace(last_stage="package", sequence=7, log_root=logs)
                events, inputs, stderr_limits = [], [], []
                clock = {"value":10}
                test = self
                class Provider:
                    def resolve(self, strict=False): return self
                    def __str__(self): return "/usr/bin/openssl"
                provider = Provider()
                def paths(value):
                    return provider if str(value) == str(provider) else Path(value)
                class Encoder:
                    def __init__(self, log_root, progress):
                        self.log_root = Path(log_root)
                        self.log_root.mkdir(mode=0o700)
                        P.client.private_directory(self.log_root)
                        self.progress, self.sequence = progress, 0
                    def run(self, stage, argv, **options):
                        events.append("child"); self.sequence += 1
                        test.assertEqual(stage, "package")
                        key = auxiliary / "recipient-public.pem"
                        test.assertEqual(key.read_bytes(), P._TEMP_SEALED_PUBLIC_PEM)
                        test.assertEqual(argv, [str(provider), "pkeyutl", "-encrypt", "-pubin", "-inkey", str(key),
                                         "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha256",
                                         "-pkeyopt", "rsa_mgf1_md:sha256"])
                        test.assertEqual(options["timeout"], 30)
                        test.assertEqual(options["output_limit"], 384)
                        test.assertFalse(options.get("binary_output", False))
                        test.assertEqual(options["env"]["OPENSSL_CONF"], "/dev/null")
                        test.assertTrue(0 < len(options["stdin_bytes"]) <= 318)
                        inputs.append(options["stdin_bytes"])
                        stderr_limits.append(options["stderr_limit"])
                        raw = b"A" * 384 if action != "malformed" else b"malformed"
                        error_log = b"S" * 65536 if action == "stderr_budget" and self.sequence == 1 else b""
                        P.client.write_new(self.log_root / f"{self.sequence:04d}-package.stdout.log", raw)
                        P.client.write_new(self.log_root / f"{self.sequence:04d}-package.stderr.log", error_log)
                        if action == "aux_cleanup":
                            self.progress.cleanup(False)
                            raise M.CommandFailure("cleanup")
                        if action == "child_exit": raise M.CommandFailure("exit")
                        if action == "stderr_budget" and self.sequence == 2:
                            test.assertEqual(options["stderr_limit"], 0)
                            raise M.CommandFailure("output")
                        if action == "encode_deadline": clock["value"] = 40
                        return raw
                def scan(selected_root, scanner, *, deadline):
                    events.append("scan")
                    self.assertEqual(selected_root, root); self.assertEqual(deadline, 130)
                    self.assertTrue((root / "sealed-package/command-logs/0001-package.stdout.log").is_file())
                    self.assertFalse((root / "sealed-package.json").exists())
                    if action == "scan_refusal": raise P.PackageFailure("scan")
                def owned_tool(value, **options):
                    if value is provider: self.assertIs(options.get("root_owned"), True)
                    return str(value)
                with mock.patch.object(P, "Path", side_effect=paths), \
                        mock.patch.object(P, "owned_tool", side_effect=owned_tool), \
                        mock.patch.object(P, "expected_revision", return_value="b" * 40), \
                        mock.patch.dict(P.os.environ, {"GITHUB_RUN_ID":"41956870809"}), \
                        mock.patch.object(P.time, "monotonic", side_effect=lambda: clock["value"]), \
                        mock.patch.object(P, "OwnedRunner", Encoder), \
                        mock.patch.object(P, "scan_private_logs", side_effect=scan):
                    scanned = P._temporary_seal_package(root, plan_path, plan, runner, progress, refusal, True, "/scanner")
                self.assertEqual(events, [] if sentinel is not None else ["child"] * len(inputs) + ["scan"])
                self.assertEqual(scanned, action not in {"scan_refusal", "aux_cleanup"})
                self.assertEqual((root / "sealed-package.json").exists(), action == "valid")
                self.assertEqual(progress.value["failure"], original_failure)
                if action in {"valid", "scan_refusal"}:
                    payload = b"".join(inputs)
                    self.assertLessEqual(len(payload), 65536)
                    self.assertEqual(len(inputs), (len(payload) + 317) // 318)
                    value = json.loads(payload)
                    for kind, body in (("stdout", stdout), ("stderr", stderr)):
                        stream = value["streams"][kind]
                        self.assertEqual(base64.b64decode(stream["head_base64"]), body[:8192])
                        self.assertEqual(base64.b64decode(stream["tail_base64"]), body[-8192:])
                        self.assertEqual(stream["size"], len(body)); self.assertTrue(stream["truncated"])
                    self.assertEqual(value["source_revision"], "b" * 40)
                    self.assertEqual(value["run_id"], 41956870809)
                if action == "stderr_budget": self.assertEqual(stderr_limits, [65536, 0])
                if action == "encode_deadline": self.assertEqual(len(inputs), 1)
                if sentinel is not None:
                    self.assertEqual(sentinel.read_bytes(), b"preserve this existing directory")
                    self.assertEqual(foreign.lstat().st_mode, foreign_mode)
                    self.assertEqual(auxiliary.is_symlink(), action == "linked_aux")


    def invoke(self, root, *, stdout=b"okay", stderr=b"private", limit=4096, binary=False,
               timeout=10, status=0, cleanup_error=None, interrupt=False, progress=None,
               stdin_bytes=None, stderr_limit=8 * 1024 * 1024, expected_status=(0,), archive_revision=None):
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
                mock.patch.object(M.client.R, "trusted_tool", return_value="/usr/bin/git"), \
                mock.patch.object(M.os, "set_blocking"), \
                mock.patch.object(M.os, "read", side_effect=lambda fd, size: chunks[fd].pop(0)):
            try:
                if archive_revision is not None:
                    result = runner.source_archive(archive_revision, cwd=root, env={"GITHUB_SHA": archive_revision})
                    self.assertEqual(spawned.call_args.args[0], ["/usr/bin/git", "-c", "core.hooksPath=/dev/null",
                        "-c", "core.fsmonitor=false", "-c", "gc.auto=0", "-c", "maintenance.auto=false",
                        "archive", "--format=tar", archive_revision])
                    return result
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

    def test_selected_source_archive_reads_above_64mib_without_log_or_cleanup_waiver(self):
        payload = b"x" * (64 * 1024 * 1024 + 1)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            self.assertEqual(self.invoke(root, stdout=payload, archive_revision="a" * 40), payload)
            self.assertEqual((root / "logs/0001-source-archive.stdout.log").read_bytes(), b"")
            self.assertEqual((root / "logs/0001-source-archive.stderr.log").read_bytes(), b"private")
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(M, "MAX_EXPORT_BYTES", 16), \
                self.assertRaises(M.CommandFailure) as caught:
            self.invoke(Path(directory).resolve(), stdout=b"x" * 17, archive_revision="a" * 40)
        self.assertEqual(caught.exception.reason, "output")
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CommandFailure) as caught:
            self.invoke(Path(directory).resolve(), archive_revision="a" * 40,
                        cleanup_error=RuntimeError("private cleanup failure"))
        self.assertEqual(caught.exception.reason, "cleanup")

    def test_source_archive_rejects_unselected_revision_and_generic_large_output_before_spawn(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            runner = M.OwnedRunner(root / "logs")
            for value, environment in [(None, {}), ("main", {}), ("a" * 7, {}), ("0" * 40, {}),
                                       ("A" * 40, {}), ("a" * 40 + "\ninput", {}),
                                       ("a" * 40, {}), ("a" * 40, {"GITHUB_SHA": "b" * 40})]:
                with self.subTest(revision=value), mock.patch.object(M.client.R, "trusted_tool") as tool, \
                        mock.patch.object(runner, "_execute") as execute, self.assertRaises(M.CommandFailure):
                    runner.source_archive(value, cwd=root, env=environment)
                tool.assert_not_called(); execute.assert_not_called()
            for tool_path, cwd in (("/tmp/git", root), ("/usr/bin/git-link", root),
                                   ("/usr/bin/git", Path("relative"))):
                with self.subTest(tool=tool_path, cwd=str(cwd)), \
                        mock.patch.object(M.client.R, "trusted_tool", return_value=tool_path), \
                        mock.patch.object(runner, "_execute") as execute, self.assertRaises(M.CommandFailure):
                    runner.source_archive("a" * 40, cwd=cwd, env={"GITHUB_SHA": "a" * 40})
                execute.assert_not_called()
            for stage, binary in (("fixture", False), ("download", True), ("source-archive", True)):
                with self.subTest(stage=stage), mock.patch.object(M.containment, "assert_contained") as contained, \
                        mock.patch.object(M.subprocess, "Popen") as spawned, self.assertRaises(M.CommandFailure):
                    runner.run(stage, ["/usr/bin/git", "archive", "--format=tar", "a" * 40], cwd=root, env={},
                               timeout=60, output_limit=64 * 1024 * 1024 + 1, binary_output=binary)
                contained.assert_not_called(); spawned.assert_not_called()
            self.assertEqual(runner.sequence, 0)
            self.assertEqual(list((root / "logs").iterdir()), [])

    def test_update_failure_scans_after_fixture_cleanup_before_closed_facts_escape(self):
        from scripts import appimage_update_acceptance as A
        for fail_scan in (False, True):
            with self.subTest(fail_scan=fail_scan), tempfile.TemporaryDirectory() as directory:
                events = []
                commands = SimpleNamespace(stage="preflight", deadline=0,
                                           run=mock.Mock(return_value="cargo-packager 0.11.8\n"))
                def mark(stage):
                    commands.stage = stage
                    events.append(stage)
                def scan(scanner):
                    events.append("scanned")
                    if fail_scan:
                        raise A.QAError("log_scan")
                commands.mark, commands.scan = mark, scan
                certificate = SimpleNamespace(close=lambda:events.append("certificate_closed"))
                args = SimpleNamespace(execute_on_free_hosted_runner=True, gitleaks="/reviewed/scanner",
                                       target_dir=Path(directory)/"target", source=Path(directory))
                original = A.QAError("command_failed", operation="package_build")
                original.package_diagnostics = {"wrapper_phase":"build","command_stage":"native-build",
                                                "reason":"command_exit"}
                with mock.patch.dict(A.os.environ, {}, clear=True), \
                        mock.patch.object(A, "hosted_guard"), \
                        mock.patch.object(A.shutil, "which", return_value="/reviewed/tool"), \
                        mock.patch.object(A, "Commands", return_value=commands), \
                        mock.patch.object(A, "Certificate", return_value=certificate), \
                        mock.patch.object(A, "export_source", side_effect=original), \
                        self.assertRaises(A.QAError) as caught:
                    A.execute(args)
                self.assertEqual(events, ["preflight","cleanup","certificate_closed","log_scan","scanned"])
                self.assertEqual(caught.exception.reason, "log_scan" if fail_scan else "command_failed")
                if fail_scan:
                    self.assertNotIn("package_diagnostics", caught.exception.facts())
                else:
                    self.assertEqual(caught.exception.facts()["package_diagnostics"]["command_stage"], "native-build")

    def test_update_child_failure_retains_only_scanned_fixed_package_tokens(self):
        from scripts import appimage_update_acceptance as A
        marker = "synthetic-private-compiler-body"
        fixed = {"schema":1,"wrapper_phase":"build","command_stage":"native-build",
                 "capture_mode":"none","capture_phase":"none","peer_facts":[],
                 "inner_cleanup":"not_started","failure":{"wrapper_phase":"build",
                 "command_stage":"native-build","capture_mode":"none","capture_phase":"none",
                 "reason":"command_exit"},"diagnostics":"scanned_fixed_facts","passed":False,
                 "outer_cleanup":"not_started","outer_reason":"command_exit"}
        output = marker + "\nMORPHEUS_APPIMAGE_RESULT " + json.dumps(fixed)
        error = A.child_failure(output)
        self.assertEqual(error.facts(), {"reason":"command_failed","operation":"package_build",
                         "package_diagnostics":{"wrapper_phase":"build","command_stage":"native-build",
                                                "reason":"command_exit"}})
        self.assertNotIn(marker, json.dumps(error.facts()))
        for changed in ({**fixed,"diagnostics":"withheld"}, {**fixed,"extra":marker},
                        {**fixed,"failure":{**fixed["failure"],"command_stage":marker}},
                        {"diagnostics":"withheld","passed":False}):
            self.assertIsNone(A.child_failure("MORPHEUS_APPIMAGE_RESULT " + json.dumps(changed)))
        self.assertIsNone(A.child_failure(output + "\nMORPHEUS_APPIMAGE_RESULT " + json.dumps(fixed)))
        duplicate = json.dumps(fixed).replace('"schema": 1', '"schema": 1, "schema": 1')
        self.assertIsNone(A.child_failure("MORPHEUS_APPIMAGE_RESULT " + duplicate))
        error.package_diagnostics = {"wrapper_phase":"build","command_stage":marker,"reason":"command_exit"}
        self.assertNotIn("package_diagnostics", error.facts())

    def test_linux_update_producer_requires_head_clean_and_full_proof_around_source_archive(self):
        revision = "a" * 40
        for head, clean, proof_error, build_error, scan_error in (
                (revision.encode() + b"\n", b"", None, None, None),
                (b"b" * 40 + b"\n", b"", None, None, None),
                (revision.encode() + b"\n", b"dirty", None, None, None),
                (revision.encode() + b"\n", b"", P.SourceRevisionError("checkout"), None, None),
                (revision.encode() + b"\n", b"", None, M.CommandFailure("exit"), None),
                (revision.encode() + b"\n", b"", None, M.CommandFailure("exit"), P.PackageFailure("scan"))):
            with self.subTest(head=head[:4], dirty=bool(clean), proof_failure=proof_error is not None), \
                    tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                exported, reference, target, case = (root / name for name in ("export", "reference", "target", "case"))
                for path in (exported, reference, target, case):
                    path.mkdir(mode=0o700)
                event = root / "event.json"
                event.write_text(json.dumps({"repository":{"owner":{"login":"brk275","id":42}},
                                             "sender":{"login":"brk275","id":42}}))
                event.chmod(0o600)
                plan = {"schema":1,"reviewed_checkout":str(reference),"revision":revision,
                        "target":str(target),"version":"0.1.0-updateqa.1",
                        "feed":"https://127.0.0.1:12345/feed.json","public_key":"fixture-public-data"}
                plan_path = case / "update-plan.json"
                plan_path.write_text(json.dumps(plan)); plan_path.chmod(0o600)
                environment = {"MORPHEUS_FREE_HOSTED_UPDATE_QA":"1","GITHUB_SHA":revision,
                               "GITHUB_EVENT_PATH":str(event),"GITHUB_ACTOR":"brk275",
                               "GITHUB_TRIGGERING_ACTOR":"brk275","GITHUB_ACTOR_ID":"42"}
                archive, proof = b"synthetic selected archive bytes", {"fixture":"complete source proof"}
                runner = SimpleNamespace(last_stage="none", run=mock.Mock(side_effect=[head, clean]),
                                         source_archive=mock.Mock(return_value=archive))
                with mock.patch.dict(P.os.environ, environment, clear=True), \
                        mock.patch.object(P, "ROOT", exported), \
                        mock.patch.object(P, "DIAGNOSTICS_PUBLISHED", False), \
                        mock.patch.object(P, "public_hosted_environment"), \
                        mock.patch.object(P.containment, "assert_contained"), \
                        mock.patch.object(P, "controlled_environment", return_value={"GITHUB_SHA":revision}), \
                        mock.patch.object(P, "command_tools", return_value={}), \
                        mock.patch.object(P.client.R, "trusted_tool", return_value="/usr/bin/git"), \
                        mock.patch.object(P, "OwnedRunner", return_value=runner), \
                        mock.patch.object(P, "verify_versioned_export", return_value=proof,
                                          side_effect=proof_error) as verified, \
                        mock.patch.object(P, "validate_update_parameters"), \
                        mock.patch.object(P, "build_unsigned", side_effect=build_error) as built, \
                        mock.patch.object(P, "owned_tool", return_value="/reviewed/scanner"), \
                        mock.patch.object(P, "scan_private_logs", side_effect=scan_error) as scanned, \
                        mock.patch.dict(sys.modules, {"appimage_notices":SimpleNamespace(
                            InventoryFailure=type("InventoryFailure", (RuntimeError,), {}))}), \
                        mock.patch("builtins.print") as printed:
                    admitted = head == revision.encode() + b"\n" and not clean
                    if not admitted or proof_error is not None or build_error is not None:
                        with self.assertRaises((P.PackageFailure, P.SourceRevisionError, M.CommandFailure)):
                            P.update_build(plan_path, "/reviewed/scanner")
                    else:
                        P.update_build(plan_path, "/reviewed/scanner")
                scanned.assert_called_once_with(case, "/reviewed/scanner")
                if build_error is not None:
                    diagnostics = [call.args[0] for call in printed.call_args_list
                                   if call.args and str(call.args[0]).startswith("MORPHEUS_APPIMAGE_RESULT ")]
                    self.assertEqual(len(diagnostics), 1)
                    fact = json.loads(diagnostics[0].removeprefix("MORPHEUS_APPIMAGE_RESULT "))
                    self.assertEqual(fact["diagnostics"], "withheld" if scan_error else "scanned_fixed_facts")
                    if scan_error is None:
                        self.assertEqual(fact["failure"]["wrapper_phase"], "build")
                        self.assertEqual(fact["failure"]["reason"], "command_exit")
                    self.assertNotIn("synthetic-private", diagnostics[0])
                self.assertEqual(runner.source_archive.call_count, int(admitted))
                if admitted:
                    self.assertEqual(runner.source_archive.call_args.args, (revision,))
                    self.assertEqual(runner.source_archive.call_args.kwargs["cwd"], reference)
                    self.assertEqual(runner.source_archive.call_args.kwargs["env"]["PATH"], "/usr/bin:/bin")
                    verified.assert_called_once_with(exported, archive, plan["version"])
                else:
                    verified.assert_not_called()
                self.assertEqual(built.call_count, int(admitted and proof_error is None))
                if admitted and proof_error is None:
                    self.assertIs(built.call_args.kwargs["update"]["source_proof"], proof)
                scanned.assert_called_once_with(case, "/reviewed/scanner")

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
        # cargo-packager0.11.8/minisign0.7.9 serialize this colon-bearing header.
        public = base64.b64encode(b"Ed" + bytes(40)).decode()
        key = base64.b64encode(("untrusted comment: minisign public key: 0000000000000000\n" + public + "\n").encode()).decode()
        valid = {"version": "0.1.0-updateqa.1", "feed": "https://127.0.0.1:12345/feed.json", "public_key": key,
                 "source_proof": {"variant": "updateqa-version-only", "version": "0.1.0-updateqa.1", "inventory_sha256": "a" * 64}}
        P.validate_update_parameters(valid)
        alternate = base64.b64encode(b"ED" + bytes(40)).decode()
        P.validate_update_parameters(valid | {"public_key": base64.b64encode(("untrusted comment: minisign public key: 0000000000000000\n" + alternate + "\n").encode()).decode()})
        for change in ({"feed": "http://127.0.0.1:12345/feed.json"}, {"feed": "https://remote.invalid/feed.json"},
                       {"feed": "https://user@127.0.0.1:12345/feed.json"}, {"feed": "https://127.0.0.1:12345/feed.json?x=1"},
                       {"version": "0.1.0"}, {"public_key": "private-value"},
                       {"public_key": base64.b64encode(b"untrusted comment: minisign encrypted secret key\nwrong\n").decode()},
                       {"public_key": base64.b64encode(("untrusted comment: minisign public key 0000000000000000\n" + public + "\n").encode()).decode()},
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
