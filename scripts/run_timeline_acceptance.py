#!/usr/bin/env python3
"""Run the real-room 10,000-event timeline acceptance on a fresh hosted Linux runner.

Use inside dbus-run-session with a throwaway unlocked gnome-keyring and an X
display (for example xvfb-run), after `scripts/local_matrix_env.py up` and an
instrumented release build of the shipping configuration:

    MORPHEUS_TIMELINE_ACCEPTANCE_BUILD=1 cargo build --release --locked \\
        -p morpheus-app --features native-calls

It enrolls one generated profile per sync adapter, seeds one encrypted room
with 10,000 variable-height events through the pinned SDK, then measures each
profile with the instrumented app. Profiles, run files, reports and raw logs
stay in a private directory outside the checkout and are never printed or
uploaded. Only a validated summary of fixed tokens and numbers is printed.
The result is instrumented-build evidence, never installed-package or
shipping acceptance. See tests/TIMELINE_10K.md.
"""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile

from fixture_paths import external_path, private_directory, write_private


REPO = Path(__file__).resolve().parent.parent
APP = REPO / "target" / "release" / "morpheus-app"
CHECK = "morpheus_timeline_10k"
ADAPTERS = ("sliding", "classic")
BACKENDS = {"sliding": "sliding_sticky", "classic": "classic"}
EVENTS = 10_000
# Each enrolled profile signs Bob in once; the seeder adds one more device.
BOB_DEVICES = len(ADAPTERS) + 1
ENROLL_SECONDS = 15 * 60
SEED_SECONDS = 60 * 60
MEASURE_SECONDS = 50 * 60
HOSTED = {
    "GITHUB_ACTIONS": "true",
    "RUNNER_ENVIRONMENT": "github-hosted",
    "RUNNER_OS": "Linux",
    "ImageOS": "ubuntu24",
}
# Only these reach the app and the seeder; no other runner variable does.
PASSED_ENVIRONMENT = (
    "PATH", "HOME", "LANG", "DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS", *HOSTED,
)
PASSED_PREFIXES = ("CARGO_", "RUSTUP_")
TOKEN = re.compile(r"[a-z_]{1,48}")
SEED_FAILURE = re.compile(r"MORPHEUS_TIMELINE_SEED_FAILURE ([a-z_]{1,48})$", re.MULTILINE)
SEED_COMPLETE = f"MORPHEUS_TIMELINE_SEED_COMPLETE events={EVENTS}"
SEED_RESULT = re.compile(r"^test result: ok\. 2 passed; 0 failed;", re.MULTILINE)

BUILD = {
    "variant": ("const", "instrumented_release"),
    "shipping_acceptance": ("const", False),
    "delta_from_shipping": (
        "const",
        ["acceptance_driver", "rendering_notifier", "generated_profile_root", "requested_sync_policy"],
    ),
}
COMMON = {
    "check": ("const", CHECK),
    "status": ("const", "passed"),
    "adapter": ("token", ADAPTERS),
    "build": BUILD,
}
ENROLL = COMMON | {
    "mode": ("const", "enroll"),
    "accounts": ("const", 2),
    "observed_backend": ("token", tuple(BACKENDS.values())),
    "first_render_hook_ms": "int",
}
MEASURE = COMMON | {
    "mode": ("const", "measure"),
    "renderer": {
        "graphics_api": ("token", ("native_opengl", "other")),
        "hook": ("const", "slint_rendering_notifier"),
        "frames_completed": "int",
    },
    "window": {"physical_width": "int", "physical_height": "int", "scale_factor_milli": "int"},
    "sync": {"observed_backend": ("token", tuple(BACKENDS.values()))},
    "startup_ms": {
        "window_shown": "int?",
        "first_render_hook": "int",
        "sync_running": "int",
        "restored_observed": "int",
        "room_listed_observed": "int",
    },
    "room_open": {"first_rows_observed_ms": "int", "initial_rows": "int"},
    "pagination": {
        "requests": "int",
        "request_until_loading_cleared_ms": "summary",
        "full_history_observed_ms": "int",
        "render_hook_frames": "int",
    },
    "history": {
        "expected_events": ("const", EVENTS),
        "model_rows": "int",
        "fixture_rows": "int",
        "fixture_distinct_decrypted": ("const", EVENTS),
        "count_method": ("const", "direct_window_model"),
    },
    "scroll": {
        "steps": "int",
        "delta_logical_px": "number",
        "steps_accepted": "int",
        "steps_with_render_hook": "int",
        "idle_render_hook_frames_before": "int",
        "dispatch_to_render_hook_us": "summary",
        "render_hook_span_us": "summary",
        "render_hook_interval_us": "summary",
    },
    "account_switch": {
        "away_observed_ms": "int",
        "back_observed_ms": "int",
        "reopen_first_rows_observed_ms": "int",
        "reopen_initial_rows": "int",
    },
    "rss_kib": {
        "source": ("token", ("proc_self_status", "unavailable")),
        "after_startup": "int?",
        "after_history": "int?",
        "after_scroll": "int?",
        "after_switch": "int?",
        "peak": "int?",
    },
    "delegates": {"instances": ("const", None), "method": ("const", "unmeasured")},
    "poll_ms": "int",
    "not_measured": ("const", ["gpu_time", "presentation", "display_refresh", "delegate_instances"]),
}
SUMMARY_KEYS = {"count", "dropped", "p50", "p95", "max"}


