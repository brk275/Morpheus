#!/usr/bin/env python3
"""Existing unittest route: Windows producer admission DATA boundaries only."""
import hashlib
import copy
import importlib.util
import io
import json
import os
import stat
import subprocess
from pathlib import Path
import tempfile
import unittest
import zipfile
from unittest import mock

PATH = Path(__file__).resolve().parents[1] / "run_native_webrtc_windows.py"
SPEC = importlib.util.spec_from_file_location("native_windows_producer", PATH)
TARGET = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TARGET)


class WindowsNativeInputsTests(unittest.TestCase):
    def test_core_manifest_failure_subcategories_are_closed_and_fatal(self):
        # Actual bound_asset/load/core_stage/entry boundaries with synthetic
        # DATA and mocked filesystem only; no Windows tool/network is invoked.
        private = "private payload must not reach the core failure summary"
        raw = b"{"
        record = {"path": "native/webrtc/synthetic.json", "bytes": len(raw),
                  "sha256": hashlib.sha256(raw).hexdigest()}
        for case, expected in (("record", "core_asset_record"), ("path", "core_asset_path"),
                ("identity", "core_expected_size"), ("hash", "core_expected_hash"),
                ("read", "core_asset_read"),
                ("json", "core_asset_json")):
            selected = {} if case == "record" else dict(record)
            if case == "path":
                selected["path"] = "native/webrtc/../private"
            observed = {key: record[key] for key in ("bytes", "sha256")}
            if case == "identity":
                observed["bytes"] += 1
            if case == "hash":
                observed["sha256"] = "0" * 64
            def fail_asset():
                TARGET.bound_asset(Path("synthetic-workspace"), selected, core_trace=True)
            output = io.StringIO()
            with mock.patch.object(TARGET, "FAILURE_CATEGORY", "entry"), \
                    mock.patch.object(TARGET, "path", side_effect=Path), \
                    mock.patch.object(TARGET, "fact", return_value=observed), \
                    mock.patch.object(TARGET, "read", return_value=raw,
                                      side_effect=RuntimeError(private) if case == "read" else None), \
                    mock.patch.object(TARGET, "main", side_effect=fail_asset), \
                    mock.patch.object(TARGET.sys, "stderr", output):
                with self.assertRaises(SystemExit) as failure:
                    TARGET.entry()
                self.assertEqual(failure.exception.code, 1)
            self.assertEqual(output.getvalue(),
                             "MORPHEUS_NATIVE_WINDOWS_PRODUCER failed category=" + expected + "\n")
        # Exercise the actual fact() metadata/read/count/identity comparisons
        # with fixed snapshots and an in-memory stream, never host files.
        for case, expected in (("stable_distinct_times", None),
                ("regular", "core_file_regular"), ("nlink", "core_file_nlink"),
                ("size", "core_file_size"), ("read", "core_file_read"),
                ("surplus", "core_file_count"), ("short", "core_file_count"),
                ("identity", "core_file_identity"), ("opened_identity", "core_file_identity"),
                ("birthtime", "core_file_identity"), ("missing_birthtime", "core_file_identity"),
                ("handle_change_time", "core_file_identity"), ("named_change_time", "core_file_identity")):
            fields = dict(st_dev=1, st_ino=2, st_size=4, st_mtime_ns=10, st_ctime_ns=20,
                          st_birthtime_ns=20, st_nlink=1, st_mode=stat.S_IFREG | 0o600)
            if case == "regular":
                fields["st_mode"] = stat.S_IFDIR | 0o700
            if case == "nlink":
                fields["st_nlink"] = 2
            if case == "size":
                fields["st_size"] = 5
            before = mock.Mock(**fields)
            # Actual 3.12.10 semantics: lstat ctime=birthtime (20), while
            # both fstats expose a distinct stable ChangeTime (30).
            opened_fields = dict(fields, st_ctime_ns=30,
                                 st_ino=3 if case == "opened_identity" else fields["st_ino"])
            if case == "missing_birthtime":
                opened_fields.pop("st_birthtime_ns")
            opened = mock.Mock(spec=list(opened_fields), **opened_fields)
            after = mock.Mock(**dict(fields, st_ctime_ns=31 if case == "handle_change_time" else 30,
                                    st_birthtime_ns=21 if case == "birthtime" else 20,
                                    st_ino=3 if case == "identity" else fields["st_ino"]))
            named = mock.Mock(**dict(fields, st_ctime_ns=21 if case == "named_change_time" else 20))
            payload = {"surplus": b"12345", "short": b"123"}.get(case, b"1234")
            stream = mock.MagicMock()
            stream.read.side_effect = RuntimeError(private) if case == "read" else io.BytesIO(payload).read
            stream.fileno.return_value = 7
            file = mock.MagicMock()
            file.lstat.side_effect = [before, named]
            file.open.return_value.__enter__.return_value = stream
            def fail_fact():
                TARGET.fact(file, 4, core_trace=True)
            output = io.StringIO()
            with mock.patch.object(TARGET, "FAILURE_CATEGORY", "entry"), \
                    mock.patch.object(TARGET.os, "name", "nt"), \
                    mock.patch.object(TARGET, "path", return_value=file), \
                    mock.patch.object(TARGET.os, "fstat", side_effect=[opened, after]) as fstat, \
                    mock.patch.object(TARGET, "main", side_effect=fail_fact), \
                    mock.patch.object(TARGET.sys, "stderr", output):
                if expected is None:
                    self.assertEqual(TARGET.fact(file, 4, core_trace=True),
                                     {"bytes": 4, "sha256": hashlib.sha256(payload).hexdigest()})
                    self.assertEqual(fstat.call_args_list, [mock.call(7), mock.call(7)])
                else:
                    with self.assertRaises(SystemExit) as failure:
                        TARGET.entry()
                    self.assertEqual(failure.exception.code, 1)
            self.assertEqual(output.getvalue(), "" if expected is None else
                             "MORPHEUS_NATIVE_WINDOWS_PRODUCER failed category=" + expected + "\n")
        value = {"baseline": TARGET.NATIVE, "tree": TARGET.TREE,
                 "targets": {"Windowsx64": [{"name": name} for name in ("gn", "ninja", "clang")]}}
        for case, expected in (("guard", "core_manifest_guard"), ("run", "core_run_identity")):
            selected = dict(value, baseline="foreign") if case == "guard" else value
            def fail_core():
                TARGET.core_stage(object(), Path("synthetic-temp"), Path("synthetic-workspace"),
                                  {"core_manifest": {"sha256": TARGET.CORE}}, mock.Mock())
            output = io.StringIO()
            with mock.patch.object(TARGET, "FAILURE_CATEGORY", "entry"), \
                    mock.patch.dict(os.environ, {}, clear=True), \
                    mock.patch.object(TARGET, "bound_asset", return_value=(Path("synthetic"), selected)), \
                    mock.patch.object(TARGET, "main", side_effect=fail_core), \
                    mock.patch.object(TARGET.sys, "stderr", output):
                with self.assertRaises(SystemExit) as failure:
                    TARGET.entry()
                self.assertEqual(failure.exception.code, 1)
            self.assertEqual(output.getvalue(),
                             "MORPHEUS_NATIVE_WINDOWS_PRODUCER failed category=" + expected + "\n")

    def test_windows_rc_original_gcs_binding_and_actual_bounded_data_download(self):
        assets = PATH.parents[1] / "native/webrtc/windows"
        plan = json.loads((assets / "pipeline-plan.json").read_bytes())
        self.assertIsNone(plan["native_tools_profile"])
        # Exercise the retained opt-in fixed DATA route, not a default build
        # prerequisite inferred from the original DEPS download hook.
        raw = (assets / "native-tools.json").read_bytes()
        record = {"path": "native/webrtc/windows/native-tools.json", "bytes": len(raw),
                  "sha256": TARGET.WINDOWS_RC_RECEIPT["profile_sha256"]}
        plan = dict(plan, native_tools_profile=record)
        self.assertEqual({"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()},
                         {key: record[key] for key in ("bytes", "sha256")})
        profile = json.loads(raw)
        self.assertEqual(profile["tool"], TARGET.WINDOWS_RC)
        metadata_record = profile["provenance"]
        metadata_raw = (PATH.parents[1] / metadata_record["path"]).read_bytes()
        self.assertEqual({"bytes": len(metadata_raw), "sha256": hashlib.sha256(metadata_raw).hexdigest()},
                         {key: metadata_record[key] for key in ("bytes", "sha256")})
        metadata = json.loads(metadata_raw)
        self.assertEqual(metadata["name"], "rc/" + TARGET.WINDOWS_RC["upstream_sha1"])
        self.assertEqual(metadata["generation"], TARGET.WINDOWS_RC["source"]["generation"])
        self.assertEqual(metadata["size"], str(TARGET.WINDOWS_RC["bytes"]))
        self.assertIn("src/build/toolchain/win/rc/win/rc.exe.sha1",
                      (assets / "reference/root-DEPS").read_text())
        self.assertIsNone(plan["build_inputs"])
        self.assertIs(plan["cache_handoff_authorized"], False)
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_rc_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        # The real fixed downloader and SHA1 check run on synthetic bytes. No
        # native executable, network, import hook or source checkout is run.
        payload = b"synthetic bounded rc DATA"
        tool = copy.deepcopy(TARGET.WINDOWS_RC)
        tool.update(bytes=len(payload), sha256=hashlib.sha256(payload).hexdigest(),
                    upstream_sha1=hashlib.sha1(payload).hexdigest())
        fake_metadata = dict(metadata, size=str(len(payload)))
        for variant in ("valid", "corrupt", "surplus", "truncated", "length", "generation", "sha1", "foreign_profile",
                        "handle_change_time", "named_change_time"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                owner = mock.Mock(root=root)
                def create(destination):
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    return destination.open("xb")
                owner.create.side_effect = create
                chosen = copy.deepcopy(tool)
                if variant == "sha1":
                    chosen["upstream_sha1"] = "0" * 40
                selected = dict(profile, tool=copy.deepcopy(chosen))
                if variant == "foreign_profile":
                    selected["tool"]["source"]["object"] = "foreign/object"
                connection, response = mock.Mock(), mock.Mock()
                content = {"corrupt": b"x" * len(payload), "surplus": payload + b"x",
                           "truncated": payload[:-1]}.get(variant, payload)
                response.read.side_effect = io.BytesIO(content).read
                headers = {"Content-Length": str(len(payload) + (1 if variant == "length" else 0)),
                           "x-goog-generation": "1" if variant == "generation" else chosen["source"]["generation"]}
                response.getheader.side_effect = headers.get
                # Preserve real opened-file identities/bytes while modeling
                # Windows3.12's distinct named CreationTime and FD ChangeTime.
                class Times:
                    def __init__(self, actual, change):
                        self.actual = actual
                        self.st_birthtime_ns = 20
                        self.st_ctime_ns = change
                    def __getattr__(self, name):
                        return getattr(self.actual, name)
                original_lstat, original_fstat = Path.lstat, os.fstat
                metadata_calls = {"named": 0, "handle": 0}
                def named_metadata(file):
                    actual = original_lstat(file)
                    metadata_calls["named"] += 1
                    changed = variant == "named_change_time" and metadata_calls["named"] == 4
                    return Times(actual, 21 if changed else 20)
                def handle_metadata(fd):
                    actual = original_fstat(fd)
                    metadata_calls["handle"] += 1
                    changed = variant == "handle_change_time" and metadata_calls["handle"] == 4
                    return Times(actual, 31 if changed else 30)
                windows_os = mock.Mock(wraps=os)
                windows_os.name = "nt"
                windows_os.fstat.side_effect = handle_metadata
                with mock.patch.object(TARGET, "WINDOWS_RC", chosen), \
                        mock.patch.object(TARGET, "os", windows_os), \
                        mock.patch.object(Path, "lstat", named_metadata), \
                        mock.patch.object(TARGET, "bound_asset", side_effect=[(None, selected), (None, fake_metadata)]), \
                        mock.patch.object(TARGET, "path", side_effect=Path), \
                        mock.patch.object(stager, "https", return_value=(connection, response)) as transport:
                    if variant == "valid":
                        result = TARGET.windows_native_tools_stage(stager, Path("synthetic-workspace"), plan, owner)
                        self.assertEqual(result, root / "native-tools")
                        self.assertEqual((result / "rc_win/rc.exe").read_bytes(), payload)
                        self.assertEqual(metadata_calls, {"named": 4, "handle": 4})
                    else:
                        with self.assertRaises((TARGET.Refused, stager.Refusal)):
                            TARGET.windows_native_tools_stage(stager, Path("synthetic-workspace"), plan, owner)
                if variant == "foreign_profile":
                    transport.assert_not_called()
                    self.assertEqual(list(root.iterdir()), [])
                else:
                    transport.assert_called_once_with("https://storage.googleapis.com/chromium-browser-clang/"
                        + chosen["source"]["object"] + "?generation=" + chosen["source"]["generation"])
                    connection.close.assert_called_once_with()
                    self.assertTrue(all(0 < call.args[0] <= 1024 * 1024
                                        for call in response.read.call_args_list))

    def test_optional_rc_absence_skips_actual_stage_and_emits_no_supplemental_receipt(self):
        owner = mock.Mock(root=Path("private-control"))
        stager = mock.Mock()
        stager.Stage.return_value = owner
        plan = {"source_profile": {"sha256": "a" * 64}, "runtime_profile": None,
                "native_tools_profile": None, "core_manifest": {"sha256": TARGET.CORE}}
        args = mock.Mock(phase="stage", plan_sha256="b" * 64)
        source, core = Path("synthetic-source"), Path("synthetic-core")
        saved = []
        with mock.patch.dict(os.environ, {"GITHUB_RUN_ID": "1", "GITHUB_RUN_ATTEMPT": "1"}), \
                mock.patch.object(TARGET.os.path, "lexists", return_value=False), \
                mock.patch.object(TARGET, "fixed_module", return_value=stager), \
                mock.patch.object(TARGET, "core_stage", return_value=core), \
                mock.patch.object(TARGET, "source_stage", return_value=source), \
                mock.patch.object(TARGET, "runtime_stage") as runtime, \
                mock.patch.object(TARGET, "windows_native_tools_stage") as tools, \
                mock.patch.object(TARGET, "bound_asset", return_value=(None, {})), \
                mock.patch.object(TARGET, "project", return_value=None) as project, \
                mock.patch.object(TARGET, "tree_files", return_value=[]), \
                mock.patch.object(TARGET, "write_json", side_effect=lambda owner, path, value:
                    saved.append(value) or {"sha256": "c" * 64}), \
                mock.patch("builtins.print"):
            TARGET.stage(args, Path("synthetic-temp"), Path("synthetic-workspace"), plan)
        tools.assert_not_called()
        runtime.assert_not_called()
        project.assert_called_once_with(owner, source, core, {}, None, None, None)
        self.assertEqual(len(saved), 1)
        self.assertIsNone(saved[0]["native_tools_profile_sha256"])
        self.assertIsNone(saved[0]["native_tool_files"])
        self.assertIs(saved[0]["build_admission_granted"], False)
        self.assertIs(saved[0]["build_ready"], False)
        self.assertIs(saved[0]["distribution_ready"], False)
        owner.finish.assert_called_once_with(True)

    def test_windows_rc_projection_keeps_original_pointer_and_all_collision_guards(self):
        owner = mock.Mock(root=Path("private"))
        source = Path("private/source")
        tools = Path("private/native-tools")
        manifest = {"targets": {"Windowsx64": []}}
        for variant in ("valid", "pointer", "existing_case_alias", "gitlink", "source_alias"):
            with self.subTest(variant=variant):
                normalizer = mock.Mock()
                normalizer.verify_source_aliases.return_value = ([{"mount": "src", "path": TARGET.WINDOWS_RC_PATH,
                    "target_path": "original/target"}] if variant == "source_alias" else [])
                originals = [{"path": TARGET.WINDOWS_RC_PATH.upper()}] if variant == "existing_case_alias" else []
                markers = [{"exported_path": "src/build/toolchain/win/rc"}] if variant == "gitlink" else []
                emitted, receipts = [], []
                def emit(owner, destination, value):
                    if destination.name == "windows-source-overlay.json":
                        emitted.extend(value)
                    else:
                        self.assertEqual(destination, source / "materials/windows-native-tools.json")
                        receipts.append(value)
                pointer = b"foreign" if variant == "pointer" else TARGET.WINDOWS_RC["upstream_sha1"].encode()
                with mock.patch.object(TARGET, "tree_files", return_value=originals), \
                        mock.patch.object(TARGET, "load_unbound", return_value=markers), \
                        mock.patch.object(TARGET, "fixed_module", return_value=normalizer), \
                        mock.patch.object(TARGET, "read", return_value=pointer) as read, \
                        mock.patch.object(TARGET, "register_parent"), mock.patch.object(TARGET, "copy_file") as copied, \
                        mock.patch.object(TARGET, "write_json", side_effect=emit):
                    if variant == "valid":
                        TARGET.project(owner, source, Path("core"), manifest, None, None, tools)
                        copied.assert_called_once_with(owner, tools / "rc_win/rc.exe",
                            source / "checkout/src" / TARGET.WINDOWS_RC_PATH,
                            {key: TARGET.WINDOWS_RC[key] for key in ("bytes", "sha256")})
                        self.assertEqual(emitted, [{"path": TARGET.WINDOWS_RC_PATH, "type": "file", "mode": "100755",
                            **{key: TARGET.WINDOWS_RC[key] for key in ("bytes", "sha256")}}])
                        self.assertEqual(receipts, [TARGET.WINDOWS_RC_RECEIPT])
                    else:
                        with self.assertRaises(TARGET.Refused):
                            TARGET.project(owner, source, Path("core"), manifest, None, None, tools)
                        copied.assert_not_called()
                        self.assertEqual(emitted, [])
                        self.assertEqual(receipts, [])
                read.assert_called_once_with(source / "checkout/src" / (TARGET.WINDOWS_RC_PATH + ".sha1"), 128)

    def test_windows_rc_builder_binds_actual_supplemental_receipt_and_overlay(self):
        builder_path = PATH.with_name("build_native_webrtc_windows.py")
        self.assertEqual(hashlib.sha256(builder_path.read_bytes()).hexdigest(), TARGET.RECIPE)
        spec = importlib.util.spec_from_file_location("windows_rc_actual_builder", builder_path)
        builder = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(builder)
        self.assertEqual(builder.SCHEMA, "morpheus-native-webrtc-windows-inputs-v5")
        self.assertEqual(builder.WINDOWS_RC_RECEIPT, TARGET.WINDOWS_RC_RECEIPT)
        value = copy.deepcopy(TARGET.WINDOWS_RC_RECEIPT)
        pointer, projection = value["source_pointer"], value["projection"]
        native = {pointer["path"]: {key: pointer[key] for key in ("bytes", "sha256")}}
        overlay = [projection]
        self.assertEqual(builder.native_tools_material(value, native, overlay), value)
        self.assertIsNone(builder.native_tools_material(None, native, []))
        for undeclared in (projection, dict(projection, path=projection["path"].upper())):
            with self.subTest(undeclared=undeclared["path"]), self.assertRaises(builder.RecipeError):
                builder.native_tools_material(None, native, [undeclared])
        for variant in ("object", "generation", "profile", "metadata", "extra", "pointer", "absent",
                        "duplicate", "case", "mode", "bytes", "hash", "execution", "license", "numeric_false"):
            changed, originals, rows = copy.deepcopy(value), copy.deepcopy(native), copy.deepcopy(overlay)
            if variant == "object":
                changed["tool"]["source"]["object"] = "foreign/object"
            elif variant == "generation":
                changed["tool"]["source"]["generation"] = "1"
            elif variant in ("profile", "metadata"):
                changed[variant + "_sha256"] = "0" * 64
            elif variant == "extra":
                changed["unreviewed"] = True
            elif variant == "pointer":
                originals[pointer["path"]]["sha256"] = "0" * 64
            elif variant == "absent":
                rows = []
            elif variant == "duplicate":
                rows.append(copy.deepcopy(projection))
            elif variant == "case":
                rows[0]["path"] = projection["path"].upper()
            elif variant in ("mode", "bytes", "hash"):
                key, replacement = {"mode": ("mode", "100644"), "bytes": ("bytes", 1),
                                    "hash": ("sha256", "0" * 64)}[variant]
                rows[0][key] = replacement
            elif variant == "execution":
                changed["executed_downloaded_tools"] = ["rc_win"]
            elif variant == "numeric_false":
                changed["license_compliance_verified"] = 0
            else:
                changed["license_compliance_verified"] = True
            with self.subTest(variant=variant), self.assertRaises(builder.RecipeError):
                builder.native_tools_material(changed, originals, rows)

    def test_producer_failure_categories_are_closed_and_keep_original_failure_fatal(self):
        # Actual core_stage exception propagation and CLI catch boundary. No
        # source/host/network/tool execution is admitted by these fake inputs.
        private = "private error payload must never reach diagnostics"
        value = {"baseline": TARGET.NATIVE, "tree": TARGET.TREE,
                 "targets": {"Windowsx64": [{"name": name} for name in ("gn", "ninja", "clang")]}}
        control = mock.Mock(root=Path("synthetic-control"))
        for helper_phase, expected in (("admission", "core_admission"),
                ("download_gn", "core_download_gn"), ("stage_clang", "core_stage_clang"),
                ("receipt", "core_receipt"), ("unknown", "core_invoke"), (private, "core_invoke"),
                (None, "core_invoke"), ("x" * 10000, "core_invoke")):
            helper = mock.Mock(PHASE=helper_phase)
            def fail_core():
                TARGET.core_stage(helper, Path("synthetic-temp"), Path("synthetic-workspace"),
                                  {"core_manifest": {"sha256": TARGET.CORE}}, control)
            output = io.StringIO()
            with mock.patch.object(TARGET, "FAILURE_CATEGORY", "entry"), \
                    mock.patch.dict(os.environ, GITHUB_RUN_ID="1", GITHUB_RUN_ATTEMPT="1", GITHUB_JOB="producer"), \
                    mock.patch.object(TARGET, "bound_asset", return_value=(Path("synthetic-manifest"), value)), \
                    mock.patch.object(TARGET, "write_json", return_value={"sha256": "a" * 64}), \
                    mock.patch.object(TARGET, "invoke", side_effect=RuntimeError(private)) as invoke, \
                    mock.patch.object(TARGET, "main", side_effect=fail_core), \
                    mock.patch.object(TARGET.sys, "stderr", output):
                with self.assertRaises(SystemExit) as failure:
                    TARGET.entry()
                self.assertEqual(failure.exception.code, 1)
                invoke.assert_called_once()
            self.assertEqual(output.getvalue(),
                             "MORPHEUS_NATIVE_WINDOWS_PRODUCER failed category=" + expected + "\n")
        # Even a future non-source category cannot become an output field.
        output = io.StringIO()
        with mock.patch.object(TARGET, "FAILURE_CATEGORY", private), \
                mock.patch.object(TARGET, "main", side_effect=RuntimeError(private)), \
                mock.patch.object(TARGET.sys, "stderr", output):
            with self.assertRaises(SystemExit) as failure:
                TARGET.entry()
            self.assertEqual(failure.exception.code, 1)
        self.assertEqual(output.getvalue(), "MORPHEUS_NATIVE_WINDOWS_PRODUCER failed category=entry\n")


    def test_genuine_windows_source_profile_matches_actual_restricted_deps_selection(self):
        assets = PATH.parents[1] / "native/webrtc/windows"
        profile = json.loads((assets / "source-profile.json").read_bytes())
        def bound(record):
            raw = (PATH.parents[1] / record["path"]).read_bytes()
            self.assertEqual(len(raw), record["bytes"])
            self.assertEqual(hashlib.sha256(raw).hexdigest(), record["sha256"])
            return json.loads(raw)
        plan = bound(profile["source_plan"])
        interface = bound(profile["producer_interface"])
        self.assertEqual(profile["producer_interface"]["sha256"], TARGET.SOURCE_INTERFACE)
        self.assertEqual(interface["source_plan_schema"], plan["schema"])
        self.assertEqual(plan["schema"], "morpheus-native-webrtc-sources-v3")
        self.assertEqual(plan["target"], "win-x64")
        helper_path = PATH.parent / "native_webrtc/git_source_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["git_source_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_actual_source_deps", helper_path)
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        context = dict(host_os="win", host_cpu="x64", checkout_win=True,
                       checkout_linux=False, checkout_mac=False,
                       checkout_android=False, checkout_ios=False, **plan["condition_values"])
        selected, declarations = [], []
        names = {"src": "root-DEPS", "src/buildtools": "buildtools-DEPS"}
        self.assertEqual({row["mount"] for row in plan["deps_files"]}, set(names))
        for row in plan["deps_files"]:
            raw = (assets / "reference" / names[row["mount"]]).read_bytes()
            self.assertEqual(len(raw), row["bytes"])
            self.assertEqual(hashlib.sha256(raw).hexdigest(), row["sha256"])
            parser = helper.Deps(raw, context, row["mount"] + "/DEPS")
            chosen, records, _ = parser.dependencies(row["mount"])
            selected.extend(chosen)
            declarations.extend(records)
        planned = {row["mount"]: row for row in plan["repositories"]}
        self.assertEqual(len(planned), 56)
        self.assertEqual(len(declarations), 127)
        self.assertEqual({row["mount"]: row for row in selected},
                         {mount: {key: row[key] for key in ("mount", "url", "revision")}
                          for mount, row in planned.items() if mount not in ("src", "outer-depot")})
        self.assertNotIn("src/third_party/instrumented_libs", planned)
        self.assertNotIn("src/third_party/fontconfig/src", planned)
        self.assertNotIn("src/third_party/lss", planned)
        self.assertEqual(len(plan["gitlinks"]), 22)
        self.assertEqual(len(plan["aliases"]), 69)
        manifest = bound(profile["patch_manifest"])
        self.assertEqual(plan["patch_manifest"]["sha256"], profile["patch_manifest"]["sha256"])
        self.assertEqual([row["sha256"] for row in manifest["patches"][:8]],
                         helper.SECURITY_PATCHES)
        self.assertEqual([(row["sha256"], row["root"]) for row in manifest["patches"][8:]],
                         [helper.PLATFORM_PATCHES[name] for name in helper.PLATFORM_ORDER["win"]])
        self.assertEqual(len(manifest["patches"]), 13)


    def test_windows_original_cipd_instance_path_and_both_hashes_are_required(self):
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(), TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_cipd_actual_transport", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        profile = json.loads((PATH.parents[1] / "native/webrtc/windows/runtime-data.json").read_bytes())
        actual = [item["tool"] for item in profile["materials"] if item["tool"]["name"] in TARGET.WINDOWS_CIPD_SHA1]
        self.assertEqual(len(actual), 3)
        for tool in actual:
            self.assertEqual(TARGET.windows_cipd_instance(tool)["hashAlgo"], "SHA1")
            for changed in (dict(tool, sha256="0" * 64), dict(tool, bytes=tool["bytes"] + 1),
                            dict(tool, source=dict(tool["source"], package="foreign/wheel"))):
                with self.assertRaises(TARGET.Refused):
                    TARGET.windows_cipd_instance(changed)
        payload = b"opaque synthetic archive DATA"
        sha1 = hashlib.sha1(payload).hexdigest()
        tool = dict(actual[0], bytes=len(payload), sha256=hashlib.sha256(payload).hexdigest())
        # A controlled fixture pin exercises the actual request and hashing
        # functions. Production pin identities are checked against real profile
        # rows above; no network/archive/tool execution or fake hash is used.
        pin = {key: tool[key] for key in ("name", "format", "bytes", "sha256", "source")}
        pin["instance_sha1"] = sha1
        signed = "https://storage.googleapis.com/chrome-infra-packages/store/SHA1/" + sha1 + "?fixture=1"
        for variant in ("valid", "corrupt", "surplus", "wrong_algorithm", "foreign_host", "duplicate", "length"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temporary:
                lookup_connection, archive_connection = mock.Mock(), mock.Mock()
                lookup, response = mock.Mock(), mock.Mock()
                url = signed.replace("/SHA1/", "/SHA256/") if variant == "wrong_algorithm" else signed
                if variant == "foreign_host":
                    url = url.replace("storage.googleapis.com", "foreign.invalid")
                metadata = b")]}'\n" + json.dumps({"signedUrl": url}).encode()
                if variant == "duplicate":
                    metadata = b")]}'\n{\"signedUrl\":\"" + signed.encode() + b"\",\"signedUrl\":\"" + signed.encode() + b"\"}"
                lookup.read.return_value = metadata
                content = b"x" * len(payload) if variant == "corrupt" else payload + (b"x" if variant == "surplus" else b"")
                response.read.side_effect = io.BytesIO(content).read
                response.getheader.side_effect = lambda name: (str(len(payload) + (1 if variant == "length" else 0))
                                                               if name == "Content-Length" else None)
                owner = mock.Mock()
                owner.create.side_effect = lambda destination: destination.open("xb")
                destination = Path(temporary) / "original.zip"
                with mock.patch.dict(TARGET.WINDOWS_CIPD_SHA1, {tool["name"]: pin}), \
                        mock.patch.object(stager, "https", side_effect=[(lookup_connection, lookup),
                                                                     (archive_connection, response)]) as transport:
                    if variant == "valid":
                        TARGET.windows_cipd_download(stager, owner, tool, destination)
                        self.assertEqual(destination.read_bytes(), payload)
                    else:
                        with self.assertRaises((TARGET.Refused, stager.Refusal)):
                            TARGET.windows_cipd_download(stager, owner, tool, destination)
                first = transport.call_args_list[0]
                self.assertEqual(first.args[0], "https://chrome-infra-packages.appspot.com/prpc/cipd.Repository/GetInstanceURL")
                self.assertEqual(json.loads(first.kwargs["body"]), {"package": tool["source"]["package"],
                    "instance": {"hashAlgo": "SHA1", "hexDigest": sha1}})
                lookup_connection.close.assert_called_once_with()
                if variant in ("wrong_algorithm", "foreign_host", "duplicate"):
                    self.assertEqual(transport.call_count, 1)
                    self.assertFalse(destination.exists())
                else:
                    self.assertEqual(transport.call_count, 2)
                    self.assertEqual(transport.call_args_list[1].args[0], signed)
                    archive_connection.close.assert_called_once_with()

    def test_complete_bound_windows_runtime_profile_reaches_actual_35_material_consumer(self):
        plan_path = PATH.parents[1] / "native/webrtc/windows/pipeline-plan.json"
        plan = json.loads(plan_path.read_bytes())
        record = plan["runtime_profile"]
        raw = (PATH.parents[1] / record["path"]).read_bytes()
        self.assertEqual({"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()},
                         {key: record[key] for key in ("bytes", "sha256")})
        profile = json.loads(raw)
        self.assertEqual(len(profile["materials"]), 35)
        source_record = plan["source_profile"]
        self.assertEqual(source_record["path"], "native/webrtc/windows/source-profile.json")
        source_raw = (PATH.parents[1] / source_record["path"]).read_bytes()
        self.assertEqual({"bytes": len(source_raw), "sha256": hashlib.sha256(source_raw).hexdigest()},
                         {key: source_record[key] for key in ("bytes", "sha256")})
        self.assertIsNone(plan["build_inputs"])
        self.assertIs(plan["cache_handoff_authorized"], False)
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(), TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_35_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        control = mock.Mock(root=Path("synthetic-windows-runtime"))
        # Run original admission, table and source checks; no network, archive
        # extraction, CIPD/vpython environment, executable tool or build runs.
        with mock.patch.object(TARGET, "bound_asset", return_value=(None, profile)), \
                mock.patch.object(stager, "download") as downloads, \
                mock.patch.object(TARGET, "windows_cipd_download") as original_instances, \
                mock.patch.object(stager, "extract") as ordinary, \
                mock.patch.object(TARGET, "windows_python_zip_extract") as python_zip, \
                mock.patch.object(TARGET, "windows_rust_extract") as rust, \
                mock.patch.object(TARGET, "write_json") as receipt:
            TARGET.runtime_stage(stager, Path("synthetic-workspace"), plan, control)
        self.assertEqual(downloads.call_count, 32)
        self.assertEqual(original_instances.call_count, 3)
        paths = {call.args[2] for call in downloads.call_args_list}
        paths.update(call.args[3] for call in original_instances.call_args_list)
        self.assertEqual(len(paths), 35)
        for call in original_instances.call_args_list:
            instance = TARGET.windows_cipd_instance(call.args[2])
            self.assertEqual(instance["hashAlgo"], "SHA1")
            self.assertRegex(instance["hexDigest"], r"^[0-9a-f]{40}$")
        self.assertEqual(ordinary.call_count, 32)
        self.assertEqual(python_zip.call_count, 2)
        self.assertEqual(rust.call_count, 1)
        self.assertEqual({call.args[2]["name"] for call in python_zip.call_args_list}, {"cpython", "vpython"})
        self.assertIs(receipt.call_args.args[2]["build_ready"], False)
        self.assertIs(receipt.call_args.args[2]["genuine_vpython_realized"], False)
        self.assertEqual(receipt.call_args.args[2]["tools_executed"], [])
        for item in profile["materials"]:
            if item["role"] not in TARGET.WINDOWS_PYTHON_ZIPS:
                continue
            tool = item["tool"]
            table = TARGET.windows_python_zip_members(stager, tool)
            self.assertEqual(len(table), len(tool["members"]))
            with self.assertRaises(stager.Refusal):
                stager.check_members(tool)
            for changed in (dict(tool, sha256="0" * 64),
                            dict(tool, source=dict(tool["source"], package="foreign/python"))):
                with self.assertRaises(TARGET.Refused):
                    TARGET.windows_python_zip_members(stager, changed)
            for name in ("CON .txt", "../escape name", " parent/file", "parent/file ", "bad\tname"):
                changed = copy.deepcopy(tool)
                changed["members"][0]["path"] = name
                with self.subTest(name=name), self.assertRaises((TARGET.Refused, stager.Refusal)):
                    TARGET.windows_python_zip_members(stager, changed)
            changed = copy.deepcopy(tool)
            changed["members"][0]["sha256"] = "0" * 64
            with self.assertRaises(TARGET.Refused):
                TARGET.windows_python_zip_members(stager, changed)

    def test_windows_python_zip_preserves_all_seven_original_spaced_spellings(self):
        plan = json.loads((PATH.parents[1] / "native/webrtc/windows/pipeline-plan.json").read_bytes())
        profile = json.loads((PATH.parents[1] / plan["runtime_profile"]["path"]).read_bytes())
        names = [row["path"] for item in profile["materials"]
                 if item["role"] in TARGET.WINDOWS_PYTHON_ZIPS
                 for row in item["tool"]["members"] if " " in row["path"]]
        self.assertEqual(len(names), 7)
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(), TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_python_zip_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        for bad_mode in (False, True):
            with self.subTest(bad_mode=bad_mode), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                root.chmod(0o700)
                archive, output = root / "synthetic.zip", root / "output"
                table, payloads = {}, {}
                with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_STORED) as target:
                    for index, name in enumerate(names):
                        data = ("synthetic original path DATA " + str(index)).encode("ascii")
                        entry = zipfile.ZipInfo(name)
                        entry.create_system = 3
                        entry.external_attr = (stat.S_IFREG | (0o600 if bad_mode else 0o400)) << 16
                        target.writestr(entry, data)
                        payloads[name] = data
                        table[name] = {"path": name, "kind": "regular", "bytes": len(data), "mode": 0o400,
                                       "output_mode": 0o600, "sha256": hashlib.sha256(data).hexdigest()}
                stage = object.__new__(stager.Stage)
                stage.root, stage.files, stage.dirs = output, [], []
                stage.windows = stager.WindowsDirectories() if os.name == "nt" else None
                stage.held, stage.identities = [], {}
                stage.mkdir(output)
                try:
                    # Full immutable archive/table admission is exercised above.
                    # This synthetic ZIP isolates the real header/write/parent
                    # boundary with original spellings and synthetic payloads.
                    with mock.patch.object(TARGET, "windows_python_zip_members", return_value=table):
                        if bad_mode:
                            with self.assertRaises(TARGET.Refused):
                                TARGET.windows_python_zip_extract(stager, stage,
                                    {"bytes": archive.stat().st_size}, archive, output)
                        else:
                            TARGET.windows_python_zip_extract(stager, stage,
                                {"bytes": archive.stat().st_size}, archive, output)
                            self.assertEqual({p.relative_to(output).as_posix() for p in output.rglob("*") if p.is_file()},
                                             set(names))
                            for name, data in payloads.items():
                                self.assertEqual(stager.snapshot(output / name, stager.MAX_FILE), data)
                finally:
                    stage.finish(False)

    def test_windows_rust_oversize_exception_is_exact_and_download_bucket_is_fixed(self):
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_rust_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        tool = copy.deepcopy(TARGET.WINDOWS_RUST) | {
            "alias_copies": [], "omitted_aliases": [], "omission_evidence": None,
            "members": [copy.deepcopy(TARGET.WINDOWS_RUST_PDB)]}
        pdb = copy.deepcopy(TARGET.WINDOWS_RUST_PDB)
        self.assertEqual(TARGET.windows_rust_member_limit(stager, tool, pdb), 556744704)
        ordinary = dict(pdb, path="bin/ordinary.pdb", raw_path="bin/ordinary.pdb", bytes=512 * 1024 ** 2)
        self.assertEqual(TARGET.windows_rust_member_limit(stager, tool, ordinary), stager.MAX_FILE)
        for field, value in (("path", "bin/other.pdb"), ("raw_path", "./" + pdb["path"]),
                             ("bytes", pdb["bytes"] + 1), ("mode", 0o777),
                             ("output_mode", 0o700), ("sha256", "0" * 64), ("kind", "symlink")):
            with self.subTest(member_field=field), self.assertRaises(TARGET.Refused):
                TARGET.windows_rust_member_limit(stager, tool, dict(pdb, **{field: value}))
        for field, value in (("name", "clang"), ("format", "zip"), ("bytes", tool["bytes"] - 1),
                             ("sha256", "0" * 64), ("source", dict(tool["source"], generation=1)),
                             ("source", dict(tool["source"], object="Linux_x64/other.tar.xz")),
                             ("source", dict(tool["source"], bucket="foreign"))):
            with self.subTest(archive_field=field), self.assertRaises(TARGET.Refused):
                TARGET.windows_rust_member_limit(stager, dict(tool, **{field: value}), pdb)
        # The ordinary stager remains capped and a partial/altered inventory
        # cannot gain the special extraction path even with the original PDB.
        with self.assertRaises(stager.Refusal):
            stager.check_members(tool)
        with self.assertRaises(TARGET.Refused):
            TARGET.windows_rust_members(stager, tool)
        original_source = dict(tool["source"])
        with mock.patch.object(stager, "download") as download:
            TARGET.windows_rust_download(stager, "owner", tool, Path("owned-rust.tar.xz"))
        submitted = download.call_args.args[1]
        self.assertEqual(submitted["source"], original_source | {"bucket": "chromium-browser-clang"})
        self.assertEqual(tool["source"], original_source)
        self.assertEqual({key: value for key, value in submitted.items() if key != "source"},
                         {key: value for key, value in tool.items() if key != "source"})

    def test_windows_rust_member_uses_bounded_chunks_and_streamed_stable_rehash(self):
        helper_path = PATH.parent / "native_webrtc/python_stager.py"
        self.assertEqual(hashlib.sha256(helper_path.read_bytes()).hexdigest(),
                         TARGET.HELPERS["python_stager.py"])
        spec = importlib.util.spec_from_file_location("windows_rust_stream_actual_stager", helper_path)
        stager = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(stager)
        tool = copy.deepcopy(TARGET.WINDOWS_RUST) | {
            "alias_copies": [], "omitted_aliases": [], "omission_evidence": None}
        payload = b"synthetic Rust DATA\0" * 65537
        row = {"path": "bin/synthetic.pdb", "raw_path": "bin/synthetic.pdb", "kind": "regular",
               "bytes": len(payload), "mode": 0o666, "output_mode": 0o600,
               "sha256": hashlib.sha256(payload).hexdigest()}
        class Fragmented(io.BytesIO):
            def __init__(self, data):
                super().__init__(data)
                self.requests = []
            def read(self, size=-1):
                self.requests.append(size)
                self.assert_bounded(size)
                return super().read(min(size, 65536))
            @staticmethod
            def assert_bounded(size):
                if not 0 < size <= 1024 * 1024:
                    raise AssertionError("unbounded member read")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owner = mock.Mock()
            owner.create.side_effect = lambda destination: destination.open("xb")
            # Test the real stable streaming fact() on POSIX too. Only its
            # independent Windows drive-path gate is replaced for this fixture.
            with mock.patch.object(TARGET, "path", side_effect=lambda value: Path(value)), \
                    mock.patch.object(stager, "snapshot", side_effect=AssertionError("full-member allocation")):
                stream = Fragmented(payload)
                destination = root / "original.pdb"
                TARGET.windows_rust_write_member(stager, owner, destination, stream, tool, row)
                self.assertEqual(TARGET.fact(destination), {key: row[key] for key in ("bytes", "sha256")})
                self.assertGreater(len(stream.requests), 2)
                for label, data, selected in (("truncated", payload[:-1], row),
                                               ("surplus", payload + b"x", row),
                                               ("hash", payload, dict(row, sha256="0" * 64))):
                    with self.subTest(label=label), self.assertRaises(TARGET.Refused):
                        TARGET.windows_rust_write_member(stager, owner, root / (label + ".pdb"),
                                                         Fragmented(data), tool, selected)
                oversized = dict(row, bytes=stager.MAX_FILE + 1)
                with self.assertRaises(TARGET.Refused):
                    TARGET.windows_rust_write_member(stager, owner, root / "foreign.pdb",
                                                     Fragmented(b""), tool, oversized)
                self.assertFalse((root / "foreign.pdb").exists())

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
                    mock.patch.object(TARGET, "windows_cipd_download") as original_instances, \
                    mock.patch.object(stager, "extract") as extract, \
                    mock.patch.object(TARGET, "write_json"):
                TARGET.runtime_stage(stager, Path("synthetic-workspace"), plan, control)
                return download.call_args_list, original_instances.call_args_list, extract.call_args_list
        downloads, original_instances, extractions = consume(profile)
        self.assertEqual(len(downloads), 2)
        self.assertEqual(len(original_instances), 1)
        paths = {call.args[2] for call in downloads}
        paths.update(call.args[3] for call in original_instances)
        self.assertEqual(len(paths), 3)
        self.assertEqual(original_instances[0].args[2], materials[0]["tool"])
        self.assertEqual(TARGET.windows_cipd_instance(original_instances[0].args[2]),
                         {"hashAlgo": "SHA1", "hexDigest": "e00dc04ad5029579cffc146324c365377f8bcd1d"})
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
        normalizer = mock.Mock()
        normalizer.verify_source_aliases.return_value = []
        with mock.patch.object(TARGET, "tree_files", return_value=[]), \
                mock.patch.object(TARGET, "load_unbound", return_value=[]), \
                mock.patch.object(TARGET, "fixed_module", return_value=normalizer) as module, \
                mock.patch.object(TARGET, "register_parent"), mock.patch.object(TARGET, "copy_file") as copy, \
                mock.patch.object(TARGET, "write_json", side_effect=lambda owner, path, rows: emitted.extend(rows)) as written:
            TARGET.project(owner, source, core, core_manifest, runtime, runtime_profile)
        module.assert_called_once_with("git_source_stager.py", TARGET.HELPERS["git_source_stager.py"])
        self.assertEqual(normalizer.verify_source_aliases.call_args_list, [mock.call(source), mock.call(source)])
        self.assertEqual(copy.call_count, 2)
        written.assert_called_once_with(owner, source / "materials/windows-source-overlay.json", emitted)
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
            source_plan = asset("source-plan.json", {"schema": "morpheus-native-webrtc-sources-v3",
                "target": "win-x64", "git": {"path": "GIT_UNBOUND"},
                "patch_manifest": {"sha256": manifest["sha256"]},
                "security_plan": {"sha256": security["sha256"]}})
            interface = asset("interface.json", {"schema": "morpheus-native-source-producer-interface-v2",
                "source_plan_schema": "morpheus-native-webrtc-sources-v3",
                "required_security_plan_sha256": security["sha256"],
                "source_git_administrative_metadata": False})
            git = workspace / "git.exe"
            git.write_bytes(b"synthetic tool DATA only")
            git_fact = {"path": str(git), "bytes": git.stat().st_size,
                        "sha256": hashlib.sha256(git.read_bytes()).hexdigest(), "version": "synthetic"}
            profile = asset("profile.json", {"schema": "morpheus-windows-source-stage-v2",
                "target": "win-x64", "source_plan": source_plan, "patch_manifest": manifest,
                "security_plan": security, "patch_files": patches, "host_git": git_fact,
                "producer_interface": interface})
            bad_profiles = []
            original_plan = json.loads((workspace / source_plan["path"]).read_bytes())
            for label, changed in (
                    ("foreign-target", dict(original_plan, target="linux-x64")),
                    ("patch-binding", dict(original_plan, patch_manifest={"sha256": "0" * 64})),
                    ("security-binding", dict(original_plan, security_plan={"sha256": "0" * 64}))):
                bad = json.loads((workspace / profile["path"]).read_bytes())
                bad["source_plan"] = asset(label + "-plan.json", changed)
                bad_profiles.append(asset(label + "-profile.json", bad))
            bad = json.loads((workspace / profile["path"]).read_bytes())
            bad["patch_files"] = patches + patches[:1]
            bad_profiles.append(asset("duplicate-patch-profile.json", bad))
            bad = json.loads((workspace / profile["path"]).read_bytes())
            bad["producer_interface"] = asset("foreign-interface.json", {
                "schema": "foreign", "source_plan_schema": "morpheus-native-webrtc-sources-v3",
                "required_security_plan_sha256": security["sha256"],
                "source_git_administrative_metadata": False})
            bad_profiles.append(asset("foreign-interface-profile.json", bad))
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
                    mock.patch.object(TARGET, "SOURCE_INTERFACE", interface["sha256"]), \
                    mock.patch.object(TARGET, "fixed_module", return_value=object()), \
                    mock.patch.object(TARGET, "invoke", side_effect=exporter):
                for bad_profile in bad_profiles:
                    with self.assertRaises(TARGET.Refused):
                        TARGET.source_stage(None, runner, workspace,
                                            {"source_profile": bad_profile}, control)
                    self.assertEqual(list(control.root.iterdir()), [])
                    self.assertEqual(observed, [])
                output = TARGET.source_stage(None, runner, workspace, {"source_profile": profile}, control)
            self.assertEqual(observed, [True])
            self.assertEqual(output, control.root / "source-provision")
            self.assertEqual(original_files,
                             {p: p.read_bytes() for p in workspace.rglob("*") if p.is_file()})

    def test_workflow_supplies_fixed_absolute_plan_and_entry_keeps_path_policy(self):
        workflow = PATH.parents[1] / ".github/workflows/native-webrtc-windows-source.yml"
        source = workflow.read_text()
        self.assertEqual([line.strip() for line in source.splitlines()
                          if line.lstrip().startswith("runs-on:")], ["runs-on: windows-2022"])
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

    if os.name == "nt":
        def test_actual_entry_wrapped_file_and_raw_directory_ancestry(self):
            # This regression requires the fresh affected Windows host. It
            # invokes only reviewed entry ancestry on generated temporary data.
            self.assertEqual(os.environ.get("GITHUB_ACTIONS"), "true")
            self.assertEqual(os.environ.get("RUNNER_ENVIRONMENT"), "github-hosted")
            workflow = (PATH.parents[1] / ".github/workflows/native-webrtc-windows-source.yml").read_text()
            start = workflow.index("            while ($null -ne $parent) {")
            end = workflow.index("            $entryFailure = 'python_identity'", start)
            walk = workflow[start:end]
            self.assertEqual(walk.count("while ($null -ne $parent) {"), 1)
            walk = walk.replace("while ($null -ne $parent) {",
                                "while ($null -ne $parent) {\n              $visited.Add($parent.FullName)", 1)
            script = r"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
try {
    $root = $env:MORPHEUS_ANCESTRY_ROOT
    $directory = Join-Path $root 'actual\nested'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $file = Join-Path $directory 'synthetic.bin'
    [IO.File]::WriteAllBytes($file, [byte[]]@(1,2,3,4))
    $item = Get-Item -LiteralPath $file -Force
    if ($item -isnot [IO.FileInfo] -or $item.PSIsContainer) { throw 'wrapped' }
    $raw = $item.Directory
    if ($raw -isnot [IO.DirectoryInfo] -or $null -ne $raw.PSObject.Properties['PSIsContainer']) { throw 'raw' }
    $visited = [Collections.Generic.List[string]]::new()
    $parent = $item
    $walk = [ScriptBlock]::Create($env:MORPHEUS_ENTRY_WALK)
    . $walk
    if ($null -ne $parent -or $visited.Count -lt 3 -or $visited[0] -cne $item.FullName -or
        $visited[1] -cne $raw.FullName -or $visited[$visited.Count - 1] -cne $raw.Root.FullName) { throw 'ancestry' }
    # A genuine temporary junction must still trigger the unchanged explicit
    # reparse refusal, rather than an absent-property/other incidental error.
    $alias = Join-Path $root 'alias'
    New-Item -ItemType Junction -Path $alias -Target $directory | Out-Null
    $item = Get-Item -LiteralPath (Join-Path $alias 'synthetic.bin') -Force
    $parent = $item
    $visited.Clear()
    $refused = $false
    try { . $walk } catch { if ($_.Exception.Message -ceq 'python') { $refused = $true } else { throw 'unexpected' } }
    if (-not $refused) { throw 'reparse' }
    Write-Output 'MORPHEUS_WINDOWS_ANCESTRY_OK'
} catch { Write-Output 'MORPHEUS_WINDOWS_ANCESTRY_FAILED'; exit 1 }
"""
            pwsh = Path(os.environ["ProgramFiles"]) / "PowerShell/7/pwsh.exe"
            with tempfile.TemporaryDirectory() as temporary:
                env = os.environ.copy()
                env["MORPHEUS_ANCESTRY_ROOT"] = temporary
                env["MORPHEUS_ENTRY_WALK"] = walk
                result = subprocess.run([str(pwsh), "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script],
                    text=True, encoding="utf-8", capture_output=True, timeout=45, env=env)
            self.assertEqual(result.returncode, 0, "fixed entry ancestry regression failed")
            self.assertEqual(result.stdout.strip(), "MORPHEUS_WINDOWS_ANCESTRY_OK")


        def test_host_inventory_actual_read_handles_paths_and_limits(self):
            # Windows-only native reader: load reviewed function ASTs, never
            # invoke the host-selection entry or touch installed SDK/tool roots.
            script = r"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$caseStage = 'source_parse'
try {
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($env:MORPHEUS_HOST_READER_SOURCE, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) { throw 'source_parse' }
$caseStage = 'source_functions'
$definitions = $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $true)
# Function declarations must remain in this scope for the actual calls below.
$definitionText = ($definitions | ForEach-Object { $_.Extent.Text }) -join "`n"
$definitionLineLimit = $definitionText.Split([char]10).Length
. ([ScriptBlock]::Create($definitionText))
function Must-Refuse([ScriptBlock] $Action) {
    $refused = $false
    try { & $Action | Out-Null } catch { $refused = $true }
    if (-not $refused) { throw 'accepted' }
}
$caseStage = 'failure_context'
foreach ($phase in @('initialization', 'bootstrap', 'vs_inventory', 'sdk_inventory',
    'role_binding', 'final_identity', 'serialization')) {
    $message = @(Get-HostPreflightFailureMessage $phase)
    if ($message.Count -ne 1 -or $message[0] -cne ('MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=' + $phase)) {
        throw 'failure_context'
    }
}
foreach ($value in @($null, '', 'BOOTSTRAP', "private`npath token payload", ('private' * 1000),
    ('boot' + [char]0 + 'strap'), ('boot' + [char]0x200b + 'strap'),
    @{ path = 'private'; error = 'private' }, [pscustomobject]@{ value = 'private' })) {
    $message = @(Get-HostPreflightFailureMessage $value)
    if ($message.Count -ne 1 -or $message[0] -cne 'MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=unknown') {
        throw 'failure_context'
    }
}
foreach ($budget in @('deadline', 'entries', 'bytes', 'handles', 'metadata')) {
    $message = @(Get-HostPreflightFailureMessage 'vs_inventory' $budget)
    if ($message.Count -ne 1 -or $message[0] -cne
        ('MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=vs_inventory budget=' + $budget)) {
        throw 'failure_context'
    }
}
foreach ($value in @($null, '', 'BYTES', "private`npath token payload", ('private' * 1000),
    ('by' + [char]0 + 'tes'), ('by' + [char]0x200b + 'tes'),
    @{ path = 'private'; error = 'private' }, [pscustomobject]@{ value = 'private' })) {
    $message = @(Get-HostPreflightFailureMessage 'vs_inventory' $value)
    if ($message.Count -ne 1 -or $message[0] -cne
        'MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=vs_inventory budget=unknown') {
        throw 'failure_context'
    }
}
$caseStage = 'metadata'
$metadata = @{ run_id = '1'; run_attempt = '1'; source_commit = ('a' * 40);
    image_os = 'win25'; image_version = '20261026.1.0'; powershell_version = '7.5.3' }
if ((Get-HostReceiptMetadata $metadata).run_id -cne '1') { throw 'metadata' }
foreach ($bad in @('private value', ('x' * 65), "line`nbreak")) {
    $changed = $metadata.Clone(); $changed.image_version = $bad
    Must-Refuse { Get-HostReceiptMetadata $changed }
}
$changed = $metadata.Clone(); $changed.extra = 'unrequested'
Must-Refuse { Get-HostReceiptMetadata $changed }
$caseStage = 'path_refusals'
# The already-created Python-owned temporary root may use short-name ancestors.
# Normalize this fixture input before forming children; the production reader
# still rejects noncanonical spellings and retains every no-follow guard.
$root = [IO.Path]::GetFullPath($env:MORPHEUS_HOST_TEST_ROOT)
foreach ($bad in @('relative', '\\server\share\x', 'C:\x:stream', 'C:\x\..\other', 'C:\x\bad.', 'C:\x\bad ', 'C:\x\CON.txt')) {
    Must-Refuse { Get-CanonicalHostPath $bad }
}
$caseStage = 'synthetic_setup'
$good = Join-Path $root 'good'
[IO.Directory]::CreateDirectory((Join-Path $good 'include')) | Out-Null
[IO.File]::WriteAllBytes((Join-Path $good 'tool.bin'), [byte[]]@(1,2,3,4))
[IO.File]::WriteAllBytes((Join-Path $good 'include\header.h'), [byte[]]@(5))
$caseStage = 'reader_initialize'
Initialize-HostReader
try {
    $ownedMemory = $script:HostInfoMemory
    $ownedScratch = $script:HostInfoScratch
    $ownedMemo = $script:HostCanonicalPaths
    $ownedCanonical = $script:HostCanonicalInvoker
    $ownedChainInvoker = $script:HostChainInvoker
    if ($ownedChainInvoker -isnot [Delegate] -or
        -not [object]::ReferenceEquals($ownedChainInvoker, $script:HostChainInvokerOwner) -or
        -not [object]::ReferenceEquals($script:HostComparisonInvoker, $script:HostChainComparison)) { throw 'identity' }
    if ($ownedMemory -eq [IntPtr]::Zero -or $ownedScratch.Length -ne 52 -or
        $script:HostCanonicalEntryLimit -ne 8192 -or $script:HostCanonicalUnitLimit -ne 1048576 -or
        $script:HostDirectoryChainLimit -ne 512 -or $null -ne $script:HostLastDirectoryPath -or
        $null -ne $script:HostLastDirectoryChain) { throw 'identity' }
    if ($ownedCanonical -isnot [Delegate]) { throw 'identity' }
    # Count actual compiled or original lexical validations, never mock results.
    # Exact repeats alone may hit; full-cache misses still validate every time.
    $script:OriginalCanonicalValidator = ${function:Get-CanonicalHostPath}
    $script:CanonicalValidationCount = 0
    $originalEntryLimit = $script:HostCanonicalEntryLimit
    $originalUnitLimit = $script:HostCanonicalUnitLimit
    Set-Item -Path Function:Get-CanonicalHostPath -Value {
        param([string] $Value)
        $script:CanonicalValidationCount++
        & $script:OriginalCanonicalValidator $Value
    }
    $canonicalProbe = [pscustomobject]@{ Delegate = $ownedCanonical }
    $canonicalProbe | Add-Member -MemberType ScriptMethod -Name Invoke -Value {
        param([string] $Value)
        $validated = $this.Delegate.Invoke($Value)
        if ($validated.Item2 -ne -1) { $script:CanonicalValidationCount++ }
        return $validated
    }
    try {
        $script:HostCanonicalInvoker = $canonicalProbe
        $first = Join-Path $root 'MemoCase'
        $differentCase = Join-Path $root 'memocase'
        $beforeCount = $ownedMemo.Count
        if ((Get-HostCanonicalPath $first) -cne $first -or
            (Get-HostCanonicalPath $first) -cne $first -or $script:CanonicalValidationCount -ne 1 -or
            $ownedMemo.Count -ne $beforeCount + 1) { throw 'identity' }
        if ((Get-HostCanonicalPath $differentCase) -cne $differentCase -or
            $script:CanonicalValidationCount -ne 2 -or $ownedMemo.Count -ne $beforeCount + 2) { throw 'identity' }
        $beforeCount = $ownedMemo.Count
        foreach ($attempt in 1..2) {
            $refusal = ''
            try { Get-HostCanonicalPath 'C:\x:stream' | Out-Null } catch { $refusal = $_.Exception.Message }
            if ($refusal -cne 'path_colon') { throw 'identity' }
        }
        if ($script:CanonicalValidationCount -ne 4 -or $ownedMemo.Count -ne $beforeCount) { throw 'identity' }
        # A full cache revalidates without refusing an otherwise valid path.
        # Only these synthetic limits change; production count/units stay fixed.
        $ownedMemo.Clear(); $script:HostCanonicalUnits = 0
        $script:HostCanonicalEntryLimit = 2
        $third = Join-Path $root 'third'
        $null = Get-HostCanonicalPath $first
        $null = Get-HostCanonicalPath $differentCase
        $beforeValidation = $script:CanonicalValidationCount
        if ((Get-HostCanonicalPath $third) -cne $third -or
            (Get-HostCanonicalPath $third) -cne $third -or $ownedMemo.Count -ne 2 -or
            $script:CanonicalValidationCount -ne $beforeValidation + 2 -or $ownedMemo.ContainsKey($third)) { throw 'identity' }
        $ownedMemo.Clear(); $script:HostCanonicalUnits = 0
        $script:HostCanonicalEntryLimit = $originalEntryLimit
        $script:HostCanonicalUnitLimit = 2 * $first.Length
        $beforeValidation = $script:CanonicalValidationCount
        $null = Get-HostCanonicalPath $first
        $null = Get-HostCanonicalPath $first
        if ($ownedMemo.Count -ne 1 -or $script:HostCanonicalUnits -ne 2 * $first.Length -or
            $script:CanonicalValidationCount -ne $beforeValidation + 1) { throw 'identity' }
        if ((Get-HostCanonicalPath $third) -cne $third -or $ownedMemo.Count -ne 1 -or
            $script:CanonicalValidationCount -ne $beforeValidation + 2) { throw 'identity' }
    } finally {
        $script:HostCanonicalInvoker = $ownedCanonical
        Set-Item -Path Function:Get-CanonicalHostPath -Value $script:OriginalCanonicalValidator
        $script:HostCanonicalEntryLimit = $originalEntryLimit
        $script:HostCanonicalUnitLimit = $originalUnitLimit
        $ownedMemo.Clear(); $script:HostCanonicalUnits = 0
    }
    # The actual compiled entry must preserve original results and first errors.
    # Unicode stays on the exact original body, including cached Regex culture.
    $lexicalProbe = [pscustomobject]@{ Delegate = $ownedCanonical; Calls = 0; Status = 0 }
    $lexicalProbe | Add-Member -MemberType ScriptMethod -Name Invoke -Value {
        param([string] $Value)
        $this.Calls++
        $result = $this.Delegate.Invoke($Value)
        $this.Status = $result.Item2
        return $result
    }
    $priorCulture = [Threading.Thread]::CurrentThread.CurrentCulture
    $lexicalCases = @('', $null, 'C:\', 'c:\normal\name.h', 'relative/:bad',
        'C:\x:stream/*', 'C:\bad/name*', ('C:\bad' + [char]0 + 'name'),
        'C:\part.', 'C:\part ', 'C:\.git', 'C:\.GIT', 'C:\CON', 'C:\nul.txt',
        'C:\com1', 'C:\LPT9.txt', 'C:\COM0', 'C:\COM10', 'C:\CONx',
        'C:\.\name', 'C:\..\name', 'C:\\name', 'C:\name\',
        ('C:\' + ('x' * 1021)), ('C:\' + ('x' * 1022)),
        ('C:\caf' + [char]0xe9 + '\name.h'),
        ('C:\cafe' + [char]0x301 + '\name.h'),
        ('C:\.gi' + [char]0xad + 't'),
        ('C:\.g' + [char]0x131 + 't'),
        ('C:\' + [char]0x130 + '\name.h'),
        ([string][char]0x212a + ':\name.h'),
        ('C:\name.' + [char]0x200b), ('C:\' + [char]0xd800 + '.h'))
    try {
        $script:HostCanonicalInvoker = $lexicalProbe
        $script:HostCanonicalEntryLimit = 0
        $script:HostCanonicalUnitLimit = 0
        foreach ($culture in @('en-US', 'tr-TR', 'ja-JP')) {
            [Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::GetCultureInfo($culture)
            foreach ($candidate in $lexicalCases) {
                $expectedPath = $null; $expectedError = ''; $expectedType = ''
                try { $expectedPath = Get-CanonicalHostPath $candidate } catch {
                    $problem = $_.Exception
                    while ($null -ne $problem.InnerException) { $problem = $problem.InnerException }
                    $expectedError = $problem.Message; $expectedType = $problem.GetType().FullName
                }
                $beforeCalls = $lexicalProbe.Calls
                $actualPath = $null; $actualError = ''; $actualType = ''
                try { $actualPath = Get-HostCanonicalPath $candidate } catch {
                    $problem = $_.Exception
                    while ($null -ne $problem.InnerException) { $problem = $problem.InnerException }
                    $actualError = $problem.Message; $actualType = $problem.GetType().FullName
                }
                if ($lexicalProbe.Calls -ne $beforeCalls + 1 -or
                    -not [string]::Equals($expectedPath, $actualPath, [StringComparison]::Ordinal) -or
                    -not [string]::Equals($expectedError, $actualError, [StringComparison]::Ordinal) -or
                    $expectedType -cne $actualType) { throw 'identity' }
                if ($candidate -ceq 'c:\normal\name.h' -and $lexicalProbe.Status -ne 0) { throw 'identity' }
                if ($null -ne $candidate -and $candidate.Contains([string][char]0xe9) -and
                    $lexicalProbe.Status -ne -1) { throw 'identity' }
            }
        }
        if ($ownedMemo.Count -ne 0 -or $script:HostCanonicalUnits -ne 0) { throw 'identity' }
    } finally {
        [Threading.Thread]::CurrentThread.CurrentCulture = $priorCulture
        $script:HostCanonicalInvoker = $ownedCanonical
        $script:HostCanonicalEntryLimit = $originalEntryLimit
        $script:HostCanonicalUnitLimit = $originalUnitLimit
        $ownedMemo.Clear(); $script:HostCanonicalUnits = 0
    }
    # A second owner cannot replace the original scratch or retained handles.
    Must-Refuse { Initialize-HostReader }
    if ($script:HostInfoMemory -ne $ownedMemory -or
        -not [object]::ReferenceEquals($ownedMemo, $script:HostCanonicalPaths)) { throw 'identity' }
    # Every case omits all later limit properties: evaluating past the first
    # refusal would throw under StrictMode instead of preserving throw 'limit'.
    # Only the scalar byte counter and limit view are synthetic; no handles,
    # native reads or filesystem authorization results are substituted.
    $originalLimits = $script:HostLimits
    $originalBytes = $script:HostBytes
    $script:HostBytes = [uint64]1
    $budgetOrder = @('deadline', 'entries', 'bytes', 'handles', 'metadata')
    try {
        for ($first = 0; $first -lt $budgetOrder.Count; $first++) {
            $values = @{ Seconds = $(if ($first -eq 0) { -1 } else { 180 }) }
            if ($first -ge 1) { $values.Entries = $(if ($first -eq 1) { -1 } else { 200000 }) }
            if ($first -ge 2) { $values.TotalBytes = $(if ($first -eq 2) { 0 } else { 68719476736 }) }
            if ($first -ge 3) { $values.Handles = $(if ($first -eq 3) { -1 } else { 200256 }) }
            if ($first -ge 4) { $values.MetadataBytes = -1 }
            $script:HostLimits = [pscustomobject]$values
            $script:HostPreflightBudgetCause = 'unknown'
            $refusal = ''
            try { Assert-HostBudget } catch { $refusal = $_.Exception.Message }
            if ($refusal -cne 'limit' -or $script:HostPreflightBudgetCause -cne $budgetOrder[$first]) {
                throw 'failure_context'
            }
        }
        # Later refusals cannot replace the first observed budget context.
        $script:HostLimits = [pscustomobject]@{ Seconds = -1 }
        Must-Refuse { Assert-HostBudget }
        if ($script:HostPreflightBudgetCause -cne 'metadata') { throw 'failure_context' }
    } finally {
        $script:HostLimits = $originalLimits
        $script:HostBytes = $originalBytes
        $script:HostPreflightBudgetCause = 'unknown'
    }
    $caseStage = 'inventory'
    $result = Get-HostRootInventory $good
    if ($result.files.Count -ne 2 -or $script:HostBytes -ne 5 -or
        $script:HostPreflightBudgetCause -cne 'unknown') { throw 'inventory' }
    $caseStage = 'digest_assertion'
    $tool = @($result.files | Where-Object { $_.path -eq 'tool.bin' })
    if ($tool.Count -ne 1 -or $tool[0].bytes -ne 4 -or
        $tool[0].sha256 -cne '9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a') { throw 'hash' }
    # Repeated real metadata checks reuse the same owned buffers but retain
    # the old exact bytes-to-hex representation and independent result values.
    $observed = Get-HostHandleInfo $script:HostFileRecords[0].handle
    if ($script:HostInfoMemory -ne $ownedMemory -or
        -not [object]::ReferenceEquals($ownedScratch, $script:HostInfoScratch) -or $script:HostInfoBusy -or
        $observed.write -cne [Convert]::ToHexString([byte[]]($ownedScratch[20..27])) -or
        $observed.identity -cne ([Convert]::ToHexString([byte[]]($ownedScratch[28..31])) +
                               [Convert]::ToHexString([byte[]]($ownedScratch[44..51])))) { throw 'identity' }
    $savedIdentity = $observed.identity
    $comparisonRecord = $script:HostFileRecords[0]
    $savedComparison = [Convert]::ToHexString($comparisonRecord.comparison)
    if ($comparisonRecord.comparison -isnot [byte[]] -or $comparisonRecord.comparison.Length -ne 52 -or
        [object]::ReferenceEquals($comparisonRecord.comparison, $ownedScratch) -or
        $observed.Count -ne 6) { throw 'identity' }
    $null = Get-HostHandleInfo $script:HostFileRecords[1].handle
    if ($observed.identity -cne $savedIdentity -or
        [Convert]::ToHexString($comparisonRecord.comparison) -cne $savedComparison) { throw 'identity' }
    # Cached lexical paths never cache native identity or file-open results.
    # Both calls must reach the real original delegate; the second open must
    # retain another independently checked handle for the same cached path.
    $originalInfo = $script:HostInfo
    $originalType = $script:HostType
    # Real delegates take the emitted production comparison path. Probe only
    # its managed entry; the actual native delegates and owned scratch remain.
    $originalComparison = $script:HostComparisonInvoker
    if ($originalComparison -isnot [Delegate] -or
        -not [object]::ReferenceEquals($originalType, $script:HostComparisonType) -or
        -not [object]::ReferenceEquals($originalInfo, $script:HostComparisonInfo)) { throw 'identity' }
    $comparisonProbe = [pscustomobject]@{ Delegate = $originalComparison; Calls = 0 }
    $comparisonProbe | Add-Member -MemberType ScriptMethod -Name Invoke -Value {
        param([Delegate] $TypeCall, [Delegate] $InfoCall, [IntPtr] $Handle,
              [IntPtr] $Memory, [byte[]] $Scratch, $Expected, [bool] $Content)
        $this.Calls++
        return $this.Delegate.Invoke($TypeCall, $InfoCall, $Handle, $Memory, $Scratch, $Expected, $Content)
    }
    try {
        $script:HostComparisonInvoker = $comparisonProbe
        $beforeChecks = $comparisonProbe.Calls
        $null = Open-HostDirectoryChain $good
        if ($comparisonProbe.Calls -ne $beforeChecks + $script:HostLastDirectoryChain.Length) { throw 'identity' }
        foreach ($content in @($false, $true)) {
            $beforeChecks = $comparisonProbe.Calls
            if (@(Assert-HostIdentity $comparisonRecord $content).Count -ne 0 -or
                $comparisonProbe.Calls -ne $beforeChecks + 1) { throw 'identity' }
        }
        foreach ($offset in @(0, 28, 31, 44, 48, 51, 20, 27, 32, 39, 40, 43)) {
            $altered = [byte[]]$comparisonRecord.comparison.Clone()
            $altered[$offset] = $altered[$offset] -bxor 1
            $changed = @{ handle = $comparisonRecord.handle; comparison = $altered }
            $beforeChecks = $comparisonProbe.Calls
            $refusal = ''
            try { Assert-HostIdentity $changed $true } catch { $refusal = $_.Exception.Message }
            if ($refusal -cne 'changed' -or $comparisonProbe.Calls -ne $beforeChecks + 1) { throw 'identity' }
            if ($offset -in @(20, 27, 32, 39, 40, 43)) {
                Assert-HostIdentity $changed $false
            } else { Must-Refuse { Assert-HostIdentity $changed $false } }
        }
        foreach ($offset in @(4, 11, 12, 19)) {
            $altered = [byte[]]$comparisonRecord.comparison.Clone()
            $altered[$offset] = $altered[$offset] -bxor 1
            Assert-HostIdentity @{ handle = $comparisonRecord.handle; comparison = $altered } $true
        }
        foreach ($invalid in @($null, [byte[]]::new(51), 'private invalid snapshot')) {
            $refusal = ''
            try { Get-HostHandleInfo $comparisonRecord.handle $invalid $true } catch { $refusal = $_.Exception.Message }
            if ($refusal -cne 'identity') { throw 'identity' }
        }
        if ([Convert]::ToHexString($comparisonRecord.comparison) -cne $savedComparison -or
            -not [object]::ReferenceEquals($ownedScratch, $script:HostInfoScratch) -or
            $script:HostInfoBusy) { throw 'identity' }
    } finally { $script:HostComparisonInvoker = $originalComparison }
    # A same-signature delegate probe is not the owned delegate: preserve the
    # original comparison path without calling the probe or skipping ancestors.
    $chainProbe = [Func[object[], Delegate, Delegate, Delegate, IntPtr, byte[], int]] {
        param([object[]] $Records, [Delegate] $Comparison, [Delegate] $TypeCall,
              [Delegate] $InfoCall, [IntPtr] $Memory, [byte[]] $Scratch)
        throw 'accepted'
    }
    try {
        $script:HostChainInvoker = $chainProbe
        $null = Open-HostDirectoryChain $good
    } finally { $script:HostChainInvoker = $ownedChainInvoker }
    # A streamed leaf is observable only after metadata scratch ownership ends.
    $null = Open-HostDirectoryChain $good | ForEach-Object {
        if ($script:HostInfoBusy) { throw 'identity' }
    }
    # Current mutable Expected bytes reach the production managed chain too.
    $realChain = $script:HostLastDirectoryChain
    $realRoot = $realChain[0]
    $realRootExpected = $realRoot.comparison
    try {
        $altered = [byte[]]$realRootExpected.Clone()
        $altered[28] = $altered[28] -bxor 1
        $realRoot.comparison = $altered
        $refusal = ''
        try { $null = Open-HostDirectoryChain $good } catch { $refusal = $_.Exception.Message }
        if ($refusal -cne 'changed' -or $script:HostInfoBusy) { throw 'identity' }
        $realRoot.comparison = $null
        $refusal = ''
        try { $null = Open-HostDirectoryChain $good } catch { $refusal = $_.Exception.Message }
        if ($refusal -cne 'identity' -or $script:HostInfoBusy) { throw 'identity' }
    } finally { $realRoot.comparison = $realRootExpected }
    $realLeaf = $realChain[$realChain.Length - 1]
    $adaptedLeaf = [pscustomobject]$realLeaf
    try {
        $realChain[$realChain.Length - 1] = $adaptedLeaf
        $returnedLeaf = Open-HostDirectoryChain $good
        if (-not [object]::ReferenceEquals($returnedLeaf, $adaptedLeaf) -or $script:HostInfoBusy) { throw 'identity' }
    } finally { $realChain[$realChain.Length - 1] = $realLeaf }
    # A ScriptProperty on an exact Hashtable shadows its same-named key.
    # Metadata admission must not invoke it; original adaptation invokes it
    # exactly once after all prior ancestor checks and before the leaf read.
    $shadowLeaf = [hashtable]$realLeaf.Clone()
    $script:ChainShadowReads = 0
    $script:ChainShadowExpected = [byte[]]$realLeaf.comparison.Clone()
    $script:ChainShadowExpected[28] = $script:ChainShadowExpected[28] -bxor 1
    Add-Member -InputObject $shadowLeaf -MemberType ScriptProperty -Name comparison -Value {
        $script:ChainShadowReads++
        return ,$script:ChainShadowExpected
    } -Force
    try {
        $realChain[$realChain.Length - 1] = $shadowLeaf
        $script:HostInfoBusy = $true
        try {
            if ($ownedChainInvoker.Invoke($realChain, $originalComparison, $originalType,
                $originalInfo, $ownedMemory, $ownedScratch) -ne -$realChain.Length -or
                $script:ChainShadowReads -ne 0) { throw 'identity' }
        } finally { $script:HostInfoBusy = $false }
        $refusal = ''
        try { $null = Open-HostDirectoryChain $good } catch { $refusal = $_.Exception.Message }
        if ($refusal -cne 'changed' -or $script:ChainShadowReads -ne 1 -or $script:HostInfoBusy) { throw 'identity' }
    } finally {
        $realChain[$realChain.Length - 1] = $realLeaf
        $script:ChainShadowExpected = $null
    }
    # Exercise emitted first-failure branches using same-signature managed
    # delegates. No native handle is fabricated or foreign pointer dereferenced.
    $firstFailureTrace = [Collections.Generic.List[string]]::new()
    $typeFailure = [Func[IntPtr, uint32]] { param([IntPtr] $Handle)
        $firstFailureTrace.Add('type'); return [uint32]0
    }
    $typeSuccess = [Func[IntPtr, uint32]] { param([IntPtr] $Handle)
        $firstFailureTrace.Add('type'); return [uint32]1
    }
    $infoFailure = [Func[IntPtr, IntPtr, int]] { param([IntPtr] $Handle, [IntPtr] $Memory)
        $firstFailureTrace.Add('info'); return 0
    }
    $typeThunk = New-HostComparisonInvoker $typeFailure $infoFailure
    if ($typeThunk.Invoke($typeFailure, $infoFailure, [IntPtr]::Zero, [IntPtr]::Zero,
            $ownedScratch, $null, $true) -ne 1 -or
        ($firstFailureTrace -join ',') -cne 'type') { throw 'identity' }
    $firstFailureTrace.Clear()
    $infoThunk = New-HostComparisonInvoker $typeSuccess $infoFailure
    if ($infoThunk.Invoke($typeSuccess, $infoFailure, [IntPtr]::Zero, [IntPtr]::Zero,
            $ownedScratch, $null, $true) -ne 2 -or
        ($firstFailureTrace -join ',') -cne 'type,info') { throw 'identity' }
    # Reparse refuses before an invalid Expected snapshot, after copying the
    # entire owned record into the same observable scratch buffer.
    $reparseInfo = [Func[IntPtr, IntPtr, int]] { param([IntPtr] $Handle, [IntPtr] $Memory)
        $firstFailureTrace.Add('info')
        $record = [byte[]]::new(52); $record[1] = 4
        [Runtime.InteropServices.Marshal]::Copy($record, 0, $Memory, 52)
        return 1
    }
    $firstFailureTrace.Clear()
    $reparseThunk = New-HostComparisonInvoker $typeSuccess $reparseInfo
    if ($reparseThunk.Invoke($typeSuccess, $reparseInfo, [IntPtr]::Zero, $ownedMemory,
            $ownedScratch, $null, $true) -ne 3 -or
        ($firstFailureTrace -join ',') -cne 'type,info' -or $ownedScratch[1] -ne 4) { throw 'identity' }
    $nativeTrace = [Collections.Generic.List[string]]::new()
    $typeProbe = [pscustomobject]@{ Delegate = $originalType; Calls = 0; Trace = $nativeTrace }
    $typeProbe | Add-Member -MemberType ScriptMethod -Name Invoke -Value {
        param([IntPtr] $Handle)
        $this.Calls++; $this.Trace.Add('type')
        return $this.Delegate.Invoke($Handle)
    }
    $infoHandles = [Collections.Generic.List[IntPtr]]::new()
    $infoProbe = [pscustomobject]@{ Delegate = $originalInfo; Calls = 0; Trace = $nativeTrace; Handles = $infoHandles }
    $infoProbe | Add-Member -MemberType ScriptMethod -Name Invoke -Value {
        param([IntPtr] $Handle, [IntPtr] $Memory)
        $this.Calls++; $this.Trace.Add('info'); $this.Handles.Add($Handle)
        return $this.Delegate.Invoke($Handle, $Memory)
    }
    try {
        $script:HostInfo = $infoProbe
        $script:HostType = $typeProbe
        # Repeated comparisons return no copied fact and still perform both
        # real Win32 calls. The original six-field fact remains unchanged.
        foreach ($content in @($false, $true)) {
            $beforeChecks = $infoProbe.Calls
            if (@(Assert-HostIdentity $comparisonRecord $content).Count -ne 0 -or
                $infoProbe.Calls -ne $beforeChecks + 1) { throw 'identity' }
        }
        foreach ($offset in @(0, 28, 31, 44, 48, 51, 20, 27, 32, 39, 40, 43)) {
            $altered = [byte[]]$comparisonRecord.comparison.Clone()
            $altered[$offset] = $altered[$offset] -bxor 1
            $changed = @{ handle = $comparisonRecord.handle; comparison = $altered }
            Must-Refuse { Assert-HostIdentity $changed $true }
            if ($offset -in @(20, 27, 32, 39, 40, 43)) {
                Assert-HostIdentity $changed $false
            } else { Must-Refuse { Assert-HostIdentity $changed $false } }
        }
        # Creation/access times were never part of the old identity guard.
        foreach ($offset in @(4, 11, 12, 19)) {
            $altered = [byte[]]$comparisonRecord.comparison.Clone()
            $altered[$offset] = $altered[$offset] -bxor 1
            Assert-HostIdentity @{ handle = $comparisonRecord.handle; comparison = $altered } $true
        }
        foreach ($invalid in @($null, [byte[]]::new(51), 'private invalid snapshot')) {
            Must-Refuse { Get-HostHandleInfo $comparisonRecord.handle $invalid $true }
        }
        if ($comparisonRecord.initial.identity -cne $observed.identity -or
            [Convert]::ToHexString($comparisonRecord.comparison) -cne $savedComparison) { throw 'identity' }
        if (-not $ownedMemo.ContainsKey($good)) { throw 'identity' }
        $null = Open-HostDirectoryChain $good
        $firstChecks = $infoProbe.Calls
        $null = Open-HostDirectoryChain $good
        if ($firstChecks -le 0 -or $infoProbe.Calls -le $firstChecks) { throw 'identity' }
        # Derive the expected prefix sequence independently from the literal
        # fixture path, not from the production cache. Every cache hit must
        # still reach each real ancestor handle in the original root-first order.
        $expectedAncestors = [Collections.Generic.List[IntPtr]]::new()
        $prefix = $good.Substring(0, 3)
        $expectedAncestors.Add($script:HostDirectories[$prefix].handle)
        foreach ($part in $good.Substring(3).Split('\')) {
            $prefix = [IO.Path]::Combine($prefix, $part)
            $expectedAncestors.Add($script:HostDirectories[$prefix].handle)
        }
        $cachedChain = $script:HostLastDirectoryChain
        # Execute the new managed loop against actual retained handles, using
        # one same-signature trace adapter around the real comparison delegate.
        # The independent literal-derived ancestor sequence above remains the oracle.
        $chainTrace = [Collections.Generic.List[IntPtr]]::new()
        $chainComparison = [Func[Delegate, Delegate, IntPtr, IntPtr, byte[], object, bool, int]] {
            param([Delegate] $TypeCall, [Delegate] $InfoCall, [IntPtr] $Handle,
                  [IntPtr] $Memory, [byte[]] $Scratch, $Expected, [bool] $Content)
            $chainTrace.Add($Handle)
            return $originalComparison.Invoke($TypeCall, $InfoCall, $Handle, $Memory, $Scratch, $Expected, $Content)
        }
        $chainTest = New-HostChainInvoker $chainComparison
        $script:HostInfoBusy = $true
        try {
            if ($ownedChainInvoker.Invoke($cachedChain, $originalComparison, $originalType,
                $originalInfo, $ownedMemory, $ownedScratch) -ne 0 -or
                $chainTest.Invoke($cachedChain, $chainComparison, $originalType,
                $originalInfo, $ownedMemory, $ownedScratch) -ne 0 -or
                $chainTrace.Count -ne $expectedAncestors.Count) { throw 'identity' }
            for ($i = 0; $i -lt $expectedAncestors.Count; $i++) {
                if ($chainTrace[$i] -ne $expectedAncestors[$i]) { throw 'identity' }
            }
            $savedRootExpected = $cachedChain[0].comparison
            try {
                $changed = [byte[]]$savedRootExpected.Clone()
                $changed[28] = $changed[28] -bxor 1
                $cachedChain[0].comparison = $changed
                $chainTrace.Clear()
                if ($chainTest.Invoke($cachedChain, $chainComparison, $originalType,
                    $originalInfo, $ownedMemory, $ownedScratch) -ne 4 -or
                    $chainTrace.Count -ne 1 -or $chainTrace[0] -ne $expectedAncestors[0]) { throw 'identity' }
            } finally { $cachedChain[0].comparison = $savedRootExpected }
            # Adaptation at the leaf resumes at its index after exactly the
            # preceding ancestor reads. No successful native result is cached.
            $adapted = [object[]]$cachedChain.Clone()
            $adapted[$adapted.Length - 1] = [pscustomobject]$cachedChain[$cachedChain.Length - 1]
            $chainTrace.Clear()
            if ($chainTest.Invoke($adapted, $chainComparison, $originalType,
                $originalInfo, $ownedMemory, $ownedScratch) -ne -$adapted.Length -or
                $chainTrace.Count -ne $adapted.Length - 1) { throw 'identity' }
            $overLimit = [object[]]::new(513)
            $chainTrace.Clear()
            if ($chainTest.Invoke($overLimit, $chainComparison, $originalType,
                $originalInfo, $ownedMemory, $ownedScratch) -ne -1 -or $chainTrace.Count -ne 0) { throw 'identity' }
        } finally { $script:HostInfoBusy = $false }
        if ($script:HostLastDirectoryPath -cne $good -or
            $script:HostDirectoryChainLimit -ne 512 -or $cachedChain.Length -ne $expectedAncestors.Count) { throw 'identity' }
        $beforeChecks = $infoProbe.Calls; $beforeTrace = $infoHandles.Count
        $leaf = Open-HostDirectoryChain $good
        if (-not [object]::ReferenceEquals($cachedChain, $script:HostLastDirectoryChain) -or
            -not [object]::ReferenceEquals($leaf, $script:HostDirectories[$good]) -or
            $infoProbe.Calls -ne $beforeChecks + $expectedAncestors.Count) { throw 'identity' }
        for ($i = 0; $i -lt $expectedAncestors.Count; $i++) {
            if ($infoHandles[$beforeTrace + $i] -ne $expectedAncestors[$i]) { throw 'identity' }
        }
        # A case-distinct key misses this Ordinal cache, but retains the old
        # case-insensitive directory map and full native checks.
        $otherCase = $good.Substring(0, $good.Length - 4) + 'GOOD'
        $null = Open-HostDirectoryChain $otherCase
        if ($script:HostLastDirectoryPath -cne $otherCase -or
            [object]::ReferenceEquals($cachedChain, $script:HostLastDirectoryChain)) { throw 'identity' }
        $null = Open-HostDirectoryChain $good
        # Cache capacity is not an admission limit: exact original checking
        # continues, twice, even when no complete chain can be retained.
        $chainLimit = $script:HostDirectoryChainLimit
        try {
            $script:HostDirectoryChainLimit = 1
            $beforeChecks = $infoProbe.Calls
            $null = Open-HostDirectoryChain $good
            $null = Open-HostDirectoryChain $good
            if ($infoProbe.Calls -ne $beforeChecks + 2 * $expectedAncestors.Count -or
                $null -ne $script:HostLastDirectoryPath -or $null -ne $script:HostLastDirectoryChain) { throw 'identity' }
        } finally { $script:HostDirectoryChainLimit = $chainLimit }
        $null = Open-HostDirectoryChain $good
        # A changed expected ROOT identity must still fail on the first real
        # comparison; cached structural references cannot hide the mutation.
        $rootRecord = $script:HostDirectories[$good.Substring(0, 3)]
        $savedRootComparison = $rootRecord.comparison
        $changedRootComparison = [byte[]]$savedRootComparison.Clone()
        $changedRootComparison[28] = $changedRootComparison[28] -bxor 1
        $beforeChecks = $infoProbe.Calls
        try {
            $rootRecord.comparison = $changedRootComparison
            $refusal = ''
            try { $null = Open-HostDirectoryChain $good } catch { $refusal = $_.Exception.Message }
            if ($refusal -cne 'changed' -or $infoProbe.Calls -ne $beforeChecks + 1) { throw 'identity' }
        } finally { $rootRecord.comparison = $savedRootComparison }
        # A failing miss must not publish a partial prefix list.
        $leafRecord = $script:HostDirectories[$good]
        $savedLeafComparison = $leafRecord.comparison
        $changedLeafComparison = [byte[]]$savedLeafComparison.Clone()
        $changedLeafComparison[44] = $changedLeafComparison[44] -bxor 1
        try {
            $leafRecord.comparison = $changedLeafComparison
            Must-Refuse { Open-HostDirectoryChain $otherCase }
            if ($null -ne $script:HostLastDirectoryPath -or $null -ne $script:HostLastDirectoryChain) { throw 'identity' }
        } finally { $leafRecord.comparison = $savedLeafComparison }
        $null = Open-HostDirectoryChain $good
        $ownedDirectoryChain = $script:HostLastDirectoryChain
        $toolPath = Join-Path $good 'tool.bin'
        if (-not $ownedMemo.ContainsKey($toolPath)) { throw 'identity' }
        $beforeHandles = $script:HostHandles.Count; $beforeChecks = $infoProbe.Calls
        $opened = Open-HostReadHandle $toolPath $false
        if ($script:HostHandles.Count -ne $beforeHandles + 1 -or $infoProbe.Calls -le $beforeChecks -or
            $opened.path -cne $toolPath -or $opened.initial.bytes -ne 4 -or
            [object]::ReferenceEquals($comparisonRecord.comparison, $opened.comparison)) { throw 'identity' }
        if ($typeProbe.Calls -ne $infoProbe.Calls -or $nativeTrace.Count -ne 2 * $infoProbe.Calls) { throw 'identity' }
        for ($i = 0; $i -lt $nativeTrace.Count; $i += 2) {
            if ($nativeTrace[$i] -cne 'type' -or $nativeTrace[$i + 1] -cne 'info') { throw 'identity' }
        }
    } finally { $script:HostInfo = $originalInfo; $script:HostType = $originalType }
    $script:HostInfoBusy = $true
    try {
        Must-Refuse { Get-HostHandleInfo $script:HostFileRecords[0].handle }
        Must-Refuse { Get-HostHandleInfo $comparisonRecord.handle $comparisonRecord.comparison $true }
        Must-Refuse { Open-HostDirectoryChain $good }
        Must-Refuse { Close-HostReader }
        if (-not [object]::ReferenceEquals($ownedCanonical, $script:HostCanonicalInvoker) -or
            -not [object]::ReferenceEquals($ownedChainInvoker, $script:HostChainInvoker) -or
            -not [object]::ReferenceEquals($ownedChainInvoker, $script:HostChainInvokerOwner) -or
            -not [object]::ReferenceEquals($originalComparison, $script:HostChainComparison) -or
            -not [object]::ReferenceEquals($originalComparison, $script:HostComparisonInvoker) -or
            -not [object]::ReferenceEquals($originalType, $script:HostComparisonType) -or
            -not [object]::ReferenceEquals($originalInfo, $script:HostComparisonInfo)) { throw 'identity' }
        if ($script:HostInfoMemory -ne $ownedMemory -or
            -not [object]::ReferenceEquals($ownedMemo, $script:HostCanonicalPaths) -or
            -not [object]::ReferenceEquals($ownedDirectoryChain, $script:HostLastDirectoryChain)) { throw 'identity' }
    }
    finally { $script:HostInfoBusy = $false }
    # Actual retained READ-sharing owner denies a new writer, not a mock lock.
    $caseStage = 'write_exclusion'
    Must-Refuse { [IO.File]::WriteAllBytes((Join-Path $good 'tool.bin'), [byte[]]@(9)) }
} finally { Close-HostReader }
if ($script:HostInfoMemory -ne [IntPtr]::Zero -or $null -ne $script:HostInfoScratch -or
    $null -ne $script:HostComparisonInvoker -or $null -ne $script:HostComparisonType -or
    $null -ne $script:HostComparisonInfo -or $null -ne $script:HostChainInvoker -or
        $null -ne $script:HostChainInvokerOwner -or $null -ne $script:HostChainComparison -or
    $script:HostInfoBusy -or $null -ne $script:HostCanonicalPaths -or $null -ne $script:HostCanonicalInvoker -or
        $script:HostCanonicalUnits -ne 0 -or
    $ownedMemo.Count -ne 0 -or $null -ne $script:HostLastDirectoryPath -or
    $null -ne $script:HostLastDirectoryChain -or @($ownedDirectoryChain | Where-Object { $null -ne $_ }).Count -ne 0) { throw 'identity' }
# Cleanup remains idempotent; no freed metadata pointer or lexical owner is reused.
Close-HostReader
Must-Refuse { Get-HostHandleInfo ([IntPtr]::Zero) }
if ((Get-HostCanonicalPath $good) -cne $good -or $null -ne $script:HostCanonicalPaths) { throw 'identity' }
Must-Refuse { Get-HostCanonicalPath 'relative' }
Initialize-HostReader
try {
    if ([object]::ReferenceEquals($ownedMemo, $script:HostCanonicalPaths) -or
        $script:HostCanonicalPaths.ContainsKey($good)) { throw 'identity' }
    $null = Get-HostCanonicalPath $good
    if (-not $script:HostCanonicalPaths.ContainsKey($good)) { throw 'identity' }
} finally { Close-HostReader }
# A pre-native initialization refusal still leaves idempotent cleanup state.
$originalSystemRoot = $env:SystemRoot
try {
    $env:SystemRoot = 'relative'
    Must-Refuse { Initialize-HostReader }
    $failedMemo = $script:HostCanonicalPaths
    Close-HostReader
    Close-HostReader
    if ($script:HostInfoMemory -ne [IntPtr]::Zero -or $null -ne $script:HostInfoScratch -or
        $null -ne $script:HostComparisonInvoker -or $null -ne $script:HostComparisonType -or
        $null -ne $script:HostComparisonInfo -or $null -ne $script:HostChainInvoker -or
        $null -ne $script:HostChainInvokerOwner -or $null -ne $script:HostChainComparison -or
        $script:HostInfoBusy -or $script:HostHandles.Count -ne 0 -or
        $null -ne $script:HostCanonicalPaths -or $null -ne $script:HostCanonicalInvoker -or
        $script:HostCanonicalUnits -ne 0 -or
        $failedMemo.Count -ne 0 -or $null -ne $script:HostLastDirectoryPath -or
        $null -ne $script:HostLastDirectoryChain) { throw 'identity' }
} finally { $env:SystemRoot = $originalSystemRoot }
foreach ($boundary in @('bytes', 'entries', 'deadline', 'handles', 'metadata')) {
    $caseStage = 'limit_' + $boundary
    Initialize-HostReader
    try {
        switch ($boundary) {
            'bytes' { $script:HostLimits.FileBytes = 3 }
            'entries' { $script:HostLimits.Entries = 1 }
            'deadline' { $script:HostLimits.Seconds = -1 }
            'handles' { $script:HostLimits.Handles = 0 }
            'metadata' { $script:HostLimits.MetadataBytes = 1 }
        }
        if ($script:HostPreflightBudgetCause -cne 'unknown') { throw 'failure_context' }
        Must-Refuse { Get-HostRootInventory $good }
        if ($script:HostPreflightBudgetCause -cne $boundary) { throw 'failure_context' }
        $message = @(Get-HostPreflightFailureMessage 'vs_inventory' $script:HostPreflightBudgetCause)
        if ($message.Count -ne 1 -or $message[0] -cne
            ('MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=vs_inventory budget=' + $boundary)) {
            throw 'failure_context'
        }
    } finally { Close-HostReader }
    if ($script:HostPreflightBudgetCause -cne $boundary) { throw 'failure_context' }
}
# The aggregate boundary is checked against the opened size before the
# first payload Read argument is evaluated, not only after a completed hash.
$caseStage = 'aggregate_setup'
$budgetRoot = Join-Path $root 'aggregate-budget'
[IO.Directory]::CreateDirectory($budgetRoot) | Out-Null
[IO.File]::WriteAllBytes((Join-Path $budgetRoot 'next.bin'), [byte[]]@(1,2,3,4))
$caseStage = 'aggregate_before_read'
Initialize-HostReader
try {
    $script:HostLimits.TotalBytes = 6; $script:HostBytes = [uint64]3
    $script:HostPayloadReadAttempted = $false
    $script:HostBuffer = [pscustomobject]@{}
    $script:HostBuffer | Add-Member -MemberType ScriptProperty -Name Length -Value {
        $script:HostPayloadReadAttempted = $true; return 1
    }
    $refusal = ''
    try { Get-HostRootInventory $budgetRoot | Out-Null } catch { $refusal = $_.Exception.Message }
    if ($refusal -cne 'limit' -or $script:HostPayloadReadAttempted -or $script:HostBytes -ne 3 -or
        $script:HostPreflightBudgetCause -cne 'bytes') {
        throw 'aggregate_before_read'
    }
} finally { Close-HostReader }
# A file that exactly consumes the remaining bytes still uses the actual
# reader/hash and is admitted; the production 64GiB cap is unchanged.
$caseStage = 'aggregate_exact_fit'
Initialize-HostReader
try {
    $script:HostLimits.TotalBytes = 7; $script:HostBytes = [uint64]3
    $result = Get-HostRootInventory $budgetRoot
    if ($result.files.Count -ne 1 -or $script:HostBytes -ne 7 -or
        $result.files[0].sha256 -cne '9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a') {
        throw 'aggregate_exact_fit'
    }
} finally { Close-HostReader }
# Close a real owned synthetic handle and then throw: the native outcome is
# intentionally uncertain to Close-HostReader. No consumed handle may be retried.
Initialize-HostReader
$originalClose = $script:HostClose
$closeProbe = [pscustomobject]@{
    Delegate = $originalClose
    Calls = [Collections.Generic.List[IntPtr]]::new()
    Consumed = [IntPtr]::Zero
    Closed = $false
    Thrown = $false
}
$closeProbe | Add-Member -MemberType ScriptMethod -Name Invoke -Value {
    param([IntPtr] $Handle)
    $this.Calls.Add($Handle)
    $closed = $this.Delegate.Invoke($Handle)
    if (-not $this.Thrown) {
        $this.Thrown = $true
        $this.Consumed = $Handle
        $this.Closed = ($closed -ne 0)
        throw 'identity'
    }
    return $closed
}
try {
    $null = Get-HostRootInventory $budgetRoot
    $ownedHandleCount = $script:HostHandles.Count
    $exceptionMemo = $script:HostCanonicalPaths
    $exceptionChain = $script:HostLastDirectoryChain
    if ($ownedHandleCount -lt 2 -or $exceptionMemo.Count -eq 0 -or $null -eq $exceptionChain) { throw 'identity' }
    $expectedConsumed = $script:HostHandles[$ownedHandleCount - 1]
    $script:HostClose = $closeProbe
    Must-Refuse { Close-HostReader }
    if (-not $closeProbe.Thrown -or -not $closeProbe.Closed -or $closeProbe.Calls.Count -ne 1 -or
        $closeProbe.Consumed -ne $expectedConsumed -or
        $script:HostHandles[$ownedHandleCount - 1] -ne [IntPtr]::Zero -or
        $script:HostInfoMemory -ne [IntPtr]::Zero -or $null -ne $script:HostInfoScratch -or
        $null -ne $script:HostComparisonInvoker -or $null -ne $script:HostComparisonType -or
        $null -ne $script:HostComparisonInfo -or $null -ne $script:HostChainInvoker -or
        $null -ne $script:HostChainInvokerOwner -or $null -ne $script:HostChainComparison -or
        $script:HostInfoBusy -or $null -ne $script:HostCanonicalPaths -or $null -ne $script:HostCanonicalInvoker -or
        $script:HostCanonicalUnits -ne 0 -or
        $exceptionMemo.Count -ne 0 -or $null -ne $script:HostLastDirectoryPath -or
        $null -ne $script:HostLastDirectoryChain -or @($exceptionChain | Where-Object { $null -ne $_ }).Count -ne 0) { throw 'identity' }
    # Remaining retained handles are still owned, and second cleanup attempts
    # each exactly once while skipping the already consumed zeroed slot.
    Close-HostReader
    $uniqueCalls = [Collections.Generic.HashSet[IntPtr]]::new()
    foreach ($handle in $closeProbe.Calls) {
        if ($handle -eq [IntPtr]::Zero -or -not $uniqueCalls.Add($handle)) { throw 'identity' }
    }
    if ($closeProbe.Calls.Count -ne $ownedHandleCount -or $script:HostHandles.Count -ne 0 -or
        $script:HostInfoMemory -ne [IntPtr]::Zero -or $null -ne $script:HostInfoScratch -or
        $null -ne $script:HostComparisonInvoker -or $null -ne $script:HostComparisonType -or
        $null -ne $script:HostComparisonInfo -or $null -ne $script:HostChainInvoker -or
        $null -ne $script:HostChainInvokerOwner -or $null -ne $script:HostChainComparison -or $null -ne $script:HostCanonicalInvoker -or
        $script:HostInfoBusy) { throw 'identity' }
    Close-HostReader
    if ($closeProbe.Calls.Count -ne $ownedHandleCount) { throw 'identity' }
} finally {
    # Even an assertion failure uses the original real closer for every still
    # owned nonzero slot; never probe the consumed handle or freed metadata.
    $script:HostClose = $originalClose
    Close-HostReader
}
$caseStage = 'junction_setup'
$junctionRoot = Join-Path $root 'junction-root'
[IO.Directory]::CreateDirectory($junctionRoot) | Out-Null
New-Item -ItemType Junction -Path (Join-Path $junctionRoot 'alias') -Target $good | Out-Null
$caseStage = 'junction_refusal'
Initialize-HostReader
try { Must-Refuse { Get-HostRootInventory $junctionRoot } } finally { Close-HostReader }
Write-Output 'MORPHEUS_HOST_READER_BOUNDARIES_OK'
} catch {
    # Output only source-fixed labels. No ErrorRecord/string rendering, native
    # error text, path, argument, environment value or child stderr is emitted.
    $category = 'unexpected'
    $closed = @('source_parse', 'metadata', 'accepted', 'path', 'path_input',
        'path_normalized', 'path_component', 'path_empty', 'path_length', 'path_drive',
        'path_colon', 'path_slash', 'path_chars', 'path_part_empty', 'path_part_dot',
        'path_part_trailingdot', 'path_part_trailingspace', 'path_part_device', 'path_part_git',
        'host', 'type',
        'identity', 'reparse', 'open', 'changed', 'limit', 'file_size', 'text_size',
        'depth', 'entries', 'alias', 'handle_cleanup', 'inventory', 'hash',
        'aggregate_before_read', 'aggregate_exact_fit', 'failure_context')
    if ($closed -ccontains $_.Exception.Message) { $category = 'check_' + $_.Exception.Message }
    else {
        # PowerShell wraps .NET invocation errors. Inspect at most eight links,
        # mapping only known runtime types to fixed categories, never printing them.
        $problem = $_.Exception
        for ($i = 0; $i -lt 8 -and $null -ne $problem; $i++) {
            if ($problem -is [UnauthorizedAccessException]) { $category = 'access'; break }
            if ($problem -is [IO.IOException]) { $category = 'io'; break }
            if ($problem -is [ArgumentException]) { $category = 'argument'; break }
            if ($problem -is [InvalidCastException]) { $category = 'cast'; break }
            if ($problem -is [MissingMemberException]) { $category = 'member'; break }
            if ($problem -is [Management.Automation.MethodInvocationException]) { $category = 'invocation' }
            elseif ($category -eq 'unexpected' -and $problem -is [Management.Automation.RuntimeException]) {
                $category = 'powershell_runtime'
            }
            $problem = $problem.InnerException
        }
    }
    $construction = ''
    if ($caseStage -ceq 'reader_initialize') {
        $failure = $_
        # Closed runtime kind and a line in our joined, reviewed function text.
        # Never render InvocationInfo, exception text, a path, or unknown types.
        $kind = switch -CaseSensitive ($failure.Exception.GetType().FullName) {
            'System.Management.Automation.MethodException' { 'binding'; break }
            'System.Management.Automation.MethodInvocationException' { 'invocation'; break }
            'System.Management.Automation.RuntimeException' { 'runtime'; break }
            'System.Management.Automation.PSInvalidCastException' { 'cast'; break }
            'System.ArgumentException' { 'argument'; break }
            'System.ArgumentNullException' { 'argument_null'; break }
            'System.InvalidOperationException' { 'invalid_operation'; break }
            'System.TypeLoadException' { 'type_load'; break }
            'System.Reflection.AmbiguousMatchException' { 'ambiguous'; break }
            default { 'unknown' }
        }
        $line = 0
        $invocation = $failure.InvocationInfo
        if ($null -ne $invocation -and [string]::IsNullOrEmpty($invocation.ScriptName) -and
            $invocation.ScriptLineNumber -gt 0 -and $invocation.ScriptLineNumber -le $definitionLineLimit) {
            $line = [int]$invocation.ScriptLineNumber
        }
        $construction = ' line=' + $line.ToString([Globalization.CultureInfo]::InvariantCulture) + ' kind=' + $kind
    }
    Write-Output ('MORPHEUS_HOST_READER_BOUNDARY_FAILED stage=' + $caseStage + ' category=' + $category + $construction)
    exit 1
}
"""
            source = PATH.parent / "native_webrtc/windows_preflight.ps1"
            pwsh = Path(os.environ["ProgramFiles"]) / "PowerShell/7/pwsh.exe"
            with tempfile.TemporaryDirectory() as temporary:
                # The script is beside, never inside, the inventoried fixture roots.
                script_path = Path(temporary).resolve() / "host-reader-case.ps1"
                with script_path.open("x", encoding="utf-8", newline="") as stream:
                    stream.write(script)
                env = os.environ.copy()
                env["MORPHEUS_HOST_READER_SOURCE"] = str(source)
                env["MORPHEUS_HOST_TEST_ROOT"] = temporary
                result = subprocess.run([str(pwsh), "-NoLogo", "-NoProfile", "-NonInteractive", "-File", str(script_path)],
                    text=True, encoding="utf-8", capture_output=True, timeout=45, env=env)
            # The only forwarded child output is one exact finite diagnostic.
            # Unknown/multiple/raw lines and all child stderr stay withheld.
            stages = ("source_parse", "source_functions", "failure_context", "metadata", "path_refusals",
                      "synthetic_setup", "reader_initialize", "inventory", "digest_assertion",
                      "write_exclusion", "limit_bytes", "limit_entries", "limit_deadline", "limit_handles",
                      "limit_metadata", "aggregate_setup", "aggregate_before_read",
                      "aggregate_exact_fit", "junction_setup", "junction_refusal")
            checks = ("source_parse", "metadata", "accepted", "path", "path_input",
                      "path_normalized", "path_component", "path_empty", "path_length", "path_drive",
                      "path_colon", "path_slash", "path_chars", "path_part_empty", "path_part_dot",
                      "path_part_trailingdot", "path_part_trailingspace", "path_part_device", "path_part_git",
                      "host", "type",
                      "identity", "reparse", "open", "changed", "limit", "file_size", "text_size",
                      "depth", "entries", "alias", "handle_cleanup", "inventory", "hash",
                      "aggregate_before_read", "aggregate_exact_fit", "failure_context")
            categories = ("unexpected", "access", "io", "argument", "cast", "member",
                          "invocation", "powershell_runtime") + tuple("check_" + item for item in checks)
            diagnostic = "stage=unknown category=unexpected"
            if len(result.stdout) <= 160:
                observed = result.stdout.strip()
                kinds = ("binding", "invocation", "runtime", "cast", "argument", "argument_null",
                         "invalid_operation", "type_load", "ambiguous", "unknown")
                # Joined non-nested function extents cannot exceed the source's
                # line count. PowerShell additionally requires an empty filename.
                line_limit = len(source.read_text(encoding="utf-8").splitlines())
                for stage in stages:
                    for category in categories:
                        expected = ("MORPHEUS_HOST_READER_BOUNDARY_FAILED stage=" + stage
                                    + " category=" + category)
                        if observed == expected:
                            diagnostic = "stage=" + stage + " category=" + category
                        elif stage == "reader_initialize" and observed.startswith(expected + " line="):
                            suffix = observed[len(expected + " line="):]
                            line, separator, kind = suffix.partition(" kind=")
                            if (separator and 0 < len(line) <= 4 and line.isascii() and line.isdecimal()
                                    and int(line) <= line_limit and kind in kinds):
                                diagnostic = ("stage=" + stage + " category=" + category
                                              + " line=" + line + " kind=" + kind)
            self.assertEqual(result.returncode, 0,
                             "fixed Windows host reader boundary case failed " + diagnostic)
            self.assertTrue(result.stdout.strip() == "MORPHEUS_HOST_READER_BOUNDARIES_OK",
                            "fixed Windows host reader success marker mismatch")

            # Exercise the actual entry/catch/exit with a refused synthetic
            # caller. The first guard fails before bootstrap tool discovery or
            # installed SDK selection; no child stderr/raw text is forwarded.
            env = os.environ.copy()
            env["GITHUB_ACTIONS"] = "false"
            result = subprocess.run([str(pwsh), "-NoLogo", "-NoProfile", "-NonInteractive",
                "-File", str(source), "-Mode", "host-toolchain"],
                text=True, encoding="utf-8", capture_output=True, timeout=45, env=env)
            self.assertEqual(result.returncode, 1, "fixed preflight failure must remain fatal")
            self.assertTrue(result.stdout.strip() ==
                            "MORPHEUS_NATIVE_WINDOWS_PREFLIGHT failed stage=bootstrap budget=unknown",
                            "fixed preflight failure context mismatch")
            self.assertTrue(not result.stderr, "preflight failure emitted unexpected stderr")


if __name__ == "__main__":
    unittest.main()
