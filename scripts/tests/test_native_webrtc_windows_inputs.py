#!/usr/bin/env python3
"""Existing unittest route: Windows producer admission DATA boundaries only."""
import hashlib
import copy
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

PATH = Path(__file__).resolve().parents[1] / "run_native_webrtc_windows.py"
SPEC = importlib.util.spec_from_file_location("native_windows_producer", PATH)
TARGET = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TARGET)


class WindowsNativeInputsTests(unittest.TestCase):
    def test_actual_two_version_wheels_and_virtualenv_keep_distinct_runtime_consumer_roots(self):
        # Exact original universal six versions and required virtualenv CIPD ZIP inventories.
        # Only the production DATA consumer/check_members run; no download, extraction or tool execution.
        materials = [{'role': 'wheels',
          'source_mount': None,
          'tool': {'name': 'wheels-321750c49ce19b7c',
                   'format': 'zip',
                   'bytes': 10762,
                   'sha256': '4a23290e689d8df1695cc54758e9c3c7e969a820c25396043ce6b893816ebb33',
                   'source': {'kind': 'cipd',
                              'package': 'infra/python/wheels/six-py2_py3',
                              'version': 'version:1.10.0'},
                   'members': [{'path': 'six-1.10.0-py2.py3-none-any.whl',
                                'kind': 'regular',
                                'bytes': 10341,
                                'mode': 384,
                                'output_mode': 384,
                                'sha256': '0ff78c403d9bccf5a425a6d31a12aa6b47f1c21ca4dc2573a7e2f32a97335eb1'},
                               {'path': '.cipdpkg/manifest.json',
                                'kind': 'regular',
                                'bytes': 109,
                                'mode': 384,
                                'output_mode': 384,
                                'sha256': '7f552b6f0052d1bcea939509cdcc29cea173dad509996611be4d3b84d9e21f5d'}],
                   'alias_copies': [],
                   'omitted_aliases': [],
                   'omission_evidence': None}},
         {'role': 'wheels',
          'source_mount': None,
          'tool': {'name': 'wheels-0f0494d6bafc1c0d',
                   'format': 'zip',
                   'bytes': 11476,
                   'sha256': 'c664d8cde41f44c5a24d8eaa8e1fc1416068cdf8e59ac9a89bd59d0958bad7e5',
                   'source': {'kind': 'cipd',
                              'package': 'infra/python/wheels/six-py2_py3',
                              'version': 'version:1.16.0'},
                   'members': [{'path': 'six-1.16.0-py2.py3-none-any.whl',
                                'kind': 'regular',
                                'bytes': 11053,
                                'mode': 256,
                                'output_mode': 384,
                                'sha256': '8abb2f1d86890a2dfb989f9a77cfcfd3e47c2a354b01111771326f8aa26e0254'},
                               {'path': '.cipdpkg/manifest.json',
                                'kind': 'regular',
                                'bytes': 111,
                                'mode': 256,
                                'output_mode': 384,
                                'sha256': 'f652ac35a9dc7b73f52c8bdd3f947beb27ff79c37d6a77ed877aaf93f3f225bf'}],
                   'alias_copies': [],
                   'omitted_aliases': [],
                   'omission_evidence': None}},
         {'role': 'wheels',
          'source_mount': None,
          'tool': {'name': 'wheels-74479c1acae88219',
                   'format': 'zip',
                   'bytes': 4406690,
                   'sha256': '2529d5c7e428d2d943fe9c22447f4cf8afef8bec39e735de30970417d4e48e24',
                   'source': {'kind': 'cipd',
                              'package': 'infra/3pp/tools/virtualenv',
                              'version': 'version:2@20.25.1.chromium.8'},
                   'members': [{'path': 'virtualenv-20.25.1/virtualenv.pyz',
                                'kind': 'regular',
                                'bytes': 5705128,
                                'mode': 256,
                                'sha256': '03eee6023553aa233b93990704694588de21631a9604ffa1eb8c19f879352f9b',
                                'output_mode': 384},
                               {'path': '.cipdpkg/manifest.json',
                                'kind': 'regular',
                                'bytes': 103,
                                'mode': 256,
                                'sha256': '863ee7fdc5ca628bb75cd0a15b746f05c5b6d677b42642c76d2e88ba65d6163b',
                                'output_mode': 384}],
                   'alias_copies': [],
                   'omitted_aliases': [],
                   'omission_evidence': None}}]
        profile = {"schema": "morpheus-windows-runtime-data-v1", "target": "win-x64", "materials": materials}
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_runtime_actual_data_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        control = mock.Mock(root=Path("synthetic-runtime-data"))
        plan = {"runtime_profile": {"sha256": "a" * 64}}
        def consume(selected):
            with mock.patch.object(TARGET, "bound_asset", return_value=(None, selected)), \
                    mock.patch.object(stager, "download") as download, \
                    mock.patch.object(stager, "extract") as extract, \
                    mock.patch.object(TARGET, "write_json"):
                TARGET.runtime_stage(stager, Path("synthetic-workspace"), plan, control)
                return download.call_args_list, extract.call_args_list
        downloads, extractions = consume(profile)
        self.assertEqual(len(downloads), 3)
        self.assertEqual(len({call.args[2] for call in downloads}), 3)
        self.assertEqual({call.args[3].name for call in extractions},
                         {item["tool"]["name"] for item in materials})
        for item, call in zip(materials, extractions):
            self.assertEqual(call.args[1], item["tool"])
            self.assertEqual(call.args[3], control.root / "runtime" / item["tool"]["name"])
        # Duplicate identities, forged derived keys and foreign-platform origins cannot become new cohorts.
        for change in ("duplicate", "key", "platform", "origin"):
            invalid = copy.deepcopy(profile)
            if change == "duplicate":
                invalid["materials"].append(copy.deepcopy(materials[0]))
            elif change == "key":
                invalid["materials"][1]["tool"]["name"] = materials[0]["tool"]["name"]
            elif change == "platform":
                invalid["materials"][1]["tool"]["source"]["package"] = "infra/python/wheels/six/linux-amd64_cp311_cp311"
            else:
                invalid["materials"][1]["tool"]["source"]["package"] = "foreign/virtualenv"
            with self.subTest(change=change), self.assertRaises(TARGET.Refused):
                consume(invalid)
        oversized = copy.deepcopy(profile)
        oversized["materials"] = [materials[0]] * 41
        with self.assertRaises(TARGET.Refused):
            consume(oversized)

    def test_original_owner_main_public_host_contract_is_required(self):
        env = dict(GITHUB_ACTIONS="true", RUNNER_ENVIRONMENT="github-hosted", RUNNER_OS="Windows",
            RUNNER_ARCH="X64", GITHUB_REPOSITORY="brk275/Morpheus", GITHUB_EVENT_NAME="workflow_dispatch",
            GITHUB_REF="refs/heads/main", GITHUB_ACTOR="brk275", GITHUB_TRIGGERING_ACTOR="brk275")
        self.assertTrue(TARGET.environment_admitted("nt", env))
        for key, value in (("GITHUB_TRIGGERING_ACTOR", "foreign"), ("GITHUB_ACTOR", "foreign"),
                           ("GITHUB_REF", "refs/pull/44/merge"), ("GITHUB_EVENT_NAME", "push"),
                           ("RUNNER_ENVIRONMENT", "self-hosted"), ("RUNNER_OS", "Linux"),
                           ("RUNNER_ARCH", "ARM64"), ("GITHUB_REPOSITORY", "foreign/Morpheus")):
            with self.subTest(key=key):
                self.assertFalse(TARGET.environment_admitted("nt", env | {key: value}))
        for key in env:
            changed = dict(env)
            changed.pop(key)
            self.assertFalse(TARGET.environment_admitted("nt", changed))
        self.assertFalse(TARGET.environment_admitted("posix", env))

    def test_duplicate_json_and_windows_path_aliases_fail_closed(self):
        with self.assertRaises(TARGET.Refused):
            TARGET.unique([("gates", "unknown"), ("gates", "approved")])
        for name in ("../secret", "a/./b", "a//b", "/absolute", "C:/drive", "a\\b", "a:stream",
                     ".git/config", "con.txt", "a/trailing.", "a/trailing ", "a/line\nfeed", "a/\x00"):
            with self.subTest(name=name), self.assertRaises(TARGET.Refused):
                TARGET.relative(name)
        self.assertEqual(TARGET.relative("third_party/rust-toolchain/bin/rustc.exe"),
                         "third_party/rust-toolchain/bin/rustc.exe")

    def test_actual_projection_rows_match_existing_builder_typed_inventory(self):
        builder_path = PATH.with_name("build_native_webrtc_windows.py")
        spec = importlib.util.spec_from_file_location("native_windows_builder_contract", builder_path)
        builder = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(builder)
        owner = mock.Mock()
        owner.root = Path("private")
        source, core, runtime = Path("private/source"), Path("core"), Path("private")
        ordinary = {"path": "gn.exe", "kind": "regular", "bytes": 4,
                    "sha256": "a" * 64, "output_mode": 0o600}
        executable = {"path": "bin/rustc.exe", "kind": "regular", "bytes": 8,
                      "sha256": "b" * 64, "output_mode": 0o700}
        core_manifest = {"targets": {"Windowsx64": [{"name": "gn", "members": [ordinary],
            "alias_copies": [], "omitted_aliases": []}]}}
        runtime_profile = {"materials": [{"role": "rust", "source_mount": "third_party/rust-toolchain",
            "tool": {"members": [executable]}}]}
        emitted = []
        with mock.patch.object(TARGET, "tree_files", return_value=[]), \
                mock.patch.object(TARGET, "load_unbound", return_value=[]), \
                mock.patch.object(TARGET, "register_parent"), mock.patch.object(TARGET, "copy_file") as copy, \
                mock.patch.object(TARGET, "write_json", side_effect=lambda owner, path, rows: emitted.extend(rows)):
            TARGET.project(owner, source, core, core_manifest, runtime, runtime_profile)
        self.assertEqual(copy.call_count, 2)
        self.assertEqual([row["mode"] for row in emitted], ["100644", "100755"])
        self.assertEqual(builder.typed_files(emitted), [
            {"path": "buildtools/win/gn.exe", "bytes": 4, "sha256": "a" * 64},
            {"path": "third_party/rust-toolchain/bin/rustc.exe", "bytes": 8, "sha256": "b" * 64}])
        with self.assertRaises(builder.RecipeError):
            builder.typed_files([{k: v for k, v in emitted[0].items() if k != "mode"}])


    def test_package_headers_retain_original_git_api_not_projected_toolchains(self):
        spec = importlib.util.spec_from_file_location("native_windows_header_contract",
                                                    PATH.with_name("build_native_webrtc_windows.py"))
        builder = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(builder)
        original = {"api/native.h": {"bytes": 4, "sha256": "a" * 64},
            "sdk/wrapper.inc": {"bytes": 8, "sha256": "b" * 64},
            "third_party/abseil-cpp/absl/base/config.h": {"bytes": 12, "sha256": "c" * 64},
            "LICENSE": {"bytes": 16, "sha256": "d" * 64}}
        complete = original | {
            "third_party/llvm-build/Release+Asserts/lib/clang/include/stddef.h":
                {"bytes": 20, "sha256": "e" * 64},
            "third_party/rust-toolchain/lib/rustlib/include/rust.h":
                {"bytes": 24, "sha256": "f" * 64}}
        saved = dict(complete)
        self.assertEqual(builder.package_source_headers(original, complete),
            ["api/native.h", "sdk/wrapper.inc", "third_party/abseil-cpp/absl/base/config.h"])
        self.assertEqual(complete, saved)
        with self.assertRaises(builder.RecipeError):
            builder.package_source_headers(original, complete | {
                "api/native.h": {"bytes": 99, "sha256": "a" * 64}})

    def test_source_stage_copies_exact_consumer_inputs_inside_owned_temp(self):
        # Actual source_stage + bound_asset/copy_file/write_json file IO. Only
        # Windows host path admission and the final exporter call are replaced;
        # this test neither admits a host nor executes Git/source provision.
        class Control:
            def __init__(self, root):
                self.root = root
            def mkdir(self, path):
                path.mkdir()
            def create(self, path):
                return path.open("xb")
            def bytes(self, path, raw):
                with self.create(path) as stream:
                    stream.write(raw)
                    stream.flush()
                    os.fsync(stream.fileno())

        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            workspace, runner = root / "workspace", root / "runner"
            workspace.mkdir()
            runner.mkdir()
            control = Control(runner / "control")
            control.root.mkdir()
            def asset(name, value):
                path = workspace / "native/webrtc/windows" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                raw = value if isinstance(value, bytes) else json.dumps(value).encode()
                path.write_bytes(raw)
                return {"path": path.relative_to(workspace).as_posix(), "bytes": len(raw),
                        "sha256": hashlib.sha256(raw).hexdigest()}
            security = asset("security.json", {"baseline_commit": TARGET.NATIVE,
                                               "baseline_tree": TARGET.TREE})
            patches = [asset("patches/%02d.patch" % n, ("synthetic patch %d\n" % n).encode())
                       for n in range(13)]
            manifest = asset("patch-manifest.json", {"schema": "morpheus-native-source-patches-v1",
                "patches": [{"file": row["path"], "bytes": row["bytes"], "sha256": row["sha256"]}
                            for row in patches]})
            source_plan = asset("source-plan.json", {"schema": "morpheus-native-webrtc-sources-v2",
                "target": "win-x64", "git": {"path": "GIT_UNBOUND"}})
            git = workspace / "git.exe"
            git.write_bytes(b"synthetic tool DATA only")
            git_fact = {"path": str(git), "bytes": git.stat().st_size,
                        "sha256": hashlib.sha256(git.read_bytes()).hexdigest(), "version": "synthetic"}
            profile = asset("profile.json", {"schema": "morpheus-windows-source-stage-v1",
                "target": "win-x64", "source_plan": source_plan, "patch_manifest": manifest,
                "security_plan": security, "patch_files": patches, "host_git": git_fact})
            original_files = {p: p.read_bytes() for p in workspace.rglob("*") if p.is_file()}
            observed = []
            def exporter(module, args):
                selected_file = Path(args[args.index("--plan") + 1])
                self.assertTrue(selected_file.is_relative_to(control.root))
                selected = json.loads(selected_file.read_bytes())
                self.assertEqual(hashlib.sha256(selected_file.read_bytes()).hexdigest(),
                                 args[args.index("--plan-sha256") + 1])
                output = Path(args[args.index("--output-root") + 1])
                security_file = Path(selected["security_plan"]["path"])
                self.assertTrue(security_file.is_relative_to(runner))
                self.assertFalse(security_file.is_relative_to(output))
                self.assertEqual(security_file.read_bytes(),
                                 (workspace / security["path"]).read_bytes())
                self.assertEqual(selected["security_plan"]["sha256"], security["sha256"])
                patch_file = Path(selected["patch_manifest"]["path"])
                self.assertTrue(patch_file.is_relative_to(runner))
                self.assertEqual(hashlib.sha256(patch_file.read_bytes()).hexdigest(),
                                 selected["patch_manifest"]["sha256"])
                relocated = json.loads(patch_file.read_bytes())
                for row, original in zip(relocated["patches"], patches, strict=True):
                    copied = Path(row["file"])
                    self.assertTrue(copied.is_relative_to(runner))
                    self.assertFalse(copied.is_relative_to(output))
                    self.assertNotEqual(copied, workspace / original["path"])
                    self.assertEqual(copied.read_bytes(), (workspace / original["path"]).read_bytes())
                    self.assertEqual({k: row[k] for k in ("bytes", "sha256")},
                                     {k: original[k] for k in ("bytes", "sha256")})
                observed.append(True)
            with mock.patch.object(TARGET, "path", side_effect=Path), \
                    mock.patch.object(TARGET, "SECURITY", security["sha256"]), \
                    mock.patch.object(TARGET, "fixed_module", return_value=object()), \
                    mock.patch.object(TARGET, "invoke", side_effect=exporter):
                output = TARGET.source_stage(None, runner, workspace, {"source_profile": profile}, control)
            self.assertEqual(observed, [True])
            self.assertEqual(output, control.root / "source-provision")
            self.assertEqual(original_files,
                             {p: p.read_bytes() for p in workspace.rglob("*") if p.is_file()})

    def test_workflow_supplies_fixed_absolute_plan_and_entry_keeps_path_policy(self):
        workflow = PATH.parents[1] / ".github/workflows/native-webrtc-windows-source.yml"
        source = workflow.read_text()
        self.assertIn("$plan = Join-Path -Path $env:GITHUB_WORKSPACE -ChildPath "
                      "'native/webrtc/windows/pipeline-plan.json'", source)
        self.assertIn("--plan $plan --plan-sha256", source)
        self.assertNotIn("--plan native/webrtc/windows/pipeline-plan.json", source)
        with self.assertRaises(TARGET.Refused):
            TARGET.path("native/webrtc/windows/pipeline-plan.json")

    def test_guard_failure_does_not_touch_repository_api_or_load_tools(self):
        with mock.patch.object(TARGET, "environment_admitted", return_value=False), \
                mock.patch.object(TARGET, "repository_public") as api, \
                mock.patch.object(TARGET, "fixed_module") as module, self.assertRaises(TARGET.Refused):
            TARGET.runner()
        api.assert_not_called()
        module.assert_not_called()


if __name__ == "__main__":
    unittest.main()
