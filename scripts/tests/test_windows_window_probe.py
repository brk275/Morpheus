"""Mocked contracts for the Windows window probe; no Win32 call or process."""
import json
from pathlib import Path
import re
import tempfile
import unittest
from unittest.mock import Mock, patch

from scripts import run_windows_capture_acceptance as capture
from scripts import windows_package_ui as ui
from scripts import windows_window_probe as P

ROOT = Path(__file__).resolve().parents[2]
EXAMPLE_SOURCE = ROOT / "crates/morpheus-app/examples/windows_window_probe.rs"
DRIVER_SOURCE = ROOT / "scripts/windows_window_probe.py"
HOSTED = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Windows",
          "ImageOS": "win25", P.OPT_IN: "1"}
PID, OTHER_PID = 48611, 52907
MAIN, TARGET, IME, FOREIGN = 0x7A5B3C, 0x6E4D21, 0x5C3B1A, 0x4B2A19


def windows():
    """winit's main window and its visible zero-size thread event target, as
    the pinned winit 0.30.13 source creates them, plus a hidden IME window."""
    return {
        MAIN: {"pid": PID, "class": "Window Class", "style": 0x16CF0000, "exstyle": 0x00040100,
               "owner": None, "rect": (10, 20, 490, 340), "visible": True, "cloaked": False},
        # WS_POPUP | WS_VISIBLE; WS_EX_NOACTIVATE | LAYERED | TOOLWINDOW | TRANSPARENT.
        TARGET: {"pid": PID, "class": "Winit Thread Event Target", "style": 0x90000000,
                 "exstyle": 0x080800A0, "owner": None, "rect": (0, 0, 0, 0), "visible": True,
                 "cloaked": False},
        IME: {"pid": PID, "class": "IME", "style": 0x8C000000, "exstyle": 0, "owner": MAIN,
              "rect": (0, 0, 0, 0), "visible": False, "cloaked": None},
        FOREIGN: {"pid": OTHER_PID, "class": "Window Class", "style": 0x16CF0000, "exstyle": 0,
                  "owner": None, "rect": (0, 0, 9, 9), "visible": True, "cloaked": False},
    }


class FakeApi:
    def __init__(self, table=None, *, failing=(), vanished=()):
        self.table = windows() if table is None else table
        self.failing, self.vanished = set(failing), set(vanished)

    def query(self, window):
        if window in self.failing or window in self.vanished:
            raise P.ApiError()
        return self.table[window]

    def top_level(self, pid):
        return [window for window, value in self.table.items() if value["pid"] == pid]

    def is_window(self, window):
        return window in self.table and window not in self.vanished

    def is_visible(self, window):
        return self.query(window)["visible"]

    def class_name(self, window):
        return self.query(window)["class"]

    def long(self, window, index):
        return self.query(window)["style" if index == P.GWL_STYLE else "exstyle"]

    def owner(self, window):
        return self.query(window)["owner"]

    def process_of(self, window):
        return self.query(window)["pid"]

    def rect(self, window):
        return self.query(window)["rect"]

    def cloaked(self, window):
        return self.query(window)["cloaked"]


class FakeJob:
    def __init__(self, *, alive=True, exit_code=0, wait_error=None, close_error=None, remaining=0):
        self.kernel = Mock()
        self.kernel.GetProcessId.return_value = PID
        self.kernel.WaitForSingleObject.return_value = P.STILL_ACTIVE if alive else 0
        self.exit_code, self.wait_error, self.close_error, self.remaining = (
            exit_code, wait_error, close_error, remaining)
        self.spawned, self.closed = [], False

    def spawn(self, command, environment, log):
        self.spawned.append((command, environment))
        return 991

    def wait(self, handle, seconds):
        if self.wait_error:
            raise self.wait_error
        return self.exit_code

    def active(self):
        return self.remaining

    def close(self):
        self.closed = True
        if self.close_error:
            raise self.close_error


def run(job, api=None):
    report = P.new_report()
    with (patch.object(P, "guard"), patch.object(P, "example_executable", return_value=Path("/x/probe.exe")),
          patch.object(P.time, "sleep")):
        try:
            P.run("/x/probe.exe", report, api_factory=lambda: api or FakeApi(), job_factory=lambda: job)
            report["status"] = "passed"
        except P.ProbeFailure as error:
            report["failure"] = error.code
    return report


def assert_private_values_absent(test, text):
    for value in (PID, OTHER_PID, MAIN, TARGET, IME, FOREIGN):
        test.assertNotIn(str(value), text)
    for value in ("Window Class", "Winit Thread Event Target", "probe.exe", "/x/"):
        test.assertNotIn(value, text)


