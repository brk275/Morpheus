"""Exercise the aggregate's actual validator without executing workflow shell."""

import ast
import itertools
import json
from pathlib import Path
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml"
PLATFORMS = ("macos-15", "macos-15-intel", "ubuntu-24.04", "windows-2025")


def load_validator():
    source = WORKFLOW.read_text(encoding="utf-8")
    start = "          python3 - <<'PY_ACCEPTANCE'\n"
    end = "          PY_ACCEPTANCE\n"
    if source.count(start) != 1 or source.count(end) != 1:
        raise AssertionError("Expected one bounded acceptance heredoc")
    python_source = textwrap.dedent(source.split(start, 1)[1].split(end, 1)[0])
    module = ast.parse(python_source)
    functions = [node for node in module.body
                 if isinstance(node, ast.FunctionDef) and node.name == "acceptance_error"]
    if len(functions) != 1:
        raise AssertionError("Expected one acceptance validator")
    function = functions[0]
    if function.decorator_list or function.args.defaults or function.args.kw_defaults:
        raise AssertionError("Validator must have no import-time execution")
    # Only the pure validator is compiled; environment handling, imports and
    # shell from the workflow are not executed by this regression test.
    namespace = {"json": json}
    exec(compile(ast.Module(body=[function], type_ignores=[]), str(WORKFLOW), "exec"), namespace)
    return namespace["acceptance_error"]


_acceptance_validator = load_validator()


def acceptance_error(needs, event_name="pull_request"):
    return _acceptance_validator(needs, event_name)


def snapshot(native=False, config=False, platforms=None, wayland=False):
    return {
        "scope": {"result": "success", "outputs": {
            "native": str(native).lower(),
            "config": str(config).lower(),
            "wayland": str(wayland).lower(),
            "manual": "false", "manual_platform": "all", "check_route": "all", "matrix_suite": "full",
            "native_matrix": json.dumps({"os": list(platforms if platforms is not None
                                                    else PLATFORMS if native else ())}),
        }},
        "helpers": {"result": "success"},
        "config": {"result": "success" if config else "skipped"},
        "compile": {"result": "success" if native else "skipped"},
    }


