import contextlib
import importlib.util
import io
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("wayland_acceptance", Path(__file__).resolve().parents[1] / "run_wayland_acceptance.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class WaylandFixtureTests(unittest.TestCase):
    def test_binding_facts_require_all_system_library_resolutions(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "facts"
            self.assertFalse(MODULE.bindings_passed(MODULE.binding_facts(path)))
            counts = {"checked": 33, "system_pipewire": 33, "executable": 0,
                      "other": 0, "unavailable": 0}
            path.write_text(MODULE.json.dumps(counts))
            self.assertTrue(MODULE.bindings_passed(MODULE.binding_facts(path)))
            # A single executable stub rejects the run even if capture/libtest
            # were otherwise marked passed by a future refactor.
            counts.update(system_pipewire=32, executable=1)
            path.write_text(MODULE.json.dumps(counts))
            self.assertFalse(MODULE.bindings_passed(MODULE.binding_facts(path)))
            self.assertEqual(MODULE.binding_facts(path)["executable"], 1)

    def test_binding_facts_reject_private_text_and_unbounded_or_invalid_counts(self):
        valid = {"checked": 33, "system_pipewire": 33, "executable": 0,
                 "other": 0, "unavailable": 0}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "facts"
            for value in ({**valid, "path": "/private/loader.so"},
                          {**valid, "checked": True}, {**valid, "checked": 34},
                          {**valid, "system_pipewire": 32},
                          {**valid, "other": "private"}, ["private"]):
                path.write_text(MODULE.json.dumps(value))
                self.assertEqual(MODULE.binding_facts(path), {"status": "invalid"})
            path.write_bytes(b"x" * 1000)
            self.assertEqual(MODULE.binding_facts(path), {"status": "invalid"})
            path.write_bytes(b"\xff\xfe")
            self.assertEqual(MODULE.binding_facts(path), {"status": "unavailable"})

    def test_runtime_binding_probe_covers_complete_pinned_archive_inventory(self):
        repo = Path(__file__).resolve().parents[2]
        inventory = (repo / "vendor/webrtc-sys/pipewire-symbols.txt").read_text().splitlines()
        names = {line.split()[1] for line in inventory}
        self.assertEqual(len(names), 33)
        self.assertEqual(sum(line.endswith(" W") for line in inventory), 33)
        self.assertEqual(sum(line.endswith(" U") for line in inventory), 46)
        source = (repo / "crates/morpheus-rtc/tests/wayland_capture.rs").read_text()
        addresses = source.split("let addresses = addresses![", 1)[1].split("];", 1)[0]
        self.assertEqual(set(MODULE.re.findall(r"\bpw_[a-z_]+\b", addresses)), names)
        self.assertLess(source.index("verify_pipewire_bindings();"), source.index("pipewire::init();"))

    def test_capture_reports_bounded_exit_and_signal_without_native_error_text(self):
        output = (
            "process didn't exit successfully: /private/executable --private-argument "
            "(signal: 6, SIGABRT: private native abort detail)\n"
            "panic in a function that cannot unwind\nlibspa::buffer::Data::chunk\n"
            "unsafe precondition(s) violated: private pointer content\n"
        )
        facts = MODULE.capture_test_facts(output, 101, "first_frame_1")
        self.assertEqual(facts["command_exit_code"], 101)
        self.assertEqual(facts["test_signal"], 6)
        self.assertEqual(facts["last_stage"], "first_frame_1")
        self.assertTrue(facts["non_unwinding_panic"])
        self.assertTrue(facts["pointer_precondition_panic"])
        self.assertTrue(facts["libspa_chunk_frame"])
        rendered = MODULE.json.dumps(facts)
        for private in ("private", "/executable", "SIGABRT", "pointer content"):
            self.assertNotIn(private, rendered)
        self.assertEqual(MODULE.capture_test_facts("", -11)["command_signal"], 11)
        self.assertEqual(MODULE.capture_test_facts("(exit status: 101)", 101)["test_exit_code"], 101)
        invalid = MODULE.capture_test_facts("(signal: 99, SIGOTHER) (exit status: 999)", 999, "private-stage")
        self.assertNotIn("test_signal", invalid)
        self.assertNotIn("test_exit_code", invalid)
        self.assertNotIn("command_exit_code", invalid)
        self.assertEqual(invalid["last_stage"], "unknown")

    def test_capture_stage_accepts_only_bounded_known_phase(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "stage"
            self.assertEqual(MODULE.capture_stage(path), "unavailable")
            path.write_text("opening_display_1")
            self.assertEqual(MODULE.capture_stage(path), "opening_display_1")
            for content in (b"private desktop ID", b"x" * 1000, b"\xff\xfe"):
                path.write_bytes(content)
                self.assertIn(MODULE.capture_stage(path), {"unknown", "unavailable"})

    def test_failed_capture_diagnostics_do_not_echo_panic_payload(self):
        output = (
            "thread 'private test name' panicked at crates/morpheus-rtc/tests/wayland_capture.rs:63:77:\n"
            "portal display selection: starting the screen-sharing stream: "
            "system screen-sharing portal could not complete the request\n"
            "private source /desktop/session identity=not-public\n"
        )
        facts = MODULE.capture_test_facts(output, 101)
        self.assertEqual(facts["fixture_panic_line"], 63)
        self.assertEqual(facts["command"], "failed")
        self.assertTrue(facts["start_stream"])
        self.assertTrue(facts["initial_selection"])
        for private in ("private", "not-public", "/desktop", "identity", "77"):
            self.assertNotIn(private, MODULE.json.dumps(facts))
        self.assertEqual(MODULE.capture_test_facts("", None)["command"], "timed_out")

    def test_reopen_pixel_failures_are_fixed_facts_without_source_or_command_data(self):
        facts = MODULE.capture_test_facts(
            "selected display did not deliver fresh expected pixels\n"
            "fixture pixel animation failed: private source, socket and command\n"
            "cannot change acceptance output: private argument\n",
            101, "first_frame_2",
        )
        self.assertTrue(facts["fresh_pixels_timeout"])
        self.assertTrue(facts["pixel_animation_failed"])
        self.assertTrue(facts["output_command_failed"])
        self.assertEqual(facts["last_stage"], "first_frame_2")
        self.assertFalse(facts["required_test_passed"])
        for private in ("private", "socket", "argument", "source,"):
            self.assertNotIn(private, MODULE.json.dumps(facts))
        clean = MODULE.capture_test_facts("", 0)
        for key in ("fresh_pixels_timeout", "pixel_animation_failed", "output_command_failed"):
            self.assertFalse(clean[key])

    def test_capture_evidence_requires_actual_named_test_and_one_pass(self):
        target = f"test {MODULE.TARGET_TEST} ... ok\n"
        summary = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n"
        self.assertTrue(MODULE.capture_test_passed(target + summary))
        self.assertFalse(MODULE.capture_test_passed(
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s\n"
        ))
        self.assertFalse(MODULE.capture_test_passed(target))
        self.assertFalse(MODULE.capture_test_passed(summary))
        self.assertFalse(MODULE.capture_test_passed(target + summary.replace("1 passed", "0 passed")))
        self.assertFalse(MODULE.capture_test_passed(target.replace("ok", "FAILED") + summary))

    def test_diagnostics_only_emit_literal_facts_not_service_text(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "portal-wlr.log").write_bytes(
                b"private desktop app-id=/private/session token=do-not-copy\n"
                b"dbus: start method invoked\nwlroots: buffer event handler\n"
                b"wlroots: unable to receive a valid format from wlr_screencopy\n"
            )
            (root / "portal.log").write_bytes(b"A backend call failed: private app-id and output\n")
            services = {"portal-wlr": mock.Mock(pid=987654), "portal": mock.Mock(pid=987655)}
            with mock.patch.object(MODULE, "peek_returncode", side_effect=[None, 1]):
                facts = MODULE.diagnostic_facts(root, "capture_test", services)
            rendered = MODULE.json.dumps(facts)
            for private in ("private", "app-id", "do-not-copy", "987654", "987655", directory):
                self.assertNotIn(private, rendered)
            self.assertTrue(facts["portal-wlr.start_received"])
            self.assertTrue(facts["portal-wlr.shm_buffer_announced"])
            self.assertTrue(facts["portal-wlr.no_valid_screencopy_format"])
            self.assertFalse(facts["portal-wlr.dmabuf_buffer_announced"])
            self.assertTrue(facts["portal.backend_call_failed"])
            self.assertEqual(facts["portal-wlr.process"], "running")
            self.assertEqual(facts["portal.process"], "exited")

    def test_diagnostics_handle_missing_logs_and_unknown_stage_without_echoing(self):
        with tempfile.TemporaryDirectory() as directory:
            facts = MODULE.diagnostic_facts(Path(directory), "private-stage-name", {})
        self.assertEqual(facts["stage"], "unknown")
        self.assertTrue(facts["portal.log_unavailable"])
        self.assertEqual(facts["portal.process"], "not_started")
        self.assertNotIn("private-stage-name", str(facts))

    def test_backend_exit_status_and_fatal_phases_are_bounded_private_facts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "portal-wlr.log").write_bytes(
                b"pipewire: fatal error event from core private-id=32767\n"
                b"pipewire: Format negotiated: PRIVATE-FORMAT\n"
                b"/private/backend.c: Assertion `PRIVATE-EXPRESSION' failed.\n"
            )
            services = {"portal-wlr": mock.Mock(pid=123456)}
            for code in (1, 255, -6, -11, -64, 999, -999, True, None):
                with self.subTest(code=code), mock.patch.object(MODULE, "peek_returncode", return_value=code):
                    facts = MODULE.diagnostic_facts(root, "capture_test", services)
                self.assertTrue(facts["portal-wlr.pipewire_core_fatal"])
                self.assertTrue(facts["portal-wlr.pipewire_format_negotiated"])
                self.assertTrue(facts["portal-wlr.assertion_failed"])
                self.assertFalse(facts["portal-wlr.pipewire_buffer_added"])
                if type(code) is int and 0 <= code <= 255:
                    self.assertEqual(facts["portal-wlr.exit_code"], code)
                else:
                    self.assertNotIn("portal-wlr.exit_code", facts)
                if type(code) is int and -64 <= code < 0:
                    self.assertEqual(facts["portal-wlr.signal"], -code)
                else:
                    self.assertNotIn("portal-wlr.signal", facts)
                rendered = MODULE.json.dumps(facts)
                for private in ("PRIVATE", "/private", "123456", "32767", directory):
                    self.assertNotIn(private, rendered)

    def test_first_frame_failure_distinguishes_fixed_end_and_timeout_reasons(self):
        ended = MODULE.capture_test_facts("first PipeWire frame: selected screen-sharing source ended PRIVATE", 101)
        timed_out = MODULE.capture_test_facts("first PipeWire frame: screen-sharing stream stopped producing frames PRIVATE", 101)
        self.assertTrue(ended["selected_source_ended"])
        self.assertFalse(ended["frame_delivery_timeout"])
        self.assertFalse(timed_out["selected_source_ended"])
        self.assertTrue(timed_out["frame_delivery_timeout"])
        self.assertNotIn("PRIVATE", MODULE.json.dumps([ended, timed_out]))

    def test_diagnostics_bound_reads_and_mark_truncation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "portal-wlr.log").write_bytes(
                b"dbus: create session method invoked\n" + b"x" * 512
                + b"\nwlroots: linux_dmabuf event handler\n"
            )
            with mock.patch.object(MODULE, "DIAGNOSTIC_LOG_LIMIT", 128):
                facts = MODULE.diagnostic_facts(root, "capture_test", {})
        self.assertTrue(facts["portal-wlr.log_truncated"])
        self.assertTrue(facts["portal-wlr.create_session_received"])
        self.assertTrue(facts["portal-wlr.dmabuf_buffer_announced"])

    def test_consumer_timeout_progress_and_producer_processing_are_fixed_facts(self):
        progress = {
            "consumer_format_pending": "before consumer format acceptance",
            "consumer_buffers_pending": "before consumer buffer allocation",
            "consumer_streaming_pending": "before consumer streaming",
            "consumer_processing_pending": "before consumer processing",
            "consumer_dequeue_pending": "without a dequeued consumer buffer",
            "consumer_buffer_seen": "after consumer buffer dequeue",
        }
        for expected, suffix in progress.items():
            facts = MODULE.capture_test_facts(
                f"screen-sharing stream stopped producing frames {suffix} PRIVATE-PIXELS", 101)
            self.assertTrue(facts["frame_delivery_timeout"])
            self.assertEqual([key for key in progress if facts[key]], [expected])
            self.assertNotIn("PRIVATE", MODULE.json.dumps(facts))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "portal-wlr.log").write_bytes(
                b'pipewire: stream state changed to "streaming" PRIVATE-NODE\n'
                b"pipewire: out of buffers PRIVATE-ID\n"
                b"pipewire: enqueueing buffer PRIVATE-ADDRESS\n"
                b"wlroots: ready event handler PRIVATE-TIMESTAMP\n"
                b"wlroots: frame copied PRIVATE-SOURCE\n"
            )
            facts = MODULE.diagnostic_facts(root, "capture_test", {})
            for key in ("pipewire_streaming", "pipewire_out_of_buffers", "pipewire_enqueue_attempted",
                        "screencopy_ready", "screencopy_submitted"):
                self.assertTrue(facts[f"portal-wlr.{key}"])
            self.assertNotIn("PRIVATE", MODULE.json.dumps(facts))

    def test_cleanup_rejects_a_reaped_leader_without_signalling_its_pid(self):
        process = mock.Mock(pid=1234, returncode=0)
        with mock.patch.object(MODULE.os, "killpg") as kill:
            with self.assertRaisesRegex(RuntimeError, "reaped before group cleanup"):
                MODULE.stop_owned_groups([process])
        kill.assert_not_called()

    def test_outer_session_keeps_waitable_anchor_until_namespace_cleanup(self):
        process = mock.Mock(pid=1234, returncode=None)
        with mock.patch.object(MODULE.subprocess, "Popen", return_value=process) as spawn, \
                mock.patch.object(MODULE, "wait_owned", return_value=7) as wait, \
                mock.patch.object(MODULE, "stop_owned") as stop:
            self.assertEqual(MODULE.run_session(["contained-wrapper"], {"fixture": "yes"}), 7)
        self.assertEqual(spawn.call_args.kwargs["stdin"], subprocess.DEVNULL)
        self.assertTrue(spawn.call_args.kwargs["start_new_session"])
        wait.assert_called_once_with(process, 840)
        stop.assert_called_once_with(process)
        process.poll.assert_not_called()
        process.wait.assert_not_called()

    def test_outer_timeout_cleans_namespace_without_reading_stale_pid_registry(self):
        process = mock.Mock(pid=1234, returncode=None)
        with mock.patch.object(MODULE.subprocess, "Popen", return_value=process), \
                mock.patch.object(MODULE, "wait_owned", side_effect=subprocess.TimeoutExpired("contained", 1)), \
                mock.patch.object(MODULE, "stop_owned") as stop, \
                mock.patch.object(MODULE, "process_identity") as identity, \
                mock.patch.object(MODULE.os, "killpg") as numeric_kill:
            with self.assertRaises(subprocess.TimeoutExpired):
                MODULE.run_session(["contained-wrapper"], {}, timeout=1)
        stop.assert_called_once_with(process)
        identity.assert_not_called()
        numeric_kill.assert_not_called()
        process.poll.assert_not_called()
        process.wait.assert_not_called()

    def test_inside_session_refuses_host_namespace_before_starting_services(self):
        with mock.patch.object(MODULE, "read_control", return_value={"outer_namespace": "pid:[111]"}), \
                mock.patch.object(MODULE.os, "readlink", return_value="pid:[111]"), \
                mock.patch.object(MODULE, "prepare_backend") as backend:
            with self.assertRaisesRegex(RuntimeError, "containment failed"):
                MODULE.session(Path("unused"))
        backend.assert_not_called()

    def test_inside_session_requires_same_namespace_as_init(self):
        with mock.patch.object(MODULE, "read_control", return_value={"outer_namespace": "pid:[111]"}), \
                mock.patch.object(MODULE.os, "readlink", side_effect=["pid:[222]", "pid:[333]"]):
            with self.assertRaisesRegex(RuntimeError, "containment failed"):
                MODULE.assert_session_namespace()

    def test_main_requires_actual_containment_preflight_before_session_launch(self):
        order = []
        with mock.patch.object(MODULE.sys, "argv", ["wayland-acceptance"]), \
                mock.patch.object(MODULE.sys, "platform", "linux"), \
                mock.patch.object(MODULE.shutil, "which", return_value="available"), \
                mock.patch.object(MODULE, "containment_check", side_effect=lambda _: order.append("probe")), \
                mock.patch.object(MODULE, "namespace_command", side_effect=lambda *args: order.append("wrap") or ["wrapped"]) as wrap, \
                mock.patch.object(MODULE, "run_session", side_effect=lambda *args: order.append("run") or 0):
            self.assertEqual(MODULE.main(), 0)
        self.assertEqual(order, ["probe", "wrap", "run"])
        self.assertIn("--contained-session", wrap.call_args.args[0])

    def test_failed_containment_preflight_never_starts_fixture(self):
        with mock.patch.object(MODULE.sys, "argv", ["wayland-acceptance"]), \
                mock.patch.object(MODULE.sys, "platform", "linux"), \
                mock.patch.object(MODULE.shutil, "which", return_value="available"), \
                mock.patch.object(MODULE, "containment_check", side_effect=RuntimeError("containment failed")), \
                mock.patch.object(MODULE, "namespace_command") as wrap, \
                mock.patch.object(MODULE, "run_session") as run:
            with self.assertRaisesRegex(RuntimeError, "containment failed"):
                MODULE.main()
        wrap.assert_not_called()
        run.assert_not_called()

    def test_fixture_only_authorizes_chosen_output_and_has_no_x11(self):
        with tempfile.TemporaryDirectory(prefix="fixture with spaces ") as directory:
            root = Path(directory)
            sway, portal = MODULE.configs(root)
            self.assertIn("xwayland disable", sway.read_text())
            self.assertIn("chooser_type=simple", portal.read_text())
            self.assertNotIn("output_name=", portal.read_text())
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(MODULE.chooser(root / "chooser-mode"), 0)
            self.assertEqual(output.getvalue(), "HEADLESS-2\n")
            (root / "chooser-mode").write_text("deny")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(MODULE.chooser(root / "chooser-mode"), 0)
            self.assertEqual(output.getvalue(), "")

    def test_private_runtime_preserves_home_and_removes_existing_desktop(self):
        with tempfile.TemporaryDirectory() as directory:
            env = MODULE.private_environment(Path(directory))
            self.assertEqual(env.get("HOME"), os.environ.get("HOME"))
            for name in ("DISPLAY", "WAYLAND_DISPLAY", "SWAYSOCK", "DBUS_SESSION_BUS_ADDRESS"):
                self.assertNotIn(name, env)
            self.assertEqual(env["WLR_HEADLESS_OUTPUTS"], "2")
            self.assertEqual(env["XDG_STATE_HOME"], str(Path(directory) / 'state'))
            self.assertEqual(env["XDG_DATA_HOME"], str(Path(directory) / 'data'))
            self.assertEqual(Path(env["XDG_RUNTIME_DIR"]).stat().st_mode & 0o777, 0o700)


