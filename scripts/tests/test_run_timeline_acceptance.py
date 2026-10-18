"""Mocked contracts for the real-room timeline acceptance runner.

No app, Matrix server, display, keyring or cargo process is started. Flow tests
use fake reports; process-boundary tests run only Python synthetic output.
"""

import contextlib
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_timeline_acceptance as runner


HOSTED = {
    "GITHUB_ACTIONS": "true",
    "RUNNER_ENVIRONMENT": "github-hosted",
    "RUNNER_OS": "Linux",
    "ImageOS": "ubuntu24",
    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1001/bus",
    "DISPLAY": ":99",
    "PATH": "/usr/bin",
    "HOME": "/home/runner",
}
ROOM_ID = "!privateroom:morpheus-a.test"


def build():
    return {
        "variant": "instrumented_release",
        "shipping_acceptance": False,
        "delta_from_shipping": ["acceptance_driver", "rendering_notifier",
                                "generated_profile_root", "requested_sync_policy"],
    }


def summary(value=10):
    return {"count": 3, "dropped": 0, "p50": value, "p95": value * 2, "max": value * 3}


def enroll_report(adapter):
    return {"check": runner.CHECK, "status": "passed", "mode": "enroll", "adapter": adapter,
            "build": build(), "accounts": 2, "observed_backend": runner.BACKENDS[adapter],
            "first_render_hook_ms": 450}


def measure_report(adapter):
    return {
        "check": runner.CHECK, "status": "passed", "mode": "measure", "adapter": adapter, "build": build(),
        "renderer": {"graphics_api": "native_opengl", "hook": "slint_rendering_notifier",
                     "frames_completed": 900},
        "window": {"physical_width": 1280, "physical_height": 800, "scale_factor_milli": 1000},
        "sync": {"observed_backend": runner.BACKENDS[adapter]},
        "startup_ms": {"window_shown": 120, "first_render_hook": 450, "sync_running": 1500,
                       "restored_observed": 1400, "room_listed_observed": 2100},
        "room_open": {"first_rows_observed_ms": 300, "initial_rows": 20},
        "pagination": {"requests": 210, "request_until_loading_cleared_ms": summary(),
                       "full_history_observed_ms": 60_000, "render_hook_frames": 400},
        "history": {"expected_events": 10_000, "model_rows": 10_000, "fixture_rows": 10_000,
                    "fixture_distinct_decrypted": 10_000, "count_method": "direct_window_model"},
        "scroll": {"steps": 240, "delta_logical_px": 480.0, "steps_accepted": 238,
                   "steps_with_render_hook": 238, "idle_render_hook_frames_before": 0,
                   "dispatch_to_render_hook_us": summary(4_000), "render_hook_span_us": summary(3_000),
                   "render_hook_interval_us": summary(16_000)},
        "account_switch": {"away_observed_ms": 800, "back_observed_ms": 700,
                           "reopen_first_rows_observed_ms": 200, "reopen_initial_rows": 10_000},
        "rss_kib": {"source": "proc_self_status", "after_startup": 150_000, "after_history": 210_000,
                    "after_scroll": 212_000, "after_switch": 220_000, "peak": 230_000},
        "delegates": {"instances": None, "method": "unmeasured"},
        "poll_ms": 5,
        "not_measured": ["gpu_time", "presentation", "display_refresh", "delegate_instances"],
    }