class AcceptanceTests(unittest.TestCase):
    def test_docs_config_and_native_result_truth_table(self):
        results = ("success", "failure", "cancelled", "skipped", "neutral", None)
        for native, config in ((False, False), (False, True), (True, False)):
            for conclusions in itertools.product(results, repeat=4):
                value = snapshot(native, config)
                for job, conclusion in zip(("scope", "helpers", "config", "compile"), conclusions):
                    value[job]["result"] = conclusion
                expected = conclusions == (
                    "success", "success",
                    "success" if config else "skipped",
                    "success" if native else "skipped",
                )
                with self.subTest(native=native, config=config, conclusions=conclusions):
                    self.assertEqual(acceptance_error(value) is None, expected)

    def test_every_supported_nonempty_platform_subset(self):
        for count in range(1, len(PLATFORMS) + 1):
            for platforms in itertools.combinations(PLATFORMS, count):
                with self.subTest(platforms=platforms):
                    self.assertIsNone(acceptance_error(snapshot(True, platforms=platforms)))

    def test_missing_or_malformed_scope_cannot_skip_validation(self):
        invalid = []
        for key in ("native", "config", "wayland", "native_matrix"):
            value = snapshot()
            del value["scope"]["outputs"][key]
            invalid.append(value)
        for flag in ("native", "config", "wayland"):
            for bad in (True, False, "True", "", None, [], {}, 1):
                value = snapshot()
                value["scope"]["outputs"][flag] = bad
                invalid.append(value)
        for bad in ("", None, "{", "[]", "null", "true", "{}",
                    '{"os": [], "include": []}', '{"os": "ubuntu-24.04"}',
                    '{"os": [null]}', '{"os": [["ubuntu-24.04"]]}',
                    '{"os": ["self-hosted"]}',
                    '{"os": ["ubuntu-24.04", "ubuntu-24.04"]}'):
            value = snapshot(True)
            value["scope"]["outputs"]["native_matrix"] = bad
            invalid.append(value)
        invalid.extend((snapshot(True, True), snapshot(True, platforms=[]),
                        snapshot(False, platforms=["ubuntu-24.04"])))
        for job in ("scope", "helpers", "config", "compile"):
            value = snapshot()
            del value[job]
            invalid.append(value)
            value = snapshot()
            value[job] = None
            invalid.append(value)
        invalid.extend((None, [], {}, {"scope": {}}))
        for value in invalid:
            with self.subTest(value=value):
                self.assertIsNotNone(acceptance_error(value))

    def test_manual_event_must_match_closed_selected_scope_and_automatic_cannot_be_narrowed(self):
        platforms = {"all": list(PLATFORMS), "linux": ["ubuntu-24.04"], "windows": ["windows-2025"],
                     "macos-arm": ["macos-15"], "macos-intel": ["macos-15-intel"]}
        for platform, native in platforms.items():
            value = snapshot(True, platforms=native, wayland="ubuntu-24.04" in native)
            outputs = value["scope"]["outputs"]
            outputs.update(manual="true", manual_platform=platform)
            self.assertIsNone(acceptance_error(value,"workflow_dispatch"))
            self.assertIsNotNone(acceptance_error(value,"pull_request"))
            self.assertIsNotNone(acceptance_error(value,"push"))
        for platform, route in (("all","helpers"), ("linux","config"), ("linux","matrix"), ("linux","wayland"),
                                ("windows","devices"), ("macos-arm","vendor"), ("macos-intel","sdk"),
                                ("all","desktop-video"), ("linux","desktop-video"),
                                ("windows","desktop-video"), ("macos-arm","desktop-video"),
                                ("macos-intel","desktop-video")):
            native = [] if route in {"helpers","config"} else platforms[platform]
            value = snapshot(bool(native),config=route=="config",platforms=native,wayland=route=="wayland")
            value["scope"]["outputs"].update(manual="true",manual_platform=platform,check_route=route)
            self.assertIsNone(acceptance_error(value,"workflow_dispatch"))
            if native:
                value["compile"]["result"]="skipped"
                self.assertIsNotNone(acceptance_error(value,"workflow_dispatch"))
        for key, bad in (("manual", True), ("manual_platform", "self-hosted"), ("check_route", "skip"),
                         ("matrix_suite", "unknown"), ("check_route", "helpers"), ("manual_platform", "linux")):
            value=snapshot(True)
            value["scope"]["outputs"][key]=bad
            with self.subTest(key=key,bad=bad):
                self.assertIsNotNone(acceptance_error(value))
        self.assertIsNotNone(acceptance_error(snapshot(True),"workflow_dispatch"))
        self.assertIsNotNone(acceptance_error(snapshot(True),"workflow_run"))


    def test_automatic_desktop_and_producer_routes_require_exact_jobs_and_full_app_matrix(self):
        for event in ("push", "pull_request"):
            for route in ("desktop", "helpers"):
                value = snapshot(route == "desktop")
                value["scope"]["outputs"]["check_route"] = route
                self.assertIsNone(acceptance_error(value, event))
                for job in ("scope", "helpers", "config", "compile"):
                    expected = value[job]["result"]
                    for bad in ("failure", "cancelled", "neutral", None, "skipped" if expected == "success" else "success"):
                        changed = json.loads(json.dumps(value)); changed[job]["result"] = bad
                        self.assertIsNotNone(acceptance_error(changed, event))
                for key, bad in (("manual", "true"), ("manual_platform", "linux"), ("matrix_suite", "oauth-only"),
                                 ("check_route", "models"), ("check_route", "devices"), ("wayland", "true"), ("config", "true")):
                    changed = json.loads(json.dumps(value)); changed["scope"]["outputs"][key] = bad
                    self.assertIsNotNone(acceptance_error(changed, event))
            for count in range(len(PLATFORMS)):
                for subset in itertools.combinations(PLATFORMS, count):
                    value = snapshot(bool(subset), platforms=subset)
                    value["scope"]["outputs"]["check_route"] = "desktop"
                    self.assertIsNotNone(acceptance_error(value, event))
            value = snapshot(True)
            value["scope"]["outputs"]["check_route"] = "helpers"
            self.assertIsNotNone(acceptance_error(value, event))
        source = WORKFLOW.read_text(encoding="utf-8")
        self.assertEqual(source.count("github.event_name != 'workflow_dispatch' && needs.scope.outputs.check_route != 'all'"), 2)

    def test_oauth_only_manual_result_requires_exact_linux_matrix_scope_and_passed_job(self):
        value = snapshot(True, platforms=["ubuntu-24.04"], wayland=False)
        value["scope"]["outputs"].update(manual="true", manual_platform="linux", check_route="matrix", matrix_suite="oauth-only")
        self.assertIsNone(acceptance_error(value, "workflow_dispatch"))
        self.assertIsNotNone(acceptance_error(value, "pull_request"))
        self.assertIsNotNone(acceptance_error(value, "push"))
        for key, bad in (("manual_platform", "all"), ("check_route", "all"),
                         ("matrix_suite", "oauth-only-extra"), ("wayland", "true")):
            changed = json.loads(json.dumps(value))
            changed["scope"]["outputs"][key] = bad
            self.assertIsNotNone(acceptance_error(changed, "workflow_dispatch"))
        for outcome in ("skipped", "failure", "cancelled", "neutral", None):
            changed = json.loads(json.dumps(value))
            changed["compile"]["result"] = outcome
            self.assertIsNotNone(acceptance_error(changed, "workflow_dispatch"))

    def test_element_interop_is_partial_and_requires_the_selected_linux_result(self):
        value = snapshot(True, platforms=["ubuntu-24.04"], wayland=False)
        value["scope"]["outputs"].update(manual="true", manual_platform="linux",
                                         check_route="matrix", matrix_suite="element-interop")
        self.assertIsNone(acceptance_error(value, "workflow_dispatch"))
        self.assertIsNotNone(acceptance_error(value, "pull_request"))
        self.assertIsNotNone(acceptance_error(value, "push"))
        for key, bad in (("manual_platform", "all"), ("check_route", "all"),
                         ("matrix_suite", "element-interop-extra"), ("wayland", "true")):
            changed = json.loads(json.dumps(value))
            changed["scope"]["outputs"][key] = bad
            self.assertIsNotNone(acceptance_error(changed, "workflow_dispatch"))
        for outcome in ("skipped", "failure", "cancelled", "neutral", None):
            changed = json.loads(json.dumps(value))
            changed["compile"]["result"] = outcome
            self.assertIsNotNone(acceptance_error(changed, "workflow_dispatch"))

    def test_selected_wayland_requires_linux_native_job(self):
        self.assertIsNone(acceptance_error(snapshot(True, wayland=True)))
        self.assertIsNone(acceptance_error(snapshot(True, platforms=["ubuntu-24.04"], wayland=True)))
        for value in (snapshot(wayland=True), snapshot(config=True, wayland=True),
                      snapshot(True, platforms=["macos-15", "windows-2025"], wayland=True)):
            self.assertIsNotNone(acceptance_error(value))

    def test_outage_result_requires_exact_owner_selected_linux_matrix_and_passed_job(self):
        value = snapshot(True, platforms=["ubuntu-24.04"], wayland=False)
        value["scope"]["outputs"].update(manual="true", manual_platform="linux",
                                          check_route="matrix", matrix_suite="rtc-recovery")
        self.assertIsNone(acceptance_error(value, "workflow_dispatch"))
        self.assertIsNotNone(acceptance_error(value, "pull_request"))
        self.assertIsNotNone(acceptance_error(value, "push"))
        for key, bad in (("manual_platform", "all"), ("check_route", "all"),
                         ("matrix_suite", "rtc-recovery-extra"), ("wayland", "true")):
            changed = json.loads(json.dumps(value))
            changed["scope"]["outputs"][key] = bad
            self.assertIsNotNone(acceptance_error(changed, "workflow_dispatch"))
        for outcome in ("skipped", "failure", "cancelled", "neutral", None):
            changed = json.loads(json.dumps(value))
            changed["compile"]["result"] = outcome
            self.assertIsNotNone(acceptance_error(changed, "workflow_dispatch"))