@unittest.skipUnless(sys.platform == "linux", "requires Linux nonreaping waitid and /proc")
class WaylandProcessTreeTests(unittest.TestCase):
    def test_completed_command_keeps_group_identity_until_cleanup(self):
        process = subprocess.Popen([sys.executable, "-c", "raise SystemExit(7)"], start_new_session=True)
        identity = MODULE.process_identity(process.pid)
        try:
            self.assertEqual(MODULE.wait_owned_process(process, timeout=3), 7)
            self.assertIsNone(process.returncode, "observing exit must not reap the leader")
            self.assertIsNotNone(identity)
            self.assertEqual(MODULE.process_identity(process.pid), identity)
            self.assertEqual(MODULE.peek_returncode(process), 7)
        finally:
            MODULE.stop_owned_groups([process])
        self.assertEqual(process.returncode, 7)

    def test_timed_out_command_remains_owned_until_cleanup(self):
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], start_new_session=True)
        identity = MODULE.process_identity(process.pid)
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                MODULE.wait_owned_process(process, timeout=0.05)
            self.assertIsNone(process.returncode)
            self.assertEqual(MODULE.process_identity(process.pid), identity)
        finally:
            MODULE.stop_owned_groups([process])
        self.assertIsNotNone(process.returncode)

    def test_cleanup_kills_term_resistant_descendant_after_leader_exits(self):
        child_program = (
            "import os, signal, sys, time\n"
            "from pathlib import Path\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "ready = Path(sys.argv[1])\n"
            "temporary = ready.with_suffix('.tmp')\n"
            "temporary.write_text(str(os.getpid()))\n"
            "temporary.replace(ready)\n"
            "while True: time.sleep(1)\n"
        )
        parent_program = (
            "import subprocess, sys, time\n"
            "subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]])\n"
            "while True: time.sleep(1)\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            ready = Path(directory) / "child-ready"
            process = subprocess.Popen(
                [sys.executable, "-c", parent_program, child_program, str(ready)],
                start_new_session=True,
            )
            identity = MODULE.process_identity(process.pid)
            child_pid = child_identity = None
            try:
                deadline = time.monotonic() + 3
                while not ready.exists():
                    self.assertIsNone(MODULE.peek_returncode(process))
                    self.assertLess(time.monotonic(), deadline, "child did not initialize")
                    time.sleep(0.01)
                child_pid = int(ready.read_text())
                child_identity = MODULE.process_identity(child_pid)
                self.assertIsNotNone(identity)
                self.assertIsNotNone(child_identity)
                kill_observations = []
                killpg = os.killpg

                def observe_kill(group, sig):
                    if sig == signal.SIGKILL:
                        kill_observations.append((group, MODULE.process_identity(group), MODULE.peek_returncode(process)))
                    killpg(group, sig)

                with mock.patch.object(MODULE.os, "killpg", side_effect=observe_kill):
                    MODULE.stop_owned_groups([process])
                self.assertEqual(process.returncode, -signal.SIGTERM)
                # The TERM-exited leader must still anchor the process group
                # at KILL, so a recycled PID can never be targeted instead.
                self.assertEqual(kill_observations, [(process.pid, identity, -signal.SIGTERM)])
                deadline = time.monotonic() + 3
                while self.child_is_running(child_pid, child_identity):
                    self.assertLess(time.monotonic(), deadline, "cleanup left its descendant running")
                    time.sleep(0.01)
            finally:
                if process.returncode is None:
                    MODULE.stop_owned_groups([process])
                # Also clean up if run against the old, broken implementation.
                if child_pid is not None and MODULE.process_identity(child_pid) == child_identity:
                    with contextlib.suppress(ProcessLookupError):
                        os.kill(child_pid, signal.SIGKILL)

    @staticmethod
    def child_is_running(pid: int, identity: str) -> bool:
        if MODULE.process_identity(pid) != identity:
            return False
        try:
            state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
        except FileNotFoundError:
            return False
        # An orphan may await reaping by the container/runner's init process.
        return state not in ("Z", "X")



class OrderedDeliveryTraceTests(unittest.TestCase):
    @staticmethod
    def line(stamp, topic, source, function, body):
        return f'[D][{stamp}] {topic} | [{source}:1 {function}()] {body}\n'.encode()

    def test_common_clock_orders_fixed_roles_without_exposing_captures(self):
        logs = {
            'pipewire': self.line('12345.000001', 'pw.node', 'impl-node.c', 'node_update_state',
                                  '(morpheus-fixture-producer-42) suspended -> running')
                        + self.line('12345.000003', 'pw.node', 'impl-node.c', 'node_update_state',
                                    '(morpheus-fixture-consumer-43) suspended -> running'),
            'capture-test': self.line('12345.000002', 'pw.stream', 'stream.c', 'impl_send_command', '0xabcdef: start 0'),
            'portal-wlr': self.line('12345.000004', 'pw.stream', 'stream.c', 'pw_stream_queue_buffer',
                                    'deprecated: use pw_stream_trigger_process() to drive the stream.')
                          + b'2026/10/11 23:12:01 [TRACE] - morpheus-fixture-order: raw=12345.000005 event=queue-valid-accepted\n',
        }
        facts = MODULE.ordered_trace_facts(logs, [(12_345_000_006, 'cleanup_begin')])
        self.assertEqual(facts['clock'], 'common')
        self.assertFalse(facts['equal_time_groups'])
        self.assertEqual([group[0]['event'] for group in facts['groups']],
                         ['node_running', 'input_start', 'node_running', 'legacy_trigger', 'queue_valid_accepted', 'cleanup_begin'])
        self.assertEqual(facts['groups'][0][0], {'process': 'daemon', 'role': 'producer', 'event': 'node_running'})
        rendered = MODULE.json.dumps(facts)
        for private in ('12345', 'abcdef', 'morpheus-fixture', '23:12:01', '42', '43'):
            self.assertNotIn(private, rendered)

    def test_equal_microseconds_are_grouped_without_claiming_causal_order(self):
        logs = {name: self.line('00001.000001', 'pw.stream', 'stream.c', 'impl_send_command', '0x123: start 0')
                for name in ('portal-wlr', 'capture-test')}
        facts = MODULE.ordered_trace_facts(logs, [])
        self.assertEqual(facts['clock'], 'partial')
        self.assertTrue(facts['equal_time_groups'])
        self.assertEqual(len(facts['groups']), 1)
        self.assertEqual({event['process'] for event in facts['groups'][0]}, {'producer', 'consumer'})

    def test_wrap_or_split_epoch_refuses_order_instead_of_guessing(self):
        for first, second in [('99999.999999', '00000.000001'), ('00001.000000', '02000.000000')]:
            with self.subTest(first=first):
                data = self.line(first, 'pw.stream', 'stream.c', 'impl_send_command', '0x123: start 0')
                data += self.line(second, 'pw.stream', 'stream.c', 'impl_send_command', '0x123: pause')
                facts = MODULE.ordered_trace_facts({'capture-test': data}, [])
                self.assertEqual(facts['clock'], 'discontinuity')
                self.assertEqual(facts['groups'], [])

    def test_default_logger_or_failed_clock_refuses_mixed_order(self):
        valid = self.line('00001.000001', 'pw.stream', 'stream.c', 'impl_send_command', '0x123: start 0')
        for invalid in [b'[D] pw.stream [stream.c:1 impl_send_command()] 0x123: pause\n',
                        b'2026/10/11 23:12:01 [TRACE] - morpheus-fixture-order: clock-unavailable\n']:
            with self.subTest(invalid=invalid):
                facts = MODULE.ordered_trace_facts({'capture-test': valid, 'portal-wlr': invalid}, [])
                self.assertEqual(facts['clock'], 'unsupported')
                self.assertEqual(facts['groups'], [])
        self.assertEqual(MODULE.ordered_trace_facts({'capture-test': valid}, [(None, 'cleanup_begin')])['clock'], 'unsupported')

    def test_named_source_and_function_are_required_for_role_and_event(self):
        good = self.line('00001.000001', 'pw.node', 'impl-node.c', 'node_update_state',
                         '(morpheus-fixture-consumer-99) idle -> running')
        for bad in [good.replace(b'pw.node', b'pw.link'), good.replace(b'impl-node.c', b'stream.c'),
                    good.replace(b'node_update_state', b'other_function'),
                    good.replace(b'morpheus-fixture-consumer', b'PRIVATE_DESKTOP'),
                    b'private error says ' + good, good.rstrip() + b' PRIVATE_SUFFIX\n']:
            with self.subTest(bad=bad):
                facts = MODULE.ordered_trace_facts({'pipewire': bad}, [])
                self.assertEqual(facts['groups'], [])
                self.assertNotIn('PRIVATE', MODULE.json.dumps(facts))

    def test_multiple_ids_and_recreated_same_id_mark_role_ambiguity(self):
        first = self.line('00001.000001', 'pw.node', 'impl-node.c', 'node_update_state',
                          '(morpheus-fixture-producer-42) idle -> running')
        other = self.line('00001.000003', 'pw.node', 'impl-node.c', 'node_update_state',
                          '(morpheus-fixture-producer-43) idle -> running')
        destroy = self.line('00001.000002', 'pw.node', 'impl-node.c', 'pw_impl_node_destroy',
                            '(morpheus-fixture-producer-42) destroy')
        for data in (first + other, first + destroy + first.replace(b'000001', b'000003')):
            self.assertTrue(MODULE.ordered_trace_facts({'pipewire': data}, [])['multiple_role_instances'])

    def test_link_identity_must_be_observed_before_activation(self):
        active = self.line('00001.000002', 'pw.link', 'impl-link.c', 'pw_impl_link_activate', '(42.0.0 -> 43.0.0) activated')
        created = self.line('00001.000001', 'pw.link', 'impl-link.c', 'pw_context_create_link',
                            '(42.0.0 -> 43.0.0) (morpheus-fixture-producer) -> (morpheus-fixture-consumer)')
        self.assertEqual(MODULE.ordered_trace_facts({'pipewire': active}, [])['groups'], [])
        facts = MODULE.ordered_trace_facts({'pipewire': created + active}, [])
        self.assertEqual([group[0]['event'] for group in facts['groups']], ['link_created', 'link_activated'])
        # An old earlier activation must not be classified retrospectively.
        facts = MODULE.ordered_trace_facts({'pipewire': active + created}, [])
        self.assertEqual([group[0]['event'] for group in facts['groups']], ['link_created'])
        removed = self.line('00001.000003', 'pw.link', 'impl-link.c', 'pw_impl_link_destroy', '(42.0.0 -> 43.0.0) destroy')
        facts = MODULE.ordered_trace_facts({'pipewire': created + removed + active.replace(b'000002', b'000004')}, [])
        self.assertEqual([group[0]['event'] for group in facts['groups']], ['link_created', 'link_destroyed'])

    def test_graph_reports_are_fixed_categories_and_teardown_boundary_is_ordered(self):
        data = self.line('00001.000001', 'pw.node', 'impl-node.c', 'node_ready',
                         '(morpheus-fixture-producer-42) graph not finished: state:0x123 quantum:1024 pending 1/1')
        data += self.line('00001.000003', 'pw.node', 'impl-node.c', 'node_ready',
                          '0x123: ready non-active node morpheus-fixture-producer in state 3')
        facts = MODULE.ordered_trace_facts({'portal-wlr': data}, [(1_000_002, 'cleanup_begin')])
        self.assertEqual([group[0]['event'] for group in facts['groups']], ['graph_incomplete', 'cleanup_begin', 'ready_nonactive'])
        for value in ('42', '0x123', '1024', '1/1'):
            self.assertNotIn(value, MODULE.json.dumps(facts))

    def test_event_count_is_capped_and_missing_drain_stays_incomplete(self):
        data = b''.join(self.line(f'00001.{index:06}', 'pw.stream', 'stream.c', 'impl_send_command', '0x123: start 0')
                        for index in range(150))
        facts = MODULE.ordered_trace_facts({'capture-test': data}, [])
        self.assertEqual(sum(map(len, facts['groups'])), MODULE.ORDER_EVENT_LIMIT)
        self.assertTrue(facts['events_truncated'])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'capture-test.log').write_bytes(data)
            self.assertTrue(MODULE.private_trace_facts(root, {})['ordered']['trace_incomplete'])

    def test_role_overrides_are_child_local_and_daemon_has_no_stream_override(self):
        with mock.patch.dict(os.environ, {'PIPEWIRE_PROPS': 'PRIVATE_TARGET', 'PIPEWIRE_LOG': 'PRIVATE_FILE'}, clear=True):
            producer = MODULE.trace_environment('portal-wlr')
            consumer = MODULE.trace_environment('capture-test')
            daemon = MODULE.trace_environment('pipewire')
            self.assertEqual(os.environ['PIPEWIRE_PROPS'], 'PRIVATE_TARGET')
        self.assertEqual(producer['PIPEWIRE_PROPS'], 'node.name=morpheus-fixture-producer')
        self.assertEqual(consumer['PIPEWIRE_PROPS'], 'node.name=morpheus-fixture-consumer')
        self.assertNotIn('PIPEWIRE_PROPS', daemon)
        self.assertTrue(all('PIPEWIRE_LOG' not in env for env in (producer, consumer, daemon)))
        with self.assertRaises(KeyError):
            MODULE.trace_environment('unknown')

    def test_thread_start_failure_leaves_stream_owned_for_context_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            read_fd, write_fd = os.pipe()
            source = os.fdopen(read_fd, 'rb', buffering=0)
            try:
                with MODULE.CappedPrivateLog(Path(directory) / 'trace') as sink:
                    with mock.patch.object(MODULE.threading.Thread, 'start', side_effect=RuntimeError('PRIVATE')):
                        with self.assertRaises(RuntimeError):
                            sink.attach(source)
                    sink.finish()
                self.assertTrue(source.closed)
                self.assertTrue(sink.output.closed)
            finally:
                os.close(write_fd)