class FakeRunner:
    """Writes what the instrumented app and seeder would write."""

    def __init__(self, failing=None, seed_log=None):
        self.calls = []
        self.environments = []
        self.failing = failing
        self.seed_log = seed_log

    def __call__(self, command, environment, timeout, log):
        self.environments.append(environment)
        if command[0] == "cargo":
            self.calls.append(("seed", None))
            log.write_text(self.seed_log if self.seed_log is not None else
                           f"{runner.SEED_COMPLETE}\ntest result: ok. 2 passed; 0 failed; 0 ignored;\n")
            if self.seed_log is None:
                Path(environment["MORPHEUS_TIMELINE_10K_SEED"]).write_text(
                    json.dumps({"room_id": ROOM_ID, "events": 10_000}))
            return 0
        run = json.loads(Path(environment["MORPHEUS_TIMELINE_ACCEPTANCE"]).read_text())
        step = (run["mode"], run["adapter"])
        self.calls.append(step)
        log.write_text(f"raw app output naming {ROOM_ID} and {run['profile']}\n")
        if self.failing == step:
            report = {"check": runner.CHECK, "status": "failed", "mode": run["mode"],
                      "adapter": run["adapter"], "stage": "history_complete", "failure": "history_incomplete"}
        elif run["mode"] == "enroll":
            Path(run["profile"], "data").mkdir()
            report = enroll_report(run["adapter"])
        else:
            report = measure_report(run["adapter"])
        Path(run["report"]).write_text(json.dumps(report))
        return 0 if self.failing != step else 1


