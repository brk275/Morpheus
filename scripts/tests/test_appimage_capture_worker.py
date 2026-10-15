"""Pure/mock per-session capture isolation; never starts a native process/bus."""
import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "packaging/linux"))
sys.path.insert(0, str(ROOT / "scripts"))
import appimage_capture as C
import appimage_capture_worker as W
import appimage_diagnostics as D
from appimage_process import CommandFailure


class WorkerTests(unittest.TestCase):
    def inputs(self, folder, mode="ordinary"):
        root = Path(folder).resolve()
        root.chmod(0o700)
        repo = root / "checkout"
        (repo / "target").mkdir(parents=True, exist_ok=True)
        owner = root / "private"
        owner.mkdir(mode=0o700, exist_ok=True)
        case = owner / ("bindings" if mode == "eager_bindings" else "ordinary")
        appdir = owner / "AppDir"
        paths = {"artifact": owner / "candidate.AppImage", "peer": repo / "target/peer",
                 "backend": owner / "backend", "fixture": owner / "fixture.json",
                 "executable": appdir / "usr/bin/morpheus-app", "library": appdir / "usr/lib/libpipewire-0.3.so.0"}
        for name, path in paths.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(("synthetic-" + name).encode())
            path.chmod(0o700 if name in {"artifact", "peer", "backend", "executable"} else 0o600)
        expected = {"schema": 1, "mode": mode, "appdir": str(appdir),
                    "files": {name: W.file_record(path, W.BOUNDS[name]) for name, path in paths.items()}}
        return repo, owner, case, appdir, paths, expected

    def result(self, expected):
        value = {"mode": expected["mode"], "actual_gui_capture": True,
                 "peer": {name: True for name in C.FACTS}, "untouched_static_reopen_verified": False,
                 "executable_sha256": expected["files"]["executable"]["sha256"],
                 "pipewire_sha256": expected["files"]["library"]["sha256"]}
        if expected["mode"] == "eager_bindings":
            value["binding_evidence"] = {"app_pipewire_imports": 1, "app_pipewire_bindings_verified": 1,
                "unexpected_providers": 0, "actual_imports": ["pw_context_new"],
                "collision_symbols_not_imported": ["pw_stream_new"],
                "executable_sha256": value["executable_sha256"], "pipewire_sha256": value["pipewire_sha256"]}
        return value

    def write_input(self, case, expected):
        case.mkdir(mode=0o700)
        path = case / "case-input.json"
        W.client.write_new(path, json.dumps(expected).encode())
        return path

    def test_private_input_accepts_only_owned_exact_files_and_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            repo, owner, case, appdir, paths, expected = self.inputs(directory)
            path = self.write_input(case, expected)
            with mock.patch.object(W, "ROOT", repo):
                self.assertEqual(W.validate_input(path)[2], paths)
                bad = copy.deepcopy(expected)
                bad["files"]["fixture"]["path"] = str(paths["peer"])
                bad["files"]["fixture"]["sha256"] = expected["files"]["peer"]["sha256"]
                for value in (dict(expected, extra="private payload"), dict(expected, schema=True), bad,
                              dict(expected, mode="eager_bindings")):
                    path.write_text(json.dumps(value))
                    with self.assertRaises(W.CaptureFailure):
                        W.validate_input(path)
                path.write_text(json.dumps(expected))
                paths["artifact"].write_bytes(b"changed")
                with self.assertRaises(W.CaptureFailure):
                    W.validate_input(path)

    def test_control_symlinks_shared_modes_and_oversize_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            repo, owner, case, appdir, paths, expected = self.inputs(directory)
            path = self.write_input(case, expected)
            with mock.patch.object(W, "ROOT", repo):
                path.chmod(0o644)
                with self.assertRaises(W.CaptureFailure):
                    W.validate_input(path)
                path.chmod(0o600)
                alias = case / "saved-input.json"
                path.rename(alias)
                path.symlink_to(alias)
                with self.assertRaises(W.CaptureFailure):
                    W.validate_input(path)
                path.unlink()
                alias.rename(path)
                path.write_bytes(b"x" * 32769)
                with self.assertRaises(RuntimeError):
                    W.validate_input(path)

    def test_results_preserve_every_fact_mode_identity_and_binding_gate(self):
        with tempfile.TemporaryDirectory() as directory:
            *_, expected = self.inputs(directory, "eager_bindings")
            good = self.result(expected)
            self.assertEqual(W.validate_result(good, expected), good)
            invalid = [dict(good, extra="private token"), dict(good, mode="ordinary"),
                       dict(good, actual_gui_capture=1), dict(good, pipewire_sha256="0" * 64)]
            for key, value in (("static_frame", False), ("cleanup", 1), ("complete", None)):
                bad = copy.deepcopy(good)
                bad["peer"][key] = value
                invalid.append(bad)
            for key, value in (("unexpected_providers", 1), ("app_pipewire_bindings_verified", 0),
                               ("app_pipewire_imports", True), ("actual_imports", ["private-url"])):
                bad = copy.deepcopy(good)
                bad["binding_evidence"][key] = value
                invalid.append(bad)
            for value in invalid:
                with self.assertRaises(W.CaptureFailure) as failure:
                    W.validate_result(value, expected)
                self.assertNotIn("token", str(failure.exception))

    def isolated(self, folder, *, child_error=None, cleanup="passed", bad_result=False, mode="ordinary", missing_progress=False, supervisor_cleanup_failed=False, prior_case=False):
        repo, owner, case, appdir, paths, expected = self.inputs(folder, mode)
        parent = D.Progress(owner)
        parent.wrapper("capture")
        if prior_case:
            parent.value["peer_facts"] = sorted(C.FACTS)
            parent.capture("ordinary", "complete")
            parent.cleanup(True)
        def execute(stage, argv, **kwargs):
            self.assertEqual(stage, "capture-case")
            self.assertEqual(argv[1], str(Path(W.__file__).resolve()))
            self.assertEqual(kwargs["timeout"], 900)
            self.assertEqual(kwargs["output_limit"], 0)
            self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", kwargs["env"])
            if prior_case:
                self.assertEqual(parent.value["peer_facts"], [])
                self.assertEqual(parent.value["inner_cleanup"], "not_started")
                self.assertEqual(parent.value["capture_mode"], mode)
                self.assertEqual(parent.value["capture_phase"], "setup")
            if supervisor_cleanup_failed:
                runner.call_args.kwargs["progress"].cleanup(False)
            if missing_progress:
                raise child_error or CommandFailure("exit")
            child = D.Progress(case)
            child.wrapper("capture")
            child_failed = child_error is not None and child_error.reason != "cleanup"
            child.capture(mode, "static_frame" if child_failed else "complete")
            if child_failed:
                child.command("capture-green")
                child.failed(CommandFailure("deadline"))
            child.cleanup(cleanup == "passed")
            result = self.result(expected)
            if bad_result:
                result["private-data"] = "never print"
            W.client.write_new(case / "case-result.json", json.dumps(result).encode())
            if child_error:
                raise child_error
            return b""
        with mock.patch.object(W.containment, "assert_contained"), \
                mock.patch.object(W, "OwnedRunner") as runner, \
                mock.patch.object(C, "run_case", side_effect=AssertionError("must execute in fresh worker")):
            runner.return_value.run.side_effect = execute
            try:
                result = W.run_isolated(case, paths["artifact"], appdir, paths["peer"], paths["backend"],
                    paths["fixture"], {"PATH": "/usr/bin:/bin"}, bindings=mode == "eager_bindings", diagnostics=parent)
                return result, runner, parent
            except BaseException as error:
                error.progress = parent
                raise

    def test_worker_exec_is_required_and_no_parent_atspi_is_initialized(self):
        with tempfile.TemporaryDirectory() as directory:
            result, runner, parent = self.isolated(directory)
            self.assertTrue(result["actual_gui_capture"])
            self.assertEqual(runner.return_value.run.call_count, 1)
            self.assertEqual(parent.value["capture_phase"], "complete")
            self.assertEqual(parent.value["inner_cleanup"], "passed")

    def test_each_case_executes_a_new_worker_with_no_cached_bus_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            first, first_runner, _ = self.isolated(directory)
            second, second_runner, _ = self.isolated(directory, mode="eager_bindings")
            first_call, second_call = first_runner.return_value.run.call_args, second_runner.return_value.run.call_args
            self.assertEqual([first["mode"], second["mode"]], ["ordinary", "eager_bindings"])
            self.assertNotEqual(first_call.args[1][-1], second_call.args[1][-1])
            for call in (first_call, second_call):
                self.assertEqual(call.args[1][0], str(Path(sys.executable).resolve()))
                self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", call.kwargs["env"])
                self.assertNotIn("AT_SPI_BUS_ADDRESS", call.kwargs["env"])

    def test_supervisor_cleanup_failure_rejects_even_completed_worker_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(CommandFailure) as caught:
                self.isolated(directory, child_error=CommandFailure("cleanup"))
            self.assertEqual(caught.exception.progress.value["failure"]["reason"], "command_cleanup")
            caught.exception.progress.cleanup(True)  # Later Docker teardown.
            self.assertEqual(caught.exception.progress.value["inner_cleanup"], "failed")

    def test_launch_failure_without_progress_records_the_worker_setup_phase(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(CommandFailure) as caught:
            self.isolated(directory, missing_progress=True)
        failure = caught.exception.progress.value["failure"]
        self.assertEqual(failure["capture_phase"], "setup")
        self.assertEqual(failure["command_stage"], "capture-case")
        self.assertEqual(failure["reason"], "command_exit")
        self.assertNotIn("private", json.dumps(failure))

    def test_deadline_without_worker_cleanup_confirmation_stays_failed_after_docker_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(CommandFailure) as caught:
                self.isolated(directory, missing_progress=True, child_error=CommandFailure("deadline"))
            self.assertEqual(caught.exception.progress.value["failure"]["reason"], "command_deadline")
            caught.exception.progress.cleanup(True)
            self.assertEqual(caught.exception.progress.value["inner_cleanup"], "failed")

    def test_second_case_launch_failure_never_inherits_first_case_peer_facts(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(CommandFailure) as caught:
            self.isolated(directory, mode="eager_bindings", missing_progress=True, prior_case=True)
        value = caught.exception.progress.value
        self.assertEqual(value["capture_mode"], "eager_bindings")
        self.assertEqual(value["capture_phase"], "setup")
        self.assertEqual(value["peer_facts"], [])
        self.assertEqual(value["inner_cleanup"], "failed")

    def test_supervisor_cleanup_failure_survives_an_earlier_command_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(CommandFailure) as caught:
                self.isolated(directory, child_error=CommandFailure("exit"), supervisor_cleanup_failed=True)
            self.assertEqual(caught.exception.reason, "exit")
            self.assertEqual(caught.exception.progress.value["failure"]["reason"], "command_deadline")
            caught.exception.progress.cleanup(True)
            self.assertEqual(caught.exception.progress.value["inner_cleanup"], "failed")

    def test_capture_failure_after_confirmed_worker_cleanup_keeps_accurate_cleanup_status(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(CommandFailure) as caught:
            self.isolated(directory, child_error=CommandFailure("exit"))
        self.assertEqual(caught.exception.progress.value["failure"]["reason"], "command_deadline")
        self.assertEqual(caught.exception.progress.value["inner_cleanup"], "passed")

    def test_worker_first_failure_and_cleanup_failure_survive_parent_adoption(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(CommandFailure) as caught:
            self.isolated(directory, child_error=CommandFailure("exit"), cleanup="failed")
        value = caught.exception.progress.value
        self.assertEqual(value["failure"]["reason"], "command_deadline")
        self.assertEqual(value["failure"]["capture_phase"], "static_frame")
        self.assertEqual(value["inner_cleanup"], "failed")

    def test_zero_exit_with_bad_result_or_failed_cleanup_never_passes(self):
        for args in ({"cleanup": "failed"}, {"bad_result": True}, {"supervisor_cleanup_failed": True}):
            with tempfile.TemporaryDirectory() as directory, self.assertRaises(W.CaptureFailure):
                self.isolated(directory, **args)

    def test_invalid_progress_never_enters_public_diagnostic_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            case = root / "case"
            case.mkdir()
            progress = D.Progress(root)
            child = D.Progress(case)
            child.value["failure"] = {"reason": "private token"}
            child.path.write_text(json.dumps(child.value))
            original = copy.deepcopy(progress.value)
            with self.assertRaises(W.CaptureFailure):
                W.adopt_progress(progress, case, "ordinary")
            self.assertEqual(progress.value, original)


class WorkerContainmentTests(unittest.TestCase):
    def guard(self, *, pid=42, ppid=1, namespace="pid:[44]", parent_namespace="pid:[44]",
              self_changes=None, parent_changes=None, bad_args=None, bad_command=False):
        control = {"uid": 1001, "gid": 1001, "groups": [1001], "outer_namespace": "pid:[33]",
                   "args": bad_args if bad_args is not None else [str(Path(sys.executable).resolve()),
                       str(Path(W.__file__).with_name("package_appimage.py").resolve()), "--inside", "/private/qa", "--cargo", "/home/runner/.cargo/bin/cargo"]}
        fields = {"Uid": "1001 1001 1001 1001", "Gid": "1001 1001 1001 1001", "Groups": "1001",
                  "NoNewPrivs": "1", **{name: "00000000" for name in W.containment.CAPABILITIES}}
        child = {**fields, "NSpid": str(pid), **(self_changes or {})}
        parent = {**fields, "NSpid": "1", **(parent_changes or {})}
        def read(path, *args, **kwargs):
            self.assertIn(str(path), {"/proc/self/status", "/proc/1/status"})
            selected = child if str(path) == "/proc/self/status" else parent
            return "\n".join(name + ":\t" + value for name, value in selected.items())
        command = b"changed-parent\0" if bad_command else b"\0".join(arg.encode() for arg in control["args"]) + b"\0"
        original_readlink = os.readlink
        def readlink(path, *args, **kwargs):
            if str(path) == "/proc/self/ns/pid":
                return namespace
            if str(path) == "/proc/1/ns/pid":
                return parent_namespace
            return original_readlink(path, *args, **kwargs)
        with mock.patch.object(W.sys, "platform", "linux"), \
                mock.patch.object(W.containment, "read_control", return_value=control), \
                mock.patch.object(W, "parent_command", return_value=command), \
                mock.patch.object(W.os, "getresuid", return_value=(1001, 1001, 1001), create=True), \
                mock.patch.object(W.os, "getresgid", return_value=(1001, 1001, 1001), create=True), \
                mock.patch.object(W.os, "getgroups", return_value=[1001]), \
                mock.patch.object(W.os, "getpid", return_value=pid), \
                mock.patch.object(W.os, "getppid", return_value=ppid), \
                mock.patch.object(W.os, "readlink", side_effect=readlink), \
                mock.patch.object(W.Path, "read_text", autospec=True, side_effect=read):
            return W.assert_worker_contained(Path("/private/qa"))

    def test_fresh_non_pid1_direct_child_is_valid_only_in_owned_namespace(self):
        self.assertEqual(self.guard(), "pid:[44]")
        for changes in ({"pid": 1}, {"ppid": 41}, {"bad_command": True}, {"namespace": "pid:[33]"},
                        {"parent_namespace": "pid:[55]"}, {"self_changes": {"NSpid": "600 42"}},
                        {"parent_changes": {"Uid": "0 0 0 0"}}, {"self_changes": {"Groups": "0 1001"}},
                        {"self_changes": {"NoNewPrivs": "0"}}, {"parent_changes": {"CapBnd": "01"}}, {"bad_args": ["/unrelated", "--inside", "/private/qa"]}):
            with self.subTest(changes=changes), self.assertRaises(W.CaptureFailure):
                self.guard(**changes)

    def test_default_runner_retains_pid1_guard_and_worker_uses_descendant_guard(self):
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(W.containment, "assert_contained") as parent, \
                mock.patch.object(W, "assert_worker_contained") as child:
            W.OwnedRunner(Path(directory) / "parent").assert_containment()
            W.WorkerRunner(Path(directory) / "child", Path(directory)).assert_containment()
            parent.assert_called_once_with()
            child.assert_called_once_with(Path(directory))


class AccessibilityBusTests(unittest.TestCase):
    def test_private_session_replaces_ambient_bus_before_api_loading(self):
        env = {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/case-bus", "XDG_CONFIG_HOME": "/private/config",
               "AT_SPI_BUS_ADDRESS": "unix:path=/wrong-a11y-bus", "DBUS_STARTER_ADDRESS": "unix:path=/wrong-session"}
        calls = []
        def command(stage, argv, **kwargs):
            self.assertEqual(dict(os.environ), {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/case-bus",
                                               "XDG_CONFIG_HOME": "/private/config"})
            self.assertEqual(stage, "capture-accessibility")
            self.assertEqual(kwargs, {"seconds": 5, "limit": 4096})
            self.assertEqual(argv[:6], ["gdbus", "call", "--session", "--dest", "org.a11y.Bus", "--object-path"])
            calls.append(argv[-2:])
        with mock.patch.dict(os.environ, {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/ambient", "DISPLAY": ":0"}, clear=True):
            C.enable_private_accessibility(command, env)
        self.assertEqual(calls, [["IsEnabled", "<true>"], ["ScreenReaderEnabled", "<true>"]])
        self.assertNotIn("AT_SPI_BUS_ADDRESS", env)

    def test_initialized_atspi_is_rejected_instead_of_cross_case_reuse(self):
        for module in ("pyatspi", "gi.repository.Atspi"):
            with mock.patch.dict(sys.modules, {module: object()}), \
                    mock.patch.dict(os.environ, {"sentinel": "retained"}, clear=True), self.assertRaises(C.CaptureFailure):
                C.enable_private_accessibility(mock.Mock(), {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/new"})
            self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", os.environ)

    def test_enablement_failure_propagates_before_any_ui_client(self):
        command = mock.Mock(side_effect=CommandFailure("exit"))
        with mock.patch.dict(os.environ, {}, clear=True), self.assertRaises(CommandFailure):
            C.enable_private_accessibility(command, {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private"})
        self.assertEqual(command.call_count, 1)


if __name__ == "__main__":
    unittest.main()
