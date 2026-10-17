"""Pure checks for the explicitly selected desktop regression repair mode."""

import ast
import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest
from unittest.mock import patch

from scripts import ci_scope

WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml"


class DesktopModeTests(unittest.TestCase):
    def run_scope(self, narrow=False):
        with tempfile.TemporaryDirectory() as folder:
            event = Path(folder) / "event.json"
            output = Path(folder) / "outputs"
            event.write_text("{}")
            args = ["ci_scope", "--github-event"] + (["--desktop-tests-only"] if narrow else [])
            stream = io.StringIO()
            with patch.object(sys, "argv", args), patch.dict(os.environ, {
                "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_EVENT_PATH": str(event),
                "GITHUB_OUTPUT": str(output),
            }), contextlib.redirect_stdout(stream):
                ci_scope.main()
            value = json.loads(stream.getvalue())
            exports = {key: json.loads(raw) for key, raw in
                       (line.split("=", 1) for line in output.read_text().splitlines())}
            self.assertEqual(exports, value)
            return value

    def test_default_manual_dispatch_remains_full_including_wayland(self):
        self.assertEqual(self.run_scope(), ci_scope.classify([], force_all=True))
        self.assertTrue(self.run_scope()["wayland"])

    def test_explicit_desktop_mode_checks_all_four_targets(self):
        self.assertEqual(self.run_scope(True), {
            "native": True, "config": False, "wayland": False,
            "native_matrix": {"os": list(ci_scope.PLATFORMS)},
        })

    def test_narrowing_cannot_be_requested_from_a_push_pr_or_plain_cli(self):
        for event in ("push", "pull_request", "", "workflow_dispatch"):
            args = ["scope", "--desktop-tests-only"]
            if event != "workflow_dispatch":
                args.append("--github-event")
            with self.subTest(event=event), patch.object(sys, "argv", args), \
                    patch.dict(os.environ, {"GITHUB_EVENT_NAME": event}), \
                    patch.object(ci_scope, "changed_paths") as changed, \
                    contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                ci_scope.main()
            self.assertEqual(error.exception.code, 2)
            changed.assert_not_called()

    def test_invalid_mode_value_cannot_claim_acceptance(self):
        source = WORKFLOW.read_text()
        start = "          python3 - <<'PY_MODE'\n"
        end = "          PY_MODE\n"
        self.assertEqual(source.count(start), 1)
        self.assertEqual(source.count(end), 1)
        tree = ast.parse(textwrap.dedent(source.split(start)[1].split(end)[0]))
        functions = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "validation_mode"]
        self.assertEqual(len(functions), 1)
        self.assertFalse(functions[0].decorator_list or functions[0].args.defaults)
        namespace = {}
        exec(compile(ast.Module(body=functions, type_ignores=[]), "reviewed_mode", "exec"), namespace)
        validate = namespace["validation_mode"]
        for raw in (None, True, False, "True", "", [], {}, 1):
            self.assertIsNone(validate(raw))
        for raw in ("true", "false"):
            self.assertEqual(validate(raw), {"check": "ci_validation_mode",
                "desktop_tests_only": raw == "true", "release_acceptance": False})

    def test_only_explicit_manual_input_can_enable_the_workflow_mode(self):
        source = WORKFLOW.read_text()
        self.assertIn("desktop_tests_only:\n", source)
        self.assertIn("        default: false\n", source)
        self.assertIn("MORPHEUS_DESKTOP_TESTS_ONLY: ${{ github.event_name == 'workflow_dispatch' && inputs.desktop_tests_only == true }}", source)
        self.assertIn('if [ "$MORPHEUS_DESKTOP_TESTS_ONLY" = "true" ]; then', source)
        self.assertIn("python3 -I ../trusted/scripts/ci_scope.py --github-event --desktop-tests-only", source)
        self.assertIn("wayland: ${{ steps.scope.outputs.wayland }}", source)

    def test_unaffected_groups_are_skipped_but_desktop_failure_stays_fatal(self):
        source = WORKFLOW.read_text()
        skipped = [
            "Test configuration, Matrix, and RTC protocol models", "Check desktop and Matrix crates",
            "Build desktop with native call and device adapters", "Capture actual native preview screens",
            "Upload native preview PNGs and provenance only", "Smoke test Linux desktop startup under Xvfb",
            "Test native device and call guards", "Test Windows capture of an owned synthetic window and display",
            "Test real Wayland portal streaming, source loss, and cancellation", "Test vendored security fixes",
            "Test pinned SDK sticky receipt and redaction regressions",
        ]
        for name in skipped:
            block = source.split("      - name: " + name + "\n", 1)[1].split("      - name:", 1)[0]
            self.assertIn("env.MORPHEUS_DESKTOP_TESTS_ONLY != 'true'", block, name)
        desktop = source.split("      - name: Test desktop controller and call guards\n", 1)[1].split("      - name:", 1)[0]
        self.assertNotIn("        if:", desktop)
        self.assertIn("cargo test --locked -p morpheus-app --features native-calls --bin morpheus-app", desktop)
        self.assertIn("steps.desktop-tests.outcome == 'failure'", source)
        self.assertIn("runs-on: ${{ matrix.os }}", source)
        self.assertIn("matrix: ${{ fromJSON(needs.scope.outputs.native_matrix) }}", source)
        self.assertIn("contents: read", source)
        self.assertIn("persist-credentials: false", source)
        self.assertIn("github.ref == 'refs/heads/main' && github.event_name != 'pull_request'", source)
        for forbidden in ("pull_request_target:", "workflow_run:", "secrets.", "self-hosted"):
            self.assertNotIn(forbidden, source)

    def test_linux_live_acceptance_is_required_for_the_repaired_app_test_crate(self):
        source = WORKFLOW.read_text()
        live = source.split("      - name: Test real Matrix accounts, recovery, moderation, and encrypted RTC\n", 1)[1].split("      - name:", 1)[0]
        self.assertIn("        if: runner.os == 'Linux'\n", live)
        self.assertNotIn("MORPHEUS_DESKTOP_TESTS_ONLY", live)
        self.assertNotIn("continue-on-error", live)
        self.assertIn("dbus-run-session -- python3 scripts/run_matrix_acceptance.py", live)
        # Ordinary desktop tests exclude ignored live cases. The mandatory
        # Linux suite exercises the same app test target with its live opt-in.
        driver = (WORKFLOW.parents[2] / "scripts/run_matrix_acceptance.py").read_text()
        call_cases = driver.split("    def outgoing_ring_cases(self):\n", 1)[1].split("    def oauth_cases(self):\n", 1)[0]
        self.assertIn('"morpheus-app", "--features", "native-calls"', call_cases)
        self.assertIn('"--bin", "morpheus-app", RING_TEST', call_cases)
        self.assertIn('"--ignored", "--exact", "--show-output"', call_cases)


if __name__ == "__main__":
    unittest.main()