class StepFailure(RuntimeError):
    """A failed step with closed tokens only."""

    def __init__(self, step: str, adapter: str | None, reason: str, stage: str | None = None):
        super().__init__(reason)
        self.step = step
        self.adapter = adapter
        self.reason = token(reason) or "invalid"
        self.stage = token(stage)

    def summary(self) -> dict:
        return {"check": CHECK, "status": "failed", "step": self.step, "adapter": self.adapter,
                "stage": self.stage, "failure": self.reason}


def token(value) -> str | None:
    return value if isinstance(value, str) and TOKEN.fullmatch(value) else None


def guard(environment) -> str | None:
    """Only a fresh standard hosted Ubuntu runner, inside its own session bus and display."""
    if any(environment.get(name) != value for name, value in HOSTED.items()):
        return "not_hosted"
    if environment.get("APPIMAGE"):
        return "packaged"
    if not environment.get("DBUS_SESSION_BUS_ADDRESS"):
        return "no_session_bus"
    if not environment.get("DISPLAY"):
        return "no_display"
    return None


def child_environment(environment) -> dict[str, str]:
    passed = {name: environment[name] for name in PASSED_ENVIRONMENT if environment.get(name)}
    passed.update({name: value for name, value in environment.items()
                   if name.startswith(PASSED_PREFIXES) and value})
    return passed


def is_int(value) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def closed(value, schema) -> bool:
    """Exact keys, fixed tokens and non-negative numbers; nothing else."""
    if isinstance(schema, dict):
        return (isinstance(value, dict) and set(value) == set(schema)
                and all(closed(value[key], schema[key]) for key in schema))
    if schema == "int":
        return is_int(value)
    if schema == "int?":
        return value is None or is_int(value)
    if schema == "number":
        return (is_int(value) or (isinstance(value, float) and math.isfinite(value) and value >= 0))
    if schema == "summary":
        return value is None or (isinstance(value, dict) and set(value) == SUMMARY_KEYS
                                 and all(is_int(item) for item in value.values())
                                 and value["p50"] <= value["p95"] <= value["max"])
    kind, expected = schema
    if kind == "const":
        return type(value) is type(expected) and value == expected
    if kind == "token":
        return isinstance(value, str) and value in expected
    return False


def valid_report(report, mode: str, adapter: str) -> bool:
    schema = ENROLL if mode == "enroll" else MEASURE
    if not closed(report, schema) or report["adapter"] != adapter:
        return False
    backend = report["observed_backend"] if mode == "enroll" else report["sync"]["observed_backend"]
    return backend == BACKENDS[adapter]


def run_process(command: list[str], environment: dict[str, str], timeout: int, log: Path) -> int | None:
    """Raw output goes only to a new owner-only log in the private directory."""
    descriptor = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "wb") as output:
        process = subprocess.Popen(command, cwd=REPO, env=environment, stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            return process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            return None


def load_json(path: Path):
    try:
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 64 * 1024:
            return None
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, ValueError):
        return None