class RunnerTest(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.folder = Path(folder.name)
        self.fixture = self.folder / "accounts.json"
        self.fixture.write_text("{}")
        self.app = self.folder / "morpheus-app"
        self.app.write_text("")
        app = patch.object(runner, "APP", self.app)
        app.start()
        self.addCleanup(app.stop)

    def acceptance(self, fake):
        return runner.Acceptance(self.fixture, self.app, self.folder / "work", fake,
                                 HOSTED | {"GITHUB_TOKEN": "private", "MORPHEUS_TIMELINE_ACCEPTANCE_BUILD": "1"})


class GuardTests(unittest.TestCase):
    def test_only_a_fresh_hosted_ubuntu_runner_with_a_bus_and_display_runs_it(self):
        self.assertIsNone(runner.guard(HOSTED))
        for name, value, reason in (("GITHUB_ACTIONS", None, "not_hosted"),
                                    ("RUNNER_ENVIRONMENT", "self-hosted", "not_hosted"),
                                    ("ImageOS", "ubuntu22", "not_hosted"),
                                    ("RUNNER_OS", "macOS", "not_hosted"),
                                    ("DBUS_SESSION_BUS_ADDRESS", "", "no_session_bus"),
                                    ("DISPLAY", None, "no_display"),
                                    ("APPIMAGE", "/tmp/Morpheus.AppImage", "packaged")):
            environment = dict(HOSTED)
            if value is None:
                environment.pop(name, None)
            else:
                environment[name] = value
            with self.subTest(name=name):
                self.assertEqual(runner.guard(environment), reason)

    def test_children_get_only_allowlisted_variables(self):
        passed = runner.child_environment(HOSTED | {"GITHUB_TOKEN": "private", "AWS_SECRET": "private",
                                                    "CARGO_HOME": "/home/runner/.cargo",
                                                    "CARGO_REGISTRY_TOKEN": "private",
                                                    "CARGO_REGISTRIES_PRIVATE_TOKEN": "private",
                                                    "RUSTUP_AUTH_TOKEN": "private",
                                                    "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER": "/unreviewed",
                                                    "RUSTUP_TOOLCHAIN": "1.99.0"})
        self.assertNotIn("GITHUB_TOKEN", passed)
        self.assertNotIn("AWS_SECRET", passed)
        self.assertEqual(passed["CARGO_HOME"], "/home/runner/.cargo")
        self.assertEqual(passed["DISPLAY"], ":99")
        self.assertEqual(passed["RUSTUP_TOOLCHAIN"], "1.99.0")
        for excluded in ("CARGO_REGISTRY_TOKEN", "CARGO_REGISTRIES_PRIVATE_TOKEN",
                         "RUSTUP_AUTH_TOKEN", "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER"):
            self.assertNotIn(excluded, passed)


class ReportTests(unittest.TestCase):
    def test_closed_reports_for_the_requested_adapter_are_accepted(self):
        for adapter in runner.ADAPTERS:
            self.assertTrue(runner.valid_report(enroll_report(adapter), "enroll", adapter))
            self.assertTrue(runner.valid_report(measure_report(adapter), "measure", adapter))
        self.assertFalse(runner.valid_report(measure_report("sliding"), "measure", "classic"))
        mixed = measure_report("classic")
        mixed["sync"]["observed_backend"] = "sliding_sticky"
        self.assertFalse(runner.valid_report(mixed, "measure", "classic"))

    def test_open_or_misleading_reports_are_refused(self):
        def mutated(change):
            value = measure_report("sliding")
            change(value)
            return runner.valid_report(value, "measure", "sliding")

        for change in (
            lambda value: value.update(room_id=ROOM_ID),
            lambda value: value["history"].update(fixture_distinct_decrypted=9_999),
            lambda value: value["build"].update(shipping_acceptance=True),
            lambda value: value["renderer"].update(graphics_api="gpu_presented"),
            lambda value: value["pagination"].update(requests=True),
            lambda value: value["room_open"].update(initial_rows=-1),
            lambda value: value["scroll"].update(render_hook_span_us={"count": 1, "dropped": 0, "p50": 9,
                                                                       "p95": 2, "max": 9}),
            lambda value: value["scroll"].update(presented_fps=60),
            lambda value: value["delegates"].update(instances=6),
            lambda value: value["startup_ms"].pop("sync_running"),
            lambda value: value.update(status="failed"),
        ):
            with self.subTest(change=change):
                self.assertFalse(mutated(change))


class FlowTests(RunnerTest):
    def test_enrolls_both_profiles_then_seeds_once_then_measures_both(self):
        fake = FakeRunner()
        result = self.acceptance(fake).run()
        self.assertEqual(fake.calls, [("enroll", "sliding"), ("enroll", "classic"), ("seed", None),
                                      ("measure", "sliding"), ("measure", "classic")])
        self.assertEqual(result["status"], "passed")
        self.assertEqual(set(result["runs"]), set(runner.ADAPTERS))
        for environment in fake.environments:
            self.assertNotIn("GITHUB_TOKEN", environment)
            self.assertNotIn("MORPHEUS_TIMELINE_ACCEPTANCE_BUILD", environment)
        seed = fake.environments[2]
        self.assertEqual(seed["MORPHEUS_TIMELINE_10K_BOB_DEVICES"], str(runner.BOB_DEVICES))
        work = self.folder / "work"
        for path in work.glob("run-*.json"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        for path in work.glob("profile-*"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)
        printed = json.dumps(result)
        for private in (ROOM_ID, str(work), str(self.fixture)):
            self.assertNotIn(private, printed)

    def test_a_failed_step_stops_the_run_with_closed_tokens_only(self):
        fake = FakeRunner(failing=("measure", "sliding"))
        with self.assertRaises(runner.StepFailure) as raised:
            self.acceptance(fake).run()
        self.assertEqual(fake.calls[-1], ("measure", "sliding"))
        self.assertEqual(raised.exception.summary(), {
            "check": runner.CHECK, "status": "failed", "step": "measure", "adapter": "sliding",
            "stage": "history_complete", "failure": "history_incomplete"})

    def test_seed_failures_report_only_their_fixed_stage(self):
        for log, stage, reason in (
            ("thread panicked at tests/x.rs:\nMORPHEUS_TIMELINE_SEED_FAILURE devices\n", "devices", "seed_failed"),
            ("test result: ok. 2 passed; 0 failed;\n", None, "seed_incomplete"),
        ):
            with self.subTest(reason=reason):
                fake = FakeRunner(seed_log=log)
                work = self.folder / f"work-{reason}"
                acceptance = runner.Acceptance(self.fixture, self.app, work, fake, HOSTED)
                with self.assertRaises(runner.StepFailure) as raised:
                    acceptance.run()
                self.assertEqual((raised.exception.step, raised.exception.stage, raised.exception.reason),
                                 ("seed", stage, reason))
                self.assertNotIn(("measure", "sliding"), fake.calls)

    def test_untrusted_failure_text_never_becomes_a_token(self):
        failure = runner.StepFailure("measure", "sliding", "Permission denied: /home/runner/x", "a b")
        self.assertEqual((failure.reason, failure.stage), ("invalid", None))

    def test_setup_refuses_other_binaries_and_reused_work(self):
        other = self.folder / "other-app"
        other.write_text("")
        with self.assertRaises(runner.StepFailure):
            runner.Acceptance(self.fixture, other, self.folder / "work-a", FakeRunner(), HOSTED)
        work = self.folder / "work-b"
        work.mkdir()
        (work / "leftover").write_text("x")
        with self.assertRaises(runner.StepFailure):
            runner.Acceptance(self.fixture, self.app, work, FakeRunner(), HOSTED)

    def test_main_prints_only_the_closed_summary(self):
        fake = FakeRunner()
        output = io.StringIO()
        with patch.dict(os.environ, HOSTED, clear=True), patch.object(runner, "run_process", fake), \
                contextlib.redirect_stdout(output):
            code = runner.main(["--fixture", str(self.fixture), "--app", str(self.app),
                                "--work", str(self.folder / "work-main")])
        self.assertEqual(code, 0)
        text = output.getvalue()
        self.assertTrue(text.startswith("MORPHEUS_TIMELINE_10K_SUMMARY "))
        for private in (ROOM_ID, str(self.folder), "raw app output"):
            self.assertNotIn(private, text)
        summary_value = json.loads(text.split(" ", 1)[1])
        self.assertEqual(summary_value["runs"]["classic"]["sync"]["observed_backend"], "classic")

    def test_main_refuses_outside_a_hosted_runner(self):
        output = io.StringIO()
        with patch.dict(os.environ, {"PATH": "/usr/bin"}, clear=True), contextlib.redirect_stdout(output):
            code = runner.main(["--fixture", str(self.fixture)])
        self.assertEqual(code, 2)
        self.assertEqual(json.loads(output.getvalue())["failure"], "not_hosted")


class ProcessBoundaryTests(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.folder = Path(folder.name)

    def run_synthetic(self, code, timeout=5):
        return runner.run_process([sys.executable, "-I", "-B", "-c", code],
                                  {"PATH": "/usr/bin"}, timeout, self.folder / "output.log")

    def test_output_is_private_bounded_and_never_printed(self):
        output = io.StringIO()
        with patch.object(runner, "MAX_LOG_BYTES", 1024), contextlib.redirect_stdout(output):
            result = self.run_synthetic("import os; os.write(1, b'synthetic-only' * 4096)")
        self.assertEqual(result, runner.OUTPUT_LIMIT)
        self.assertEqual(output.getvalue(), "")
        log = self.folder / "output.log"
        self.assertLessEqual(log.stat().st_size, 1024)
        self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)

    def test_normal_exit_captures_both_streams(self):
        result = self.run_synthetic("import os; os.write(1,b'first'); os.write(2,b'second')")
        self.assertEqual(result, 0)
        self.assertEqual((self.folder / "output.log").read_bytes(), b"firstsecond")

    def test_quiet_timeout_stops_and_reaps_the_child(self):
        self.assertIsNone(self.run_synthetic("import time; time.sleep(60)", timeout=.1))

    def test_reports_reject_links_special_files_and_oversize(self):
        report = self.folder / "report.json"
        report.write_text('{"synthetic":true}')
        self.assertEqual(runner.load_json(report), {"synthetic": True})
        alias = self.folder / "alias.json"
        alias.symlink_to(report)
        self.assertIsNone(runner.load_json(alias))
        report.write_bytes(b"x" * (64 * 1024 + 1))
        self.assertIsNone(runner.load_json(report))
        pipe = self.folder / "pipe"
        os.mkfifo(pipe)
        self.assertIsNone(runner.load_json(pipe))


if __name__ == "__main__":
    unittest.main()
