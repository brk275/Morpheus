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
                                ("windows","devices"), ("macos-arm","vendor"), ("macos-intel","sdk")):
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

    def test_selected_wayland_requires_linux_native_job(self):
        self.assertIsNone(acceptance_error(snapshot(True, wayland=True)))
        self.assertIsNone(acceptance_error(snapshot(True, platforms=["ubuntu-24.04"], wayland=True)))
        for value in (snapshot(wayland=True), snapshot(config=True, wayland=True),
                      snapshot(True, platforms=["macos-15", "windows-2025"], wayland=True)):
            self.assertIsNotNone(acceptance_error(value))


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
            for route in ("all","models","desktop","devices","vendor","sdk","matrix","wayland"):
                if route in ("matrix","wayland") and runner!="Linux":
                    continue
                wayland = "true" if runner=="Linux" and route in ("all","wayland") else "false"
                selected = {"model-tests":route in ("all","models"), "desktop-tests":route in ("all","desktop"),
                            "device-tests":route in ("all","devices"), "vendor-tests":route in ("all","vendor"),
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
                "model-tests", "desktop-tests", "device-tests", "vendor-tests", "sdk-receipt-tests")}
            outcomes.update({name: {"outcome": "skipped"} for name in (
                "wayland-tests", "matrix-tests", "preview-capture", "preview-upload")})
            outcomes["windows-capture-tests"] = {"outcome": "success" if runner == "Windows" else "skipped"}
            self.assertIsNone(validate("all", runner, project("all", runner, "true"), outcomes))
            self.assertIsNotNone(validate("all", runner, "true", outcomes))
            for route in ("models", "desktop", "devices", "vendor", "sdk", "matrix", "wayland", "foreign"):
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
