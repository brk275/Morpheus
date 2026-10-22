import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest import mock

from scripts import run_pipewire_startup_probe as probe


def outcome(passed=True, **overrides):
    return {"schema": 1, "capture_attempted": True, "capture_completed": True,
            "bindings_verified": True, "capture_passed": passed,
            "cleanup_passed": True, "services_system": True, "trace_complete": True, **overrides}


class ProbeOrchestrationTests(unittest.TestCase):
    def test_stock_services_reject_an_additional_native_client_mapping(self):
        stock = b"1000-2000 r-xp 0 08:01 42 /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0.1005.0\n"
        owned = b"3000-4000 r-xp 0 08:01 43 /private/libmorpheus-pipewire-0.3.so.0\n"
        services = {role: SimpleNamespace(pid=100) for role in ("pipewire", "wireplumber", "portal-wlr", "portal")}
        info = SimpleNamespace(st_mode=0o100644, st_uid=0, st_ino=42, st_dev=os.makedev(8, 1))
        with mock.patch.object(probe.wayland, "peek_returncode", return_value=None), \
             mock.patch.object(Path, "resolve", autospec=True, side_effect=lambda path, **_: path), \
             mock.patch.object(Path, "stat", return_value=info), \
             mock.patch.object(Path, "open", side_effect=lambda *_args, **_kwargs: io.BytesIO(stock)):
            self.assertTrue(probe.service_library_facts(services))
            with mock.patch.object(Path, "open", side_effect=lambda *_args, **_kwargs: io.BytesIO(stock + owned)):
                self.assertFalse(probe.service_library_facts(services))

    def test_public_provenance_retains_only_validated_digest_fields(self):
        record = {"binary_sha256": "a" * 64, "binary": "/private/binary",
                  "provenance": {key: "b" * 64 for key in ("loader_sha256", "configuration_sha256", "build_recipe_sha256")},
                  "control": {"sha256": "c" * 64, "build_id": "d" * 40, "library_path": "/private/library"},
                  "patched": {"sha256": "e" * 64, "build_id": "f" * 40}}
        public = probe.public_provenance(record)
        self.assertEqual(public["control"]["build_id"], "d" * 40)
        self.assertNotIn("private", json.dumps(public))
        for value in ("private", "A" * 64, "a" * 65, True):
            record["binary_sha256"] = value
            with self.assertRaises(RuntimeError):
                probe.public_provenance(record)

    def test_private_bindings_never_satisfy_the_normal_acceptance_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "facts"
            facts = {"schema": 1, "variant": "control", "checked": 33, "private_probe": 33,
                     "digest_verified": True, "build_id_verified": True, "mapped_file_verified": True}
            path.write_text(json.dumps(facts))
            self.assertEqual(probe.private_binding_facts(path, "control")["status"], "valid")
            self.assertEqual(probe.private_binding_facts(path, "patched"), {"status": "invalid"})
            self.assertFalse(probe.wayland.bindings_passed(probe.wayland.binding_facts(path)))
            for changed in ({"schema": True}, {"checked": 32}, {"private_probe": 32},
                            {"digest_verified": 1}, {"build_id_verified": False},
                            {"mapped_file_verified": False}, {"path": "/private/not-public"}):
                path.write_text(json.dumps({**facts, **changed}))
                self.assertEqual(probe.private_binding_facts(path, "control"), {"status": "invalid"})

    def test_normal_and_diagnostic_test_evidence_cannot_be_exchanged(self):
        summary = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out;\n"
        system = f"test {probe.wayland.TARGET_TEST} ... ok\n" + summary
        private = f"test {probe.wayland.PROBE_TEST} ... ok\n" + summary
        self.assertTrue(probe.wayland.capture_test_passed(system))
        self.assertFalse(probe.wayland.capture_test_passed(private))
        self.assertTrue(probe.wayland.capture_test_passed(private, private_client=True))
        self.assertFalse(probe.wayland.capture_test_passed(system, private_client=True))
        self.assertFalse(probe.wayland.capture_test_passed(private.replace("1 passed", "0 passed"), private_client=True))
        failed = (f"test {probe.wayland.PROBE_TEST} ... FAILED\n"
                  "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out;\n")
        self.assertTrue(probe.wayland.capture_test_completed(failed, private_client=True))
        self.assertFalse(probe.wayland.capture_test_completed(failed))
        self.assertFalse(probe.wayland.capture_test_completed("could not launch test", private_client=True))
        self.assertFalse(probe.wayland.capture_test_completed(failed.replace("1 failed", "0 failed"), private_client=True))

    def test_runtime_environment_has_no_inherited_loader_or_pipewire_override(self):
        original = {"PATH": "/pinned/rust:/usr/bin", "GITHUB_ACTIONS": "true",
                    "LD_PRELOAD": "private", "LD_LIBRARY_PATH": "private", "DYLD_INSERT_LIBRARIES": "private",
                    "GLIBC_TUNABLES": "private", "GCONV_PATH": "private", "SPA_PLUGIN_DIR": "private",
                    "PIPEWIRE_REMOTE": "private", "PIPEWIRE_MODULE_DIR": "private"}
        self.assertEqual(probe.runtime_environment(original), {"PATH": original["PATH"], "GITHUB_ACTIONS": "true"})
        self.assertIn("LD_PRELOAD", original)  # No mutation of caller state.

    def test_collected_failure_is_allowed_only_after_owned_cleanup_and_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            path = root / "probe-outcome.json"
            def write(value):
                path.write_text(json.dumps(value))
                path.chmod(0o600)
            write(outcome(False))
            self.assertEqual(probe.collect_outcome(root, 1), outcome(False))
            for value, status in [(outcome(False, cleanup_passed=False), 1),
                                  (outcome(False, services_system=False), 1),
                                  (outcome(False, capture_attempted=False), 1),
                                  (outcome(False, capture_completed=False), 1),
                                  (outcome(False, bindings_verified=False), 1),
                                  (outcome(False), 0), (outcome(True), 1), (outcome(True), -9),
                                  (outcome(True, schema=True), 0), (outcome(True, capture_passed=1), 0),
                                  (outcome(True, raw="private"), 0)]:
                write(value)
                with self.assertRaises(RuntimeError):
                    probe.collect_outcome(root, status)

    def test_private_record_rejects_symlinks_and_readable_or_oversized_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            path = root / "record"
            probe.write_json(path, {"schema": 1})
            self.assertEqual(probe.read_json(path), {"schema": 1})
            alias = root / "alias"
            alias.symlink_to(path)
            with self.assertRaises(RuntimeError):
                probe.read_json(alias)
            path.chmod(0o644)
            with self.assertRaises(RuntimeError):
                probe.read_json(path)
            path.chmod(0o600)
            path.write_bytes(b" " * (64 * 1024 + 1))
            with self.assertRaises(probe.preparation.ProbeFailure):
                probe.read_json(path)

    def test_changed_test_binary_is_rejected_before_loader_invocation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            artifact = {"variant": "control", "library_path": str(root / probe.preparation.LIBRARY)}
            path = root / "artifact.json"
            probe.write_json(path, artifact)
            binary = root / "test"
            binary.write_bytes(b"changed")
            record = {"control": artifact, "control_metadata_path": str(path), "binary": str(binary),
                      "binary_sha256": hashlib.sha256(b"original").hexdigest(), "loader_path": "/unreached"}
            client = probe.PrivateClient(record, "control")
            with self.assertRaisesRegex(RuntimeError, "executable identity changed"):
                client.command()

    def run_mocked(self, outcomes, prepare_status=0):
        calls = []
        def session(argv, env, timeout=840):
            if "--prepare" in argv:
                calls.append("prepare")
                return prepare_status
            variant = argv[argv.index("--variant") + 1]
            root = Path(argv[argv.index("--contained") + 1])
            calls.append(variant)
            value = outcomes[variant]
            probe.write_json(root / "probe-outcome.json", value)
            return 0 if value["capture_passed"] else 1
        with mock.patch.dict(os.environ, {"PATH": "/test"}, clear=True), \
             mock.patch.object(probe.preparation, "check_eligibility"), \
             mock.patch.object(probe, "containment_check"), \
             mock.patch.object(probe, "namespace_command", side_effect=lambda argv, *_: argv), \
             mock.patch.object(probe.wayland, "configs"), \
             mock.patch.object(probe.wayland, "private_environment", return_value={}), \
             mock.patch.object(probe.wayland, "run_session", side_effect=session), \
             contextlib.redirect_stdout(io.StringIO()):
            try:
                result = probe.run()
            except RuntimeError:
                result = "aborted"
        return result, calls

    def test_system_failure_is_preserved_while_both_diagnostics_run_once(self):
        result, calls = self.run_mocked({"system": outcome(False), "control": outcome(False), "patched": outcome(True)})
        self.assertEqual(result, 1)
        self.assertEqual(calls, ["prepare", "system", "control", "patched"])

    def test_infrastructure_failure_aborts_following_sessions(self):
        result, calls = self.run_mocked({"system": outcome(False, cleanup_passed=False)})
        self.assertEqual(result, "aborted")
        self.assertEqual(calls, ["prepare", "system"])
        result, calls = self.run_mocked({}, prepare_status=1)
        self.assertEqual((result, calls), ("aborted", ["prepare"]))

    def test_incomplete_trace_does_not_become_a_pass(self):
        cases = {variant: outcome(True) for variant in probe.VARIANTS}
        self.assertEqual(self.run_mocked(cases)[0], 0)
        cases["control"]["trace_complete"] = False
        self.assertEqual(self.run_mocked(cases)[0], 1)


if __name__ == "__main__":
    unittest.main()