class InventoryTests(unittest.TestCase):
    def test_snapshot_counts_owned_visible_windows_like_the_keyboard_driver(self):
        result = P.snapshot(FakeApi(), PID)
        self.assertEqual(len(result["visible"]), 2)
        self.assertEqual(result["hidden"], ["ime"])
        self.assertEqual(result["hidden_count"], 1)
        target, window = sorted(result["visible"], key=lambda facts: facts["family"])
        self.assertEqual(target["family"], "winit_event_target")
        self.assertEqual(target["style"], ["popup", "visible"])
        self.assertEqual(target["exstyle"], ["layered", "noactivate", "toolwindow", "transparent"])
        self.assertEqual((target["owner"], target["area"]), ("none", "zero"))
        self.assertEqual(window["family"], "winit_window")
        self.assertEqual(window["area"], "nonzero")
        self.assertIn("caption", window["style"])

    def test_unknown_class_names_and_owners_reduce_to_closed_tokens(self):
        table = windows()
        table[MAIN]["class"] = r"C:\Users\private-person\Secret Window"
        table[TARGET]["owner"] = FOREIGN
        result = P.snapshot(FakeApi(table), PID)
        families = {facts["family"]: facts for facts in result["visible"]}
        self.assertEqual(set(families), {"other", "winit_event_target"})
        self.assertEqual(families["winit_event_target"]["owner"], "other_process")
        self.assertNotIn("private", json.dumps(result))
        self.assertEqual(P.family(None), "other")

    def test_a_vanished_window_is_counted_but_a_live_query_failure_fails_closed(self):
        result = P.snapshot(FakeApi(vanished={IME}), PID)
        self.assertEqual((result["vanished"], result["hidden_count"], len(result["visible"])), (1, 0, 2))
        with self.assertRaises(P.ProbeFailure) as caught:
            P.snapshot(FakeApi(failing={MAIN}), PID)
        self.assertEqual(caught.exception.code, "window_facts")

    def test_window_count_is_bounded(self):
        table = {index: {**windows()[MAIN]} for index in range(1, P.WINDOW_BOUND + 2)}
        with self.assertRaises(P.ProbeFailure) as caught:
            P.snapshot(FakeApi(table), PID)
        self.assertEqual(caught.exception.code, "window_bound")

    def test_summary_reports_counts_kinds_and_the_driver_rule(self):
        two = P.snapshot(FakeApi(), PID)
        one_table = windows()
        one_table[TARGET]["visible"] = False
        one = P.snapshot(FakeApi(one_table), PID)
        summary = P.summarize([two, two, one])
        self.assertEqual((summary["visible_count_min"], summary["visible_count_max"]), (1, 2))
        self.assertEqual(summary["driver_rule"], "not_one")
        self.assertEqual({kind["family"]: kind["snapshots"] for kind in summary["visible_kinds"]},
                         {"winit_window": 3, "winit_event_target": 2})
        self.assertEqual(summary["hidden_families"], ["ime", "winit_event_target"])
        self.assertEqual(P.summarize([one, one])["driver_rule"], "exactly_one")
        self.assertEqual(P.summarize([])["driver_rule"], "not_one")