def load_wayland_validator():
    source = WORKFLOW.read_text(encoding="utf-8")
    start = "          python3 - <<'PY_WAYLAND_ACCEPTANCE'\n"
    end = "          PY_WAYLAND_ACCEPTANCE\n"
    if source.count(start) != 1 or source.count(end) != 1:
        raise AssertionError("Expected one Wayland acceptance heredoc")
    module = ast.parse(textwrap.dedent(source.split(start, 1)[1].split(end, 1)[0]))
    functions = [node for node in module.body if isinstance(node, ast.FunctionDef) and node.name == "wayland_error"]
    if len(functions) != 1:
        raise AssertionError("Expected one pure Wayland validator")
    function = functions[0]
    if function.decorator_list or function.args.defaults or function.args.kw_defaults:
        raise AssertionError("Wayland validator must have no import-time execution")
    namespace = {}
    exec(compile(ast.Module(body=[function], type_ignores=[]), str(WORKFLOW), "exec"), namespace)
    return namespace["wayland_error"]


class WaylandWorkflowTests(unittest.TestCase):
    def test_selected_fixture_must_execute_and_pass_unselected_must_skip(self):
        validate = load_wayland_validator()
        for selected in ("true", "false", "", None, True, [], "True"):
            for outcome in ("success", "failure", "cancelled", "skipped", "neutral", None, ""):
                expected = (selected, outcome) in (("true", "success"), ("false", "skipped"))
                self.assertEqual(validate(selected, outcome) is None, expected)

    def test_wayland_step_and_gate_are_wired_without_relaxing_linux_live_checks(self):
        source = WORKFLOW.read_text(encoding="utf-8")
        step = source.split("      - name: Test real Wayland portal streaming, source loss, and cancellation\n", 1)[1].split("      - name:", 1)[0]
        self.assertIn("if: runner.os == 'Linux' && needs.scope.outputs.wayland != 'false'", step)
        self.assertIn("run: python3 scripts/run_wayland_acceptance.py", step)
        gate = source.split("      - name: Require the selected Wayland result\n", 1)[1].split("      - name:", 1)[0]
        self.assertIn("if: ${{ !cancelled() && runner.os == 'Linux' }}", gate)
        self.assertIn("WAYLAND_SELECTED: ${{ needs.scope.outputs.wayland }}", gate)
        self.assertIn("WAYLAND_OUTCOME: ${{ steps.wayland-tests.outcome }}", gate)
        live = source.split("      - name: Test real Matrix accounts, recovery, moderation, and encrypted RTC\n", 1)[1].split("      - name:", 1)[0]
        self.assertIn("if: runner.os == 'Linux'", live)
        self.assertNotIn("wayland", live)
        self.assertIn("wayland: ${{ steps.scope.outputs.wayland }}", source)
        self.assertIn("ref: ${{ github.event_name == 'pull_request' && github.event.pull_request.base.sha || github.sha }}", source)

    def test_pr_preview_uses_same_bounded_allowlist_without_changing_trust_guards(self):
        source = WORKFLOW.read_text(encoding="utf-8")
        route = "runner.os == 'Linux' && github.repository == 'brk275/Morpheus' && (github.event_name == 'pull_request' || (github.ref == 'refs/heads/main' && github.event_name == 'push'))"
        capture = source.split("      - name: Capture actual native preview screens\n", 1)[1].split("      - name:", 1)[0]
        upload = source.split("      - name: Upload native preview PNGs and provenance only\n", 1)[1].split("      - name:", 1)[0]
        self.assertIn("if: " + route + "\n", capture)
        self.assertIn("if: " + route + " && steps.preview-capture.outcome == 'success'\n", upload)
        self.assertIn("--morpheus-capture-preview", capture)
        self.assertIn('>"$preview_root/native.log" 2>&1', capture)
        allowed = [line.strip().rsplit("/", 1)[-1] for line in upload.splitlines() if "${{ runner.temp }}" in line]
        self.assertEqual(allowed, ["desktop-preview.png", "account-switcher.png", "sign-in.png", "incoming-call.png",
            "room-settings.png", "paper-preview.png", "manage-accounts.png", "account-password.png",
            "account-password-large.png", "sign-in-large.png", "terminal-sidebar.png", "terminal-space-labels.png",
            "send-recovery.png", "send-recovery-thread-large.png", "provenance.json"])
        self.assertIn("retention-days: 1", upload)
        self.assertIn("include-hidden-files: false", upload)
        self.assertNotIn("pull_request_target:", source)
        self.assertNotIn("workflow_run:", source)
        self.assertNotIn("secrets.", source)
        self.assertIn("contents: read", source)
        self.assertIn("github.ref == 'refs/heads/main' && github.event_name != 'pull_request'", source)