class DeliveryTraceTests(unittest.TestCase):
    def test_trace_environment_is_private_and_targeted(self):
        with mock.patch.dict(os.environ, {'PIPEWIRE_LOG': '/private/user-log', 'OTHER': 'unchanged'}, clear=True):
            env = MODULE.trace_environment("portal-wlr")
        self.assertNotIn('PIPEWIRE_LOG', env)
        self.assertEqual(env['PIPEWIRE_DEBUG'], '1,pw.stream:4,pw.node:4,pw.link:4')
        self.assertEqual(env['PIPEWIRE_PROPS'], 'node.name=morpheus-fixture-producer')
        self.assertEqual(env['PIPEWIRE_LOG_SYSTEMD'], 'false')
        self.assertEqual(env['PIPEWIRE_LOG_COLOR'], 'false')
        self.assertEqual(env['PIPEWIRE_LOG_LINE'], 'true')
        self.assertEqual(env['OTHER'], 'unchanged')

    def test_source_verified_trace_shapes_emit_booleans_only(self):
        lines = [
            '[D][12345.000001] pw.stream    | [        stream.c: 2468 pw_stream_queue_buffer()] deprecated: use pw_stream_trigger_process() to drive the stream.',
            '[D][12345.000001] pw.stream    | [        stream.c:  717 impl_send_command()] 0xabcdef: start 0',
            '[D] pw.stream [stream.c:717 impl_send_command()] 0x123456: start 1',
            '[D] pw.stream [stream.c:517 impl_set_io()] 0xabcdef: set io id 3 (Spa:Enum:IO:Clock) 0xabcdef 160',
            '[D] pw.stream [stream.c:517 impl_set_io()] 0xabcdef: set io id 7 (Spa:Enum:IO:Position) 0xabcdef 512',
            '[D] pw.node [impl-node.c:890 pw_impl_node_set_driver()] 0xabcdef: driver 0x123456 driving:1',
            '[I] pw.node [impl-node.c:1779 node_ready()] 0xabcdef: ready non-active node PRIVATE_SOURCE in state 3',
            '[D] pw.node [impl-node.c:1246 process_node()] 0xabcdef: scheduling non-active node PRIVATE_SOURCE',
            '[D] pw.node [impl-node.c:1791 node_ready()] (PRIVATE_SOURCE-42) graph not finished: state:0xabcdef quantum:1024 pending 1/1',
        ]
        facts = MODULE.pipewire_trace_facts('\n'.join(lines).encode())
        self.assertTrue(all(facts.values()), facts)
        self.assertTrue(all(type(value) is bool for value in facts.values()))
        rendered = MODULE.json.dumps(facts)
        for private in ('PRIVATE_SOURCE', 'abcdef', '123456', '12345', '1024'):
            self.assertNotIn(private, rendered)
        # Function/topic/location are part of the allowlist. Free text and
        # old fastpath markers cannot masquerade as compiled observations.
        for line in lines:
            self.assertFalse(any(MODULE.pipewire_trace_facts(line.replace('pw.', 'other.').encode()).values()))
            self.assertFalse(any(MODULE.pipewire_trace_facts(('private error says ' + line).encode()).values()))
        fastpath = b'[T] pw.stream [stream.c:1047 impl_node_process_input()] 0x1: process in status:2 id:1 ticks:0 delay:0'
        self.assertFalse(any(MODULE.pipewire_trace_facts(fastpath).values()))

    def test_producer_queue_samples_are_correlated_bounded_and_redacted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            prefix = b'[TRACE] private-prefix morpheus-fixture-producer: '
            data = (prefix + b'buffer-present\n'
                    + prefix + b'queued frame=valid driving=no result=accepted\n'
                    + prefix + b'queued frame=corrupt driving=yes result=rejected\n'
                    + prefix + b'queued frame=PRIVATE driving=yes result=accepted\n')
            (root / 'portal-wlr.log').write_bytes(data + (prefix + b'queued frame=valid driving=yes result=accepted\n') * 50)
            facts = MODULE.private_trace_facts(root, {})
            producer = facts['portal-wlr']['producer']
            self.assertTrue(producer['buffer_present'])
            self.assertEqual(len(producer['queue_samples']), 16)
            self.assertEqual(producer['queue_samples'][:2], [
                {'frame': 'valid', 'driving': False, 'result': 'accepted'},
                {'frame': 'corrupt', 'driving': True, 'result': 'rejected'},
            ])
            rendered = MODULE.json.dumps(facts)
            for private in ('PRIVATE', 'private-prefix', directory):
                self.assertNotIn(private, rendered)
            self.assertTrue(facts['capture-test']['unavailable'])

    def test_capped_log_drains_excess_without_process_or_unbounded_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'private.log'
            read_fd, write_fd = os.pipe()
            try:
                with MODULE.CappedPrivateLog(path, limit=32) as sink:
                    sink.attach(os.fdopen(read_fd, 'rb', buffering=0))
                    os.write(write_fd, b'x' * 2048)
                    os.close(write_fd)
                    write_fd = None
                    sink.finish()
                    self.assertFalse(sink.failed)
                    self.assertTrue(sink.drained)
                    self.assertTrue(sink.truncated)
                    self.assertEqual(path.read_bytes(), b'x' * 32)
                    self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                self.assertFalse(sink.thread.is_alive())
            finally:
                if write_fd is not None:
                    os.close(write_fd)

    def test_capped_log_stops_when_writer_remains_open_and_reports_write_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            read_fd, write_fd = os.pipe()
            try:
                with MODULE.CappedPrivateLog(Path(directory) / 'private.log', limit=32) as sink:
                    sink.attach(os.fdopen(read_fd, 'rb', buffering=0))
                    real_output = sink.output
                    sink.output = mock.Mock()
                    sink.output.write.side_effect = OSError('PRIVATE DISK ERROR')
                    os.write(write_fd, b'x' * 64)
                    deadline = time.monotonic() + 2
                    while not sink.failed and time.monotonic() < deadline:
                        time.sleep(0.005)
                    self.assertTrue(sink.failed)
                    self.assertNotIn('PRIVATE', MODULE.json.dumps(sink.facts()))
                    sink.output = real_output
                self.assertFalse(sink.thread.is_alive())
            finally:
                os.close(write_fd)

    def test_capped_log_completes_partial_writes_without_duplication(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'private.log'
            read_fd, write_fd = os.pipe()
            try:
                with MODULE.CappedPrivateLog(path, limit=32) as sink:
                    real_output = sink.output
                    writer = mock.Mock()
                    writer.write.side_effect = lambda data: real_output.write(data[:3])
                    sink.output = writer
                    sink.attach(os.fdopen(read_fd, 'rb', buffering=0))
                    os.write(write_fd, b'0123456789abcdefghij')
                    os.close(write_fd)
                    write_fd = None
                    sink.finish()
                    self.assertFalse(sink.failed)
                    self.assertFalse(sink.truncated)
                    self.assertEqual(path.read_bytes(), b'0123456789abcdefghij')
                    self.assertEqual(sink.written, 20)
                    self.assertGreater(writer.write.call_count, 1)
                    sink.output = real_output
            finally:
                if write_fd is not None:
                    os.close(write_fd)

    def test_capped_log_rejects_nonprogressing_write_without_spinning(self):
        for result in (0, None):
            with self.subTest(result=result), tempfile.TemporaryDirectory() as directory:
                read_fd, write_fd = os.pipe()
                try:
                    with MODULE.CappedPrivateLog(Path(directory) / 'private.log', limit=32) as sink:
                        real_output = sink.output
                        writer = mock.Mock()
                        writer.write.return_value = result
                        sink.output = writer
                        sink.attach(os.fdopen(read_fd, 'rb', buffering=0))
                        os.write(write_fd, b'bounded output')
                        os.close(write_fd)
                        write_fd = None
                        sink.finish()
                        self.assertTrue(sink.failed)
                        self.assertEqual(writer.write.call_count, 1)
                        sink.output = real_output
                finally:
                    if write_fd is not None:
                        os.close(write_fd)


if __name__ == "__main__":
    unittest.main()
