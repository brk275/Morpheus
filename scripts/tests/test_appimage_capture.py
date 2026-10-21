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
import linux_package_ui as UI


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


class SelectorTests(unittest.TestCase):
    class TransientError(Exception):
        pass

    def node(self, label, *, children=(), action=None, editable=None, count=None):
        def query(value):
            if value is None:
                return mock.Mock(side_effect=NotImplementedError)
            if isinstance(value, BaseException):
                return mock.Mock(side_effect=value)
            return mock.Mock(return_value=value)
        return SimpleNamespace(
            name=label, clearCache=mock.Mock(), get_process_id=mock.Mock(return_value=42),
            get_child_count=mock.Mock(return_value=len(children) if count is None else count),
            get_child_at_index=mock.Mock(side_effect=lambda index: children[index]),
            queryAction=query(action), queryEditableText=query(editable),
            getState=mock.Mock(return_value=SimpleNamespace(contains=lambda _: True)))

    def ui(self, children, *, foreign=()):
        ui = M.InstalledUI.__new__(M.InstalledUI)
        application = self.node("Owned app", children=children)
        other = self.node("Other app", children=foreign)
        other.get_process_id.return_value = 99
        desktop = self.node("Desktop", children=(other, application))
        ui.api = SimpleNamespace(Registry=SimpleNamespace(getDesktop=lambda _: desktop), STATE_ENABLED=1)
        ui.transient_error = self.TransientError
        ui.owned = mock.Mock(side_effect=lambda pid: pid == 42)
        ui.diagnostics = UI.Diagnostics({})
        return ui

    def test_complete_owned_action_and_editable_matches_are_accepted(self):
        for editable in (False, True):
            with self.subTest(editable=editable):
                interface = SimpleNamespace(nActions=1)
                node = self.node("Target", **{"editable" if editable else "action": interface})
                foreign = self.node("Target", action=SimpleNamespace(nActions=1), editable=object())
                ui = self.ui((node,), foreign=(foreign,))
                self.assertEqual(ui.find_control("Target", editable=editable), (node, interface))
                self.assertFalse(ui.pass_incomplete)
                self.assertEqual(ui.diagnostics.value["incomplete_passes"], 0)

    def test_incomplete_owned_tree_cannot_prove_presence_or_absence(self):
        for editable in (False, True):
            for present in (False, True):
                for missing_child in (False, True):
                    with self.subTest(editable=editable, present=present, missing_child=missing_child):
                        broken = self.node("Branch", children=(None,)) if missing_child else self.node("Branch", count=-1)
                        found = self.node("Target", action=SimpleNamespace(nActions=1), editable=object())
                        ui = self.ui((broken, found) if present else (broken,))
                        with self.assertRaises(UI.IncompleteTraversal):
                            ui.find_control("Target", editable=editable)
                        self.assertTrue(ui.pass_incomplete)
                        self.assertEqual(ui.diagnostics.value["incomplete_passes"], 1)

    def test_transient_interface_errors_cannot_hide_a_potential_second_match(self):
        for editable in (False, True):
            for error_type in (self.TransientError, LookupError):
                with self.subTest(editable=editable, error_type=error_type):
                    good = self.node("Target", action=SimpleNamespace(nActions=1), editable=object())
                    hidden = self.node("Target", **{"editable" if editable else "action": error_type()})
                    ui = self.ui((hidden, good))
                    with self.assertRaises(UI.IncompleteTraversal):
                        ui.find_control("Target", editable=editable)
                    self.assertTrue(ui.pass_incomplete)
                    self.assertEqual(ui.diagnostics.value["incomplete_passes"], 1)

    def test_unsupported_interface_and_zero_action_caption_are_ordinary_nonmatches(self):
        for editable in (False, True):
            with self.subTest(editable=editable):
                interface = SimpleNamespace(nActions=1)
                good = self.node("Target", **{"editable" if editable else "action": interface})
                unsupported = self.node("Target")
                captions = () if editable else (self.node("Target", action=SimpleNamespace(nActions=0)),)
                ui = self.ui((unsupported, *captions, good))
                self.assertEqual(ui.find_control("Target", editable=editable), (good, interface))
                self.assertFalse(ui.pass_incomplete)
                self.assertEqual(ui.diagnostics.value["incomplete_passes"], 0)

    def test_duplicate_matches_still_fail_before_an_incomplete_pass_can_retry(self):
        for editable in (False, True):
            for interface_error in (False, True):
                with self.subTest(editable=editable, interface_error=interface_error):
                    first = self.node("Target", action=SimpleNamespace(nActions=1), editable=object())
                    second = self.node("Target", action=SimpleNamespace(nActions=1), editable=object())
                    broken = (self.node("Target", **{"editable" if editable else "action": self.TransientError()})
                              if interface_error else self.node("Branch", count=-1))
                    ui = self.ui((second, first, broken))
                    with self.assertRaises(M.CaptureFailure):
                        ui.find_control("Target", editable=editable)
                    self.assertTrue(ui.pass_incomplete)
                    self.assertEqual(ui.diagnostics.value["incomplete_passes"], 0)

    def test_known_duplicate_is_fatal_before_a_later_name_lookup_failure(self):
        for error_type in (self.TransientError, LookupError):
            with self.subTest(error_type=error_type):
                class NameUnavailable:
                    def clearCache(self):
                        pass
                    @property
                    def name(self):
                        raise error_type()
                first = self.node("Target", action=SimpleNamespace(nActions=1))
                second = self.node("Target", action=SimpleNamespace(nActions=1))
                ui = self.ui((NameUnavailable(), first, second))
                with self.assertRaises(M.CaptureFailure):
                    ui.find_control("Target")

    def test_incomplete_pass_never_enters_text_or_presses_an_action(self):
        for editable in (False, True):
            with self.subTest(editable=editable):
                action = SimpleNamespace(nActions=1, doAction=mock.Mock(return_value=True))
                field = SimpleNamespace(setTextContents=mock.Mock(return_value=True))
                good = self.node("Target", action=action, editable=field)
                ui = self.ui((self.node("Branch", count=-1), good))
                ui.process = object()
                with mock.patch.object(UI.smoke, "process_status", return_value=None), \
                        mock.patch.object(UI.time, "monotonic", side_effect=(0, 0, 13, 13)), \
                        mock.patch.object(UI.time, "sleep"):
                    with self.assertRaises(UI.DiagnosticFailure) as error:
                        if editable:
                            ui.enter("Target", "synthetic value")
                        else:
                            ui.press("Target")
                self.assertEqual(error.exception.code, "wait_timeout")
                action.doAction.assert_not_called()
                field.setTextContents.assert_not_called()

    def test_unowned_tree_and_the_existing_tree_bound_remain_enforced(self):
        foreign = self.node("Target", action=SimpleNamespace(nActions=1))
        ui = self.ui((), foreign=(foreign,))
        self.assertIsNone(ui.find_control("Target"))
        ui = self.ui((self.node("Other"),) * 5000)
        with self.assertRaises(UI.DiagnosticFailure) as error:
            ui.find_control("Target")
        self.assertEqual(error.exception.code, "tree_bound")


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