def load_native_validator(function_name="native_check_error"):
    source = WORKFLOW.read_text(encoding="utf-8")
    start, end = "          python3 - <<'PY_NATIVE_ACCEPTANCE'\n", "          PY_NATIVE_ACCEPTANCE\n"
    if source.count(start) != 1 or source.count(end) != 1:
        raise AssertionError("Expected one bounded native outcome validator")
    module = ast.parse(textwrap.dedent(source.split(start,1)[1].split(end,1)[0]))
    functions = [node for node in module.body if isinstance(node,ast.FunctionDef) and node.name==function_name]
    if len(functions)!=1 or functions[0].decorator_list or functions[0].args.defaults or functions[0].args.kw_defaults:
        raise AssertionError("Expected one pure native outcome validator")
    namespace = {}
    exec(compile(ast.Module(body=functions,type_ignores=[]),str(WORKFLOW),"exec"),namespace)
    return namespace[function_name]


class NativeRouteWorkflowTests(unittest.TestCase):
    def test_selected_steps_must_pass_and_unselected_steps_must_be_skipped(self):
        validate = load_native_validator()
        for runner in ("Linux","Windows","macOS"):
            for route in ("all","models","desktop","desktop-video","devices","vendor","sdk","matrix","wayland"):
                if route in ("matrix","wayland") and runner!="Linux":
                    continue
                wayland = "true" if runner=="Linux" and route in ("all","wayland") else "false"
                selected = {"model-tests":route in ("all","models"), "desktop-tests":route in ("all","desktop","desktop-video"),
                            "device-tests":route in ("all","devices"), "vendor-tests":route in ("all","vendor"),
                            "video-source-tests":route in ("all","vendor","desktop-video"),
                            "sdk-receipt-tests":route in ("all","sdk"),
                            "windows-capture-tests":runner=="Windows" and route in ("all","devices"),
                            "wayland-tests":wayland=="true", "matrix-tests":runner=="Linux" and route in ("all","matrix")}
                outcomes={name:{"outcome":"success" if yes else "skipped"} for name,yes in selected.items()}
                outcomes.update({name:{"outcome":"skipped"} for name in ("preview-capture","preview-upload")})
                self.assertIsNone(validate(route,runner,wayland,outcomes))
                for name, yes in selected.items():
                    for bad in (None,"failure","cancelled","neutral","skipped" if yes else "success"):
                        changed=outcomes | {name:{"outcome":bad}}
                        with self.subTest(route=route,runner=runner,step=name,bad=bad):
                            self.assertIsNotNone(validate(route,runner,wayland,changed))
                for name in ("preview-capture","preview-upload"):
                    self.assertIsNotNone(validate(route,runner,wayland,outcomes | {name:{"outcome":"failure"}}))
        for route in (None,[],"skip","helpers","config"):
            self.assertIsNotNone(validate(route,"Linux","false",{}))
        self.assertIsNotNone(validate("matrix","Windows","false",{}))
        self.assertIsNotNone(validate("wayland","Linux","false",{}))

    def test_global_wayland_selection_projects_to_runner_without_hiding_invalid_scope(self):
        project = load_native_validator("native_wayland_selection")
        validate = load_native_validator()
        self.assertEqual(project("all", "Linux", "true"), "true")
        for runner in ("Linux", "Windows", "macOS"):
            self.assertEqual(project("all", runner, "false"), "false")
            for malformed in (None, "", "TRUE", "False", "1", True, False, [], {}):
                with self.subTest(runner=runner, malformed=malformed):
                    self.assertIs(project("all", runner, malformed), malformed)
        # The actual Windows failure had global Wayland=true, successful native
        # outcomes, and a correctly skipped Linux-only Wayland step. Mac uses
        # the same matrix-wide scope with its Windows-only capture also skipped.
        for runner in ("Windows", "macOS"):
            outcomes = {name: {"outcome": "success"} for name in (
                "model-tests", "desktop-tests", "device-tests", "vendor-tests", "video-source-tests", "sdk-receipt-tests")}
            outcomes.update({name: {"outcome": "skipped"} for name in (
                "wayland-tests", "matrix-tests", "preview-capture", "preview-upload")})
            outcomes["windows-capture-tests"] = {"outcome": "success" if runner == "Windows" else "skipped"}
            self.assertIsNone(validate("all", runner, project("all", runner, "true"), outcomes))
            self.assertIsNotNone(validate("all", runner, "true", outcomes))
            for route in ("models", "desktop", "desktop-video", "devices", "vendor", "sdk", "matrix", "wayland", "foreign"):
                # Only the legitimate whole-matrix route may project true.
                # Invalid route/global-Wayland pairs must stay fail-closed.
                self.assertEqual(project(route, runner, "true"), "true")
                self.assertIsNotNone(validate(route, runner, project(route, runner, "true"), outcomes))
            for malformed in (None, "", "TRUE", "False", "1", True, False, [], {}):
                self.assertIsNotNone(validate("all", runner, project("all", runner, malformed), outcomes))
        # Bind this regression to the real wrapper, not just an unused helper.
        source = WORKFLOW.read_text(encoding="utf-8")
        start, end = "          python3 - <<'PY_NATIVE_ACCEPTANCE'\n", "          PY_NATIVE_ACCEPTANCE\n"
        module = ast.parse(textwrap.dedent(source.split(start, 1)[1].split(end, 1)[0]))
        calls = [node for node in ast.walk(module) if isinstance(node, ast.Call)
                 and isinstance(node.func, ast.Name) and node.func.id == "native_check_error"]
        self.assertEqual(len(calls), 1)
        projection = calls[0].args[2]
        self.assertIsInstance(projection, ast.Call)
        self.assertIsInstance(projection.func, ast.Name)
        self.assertEqual(projection.func.id, "native_wayland_selection")
        self.assertEqual(ast.dump(projection.args[0]), ast.dump(calls[0].args[0]))
        self.assertEqual(ast.dump(projection.args[1]), ast.dump(calls[0].args[1]))
        self.assertEqual(ast.unparse(projection.args[2]), "os.environ.get('WAYLAND_SELECTED')")

    def test_native_video_step_selects_exact_synthetic_cases_and_is_required(self):
        source = WORKFLOW.read_text(encoding="utf-8")
        start = "      - name: Test synthetic native video source ownership\n"
        end = "      - name: Test pinned SDK sticky receipt and redaction regressions\n"
        self.assertEqual(source.count(start), 1)
        step = source.split(start, 1)[1].split(end, 1)[0]
        self.assertIn("        id: video-source-tests\n", step)
        self.assertIn("if: (needs.scope.outputs.check_route == 'all' || needs.scope.outputs.check_route == 'vendor' || needs.scope.outputs.check_route == 'desktop-video')", step)
        cases = (
            "imp::video_source::tests::unused_source_drop_before_first_poll_releases_native_owner",
            "imp::video_source::tests::last_source_clone_drop_while_warmup_pending_releases_native_owner",
            "imp::video_source::tests::first_real_capture_stops_warmup_without_releasing_live_source",
            "imp::stats_tests::native_stats_parser_preserves_empty_and_typed_reports",
            "imp::stats_tests::native_stats_parser_rejects_malformed_and_unknown_reports",
            "imp::stats_tests::native_stats_parser_enforces_byte_and_record_limits",
        )
        for case in cases:
            self.assertEqual(source.count(case), 1)
            self.assertIn(case, step)
        self.assertIn('cargo test --locked -p libwebrtc --lib "$libwebrtc_source_test" -- --exact --test-threads=1', step)
        self.assertIn('grep -Fxc -- "test $libwebrtc_source_test ... ok"', step)
        self.assertIn("VIDEO_SOURCE_OUTCOME: ${{ steps.video-source-tests.outcome }}", source)
        self.assertIn('("video-source-tests", "VIDEO_SOURCE_OUTCOME")', source)

    def test_transport_key_acceptance_regressions_are_explicit_in_vendor_gate(self):
        source = WORKFLOW.read_text(encoding="utf-8")
        start = "          rtc_constraint_tests=(\n"
        self.assertEqual(source.count(start), 1)
        selected = source.split(start, 1)[1].split("          )\n", 1)[0]
        expected = (
            'pool::tests::constraint_writer_settles_older_apply_then_coalesces_newest_intent',
            'pool::tests::removed_constraint_writer_cannot_finish_after_new_same_key_intent',
            'pool::tests::replacement_connection_receives_latest_intent_without_old_target_retention',
            'pool::tests::dropping_pool_retires_queued_constraint_before_connection_lock_release',
            'pool::tests::dropping_pool_settles_entered_apply_without_running_queued_successor',
            'pool::tests::constraint_first_poll_uses_current_writer_admission_before_removal',
            'pool::tests::first_poll_panic_remains_failed_but_later_real_intent_can_settle',
            'keys::tests::sender_registration_adopts_only_an_accepted_own_index',
            'keys::tests::registration_and_in_flight_acceptance_share_one_sender_order',
            'pool::tests::a_member_returning_under_the_same_member_id_gets_their_media_back',
            'pool::tests::parked_media_is_not_restored_after_its_track_or_connection_retires',
            'pool::tests::retired_generation_events_cannot_refill_parked_media',
            'pool::tests::replacing_the_adopted_connection_discards_its_parked_handles',
            'pool::tests::an_old_connection_removal_cannot_remove_the_replacement_handle',
            'pool::tests::an_active_old_incarnation_cannot_be_parked_after_same_key_adoption',
            'pool::tests::an_active_idle_retired_focus_cannot_be_parked_after_a_lapse',
        )
        self.assertEqual(tuple(line.strip() for line in selected.splitlines() if line.strip()), expected)
        for case in expected:
            self.assertEqual(source.count(case), 1)
            self.assertIn(case, selected)
        step = source.split("      - name: Test vendored security fixes\n", 1)[1].split("      - name:", 1)[0]
        self.assertEqual(step.count("          cargo test --locked -p matrix-rtc-call-sdk --lib\n"), 1)
        self.assertNotIn("matrix-rtc-call-sdk --lib --no-run", step)
        self.assertEqual(step.count("          cargo test --locked -p matrix-rtc-matrix-sdk --lib\n"), 1)
        self.assertIn('cargo test --locked -p matrix-rtc-matrix-sdk --lib -- --list', step)
        self.assertIn('grep -Fxc -- "sdk::tests::device_wakes_do_not_restart_failed_room_subject_retry: test"', step)
        self.assertNotIn("matrix-rtc-matrix-sdk --lib --no-run", step)
        # The package selector must reach this actual SDK in the patched root
        # workspace, rather than an excluded dependency or standalone graph.
        import tomllib
        root = WORKFLOW.parent.parent.parent
        workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
        sdk_path = "vendor/matrix-rtc-call-sdk"
        self.assertIn(sdk_path, workspace["workspace"]["members"])
        self.assertNotIn(sdk_path, workspace["workspace"].get("exclude", []))
        self.assertNotIn(sdk_path, workspace["workspace"]["default-members"])
        self.assertEqual(workspace["patch"]["https://github.com/element-hq/matrix-rust-rtc"]
                         ["matrix-rtc-call-sdk"]["path"], sdk_path)
        sdk = tomllib.loads((root / sdk_path / "Cargo.toml").read_text(encoding="utf-8"))
        locked = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
        matches = [package for package in locked["package"]
                   if package["name"] == "matrix-rtc-call-sdk"]
        self.assertEqual(len(matches), 1)
        locked_names = {dependency.split()[0] for dependency in matches[0]["dependencies"]}
        self.assertLessEqual(set(sdk["dev-dependencies"]), locked_names)
        adapter_path = "vendor/matrix-rtc-matrix-sdk"
        self.assertIn(adapter_path, workspace["workspace"]["members"])
        self.assertNotIn(adapter_path, workspace["workspace"].get("exclude", []))
        self.assertNotIn(adapter_path, workspace["workspace"]["default-members"])
        self.assertEqual(workspace["patch"]["https://github.com/element-hq/matrix-rust-rtc"]
                         ["matrix-rtc-matrix-sdk"]["path"], adapter_path)
        adapter = tomllib.loads((root / adapter_path / "Cargo.toml").read_text(encoding="utf-8"))
        adapter_locked = [package for package in locked["package"]
                          if package["name"] == "matrix-rtc-matrix-sdk"]
        self.assertEqual(len(adapter_locked), 1)
        adapter_names = {dependency.split()[0] for dependency in adapter_locked[0]["dependencies"]}
        self.assertLessEqual(set(adapter["dev-dependencies"]), adapter_names)
        self.assertIn("futures-util", adapter_names)
        self.assertIn("testing", adapter["dev-dependencies"]["matrix-rtc-core"]["features"])
        adapter_source = (root / adapter_path / "src/sdk.rs").read_text(encoding="utf-8")
        self.assertIn("async fn device_wakes_do_not_restart_failed_room_subject_retry()", adapter_source)
        # Retained clap defaults/derive activate optional edges even though
        # clap itself was already locked as an application dependency.
        clap = next(package for package in locked["package"] if package["name"] == "clap")
        builder = next(package for package in locked["package"] if package["name"] == "clap_builder")
        self.assertIn("derive", sdk["dev-dependencies"]["clap"]["features"])
        self.assertIsNot(sdk["dev-dependencies"]["clap"].get("default-features"), False)
        self.assertIn("clap_derive", {dependency.split()[0] for dependency in clap["dependencies"]})
        self.assertIn("anstream", {dependency.split()[0] for dependency in builder["dependencies"]})
        derive = [package for package in locked["package"] if package["name"] == "clap_derive"]
        self.assertEqual(len(derive), 1)
        self.assertEqual(derive[0]["version"], clap["version"])
        self.assertIn('cargo test --locked -p matrix-rtc-transport --lib -- --list', source)
        self.assertIn('grep -Fxc -- "$rtc_constraint_test: test"', source)
        self.assertIn('cargo test --locked -p matrix-rtc-transport --lib "$rtc_constraint_test" -- --exact', source)
        self.assertIn('grep -Fxc -- "test $rtc_constraint_test ... ok"', source)
        self.assertIn("VENDOR_OUTCOME: ${{ steps.vendor-tests.outcome }}", source)
        self.assertIn('("vendor-tests", "VENDOR_OUTCOME")', source)

    def test_manual_inputs_and_fixed_routes_keep_existing_security_and_commands(self):
        source=WORKFLOW.read_text(encoding="utf-8")
        for value in ("options: [all, linux, windows, macos-arm, macos-intel]",
                      "cancel-in-progress: ${{ github.event_name != 'workflow_dispatch' }}",
                      "github.actor == github.repository_owner", "github.triggering_actor == github.repository_owner",
                      "CHECK_EVENT: ${{ github.event_name }}", "MATRIX_OUTCOME: ${{ steps.matrix-tests.outcome }}",
                      'group: native-${{ github.sha }}-${{ matrix.os }}',
                      'cargo test --locked -p cargo-packager-updater --lib',
                      '--gitleaks "$RUNNER_TEMP/gitleaks/gitleaks" --suite "$MATRIX_SUITE"'):
            self.assertIn(value,source)
        for key, default in (("manual","false"),("manual_platform","all"),("check_route","all"),("matrix_suite","full")):
            self.assertIn(key + ": ${{ steps.scope.outputs." + key
                          + " || (github.event_name != 'workflow_dispatch' && '" + default + "' || '') }}",source)
        self.assertIn("'Focused CI acceptance (partial)' || 'CI acceptance'",source)
        self.assertIn("format('Focused {0} ({1}, partial)'",source)
        self.assertNotIn("pull_request_target:",source)
        self.assertNotIn("workflow_run:",source)
        self.assertNotIn("secrets.",source)


if __name__ == "__main__":
    unittest.main()
