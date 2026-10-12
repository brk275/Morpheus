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


def snapshot(native=False, config=False, platforms=None):
    return {
        "scope": {"result": "success", "outputs": {
            "native": str(native).lower(),
            "config": str(config).lower(),
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
        for key in ("native", "config", "native_matrix"):
            value = snapshot()
            del value["scope"]["outputs"][key]
            invalid.append(value)
        for flag in ("native", "config"):
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


if __name__ == "__main__":
    unittest.main()