class GuardTests(unittest.TestCase):
    def test_guard_requires_opt_in_on_a_reviewed_hosted_windows_image(self):
        P.guard(HOSTED, "win32")
        self.assertEqual(P.HOSTED_WINDOWS_IMAGES, ui.HOSTED_WINDOWS_IMAGES)
        for change in ({P.OPT_IN: "0"}, {"ImageOS": "win22"}, {"RUNNER_ENVIRONMENT": "self-hosted"},
                       {"GITHUB_ACTIONS": "false"}, {"RUNNER_OS": "Linux"}):
            with self.subTest(change=change), self.assertRaises(P.ProbeFailure):
                P.guard({**HOSTED, **change}, "win32")
        with self.assertRaises(P.ProbeFailure):
            P.guard(HOSTED, "linux")

    def test_only_the_cargo_example_under_this_target_directory_is_accepted(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            for profile in ("debug", "release", "other"):
                (root / "target" / profile / "examples").mkdir(parents=True)
                (root / "target" / profile / "examples" / P.EXAMPLE).write_bytes(b"MZ")
            (root / "target/debug/examples/morpheus-app.exe").write_bytes(b"MZ")
            for profile in ("debug", "release"):
                path = root / "target" / profile / "examples" / P.EXAMPLE
                self.assertEqual(P.example_executable(path, root), path)
            for bad in (root / "target/other/examples" / P.EXAMPLE, root / "target/debug/examples/morpheus-app.exe",
                        root / "target/debug/examples/absent.exe", Path("target/debug/examples") / P.EXAMPLE):
                with self.subTest(bad=bad), self.assertRaises(P.ProbeFailure):
                    P.example_executable(bad, root)

    def test_child_environment_carries_no_profile_account_or_app_variables(self):
        source = {"SystemRoot": r"C:\Windows", "ProgramFiles": r"C:\Program Files", "PATH": r"C:\tools",
                  "USERPROFILE": r"C:\Users\person", "APPDATA": "roaming", "LOCALAPPDATA": "local",
                  "MORPHEUS_QA_PROFILE_DIR": "profile", "GITHUB_TOKEN": "secret", "SLINT_BACKEND": "skia"}
        child = P.child_environment(r"C:\tmp\probe", source)
        self.assertEqual(child["PATH"], r"C:\Windows\System32;C:\Windows")
        self.assertEqual((child["TEMP"], child["TMP"]), (r"C:\tmp\probe", r"C:\tmp\probe"))
        self.assertEqual(child["ProgramFiles"], r"C:\Program Files")
        for name in ("USERPROFILE", "APPDATA", "LOCALAPPDATA", "MORPHEUS_QA_PROFILE_DIR", "GITHUB_TOKEN",
                     "SLINT_BACKEND"):
            self.assertNotIn(name, child)


class RunTests(unittest.TestCase):
    def test_complete_run_records_fixed_facts_and_lets_the_example_close_itself(self):
        job = FakeJob()
        report = run(job)
        self.assertEqual((report["status"], report["failure"], report["stage"]), ("passed", "none", "complete"))
        self.assertEqual((report["exit"], report["cleanup"]), ("normal", "passed"))
        self.assertEqual((report["snapshots"], report["visible_count_max"]), (P.SNAPSHOTS, 2))
        self.assertEqual(report["driver_rule"], "not_one")
        self.assertTrue(job.closed)
        command, environment = job.spawned[0]
        self.assertEqual(command, [str(Path("/x/probe.exe"))])
        self.assertNotIn("USERPROFILE", environment)
        public = P.public(report)
        self.assertEqual(public, report)
        assert_private_values_absent(self, json.dumps(public))

    def test_failures_keep_fixed_reasons_and_always_close_the_job(self):
        cases = (
            (FakeJob(alive=False), "app_exited"),
            (FakeJob(wait_error=capture.Failure("deadline", operation="process_wait")), "exit_timeout"),
            (FakeJob(wait_error=capture.Failure("process_wait")), "process_wait"),
            (FakeJob(remaining=1), "process_remaining"),
            (FakeJob(close_error=capture.Failure("cleanup")), "cleanup"),
        )
        for job, failure in cases:
            with self.subTest(failure=failure):
                report = run(job)
                self.assertEqual((report["status"], report["failure"]), ("failed", failure))
                self.assertTrue(job.closed)
                self.assertEqual(report["cleanup"], "failed" if failure == "cleanup" else "passed")
                self.assertEqual(P.public(report), report)
        no_window = windows()
        for value in no_window.values():
            value["visible"] = False
        with patch.object(P, "FIRST_WINDOW_SECONDS", 0):
            report = run(FakeJob(), FakeApi(no_window))
        self.assertEqual(report["failure"], "first_window_timeout")

    def test_public_report_withholds_any_unvalidated_value(self):
        report = run(FakeJob())
        for key, value in (("hidden_families", ["Window Class"]), ("failure", "private text"),
                           ("visible_count_max", P.WINDOW_BOUND + 1), ("extra", 1)):
            with self.subTest(key=key):
                self.assertEqual(P.public({**report, key: value}),
                                 {"check": P.CHECK, "status": "failed", "failure": "unclassified",
                                  "diagnostics": "withheld"})
        kind = {**report["visible_kinds"][0], "family": "Winit Thread Event Target"}
        self.assertEqual(P.public({**report, "visible_kinds": [kind]})["diagnostics"], "withheld")

    def test_main_prints_one_fixed_record_for_bad_arguments(self):
        with patch("builtins.print") as printed:
            self.assertEqual(P.main([]), 1)
        record = json.loads(printed.call_args.args[0])
        self.assertEqual((record["failure"], record["stage"]), ("arguments", "setup"))


class SourceContractTests(unittest.TestCase):
    def test_example_is_a_bounded_gui_window_with_no_unsafe_or_project_code(self):
        source = EXAMPLE_SOURCE.read_text()
        self.assertIn('#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]', source)
        for forbidden in ("unsafe", "std::fs", "std::net", "std::env", "morpheus_", "Command::new",
                          "BackendSelector"):
            self.assertNotIn(forbidden, source)
        seconds = {name: int(value) for name, value in
                   re.findall(r"const (LIFETIME|WATCHDOG): Duration = Duration::from_secs\((\d+)\);", source)}
        self.assertEqual(set(seconds), {"LIFETIME", "WATCHDOG"})
        observed = P.FIRST_WINDOW_SECONDS + P.SNAPSHOTS * P.SNAPSHOT_SECONDS
        self.assertGreater(seconds["LIFETIME"], observed + 2)
        self.assertLess(seconds["LIFETIME"], seconds["WATCHDOG"])
        self.assertLess(seconds["WATCHDOG"], P.EXIT_SECONDS)
        self.assertLessEqual(observed + P.EXIT_SECONDS, P.RUN_SECONDS)

    def test_driver_reads_windows_without_input_messages_titles_or_app_code(self):
        source = DRIVER_SOURCE.read_text()
        for forbidden in ("SendInput", "keybd_event", "PostMessage", "SendMessage", "GetWindowText",
                          "SetForegroundWindow", "import subprocess"):
            self.assertNotIn(forbidden, source)
        # Only the reviewed job helper is imported, never the app smoke/driver.
        imports = re.findall(r"^\s*(?:from\s+\S+\s+)?import\s+(.+)$", source, re.MULTILINE)
        self.assertFalse([line for line in imports if re.search(r"package_smoke|windows_package_ui", line)])


if __name__ == "__main__":
    unittest.main()
