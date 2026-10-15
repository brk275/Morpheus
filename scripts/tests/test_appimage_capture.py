"""Pure/mock installed-capture contracts; no process/native service is run."""
import hashlib
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
sys.path.insert(0, str(ROOT / "scripts"))
import appimage_capture as M
import appimage_capture_worker as W


class ProofTests(unittest.TestCase):
    def proof(self, outcome="ok", summary="ok. 1 passed; 0 failed; 0 ignored;"):
        return ("test " + M.PEER_TEST + " ... " + outcome + "\n"
                + "test result: " + summary + " 0 measured; 0 filtered out; finished in 1s\n").encode()

    def test_named_peer_and_one_full_success_are_both_required(self):
        M.peer_proof(self.proof())
        for value in (b"", self.proof("ignored"), self.proof(summary="ok. 0 passed; 0 failed; 1 ignored;"),
                      self.proof() * 2, self.proof().replace(M.PEER_TEST.encode(), b"different_test")):
            with self.subTest(value=value), self.assertRaises(M.CaptureFailure):
                M.peer_proof(value)

    def test_source_markers_match_all_required_peer_receipts(self):
        import re
        source = (ROOT / "packaging/linux/appimage_peer.rs").read_text()
        self.assertEqual(set(re.findall(r'fact\(&root, "([a-z_]+)"\)', source)), set(M.FACTS))
        commands = set(re.findall(r'command\(&root, "([a-z_]+)"\)', source))
        self.assertEqual(commands, {"static", "changed", "stopped", "reopened", "changed_again", "source_lost", "left"})

    def test_private_fact_cannot_be_overwritten_or_fabricated_by_other_value(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertFalse(M.fact(root, "static_frame"))
            path = root / "static_frame.json"
            path.write_bytes(b'{"passed":false}\n')
            self.assertFalse(M.fact(root, "static_frame"))
            path.write_bytes(b'{"passed":true}\n')
            self.assertTrue(M.fact(root, "static_frame"))
            M.request(root, "static")
            with self.assertRaises(FileExistsError):
                M.request(root, "static")
            with self.assertRaises(M.CaptureFailure):
                M.request(root, "../private")

    def test_capture_output_cannot_contain_the_app_or_ambiguous_app_window(self):
        def output(name, *nodes):
            return {"type": "output", "name": name, "nodes": list(nodes)}
        good = {"nodes": [output("HEADLESS-1", {"pid": 14}), output("HEADLESS-2")]}
        M.owned_window_on_first_output(good, 14)
        for bad in ({"nodes": [output("HEADLESS-2", {"pid": 14})]},
                    {"nodes": [output("HEADLESS-1", {"pid": 14}, {"pid": 14})]}, {}):
            with self.assertRaises(M.CaptureFailure):
                M.owned_window_on_first_output(bad, 14)


class OwnershipTests(unittest.TestCase):
    class Sink:
        def __init__(self, path):
            self.attach = mock.Mock()
        def __enter__(self):
            return self
        def __exit__(self, *_):
            pass

    def test_spawn_is_owned_before_any_reader_attachment_failure(self):
        owner = M.Processes(Path("/private"))
        child = SimpleNamespace(pid=12, stdout=io.BytesIO(), stderr=io.BytesIO())
        sinks = [self.Sink(None), self.Sink(None)]
        sinks[0].attach.side_effect = RuntimeError("untrusted raw detail")
        with mock.patch.object(M, "CappedPrivateLog", side_effect=sinks), \
                mock.patch.object(M.subprocess, "Popen", return_value=child) as popen, \
                mock.patch.object(M.containment, "stop_owned") as stop:
            with self.assertRaises(RuntimeError):
                owner.start("app", ["/private/installed.AppImage"], {})
            self.assertEqual(owner.owned, [child])
            owner.close()
            stop.assert_called_once_with(child)
            self.assertTrue(popen.call_args.kwargs["start_new_session"])

    def test_one_cleanup_failure_does_not_skip_other_owned_groups(self):
        owner = M.Processes(Path("/private"))
        first, second = object(), object()
        owner.owned = [first, second]
        with mock.patch.object(M.containment, "stop_owned", side_effect=[RuntimeError("private"), None]) as stop:
            with self.assertRaises(M.CaptureFailure) as error:
                owner.close()
            self.assertEqual(stop.call_args_list, [mock.call(second), mock.call(first)])
            self.assertNotIn("private", str(error.exception))


class TeardownTests(unittest.TestCase):
    def execute(self, root, *, inventory_failure=False, remaining=False, case_error=None, cleanup_failure=False):
        root = root.resolve()
        artifact = root / "candidate.AppImage"
        artifact.write_bytes(b"unsigned-private-fixture")
        fixture = root / "accounts.json"
        fixture.write_text('{}')
        fixture.chmod(0o600)
        calls = []
        fixture_count = 0
        def run(label, argv, **kwargs):
            nonlocal fixture_count
            calls.append((label, argv))
            if label == "capture-inventory":
                if inventory_failure:
                    raise RuntimeError("private daemon body")
                return M.CONTAINERS[0].encode() if remaining else b""
            if label.endswith("rtc-fixture"):
                fixture_count += 1
                fixture.write_text(json.dumps({"fresh_fixture": fixture_count}))
                return str(fixture).encode()
            if cleanup_failure and label == "capture-rtc-down":
                raise RuntimeError("private teardown body")
            return b""
        runner = SimpleNamespace(run=mock.Mock(side_effect=run))
        with mock.patch.object(M.containment, "assert_contained"), \
                mock.patch.object(M, "executable", side_effect=lambda name: "/usr/bin/" + name), \
                mock.patch.object(M.shutil, "which", return_value="/usr/bin/cargo"), \
                mock.patch.object(M, "compile_peer", return_value=Path("/private/peer")), \
                mock.patch.object(M, "prepare_backend", return_value=Path("/private/backend")), \
                mock.patch.object(W, "run_isolated", side_effect=case_error or [{"mode": "ordinary"}, {"mode": "eager_bindings"}]) as case:
            try:
                result = M.run(runner, root, artifact, root / "appdir", {
                    "appimage_sha256": hashlib.sha256(artifact.read_bytes()).hexdigest()}, {})
                return result, calls, case
            except BaseException as error:
                error.calls, error.case = calls, case
                raise

    def test_preexisting_or_unavailable_inventory_never_takes_cleanup_ownership(self):
        for unavailable in (False, True):
            with tempfile.TemporaryDirectory() as directory, self.assertRaises(Exception) as error:
                root = Path(directory)
                self.execute(root, inventory_failure=unavailable, remaining=not unavailable)
                self.fail("inventory refusal expected")
            labels = [label for label, _ in error.exception.calls]
            self.assertFalse(any(label.endswith("-up") or label.endswith("-down") for label in labels))
            error.exception.case.assert_not_called()

    def test_normal_and_binding_runs_use_fresh_fixtures_and_retained_volumes(self):
        with tempfile.TemporaryDirectory() as directory:
            result, calls, case = self.execute(Path(directory))
            self.assertTrue(result["installed_capture_passed"])
            self.assertTrue(result["fixture_volumes_retained"])
            self.assertFalse(result["system_gate_replaced"])
            self.assertEqual([call.kwargs["bindings"] for call in case.call_args_list], [False, True])
            self.assertEqual(sum(label == "capture-rtc-rtc-fixture" for label, _ in calls), 2)
            self.assertEqual(sum(label == "capture-inventory" and argv[1] == "volume" for label, argv in calls), 1)

    def test_original_interrupt_survives_both_cleanup_attempts_and_final_inventory(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(KeyboardInterrupt) as error:
            self.execute(Path(directory), case_error=KeyboardInterrupt(), cleanup_failure=True)
        labels = [label for label, _ in error.exception.calls]
        self.assertIn("capture-rtc-down", labels)
        self.assertIn("capture-env-down", labels)
        self.assertEqual(labels[-2:], ["capture-inventory", "capture-inventory"])

    def test_cleanup_failure_cannot_turn_capture_into_success(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(M.CaptureFailure) as error:
            self.execute(Path(directory), cleanup_failure=True)
        self.assertIn("capture-env-down", [label for label, _ in error.exception.calls])


if __name__ == "__main__":
    unittest.main()