class Acceptance:
    def __init__(self, fixture: Path, app: Path, work: Path, runner=None, environment=None):
        self.fixture = external_path(fixture)
        if not self.fixture.is_file():
            raise StepFailure("setup", None, "fixture_missing")
        if app.resolve() != APP.resolve() or not app.is_file():
            raise StepFailure("setup", None, "app_not_release_build")
        self.app = APP.resolve()
        self.work = private_directory(work)
        if any(self.work.iterdir()):
            raise StepFailure("setup", None, "work_not_fresh")
        self.runner = runner or run_process
        self.environment = child_environment(os.environ if environment is None else environment)
        self.environment.pop("MORPHEUS_TIMELINE_ACCEPTANCE_BUILD", None)
        self.room = self.work / "room.json"

    def run(self) -> dict:
        for adapter in ADAPTERS:
            self.app_run("enroll", adapter)
        self.seed()
        runs = {adapter: self.app_run("measure", adapter) for adapter in ADAPTERS}
        return {"check": CHECK, "status": "passed", "seed": {"events": EVENTS}, "runs": runs}

    def app_run(self, mode: str, adapter: str) -> dict:
        profile = self.work / f"profile-{adapter}"
        if mode == "enroll":
            profile.mkdir(mode=0o700)
            os.chmod(profile, 0o700)
        report = self.work / f"report-{mode}-{adapter}.json"
        run_file = self.work / f"run-{mode}-{adapter}.json"
        content = {"mode": mode, "adapter": adapter, "profile": str(profile),
                   "fixture": str(self.fixture), "report": str(report)}
        if mode == "measure":
            content["room"] = str(self.room)
        write_private(run_file, json.dumps(content), create=True)
        environment = self.environment | {"MORPHEUS_TIMELINE_ACCEPTANCE": str(run_file)}
        timeout = ENROLL_SECONDS if mode == "enroll" else MEASURE_SECONDS
        code = self.runner([str(self.app)], environment, timeout, self.work / f"log-{mode}-{adapter}.txt")
        value = load_json(report)
        if not isinstance(value, dict):
            raise StepFailure(mode, adapter, "timeout" if code is None else "no_report")
        if value.get("status") != "passed":
            raise StepFailure(mode, adapter, value.get("failure"), value.get("stage"))
        if code != 0:
            raise StepFailure(mode, adapter, "exit_status")
        if not valid_report(value, mode, adapter):
            raise StepFailure(mode, adapter, "report_invalid")
        return value

    def seed(self) -> None:
        environment = self.environment | {
            "MORPHEUS_MATRIX_TEST_ACCOUNTS": str(self.fixture),
            "MORPHEUS_TIMELINE_10K_SEED": str(self.room),
            "MORPHEUS_TIMELINE_10K_BOB_DEVICES": str(BOB_DEVICES),
        }
        log = self.work / "log-seed.txt"
        command = ["cargo", "test", "--locked", "-p", "morpheus-matrix", "--test", "timeline_10k_local",
                   "--", "--nocapture", "--test-threads=1"]
        code = self.runner(command, environment, SEED_SECONDS, log)
        try:
            output = log.read_text(encoding="utf-8", errors="replace")
        except OSError:
            output = ""
        failure = SEED_FAILURE.search(output)
        if code is None:
            raise StepFailure("seed", None, "timeout")
        if failure:
            raise StepFailure("seed", None, "seed_failed", failure.group(1))
        if code != 0 or SEED_COMPLETE not in output or not SEED_RESULT.search(output):
            raise StepFailure("seed", None, "seed_incomplete")
        room = load_json(self.room)
        if (not isinstance(room, dict) or set(room) != {"room_id", "events"} or room["events"] != EVENTS
                or not isinstance(room["room_id"], str) or not room["room_id"].startswith("!")):
            raise StepFailure("seed", None, "room_file_invalid")


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fixture", type=Path, required=True,
                        help="accounts.json printed by scripts/local_matrix_env.py up")
    parser.add_argument("--app", type=Path, default=APP)
    parser.add_argument("--work", type=Path,
                        help="new private directory outside the checkout (default: under RUNNER_TEMP)")
    args = parser.parse_args(argv)
    refusal = guard(os.environ)
    if refusal:
        print(json.dumps({"check": CHECK, "status": "refused", "failure": refusal}, sort_keys=True))
        return 2
    work = args.work or Path(tempfile.mkdtemp(prefix="morpheus-timeline-10k-",
                                              dir=os.environ.get("RUNNER_TEMP")))
    try:
        summary = Acceptance(args.fixture, args.app, work).run()
    except StepFailure as failure:
        summary = failure.summary()
    except (OSError, ValueError):
        summary = StepFailure("setup", None, "setup_error").summary()
    print("MORPHEUS_TIMELINE_10K_SUMMARY " + json.dumps(summary, sort_keys=True), flush=True)
    return 0 if summary["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
