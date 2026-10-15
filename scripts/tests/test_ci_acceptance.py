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


acceptance_error = load_validator()


def snapshot(native=False, config=False, platforms=None, wayland=False):
    return {
        "scope": {"result": "success", "outputs": {
            "native": str(native).lower(),
            "config": str(config).lower(),
            "wayland": str(wayland).lower(),
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
            "account-password-large.png", "sign-in-large.png", "terminal-sidebar.png", "terminal-space-labels.png", "provenance.json"])
        self.assertIn("retention-days: 1", upload)
        self.assertIn("include-hidden-files: false", upload)
        self.assertNotIn("pull_request_target:", source)
        self.assertNotIn("workflow_run:", source)
        self.assertNotIn("secrets.", source)
        self.assertIn("contents: read", source)
        self.assertIn("github.ref == 'refs/heads/main' && github.event_name != 'pull_request'", source)


if __name__ == "__main__":
    unittest.main()
