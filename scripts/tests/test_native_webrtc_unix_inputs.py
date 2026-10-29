"""Input traversal regressions for the native Unix builder; synthetic files only."""
from pathlib import Path
import hashlib
import tarfile
import io
import json
import importlib.util
import os
import stat
import sys
import time
from tempfile import TemporaryDirectory
import unittest
import zipfile
from unittest import mock

_SCRIPT = Path(__file__).resolve().parents[1] / "build_native_webrtc_unix.py"
_SPEC = importlib.util.spec_from_file_location("morpheus_unix_input_tests", _SCRIPT)
assert _SPEC is not None and _SPEC.loader is not None
_BUILDER = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_BUILDER)


@unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA profiles")
class NativeUnixInputTraversalTests(unittest.TestCase):
    def fixture(self, root):
        root.chmod(0o700)
        path = root / "admitted"
        path.write_bytes(b"synthetic admitted data\n")
        path.chmod(0o600)
        row = {"path": "admitted", "type": "file", "bytes": path.stat().st_size,
               "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        build = object.__new__(_BUILDER.Build)
        build.roots = {"instrumented-data": root}
        build.instrumented_expected = {"admitted": (row["bytes"], row["sha256"], 0o600)}
        build.instrumented_directories = {"."}
        return row, build

    def test_complete_inventory_refuses_an_enumeration_error(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            row, _ = self.fixture(root)
            def unreadable(path, followlinks=False, onerror=None):
                if onerror is not None:
                    onerror(PermissionError("synthetic enumeration refusal"))
                return [(str(path), [], ["admitted"])]
            with mock.patch.object(_BUILDER.os, "walk", side_effect=unreadable):
                with self.assertRaises(_BUILDER.Refused):
                    _BUILDER.inventory(root, [row])

    def test_private_profile_checks_a_listed_child_before_skipped_descent(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            _, build = self.fixture(root)
            child = root / "unreadable"
            child.mkdir(mode=0o700)
            child.chmod(0)
            try:
                # Model os.walk's old default: the parent listed the child,
                # then its failed enumeration silently yielded no child row.
                with mock.patch.object(_BUILDER.os, "walk",
                                       return_value=[(str(root), ["unreadable"], ["admitted"])]):
                    with self.assertRaises(_BUILDER.Refused):
                        build.verify_instrumented_materials()
            finally:
                child.chmod(0o700)

    def test_exact_private_profile_passes_but_extra_files_are_refused(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            row, build = self.fixture(root)
            self.assertEqual(_BUILDER.inventory(root, [row]), {"admitted": row})
            build.verify_instrumented_materials()
            (root / "extra").write_bytes(b"synthetic extra\n")
            with self.assertRaises(_BUILDER.Refused):
                build.verify_instrumented_materials()


_MATERIAL_PATH = _SCRIPT.parent / "native_webrtc/linux_materials.py"
_MATERIAL_SPEC = importlib.util.spec_from_file_location("morpheus_linux_material_tests", _MATERIAL_PATH)
assert _MATERIAL_SPEC is not None and _MATERIAL_SPEC.loader is not None
_MATERIAL = importlib.util.module_from_spec(_MATERIAL_SPEC)
_MATERIAL_SPEC.loader.exec_module(_MATERIAL)


@unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA profiles")
class NativeLinuxProjectionBoundaryTests(unittest.TestCase):
    def fixture(self, root):
        root.chmod(0o700)
        path = root / "admitted"
        path.write_bytes(b"synthetic admitted projection data\n")
        path.chmod(0o600)
        row = {"path": "admitted", "bytes": path.stat().st_size,
               "sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "mode": 0o600}
        return {"admitted": row}, {"."}

    def test_projection_refuses_extra_unreadable_child_before_descent(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            files, directories = self.fixture(root)
            child = root / "unbound"
            child.mkdir(mode=0o700)
            child.chmod(0)
            try:
                with self.assertRaises(_MATERIAL.Refused):
                    _MATERIAL.topology(root, files, directories)
            finally:
                child.chmod(0o700)

    def test_projection_refuses_changed_bytes_links_and_extra_files(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            files, directories = self.fixture(root)
            _MATERIAL.topology(root, files, directories)
            path = root / "admitted"
            original = path.read_bytes()
            path.write_bytes(b"changed synthetic data\n")
            with self.assertRaises(_MATERIAL.Refused):
                _MATERIAL.topology(root, files, directories)
            path.unlink()
            path.symlink_to(root / "unbound-target")
            with self.assertRaises(_MATERIAL.Refused):
                _MATERIAL.topology(root, files, directories)
            path.unlink()
            path.write_bytes(original)
            path.chmod(0o600)
            (root / "extra").write_bytes(b"synthetic extra\n")
            with self.assertRaises(_MATERIAL.Refused):
                _MATERIAL.topology(root, files, directories)

    def test_only_exact_shared_llvm_nm_metadata_can_be_coalesced(self):
        row = {"path": "third_party/llvm-build/Release+Asserts/bin/llvm-nm",
               "type": "file", "bytes": 21311248,
               "sha256": "d58075cc6c8e423ae5f07a6dc464563a45c3738db8d272e9a869898f9c28572f",
               "mode": "100755"}
        self.assertTrue(_MATERIAL.shared_llvm_nm(row, dict(row)))
        for key, value in (("path", "third_party/llvm-build/Release+Asserts/bin/clang"),
                           ("type", "symlink"), ("bytes", 21311247),
                           ("sha256", "0" * 64), ("mode", "100644")):
            with self.subTest(key=key):
                self.assertFalse(_MATERIAL.shared_llvm_nm(row, {**row, key: value}))
                self.assertFalse(_MATERIAL.shared_llvm_nm({**row, key: value}, row))

    def test_package_headers_exclude_projected_toolchain_and_sysroot_headers(self):
        native = [{"path": "api/peer.h", "type": "file", "bytes": 1, "sha256": "1" * 64,
                   "mode": "100644"},
                  {"path": "api/helper.inc", "type": "file", "bytes": 2, "sha256": "2" * 64,
                   "mode": "100644"}]
        complete = {row["path"]: row for row in native}
        for name in ("third_party/llvm-build/Release+Asserts/lib/clang/22/include/stddef.h",
                     "build/linux/debian_bullseye_amd64-sysroot/usr/include/stdio.h"):
            complete[name] = {"path": name, "type": "file", "bytes": 3, "sha256": "3" * 64,
                              "mode": "100644"}
        self.assertEqual(_BUILDER.package_source_headers(native, complete),
                         {"api/peer.h", "api/helper.inc"})
        complete["api/peer.h"] = {**native[0], "sha256": "4" * 64}
        with self.assertRaises(_BUILDER.Refused):
            _BUILDER.package_source_headers(native, complete)


_PIPELINE_PATH = _SCRIPT.parent / "run_native_webrtc_linux.py"
_PIPELINE_SPEC = importlib.util.spec_from_file_location("morpheus_native_cache_tests", _PIPELINE_PATH)
assert _PIPELINE_SPEC is not None and _PIPELINE_SPEC.loader is not None
_PIPELINE = importlib.util.module_from_spec(_PIPELINE_SPEC)
_PIPELINE_SPEC.loader.exec_module(_PIPELINE)


class NativeLinuxStageConsumerTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA profiles")
    def test_actual_selected_linux_profile_passes_the_actual_stager_consumer(self):
        # Read the actual checked-in cohort and invoke only its DATA validator.
        # No archives, network, subprocess, staged executable, admission or build is involved.
        profile = _SCRIPT.parents[1] / "native/webrtc/linux/selected-linux-build-inputs.json"
        helper = _PIPELINE.module("linux_stager.py")
        manifest = helper.decode(profile.read_bytes())
        materials = helper.linux_inputs_validate(manifest, "Linuxx64")
        self.assertEqual([row["name"] for row in materials],
                         ["rust", "sysroot-amd64", "sysroot-i386", "llvmobjdump"])
        self.assertIs(materials, manifest["materials"])

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA profiles")
    def test_actual_python_profile_admits_all_original_zip_member_spellings(self):
        helper = _PIPELINE.module("python_stager.py")
        raw = (_SCRIPT.parents[1] / "native/webrtc/linux/selected-python-inputs.json").read_bytes()
        plan = helper.python_data_validate(raw, "Linuxx64")
        original = next(row for row in plan["materials"] if row["name"] == "vpython")
        table, copies = helper.python_zip_members(original)
        self.assertEqual(len(table), len(original["members"]))
        self.assertEqual(copies, original["alias_copies"])
        self.assertEqual({name for name in table if " " in name}, {
            "3.11/lib/python3.11/site-packages/setuptools/_vendor/jaraco/text/Lorem ipsum.txt",
            "3.11/lib/python3.11/site-packages/setuptools/command/launcher manifest.xml",
            "3.11/lib/python3.11/site-packages/setuptools/script (dev).tmpl",
            "3.8/lib/python3.8/site-packages/setuptools/command/launcher manifest.xml",
            "3.8/lib/python3.8/site-packages/setuptools/script (dev).tmpl",
        })
        # The unchanged generic/core-tool path policy still refuses spaces.
        for name in table:
            if " " in name:
                with self.assertRaises(helper.Refusal):
                    helper.rel(name)

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA profiles")
    def test_original_space_names_survive_zip_stage_snapshot_and_full_inventory(self):
        helper = _PIPELINE.module("python_stager.py")
        raw = (_SCRIPT.parents[1] / "native/webrtc/linux/selected-python-inputs.json").read_bytes()
        plan = helper.python_data_validate(raw, "Linuxx64")
        original = next(row for row in plan["materials"] if row["name"] == "vpython")
        names = [row["path"] for row in original["members"] if " " in row["path"]]
        self.assertEqual(len(names), 5)
        with TemporaryDirectory() as temporary:
            parent = Path(temporary).resolve()
            parent.chmod(0o700)
            archive, output = parent / "synthetic.zip", parent / "output"
            rows, payloads = [], {}
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_STORED) as target:
                for index, name in enumerate(names):
                    data = ("synthetic ordinary DATA " + str(index)).encode("ascii")
                    entry = zipfile.ZipInfo(name)
                    entry.create_system = 3
                    entry.external_attr = (stat.S_IFREG | 0o600) << 16
                    target.writestr(entry, data)
                    payloads[name] = data
                    rows.append({"path": name, "kind": "regular", "mode": stat.S_IFREG | 0o600,
                                 "output_mode": 0o600, "bytes": len(data),
                                 "sha256": hashlib.sha256(data).hexdigest()})
            material = {"name": "synthetic", "members": rows, "alias_copies": [],
                        "bytes": archive.stat().st_size}
            # Exercise the real Stage file/parent/identity methods inside an
            # already private synthetic root, not production runner admission.
            stage = object.__new__(helper.Stage)
            stage.root, stage.files, stage.dirs = output, [], []
            stage.windows, stage.held, stage.identities = None, [], {}
            stage.mkdir(output)
            try:
                helper.python_zip_extract(stage, material, archive, output)
                files, _ = _PIPELINE.scan_tree(_MATERIAL, output)
                self.assertEqual(set(files), set(names))
                inventory = [{"path": row["path"], "type": "file", "bytes": row["bytes"],
                              "sha256": row["sha256"]} for row in rows]
                self.assertEqual(set(_BUILDER.inventory(output, inventory)), set(names))
                for name, data in payloads.items():
                    self.assertEqual(helper.snapshot(output / name, helper.MAX_FILE), data)
                    self.assertEqual(files[name]["sha256"], hashlib.sha256(data).hexdigest())
            finally:
                stage.finish(False)

    def test_python_zip_space_policy_refuses_traversal_devices_and_name_aliases(self):
        helper = _PIPELINE.module("python_stager.py")
        row = {"path": "ordinary name.txt", "kind": "regular", "mode": stat.S_IFREG | 0o600,
               "output_mode": 0o600, "bytes": 0, "sha256": hashlib.sha256(b"").hexdigest()}
        for name in ("/absolute name", "C:/drive name", "name:stream", "a\\b name", "a/../b name",
                     "a/./b name", "a//b name", " name", "name ", "parent /name", "parent/ name",
                     "name.", "name\tvalue", "name\nvalue", "name\x00value", "name\x7fvalue",
                     "name\u00a0value", "name\u00e9value", "CON .txt", "aux .dat", "COM1 .bin",
                     "parent/LPT9 .log", "CON", "NUL.txt", "a" * 513):
            with self.subTest(name=name):
                with self.assertRaises(helper.Refusal):
                    helper.python_zip_members({"name": "synthetic", "members": [row | {"path": name}],
                                               "alias_copies": []})
        with self.assertRaises(helper.Refusal):
            helper.python_zip_members({"name": "synthetic", "members": [row, row | {"path": "ORDINARY NAME.TXT"}],
                                       "alias_copies": []})
        with self.assertRaises(helper.Refusal):
            helper.python_zip_members({"name": "synthetic", "members": [row, row | {"path": "ordinary name.txt/child"}],
                                       "alias_copies": []})

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA profiles")
    def test_linux_xz_decoder_allows_overhead_only_for_exact_pinned_sysroots(self):
        helper = _PIPELINE.module("linux_stager.py")
        profile = _SCRIPT.parents[1] / "native/webrtc/linux/selected-linux-build-inputs.json"
        materials = helper.linux_inputs_validate(helper.decode(profile.read_bytes()), "Linuxx64")
        tools = {row["name"]: row for row in materials}
        # Valid synthetic one-byte XZ DATA with the original sysroot dictionary
        # size. No archive/network/tool/source execution is involved.
        raw = bytes.fromhex("fd377a585a0000016922de3602002101200000000988a57601000078000000008316dc8c00011501a96334609042990d010000000001595a")
        for name in ("sysroot-amd64", "sysroot-i386"):
            with self.subTest(name=name):
                self.assertEqual(helper.linux_xz_decoder(tools[name]).decompress(raw), b"x")
        # Every other pinned input retains the smaller original total limit.
        with self.assertRaises(helper.lzma.LZMAError):
            helper.linux_xz_decoder(tools["rust"]).decompress(raw)
        for change in ({"sha256": "0" * 64}, {"bytes": 1}, {"format": "tar.gz"}, {"name": "other"}):
            with self.subTest(change=next(iter(change))):
                with self.assertRaises(helper.Refusal):
                    helper.linux_xz_decoder(tools["sysroot-amd64"] | change)

    def test_imported_helper_failure_exposes_only_closed_context_and_reraises(self):
        helper = _PIPELINE.module("linux_stager.py")
        original = sys.argv
        error = ValueError("PRIVATE token=https://private.invalid/?credential=SECRET /private/owner")
        helper.__dict__["synthetic_error"] = error
        # A reviewed synthetic function whose filename/globals match the actual fixed helper.
        exec(compile("def main():\n    raise synthetic_error\n", helper.__file__, "exec"), helper.__dict__)
        for phase, expected in (("admission", "admission"),
                                ("download_rust", "download_rust"),
                                ("PRIVATE/SECRET", "unknown")):
            with self.subTest(phase=expected):
                helper.PHASE = phase
                output = io.StringIO()
                with mock.patch.object(_PIPELINE.sys, "stderr", output):
                    with self.assertRaises(ValueError) as caught:
                        _PIPELINE.cli(helper, ["PRIVATE_ARG=SECRET"])
                self.assertIs(caught.exception, error)
                self.assertIs(sys.argv, original)
                raw = output.getvalue()
                self.assertLess(len(raw), 256)
                self.assertEqual(raw.split(" ", 1)[0], "MORPHEUS_NATIVE_HELPER_FAILED")
                context = json.loads(raw.split(" ", 1)[1])
                self.assertEqual(context, {"helper": "linux_stager.py", "phase": expected, "lines": [2]})
                for private in ("PRIVATE", "SECRET", "private.invalid", "/private/owner", "ValueError"):
                    self.assertNotIn(private, raw)
                # The same failure cannot attribute its lines to an unregistered/renamed module.
                with mock.patch.dict(helper.__dict__, {"__name__": "unreviewed_module"}):
                    self.assertEqual(_PIPELINE.helper_failure_context(helper, error),
                                     {"helper": "unknown", "phase": "unknown", "lines": []})


    def test_postassembly_failure_context_is_fixed_and_remains_fatal(self):
        expected = frozenset(("post_assembly_scan", "alias_validation", "material_projection",
                              "final_inventory", "handoff_pack", "handoff_verify", "handoff_emit"))
        self.assertEqual(_PIPELINE.POST_ASSEMBLY_PHASES, expected)
        private = "PRIVATE token=SECRET https://private.invalid /private/owner"
        for phase in (*sorted(expected), private, {"PRIVATE": "SECRET"}):
            category = phase if type(phase) is str and phase in expected else "unknown"
            def fail():
                _PIPELINE.POST_ASSEMBLY_PHASE = phase
                raise RuntimeError(private)
            output = io.StringIO()
            with mock.patch.object(_PIPELINE, "main", side_effect=fail), \
                    mock.patch.object(_PIPELINE.sys, "stderr", output):
                with self.assertRaises(SystemExit) as caught:
                    _PIPELINE.entry()
            self.assertEqual(caught.exception.code, 1)
            self.assertEqual(output.getvalue(), json.dumps({"native_linux_producer": "failed",
                "build_or_release_acceptance": False, "category": category},
                sort_keys=True, separators=(",", ":")) + "\n")
            self.assertLess(len(output.getvalue()), 160)
        # No previous invocation may supply a stale postassembly category.
        output = io.StringIO()
        with mock.patch.object(_PIPELINE, "POST_ASSEMBLY_PHASE", "handoff_pack"), \
                mock.patch.object(_PIPELINE, "main", side_effect=RuntimeError(private)), \
                mock.patch.object(_PIPELINE.sys, "stderr", output):
            with self.assertRaises(SystemExit) as caught:
                _PIPELINE.entry()
        self.assertEqual(caught.exception.code, 1)
        self.assertEqual(json.loads(output.getvalue())["category"], "unknown")


    def test_handoff_writer_retains_first_capacity_short_write_and_os_failure(self):
        class Short(io.BytesIO):
            def write(self, data): return 0
        class Full(io.BytesIO):
            def write(self, data): raise OSError(_PIPELINE.errno.ENOSPC, "opaque-fixture")
        cases = ((io.BytesIO(), 0, "compressed_capacity", _PIPELINE.Refused),
                 (Short(), 8, "compressed_short_write", _PIPELINE.Refused),
                 (Full(), 8, "os_space", OSError))
        try:
            for sink, cap, failure, expected in cases:
                with self.subTest(failure=failure):
                    _PIPELINE.reset_pack_capture(True)
                    _PIPELINE.pack_at("file_add")
                    writer = _PIPELINE.BoundedWriter(sink, cap)
                    with self.assertRaises(expected): writer.write(b"x")
                    first = _PIPELINE.pack_frame()
                    self.assertEqual(first, {"pack_operation": "file_add", "pack_failure": failure})
                    _PIPELINE.pack_at("gzip_finish")
                    _PIPELINE.pack_failed(RuntimeError("opaque-fixture"))
                    self.assertEqual(_PIPELINE.pack_frame(), first)
            _PIPELINE.reset_pack_capture()
            with self.assertRaises(_PIPELINE.Refused):
                _PIPELINE.BoundedWriter(io.BytesIO(), 0).write(b"x")
            self.assertEqual(_PIPELINE.pack_frame(), {})
        finally:
            _PIPELINE.reset_pack_capture()

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA handoff")
    def test_nonpublishing_stage_validates_all_data_without_creating_a_bundle(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); root.chmod(0o700)
            source = root / "fixture-source"; source.mkdir(mode=0o700)
            payload = b"owned source DATA"
            path = source / "fixture.data"; path.write_bytes(payload); path.chmod(0o600)
            row = {"path": "fixture.data", "bytes": len(payload),
                   "sha256": hashlib.sha256(payload).hexdigest(), "mode": 0o600}
            inventories = {source.name: {"role": "fixture", "files": [row], "directories": ["."]}}
            handoff = {"schema": "synthetic", "roots": inventories}
            # A non-publishing stage must not create the bounded tar at all,
            # even when its genuine DATA could not fit a compressed cap of1.
            with mock.patch.object(_PIPELINE, "pack_handoff", side_effect=AssertionError("unused bundle")), \
                    mock.patch.object(_PIPELINE, "MAX_HANDOFF", 1), \
                    mock.patch.object(_MATERIAL, "topology", wraps=_MATERIAL.topology) as topology:
                self.assertEqual(_PIPELINE.stage_handoff(root, handoff, inventories, _MATERIAL,
                                                       publish_handoff=False), (None, None))
                self.assertEqual(topology.call_count, 1)
                self.assertFalse((root / "native-webrtc-handoff").exists())
                path.write_bytes(b"changed same length"[:len(payload)])
                with self.assertRaises(_MATERIAL.Refused):
                    _PIPELINE.stage_handoff(root, handoff, inventories, _MATERIAL, publish_handoff=False)
            # Omitting an unused archive cannot omit the original metadata cap.
            with mock.patch.object(_PIPELINE, "MAX_JSON", 1), \
                    mock.patch.object(_MATERIAL, "topology", side_effect=AssertionError("metadata must refuse first")):
                with self.assertRaises(_PIPELINE.Refused):
                    _PIPELINE.stage_handoff(root, handoff, inventories, _MATERIAL, publish_handoff=False)

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA handoff")
    def test_publishing_stage_keeps_actual_pack_cap_and_verified_archive_path(self):
        def fixture(root, payload):
            root.chmod(0o700); source = root / "fixture-source"; source.mkdir(mode=0o700)
            path = source / "fixture.data"; path.write_bytes(payload); path.chmod(0o600)
            row = {"path": "fixture.data", "bytes": len(payload),
                   "sha256": hashlib.sha256(payload).hexdigest(), "mode": 0o600}
            inventories = {source.name: {"role": "fixture", "files": [row], "directories": ["."]}}
            return inventories, {"schema": "synthetic", "roots": inventories}
        try:
            with TemporaryDirectory() as temporary:
                root = Path(temporary).resolve(); inventories, handoff = fixture(root, b"owned DATA")
                output, fact = _PIPELINE.stage_handoff(root, handoff, inventories, _MATERIAL, publish_handoff=True)
                self.assertEqual(output, root / "native-webrtc-handoff/handoff.tar.gz")
                self.assertEqual(fact, _MATERIAL.file_fact(output))
                with tarfile.open(output, "r:gz") as archive:
                    self.assertEqual([member.name for member in archive],
                                     ["HANDOFF.json", "fixture-source", "fixture-source/fixture.data"])
                    self.assertEqual(json.loads(archive.extractfile("HANDOFF.json").read()), handoff)
                    self.assertEqual(archive.extractfile("fixture-source/fixture.data").read(), b"owned DATA")
            with TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                payload = b"".join(hashlib.sha256(str(i).encode()).digest() for i in range(2048))
                inventories, handoff = fixture(root, payload)
                with mock.patch.object(_PIPELINE, "MAX_HANDOFF", 4096):
                    with self.assertRaises(_PIPELINE.Refused):
                        _PIPELINE.stage_handoff(root, handoff, inventories, _MATERIAL, publish_handoff=True)
                self.assertEqual(_PIPELINE.pack_frame(),
                                 {"pack_operation": "file_add", "pack_failure": "compressed_capacity"})
                self.assertLessEqual((root / "native-webrtc-handoff/handoff.tar.gz").stat().st_size, 4096)
        finally:
            _PIPELINE.reset_pack_capture()

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA handoff")
    def test_versioned_handoff_deduplicates_bytes_but_restores_independent_files_and_modes(self):
        for schema in ("morpheus-native-linux-handoff-v2", _PIPELINE.LINUX_XZ_SCHEMA):
            # Two identical 64KiB blocks exceed DEFLATE history but fit the
            # fixed XZ dictionary. The original whole-file alias proof remains.
            payload = b"".join(hashlib.sha256(str(i).encode()).digest() for i in range(2048))
            payload += payload
            roles = ("assembled", "core", "linux", "python", "instrumented", "control")
            with TemporaryDirectory() as temporary:
                root = Path(temporary).resolve(); root.chmod(0o700)
                inventories, paths = {}, []
                for index, role in enumerate(roles):
                    name = "morpheus-webrtc-tools-" + f"{index:016x}"
                    folder = root / name; folder.mkdir(mode=0o700)
                    body = payload if index < 3 else b"owned control DATA"
                    path = folder / "owned.data"; path.write_bytes(body)
                    mode = (0o400, 0o600, 0o755)[index % 3]; path.chmod(mode)
                    rows = [{"path": "owned.data", "bytes": len(body),
                             "sha256": hashlib.sha256(body).hexdigest(), "mode": mode}]
                    paths.append((name + "/owned.data", mode, body))
                    inventories[name] = {"role": role, "files": rows, "directories": ["."]}
                handoff = {"schema": schema, "roots": inventories,
                           "pipeline_plan_sha256": "0" * 64, "build_admission_granted": False,
                           "native_sources_complete": False, "build_ready": False, "distribution_ready": False}
                with mock.patch.object(_PIPELINE, "MAX_HANDOFF", 200_000):
                    archive = _PIPELINE.pack_handoff(root, handoff, inventories, _MATERIAL)
                packed = archive.read_bytes()
                self.assertLess(len(packed), 200_000)
                if schema == "morpheus-native-linux-handoff-v2":
                    gzip_bytes = len(packed)
                    self.assertGreater(gzip_bytes, 100_000)
                else:
                    self.assertLess(len(packed), 100_000)
                    self.assertLess(len(packed), gzip_bytes)
                with tarfile.open(archive, "r:*") as source:
                    members = [(member, source.extractfile(member).read() if member.isfile() else None)
                               for member in source]
                self.assertEqual([member.name for member, _ in members if member.isfile()],
                                 ["HANDOFF.json", paths[0][0], paths[3][0]])
                self.assertTrue(all(member.isfile() or member.isdir() for member, _ in members))

                def restore_bytes(content, *, maximum=None):
                    with TemporaryDirectory() as destination_temporary:
                        destination = Path(destination_temporary).resolve(); destination.chmod(0o700)
                        bundle = destination / "native-webrtc-handoff"; bundle.mkdir(mode=0o700)
                        (bundle / "handoff.tar.gz").write_bytes(content)
                        (bundle / "handoff.tar.gz").chmod(0o600)
                        args = _PIPELINE.types.SimpleNamespace(plan_sha256="0" * 64,
                            handoff_sha256=hashlib.sha256(content).hexdigest(), handoff_bytes=len(content))
                        with mock.patch.object(_PIPELINE, "cache_identity", return_value={}), \
                                mock.patch.object(_PIPELINE, "MAX_EXPANDED", maximum or _PIPELINE.MAX_EXPANDED):
                            restored, _ = _PIPELINE.restore(args, destination, _MATERIAL)
                        self.assertEqual(restored, handoff)
                        identities = set()
                        for name, mode, body in paths:
                            path = destination / name
                            self.assertEqual(path.read_bytes(), body)
                            self.assertEqual(stat.S_IMODE(path.stat().st_mode), mode)
                            self.assertEqual(path.stat().st_nlink, 1)
                            identities.add((path.stat().st_dev, path.stat().st_ino))
                        self.assertEqual(len(identities), 6)
                restore_bytes(packed)
                if schema == _PIPELINE.LINUX_XZ_SCHEMA:
                    self.assertTrue(packed.startswith(_PIPELINE.XZ_MAGIC))
                    # Recomputed outer digests cannot authorize truncated, joined,
                    # trailing or excessive-memory compressed streams.
                    for invalid in (packed[:-1], packed + b"suffix", packed + packed):
                        with self.assertRaises(_PIPELINE.Refused):
                            restore_bytes(invalid)
                    with mock.patch.object(_PIPELINE, "MAX_XZ_MEMORY", 1024):
                        with self.assertRaises(_PIPELINE.Refused):
                            restore_bytes(packed)
                # Duplicate logical paths still count against the expanded limit.
                with self.assertRaises(_PIPELINE.Refused):
                    restore_bytes(packed, maximum=sum(len(body) for _, _, body in paths) - 1)

                def changed_archive(change):
                    output = io.BytesIO()
                    with tarfile.open(fileobj=output, mode="w:xz" if schema == _PIPELINE.LINUX_XZ_SCHEMA else "w:gz", format=tarfile.GNU_FORMAT) as target:
                        for member, body in change(members):
                            target.addfile(member, io.BytesIO(body) if body is not None else None)
                    return output.getvalue()
                # Compression magic cannot select a different internal schema.
                opposite = io.BytesIO()
                with tarfile.open(fileobj=opposite,
                                  mode="w:gz" if schema == _PIPELINE.LINUX_XZ_SCHEMA else "w:xz",
                                  format=tarfile.GNU_FORMAT) as target:
                    for member, body in members:
                        target.addfile(member, io.BytesIO(body) if body is not None else None)
                with self.assertRaises(_PIPELINE.Refused):
                    restore_bytes(opposite.getvalue())
                first = next((member, body) for member, body in members if member.name == paths[0][0])
                for label, change in [
                    ("duplicate", lambda rows: rows + [first]),
                    ("missing", lambda rows: [row for row in rows if row[0].name != paths[0][0]]),
                    ("digest", lambda rows: [(member, b"x" * len(body) if member.name == paths[0][0] else body)
                                              for member, body in rows]),
                ]:
                    with self.subTest(case=label), self.assertRaises(_PIPELINE.Refused):
                        restore_bytes(changed_archive(change))
                for label in ("alias", "mode", "hardlink", "unexpected"):
                    member = tarfile.TarInfo(paths[0][0]); member.size = len(payload); member.mode = 0o400
                    if label == "alias": member.name = paths[1][0]
                    elif label == "mode": member.mode = 0o600
                    elif label == "hardlink":
                        member.type = tarfile.LNKTYPE; member.linkname = paths[1][0]; member.size = 0
                    else: member.name = "outside.data"
                    body = payload if member.isfile() else None
                    rows = [(member, body) if item.name == paths[0][0] else (item, content)
                            for item, content in members]
                    with self.subTest(case=label), self.assertRaises(_PIPELINE.Refused):
                        restore_bytes(changed_archive(lambda _: rows))

        # Linux v4 retains the original approved gzip files AND every decoded
        # member/alias. It stores only one representation of member contents.
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); root.chmod(0o700)
            roles = ("assembled", "core", "linux", "python", "instrumented", "control")
            folders = {}
            for index, role in enumerate(roles):
                folder = root / ("morpheus-webrtc-tools-" + f"{index:016x}")
                folder.mkdir(mode=0o700); folders[role] = folder
            profile, pins, bodies = {"materials": []}, {}, []
            archive_paths, raw_archives = [], []
            for index in range(2):
                name = "synthetic-instrumented-" + str(index)
                body = bytes(range(256))[::1 if index == 0 else -1] * 601 + b"partial"
                bodies.append(body)
                members = [{"raw_path": name + "/payload.bin", "kind": "regular",
                            "bytes": len(body), "mode": 0o600, "sha256": hashlib.sha256(body).hexdigest()}]
                if index == 0:
                    members.append({"raw_path": name + "/empty.data", "kind": "regular",
                                    "bytes": 0, "mode": 0o400, "sha256": hashlib.sha256(b"").hexdigest()})
                uncompressed = io.BytesIO()
                with tarfile.open(fileobj=uncompressed, mode="w:", format=tarfile.GNU_FORMAT) as tar:
                    for row in members:
                        data = b"" if row["bytes"] == 0 else body
                        info = tarfile.TarInfo(row["raw_path"]); info.size = len(data); info.mode = row["mode"]
                        tar.addfile(info, io.BytesIO(data))
                raw = uncompressed.getvalue(); raw_archives.append(raw)
                packed_gzip = _PIPELINE.gzip.compress(raw, mtime=0)
                archive = folders["instrumented"] / "archives" / (name + ".tar.gz")
                archive.parent.mkdir(mode=0o700, exist_ok=True)
                archive.write_bytes(packed_gzip); archive.chmod(0o600); archive_paths.append(archive)
                digest = hashlib.sha256(packed_gzip).hexdigest()
                pins[digest] = {"name": name, "bytes": len(packed_gzip)}
                profile["materials"].append({"name": name, "bytes": len(packed_gzip), "sha256": digest,
                                             "format": "tar.gz", "members": members})
                output = folders["instrumented"] / "tools" / name
                output.parent.mkdir(mode=0o700, exist_ok=True); output.mkdir(mode=0o700)
                for row in members:
                    data = b"" if row["bytes"] == 0 else body
                    path = output / Path(row["raw_path"]).name
                    path.write_bytes(data); path.chmod(row["mode"])
            for index, role in enumerate(roles):
                if role == "instrumented": continue
                path = folders[role] / "owned.data"
                path.write_bytes(bodies[index % 2] if index < 3 else b"owned control DATA")
                path.chmod((0o400, 0o600, 0o755)[index % 3])
            inventories, expected_files = {}, {}
            for role, folder in folders.items():
                rows, directories = _PIPELINE.scan_tree(_MATERIAL, folder)
                inventories[folder.name] = {"role": role, "files": list(rows.values()),
                                            "directories": sorted(directories)}
                for name, row in rows.items():
                    expected_files[folder.name + "/" + name] = ((folder / name).read_bytes(), row["mode"])
            header = {"schema": _PIPELINE.LINUX_GZIP_MEMBERS_SCHEMA, "roots": inventories,
                      "pipeline_plan_sha256": "0" * 64, "build_admission_granted": False,
                      "native_sources_complete": False, "build_ready": False, "distribution_ready": False}
            with mock.patch.object(_PIPELINE, "GZIP_MEMBER_ARCHIVES", pins):
                header["gzip_members"] = _PIPELINE.gzip_member_recipes(root, inventories,
                    folders["instrumented"], profile, _MATERIAL)
                canonical = _PIPELINE.handoff_canonical(inventories)
                targets = _PIPELINE.gzip_recipe_index(header, canonical)
                self.assertEqual(len(targets), 3)  # Two data blobs and one empty regular file.
                self.assertTrue(all(row["offset"] % 512 == 0 for group in header["gzip_members"]
                                    for row in group["members"]))
                archive = _PIPELINE.pack_handoff(root, header, inventories, _MATERIAL)
                packed = archive.read_bytes()
                with tarfile.open(archive, "r:xz") as source:
                    entries = [(member, source.extractfile(member).read() if member.isfile() else None)
                               for member in source]
                expected_physical = set(canonical.values()) - {canonical[value] for value in targets}
                self.assertEqual({row.name for row, _ in entries if row.isfile()},
                                 {"HANDOFF.json"} | expected_physical)
                self.assertTrue(all(row.isfile() or row.isdir() for row, _ in entries))

                def v4_archive(changed, change=lambda rows: rows):
                    stream = io.BytesIO()
                    with tarfile.open(fileobj=stream, mode="w:xz", format=tarfile.GNU_FORMAT) as target:
                        for member, body in change(entries):
                            if member.name == "HANDOFF.json":
                                body = (json.dumps(changed, sort_keys=True, separators=(",", ":")) + "\n").encode()
                                member = tarfile.TarInfo("HANDOFF.json"); member.mode = 0o600; member.size = len(body)
                            target.addfile(member, io.BytesIO(body) if body is not None else None)
                    return stream.getvalue()

                def restore_v4(content, *, maximum=None, mac_target=None):
                    with TemporaryDirectory() as destination_temporary:
                        destination = Path(destination_temporary).resolve(); destination.chmod(0o700)
                        bundle = destination / "native-webrtc-handoff"; bundle.mkdir(mode=0o700)
                        path = bundle / "handoff.tar.gz"; path.write_bytes(content); path.chmod(0o600)
                        args = _PIPELINE.types.SimpleNamespace(plan_sha256="0" * 64,
                            handoff_sha256=hashlib.sha256(content).hexdigest(), handoff_bytes=len(content))
                        with mock.patch.object(_PIPELINE, "cache_identity", return_value={}), \
                                mock.patch.object(_PIPELINE, "MAX_EXPANDED", maximum or _PIPELINE.MAX_EXPANDED):
                            restored, _ = _PIPELINE.restore(args, destination, _MATERIAL, mac_target=mac_target)
                        self.assertEqual(restored, header)
                        inodes = set()
                        for name, (body, mode) in expected_files.items():
                            value = destination / name
                            self.assertEqual(value.read_bytes(), body)
                            self.assertEqual(stat.S_IMODE(value.stat().st_mode), mode)
                            self.assertEqual(value.stat().st_nlink, 1)
                            inodes.add((value.stat().st_dev, value.stat().st_ino))
                        self.assertEqual(len(inodes), len(expected_files))
                restore_v4(packed)
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v4(packed, maximum=sum(len(value[0]) for value in expected_files.values()) - 1)
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v4(packed, mac_target="mac-arm64")
                for label in ("source", "source_bytes", "source_cycle", "duplicate", "offset", "slice", "offset_type", "overlap",
                              "extent", "target_hash", "stream_hash", "stream_size", "unexpected", "missing"):
                    changed = json.loads(json.dumps(header)); groups = changed["gzip_members"]
                    group, member = groups[0], groups[0]["members"][0]
                    if label == "source": group["sha256"] = "0" * 64
                    elif label == "source_bytes": group["bytes"] += 1
                    elif label == "source_cycle":
                        member["sha256"], member["bytes"] = group["sha256"], group["bytes"]
                    elif label == "duplicate": group["members"].append(dict(member))
                    elif label == "offset": member["offset"] += 1
                    elif label == "slice": member["offset"] += 512
                    elif label == "offset_type": member["offset"] = True
                    elif label == "overlap": group["members"].append(dict(member, sha256="0" * 64))
                    elif label == "extent": member["bytes"] = group["expanded_bytes"] + 1
                    elif label == "target_hash": member["sha256"] = "0" * 64
                    elif label == "stream_hash": group["expanded_sha256"] = "0" * 64
                    elif label == "stream_size": group["expanded_bytes"] += 512
                    elif label == "unexpected": member["extra"] = 0
                    elif label == "missing": groups.pop()
                    with self.subTest(recipe_fault=label), self.assertRaises(_PIPELINE.Refused):
                        restore_v4(v4_archive(changed))
                derived = canonical[next(value for value in targets if value[1] > 0)]
                body, mode = expected_files[derived]
                duplicate = tarfile.TarInfo(derived); duplicate.size = len(body); duplicate.mode = mode
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v4(v4_archive(header, lambda rows: rows + [(duplicate, body)]))
                physical_source = canonical[(header["gzip_members"][0]["sha256"],
                                             header["gzip_members"][0]["bytes"])]
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v4(v4_archive(header, lambda rows: [row for row in rows if row[0].name != physical_source]))
                # Invalid v4 metadata must retain the first refusal and clear
                # PACK_ACTIVE even before any cache/output is created.
                bad = dict(header, gzip_members=[])
                with self.assertRaises(_PIPELINE.Refused):
                    _PIPELINE.pack_handoff(root, bad, inventories, _MATERIAL)
                self.assertFalse(_PIPELINE.PACK_ACTIVE)
                self.assertEqual(_PIPELINE.pack_frame(), {"pack_failure": "refused", "pack_operation": "metadata_limit"})
            # Inner gzip parsing is one stream, checksum/EOF-bound, supports
            # partial reads, and cannot accept a newly hashed invalid suffix.
            original = archive_paths[0].read_bytes()
            for content in (original, original[:-1], original + b"suffix", original + original):
                path = root / "gzip-case.data"; path.write_bytes(content); path.chmod(0o600)
                row = {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
                if content != original:
                    with self.assertRaises(_PIPELINE.Refused):
                        with _PIPELINE.gzip_member_stream(path, row, _MATERIAL) as stream:
                            stream.payload = True
                            while stream.read(31): pass
                else:
                    with _PIPELINE.gzip_member_stream(path, row, _MATERIAL) as stream:
                        stream.payload = True; observed = bytearray()
                        while True:
                            block = stream.read(31)
                            if not block: break
                            observed.extend(block)
                        self.assertEqual(bytes(observed), raw_archives[0])
                    with mock.patch.object(_PIPELINE, "MAX_EXPANDED", len(raw_archives[0]) - 1):
                        with self.assertRaises(_PIPELINE.Refused):
                            with _PIPELINE.gzip_member_stream(path, row, _MATERIAL) as stream:
                                stream.payload = True
                                while stream.read(1024 * 1024): pass
                path.unlink()

        # The inner source decoder reuses the exact already-admitted limits;
        # the outer transport decoder remains independently capped at128MiB.
        self.assertEqual(_PIPELINE.MAX_XZ_MEMORY, 128 * 1024 * 1024)
        for digest, pin in _PIPELINE.XZ_MEMBER_ARCHIVES.items():
            limit = (320 if pin["name"] in ("sysroot-amd64", "sysroot-i386") else 256) * 1024 * 1024
            with mock.patch.object(_PIPELINE.lzma, "LZMADecompressor", return_value=object()) as factory:
                _PIPELINE.xz_member_decoder({"sha256": digest, "bytes": pin["bytes"]})
                factory.assert_called_once_with(format=_PIPELINE.lzma.FORMAT_XZ, memlimit=limit)
            with self.assertRaises(_PIPELINE.Refused):
                _PIPELINE.xz_member_decoder({"sha256": digest, "bytes": pin["bytes"] + 1})
        with self.assertRaises(_PIPELINE.Refused):
            _PIPELINE.xz_member_decoder({"sha256": "0" * 64, "bytes": 1})

        # Linux v5 also retains the original approved XZ files AND every decoded
        # member/alias. It stores only one representation of member contents.
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); root.chmod(0o700)
            roles = ("assembled", "core", "linux", "python", "instrumented", "control")
            folders = {}
            for index, role in enumerate(roles):
                folder = root / ("morpheus-webrtc-tools-" + f"{index:016x}")
                folder.mkdir(mode=0o700); folders[role] = folder
            profile, pins, bodies = {"materials": []}, {}, []
            archive_paths, raw_archives = [], []
            for index in range(2):
                name = "synthetic-instrumented-" + str(index)
                body = bytes(range(256))[::1 if index == 0 else -1] * 601 + b"partial"
                bodies.append(body)
                members = [{"raw_path": "./" + name + "/payload.bin", "path": name + "/payload.bin", "kind": "regular",
                            "bytes": len(body), "mode": 0o600, "sha256": hashlib.sha256(body).hexdigest()}]
                if index == 0:
                    members.append({"raw_path": "./" + name + "/empty.data", "path": name + "/empty.data", "kind": "regular",
                                    "bytes": 0, "mode": 0o400, "sha256": hashlib.sha256(b"").hexdigest()})
                members += [
                    {"raw_path": ".", "path": "", "kind": "directory", "bytes": 0, "mode": 0o755},
                    {"raw_path": "./" + name + "/symlink", "path": name + "/symlink",
                     "kind": "symlink", "bytes": 0, "mode": 0o777, "linkname": "payload.bin"},
                    {"raw_path": "./" + name + "/hardlink", "path": name + "/hardlink",
                     "kind": "hardlink", "bytes": 0, "mode": 0o600,
                     "linkname": "./" + name + "/payload.bin"},
                ]
                uncompressed = io.BytesIO()
                with tarfile.open(fileobj=uncompressed, mode="w:", format=tarfile.GNU_FORMAT) as tar:
                    for row in members:
                        data = b"" if row["bytes"] == 0 else body
                        info = tarfile.TarInfo(row["raw_path"]); info.size = len(data); info.mode = row["mode"]
                        if row["kind"] != "regular":
                            info.type = {"directory": tarfile.DIRTYPE, "symlink": tarfile.SYMTYPE,
                                         "hardlink": tarfile.LNKTYPE}[row["kind"]]
                            info.linkname = row.get("linkname", "")
                        tar.addfile(info, io.BytesIO(data) if row["kind"] == "regular" else None)
                raw = uncompressed.getvalue(); raw_archives.append(raw)
                packed_gzip = _PIPELINE.lzma.compress(raw, format=_PIPELINE.lzma.FORMAT_XZ)
                archive = folders["instrumented"] / "archives" / (name + ".tar.xz")
                archive.parent.mkdir(mode=0o700, exist_ok=True)
                archive.write_bytes(packed_gzip); archive.chmod(0o600); archive_paths.append(archive)
                digest = hashlib.sha256(packed_gzip).hexdigest()
                pins[digest] = {"name": name, "bytes": len(packed_gzip), "member_limit": 10000, "decoder_memory": 256 * 1024 * 1024}
                profile["materials"].append({"name": name, "bytes": len(packed_gzip), "sha256": digest,
                                             "format": "tar.xz", "members": members})
                output = folders["instrumented"] / "tools" / name
                output.parent.mkdir(mode=0o700, exist_ok=True); output.mkdir(mode=0o700)
                for row in members:
                    if row["kind"] != "regular": continue
                    data = b"" if row["bytes"] == 0 else body
                    path = output / Path(row["raw_path"]).name
                    path.write_bytes(data); path.chmod(row["mode"])
            for index, role in enumerate(roles):
                if role == "instrumented": continue
                path = folders[role] / "owned.data"
                path.write_bytes(bodies[index % 2] if index < 3 else b"owned control DATA")
                path.chmod((0o400, 0o600, 0o755)[index % 3])
            inventories, expected_files = {}, {}
            for role, folder in folders.items():
                rows, directories = _PIPELINE.scan_tree(_MATERIAL, folder)
                inventories[folder.name] = {"role": role, "files": list(rows.values()),
                                            "directories": sorted(directories)}
                for name, row in rows.items():
                    expected_files[folder.name + "/" + name] = ((folder / name).read_bytes(), row["mode"])
            header = {"schema": _PIPELINE.LINUX_XZ_MEMBERS_SCHEMA, "roots": inventories, "gzip_members": [],
                      "pipeline_plan_sha256": "0" * 64, "build_admission_granted": False,
                      "native_sources_complete": False, "build_ready": False, "distribution_ready": False}
            with mock.patch.object(_PIPELINE, "XZ_MEMBER_ARCHIVES", pins), \
                    mock.patch.object(_PIPELINE, "GZIP_MEMBER_ARCHIVES", {}):
                header["xz_members"] = _PIPELINE.xz_member_recipes(root, inventories,
                    [(folders["instrumented"], tool) for tool in profile["materials"]], _MATERIAL, set())
                canonical = _PIPELINE.handoff_canonical(inventories)
                targets = _PIPELINE.archive_recipe_index(header, canonical)
                self.assertEqual(len(targets), 3)  # Two data blobs and one empty regular file.
                self.assertTrue(all(row["offset"] % 512 == 0 for group in header["xz_members"]
                                    for row in group["members"]))
                archive = _PIPELINE.pack_handoff(root, header, inventories, _MATERIAL)
                packed = archive.read_bytes()
                with tarfile.open(archive, "r:xz") as source:
                    entries = [(member, source.extractfile(member).read() if member.isfile() else None)
                               for member in source]
                expected_physical = set(canonical.values()) - {canonical[value] for value in targets}
                self.assertEqual({row.name for row, _ in entries if row.isfile()},
                                 {"HANDOFF.json"} | expected_physical)
                self.assertTrue(all(row.isfile() or row.isdir() for row, _ in entries))

                def v5_archive(changed, change=lambda rows: rows):
                    stream = io.BytesIO()
                    with tarfile.open(fileobj=stream, mode="w:xz", format=tarfile.GNU_FORMAT) as target:
                        for member, body in change(entries):
                            if member.name == "HANDOFF.json":
                                body = (json.dumps(changed, sort_keys=True, separators=(",", ":")) + "\n").encode()
                                member = tarfile.TarInfo("HANDOFF.json"); member.mode = 0o600; member.size = len(body)
                            target.addfile(member, io.BytesIO(body) if body is not None else None)
                    return stream.getvalue()

                def restore_v5(content, *, maximum=None, mac_target=None):
                    with TemporaryDirectory() as destination_temporary:
                        destination = Path(destination_temporary).resolve(); destination.chmod(0o700)
                        bundle = destination / "native-webrtc-handoff"; bundle.mkdir(mode=0o700)
                        path = bundle / "handoff.tar.gz"; path.write_bytes(content); path.chmod(0o600)
                        args = _PIPELINE.types.SimpleNamespace(plan_sha256="0" * 64,
                            handoff_sha256=hashlib.sha256(content).hexdigest(), handoff_bytes=len(content))
                        with mock.patch.object(_PIPELINE, "cache_identity", return_value={}), \
                                mock.patch.object(_PIPELINE, "MAX_EXPANDED", maximum or _PIPELINE.MAX_EXPANDED):
                            restored, _ = _PIPELINE.restore(args, destination, _MATERIAL, mac_target=mac_target)
                        self.assertEqual(restored, header)
                        inodes = set()
                        for name, (body, mode) in expected_files.items():
                            value = destination / name
                            self.assertEqual(value.read_bytes(), body)
                            self.assertEqual(stat.S_IMODE(value.stat().st_mode), mode)
                            self.assertEqual(value.stat().st_nlink, 1)
                            inodes.add((value.stat().st_dev, value.stat().st_ino))
                        self.assertEqual(len(inodes), len(expected_files))
                restore_v5(packed)
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(packed, maximum=sum(len(value[0]) for value in expected_files.values()) - 1)
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(packed, mac_target="mac-arm64")
                for label in ("source", "source_bytes", "source_cycle", "duplicate", "offset", "slice", "offset_type", "overlap",
                              "extent", "target_hash", "stream_hash", "stream_size", "unexpected", "missing"):
                    changed = json.loads(json.dumps(header)); groups = changed["xz_members"]
                    group, member = groups[0], groups[0]["members"][0]
                    if label == "source": group["sha256"] = "0" * 64
                    elif label == "source_bytes": group["bytes"] += 1
                    elif label == "source_cycle":
                        member["sha256"], member["bytes"] = group["sha256"], group["bytes"]
                    elif label == "duplicate": group["members"].append(dict(member))
                    elif label == "offset": member["offset"] += 1
                    elif label == "slice": member["offset"] += 512
                    elif label == "offset_type": member["offset"] = True
                    elif label == "overlap": group["members"].append(dict(member, sha256="0" * 64))
                    elif label == "extent": member["bytes"] = group["expanded_bytes"] + 1
                    elif label == "target_hash": member["sha256"] = "0" * 64
                    elif label == "stream_hash": group["expanded_sha256"] = "0" * 64
                    elif label == "stream_size": group["expanded_bytes"] += 512
                    elif label == "unexpected": member["extra"] = 0
                    elif label == "missing": groups.pop()
                    with self.subTest(recipe_fault=label), self.assertRaises(_PIPELINE.Refused):
                        restore_v5(v5_archive(changed))
                cross = json.loads(json.dumps(header))
                first = cross["xz_members"][0]
                # A gzip recipe may not derive an XZ physical source, even if
                # both per-codec descriptors separately look acyclic.
                cross_pin = {"0" * 64: {"bytes": 1, "name": "synthetic-gzip"}}
                cross_canonical = dict(canonical, **{})
                cross_canonical[("0" * 64, 1)] = "synthetic-source"
                cross["gzip_members"] = [{"sha256": "0" * 64, "bytes": 1,
                    "expanded_bytes": first["bytes"] + 1024, "expanded_sha256": "1" * 64,
                    "members": [{"offset": 512, "bytes": first["bytes"], "sha256": first["sha256"]}]}]
                with mock.patch.object(_PIPELINE, "GZIP_MEMBER_ARCHIVES", cross_pin):
                    with self.assertRaises(_PIPELINE.Refused):
                        _PIPELINE.archive_recipe_index(cross, cross_canonical)
                derived = canonical[next(value for value in targets if value[1] > 0)]
                body, mode = expected_files[derived]
                duplicate = tarfile.TarInfo(derived); duplicate.size = len(body); duplicate.mode = mode
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(v5_archive(header, lambda rows: rows + [(duplicate, body)]))
                physical_source = canonical[(header["xz_members"][0]["sha256"],
                                             header["xz_members"][0]["bytes"])]
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(v5_archive(header, lambda rows: [row for row in rows if row[0].name != physical_source]))
                # Invalid v5 metadata must retain the first refusal and clear
                # PACK_ACTIVE even before any cache/output is created.
                bad = dict(header, xz_members=[])
                with self.assertRaises(_PIPELINE.Refused):
                    _PIPELINE.pack_handoff(root, bad, inventories, _MATERIAL)
                self.assertFalse(_PIPELINE.PACK_ACTIVE)
                self.assertEqual(_PIPELINE.pack_frame(), {"pack_failure": "refused", "pack_operation": "metadata_limit"})
                # Version6 shares raw ORIGINAL Git batch-capture payloads,
                # while keeping both capture files and all checkout versions.
                previous = folders["assembled"]
                folders["assembled"] = root / ("morpheus-webrtc-assembled-" + "0" * 16)
                previous.rename(folders["assembled"])
                provision = folders["assembled"] / "provision"; provision.mkdir(mode=0o700)
                for directory in ("private-logs", "materials", "checkout"):
                    (provision / directory).mkdir(mode=0o700)
                checkout_body = b"synthetic original checkout body\0" * 127
                postpatch_body = b"changed postpatch bytes stay physical"
                opaque_pack = b"PACK\0opaque original object-store DATA"
                (provision / "objects").mkdir(mode=0o700)
                (provision / "objects/001.git").mkdir(mode=0o700)
                for path, body, mode in (
                        (provision / "checkout/source.data", checkout_body, 0o755),
                        (provision / "checkout/patched.data", postpatch_body, 0o600),
                        (provision / "objects/001.git/source.pack", opaque_pack, 0o600),
                        (folders["core"] / "independent-alias.data", checkout_body, 0o400)):
                    path.write_bytes(body); path.chmod(mode)
                def captured(body):
                    blob = hashlib.sha1(b"blob " + str(len(body)).encode() + b"\0" + body).hexdigest()
                    return blob.encode() + b" blob " + str(len(body)).encode() + b"\n" + body + b"\n"
                capture = (captured(checkout_body) + captured(checkout_body)
                           + captured(b"prepatch-only bytes") + captured(b""))
                commands = []
                for number in (1, 2):
                    name = "private-logs/" + f"{number:04d}" + ".stdout"
                    path = provision / name; path.write_bytes(capture); path.chmod(0o600)
                    commands.append({"argv": ["/reviewed/host/git", "--git-dir=synthetic", "cat-file", "--batch"],
                                     "stdout": name})
                command_path = provision / "materials/commands.json"
                command_path.write_text(json.dumps(commands) + "\n"); command_path.chmod(0o600)
                inventories, expected_files = {}, {}
                for role, folder in folders.items():
                    rows, directories = _PIPELINE.scan_tree(_MATERIAL, folder)
                    inventories[folder.name] = {"role": role, "files": list(rows.values()),
                                                "directories": sorted(directories)}
                    for name, row in rows.items():
                        expected_files[folder.name + "/" + name] = ((folder / name).read_bytes(), row["mode"])
                header = json.loads(json.dumps(header))
                header.update(schema=_PIPELINE.LINUX_BATCH_MEMBERS_SCHEMA, roots=inventories)
                # Cover zero-length batch-derived files too; retain the exact
                # archive/member inventory, merely choose another DATA source.
                for group in header["xz_members"]:
                    group["members"] = [row for row in group["members"] if row["bytes"] != 0]
                canonical = _PIPELINE.handoff_canonical(inventories)
                excluded = _PIPELINE.archive_recipe_index(header, canonical)
                header["batch_members"] = _PIPELINE.batch_member_recipes(root, inventories,
                    folders["assembled"], _MATERIAL, excluded)
                self.assertEqual(len(header["batch_members"]), 1)  # Identical captures coalesce physically.
                batch_targets = _PIPELINE.batch_recipe_index(header, canonical)
                self.assertEqual(batch_targets, {(hashlib.sha256(checkout_body).hexdigest(), len(checkout_body)),
                                                 (hashlib.sha256(b"").hexdigest(), 0)})
                self.assertNotIn((hashlib.sha256(postpatch_body).hexdigest(), len(postpatch_body)), batch_targets)
                self.assertNotIn((hashlib.sha256(opaque_pack).hexdigest(), len(opaque_pack)), batch_targets)
                self.assertNotIn((hashlib.sha256(b"prepatch-only bytes").hexdigest(), len(b"prepatch-only bytes")), canonical)
                targets = _PIPELINE.all_recipe_index(header, canonical)
                archive.unlink(); archive.parent.rmdir()  # Only this fixture's prior owned output.
                archive = _PIPELINE.pack_handoff(root, header, inventories, _MATERIAL)
                packed = archive.read_bytes()
                with tarfile.open(archive, "r:xz") as source:
                    entries = [(member, source.extractfile(member).read() if member.isfile() else None)
                               for member in source]
                self.assertEqual({row.name for row, _ in entries if row.isfile()},
                                 {"HANDOFF.json"} | (set(canonical.values()) - {canonical[value] for value in targets}))
                restore_v5(packed)  # Same exact mode/inode/full-file assertions, now for v6 roots.
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(packed, mac_target="mac-arm64")
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(packed, maximum=sum(len(value[0]) for value in expected_files.values()) - 1)
                for label in ("source", "source_size", "path", "duplicate_source", "source_cycle", "archive_cycle",
                              "duplicate_target", "offset", "offset_type", "overlap", "extent", "hash", "extra", "missing"):
                    changed = json.loads(json.dumps(header)); groups = changed["batch_members"]
                    group, member = groups[0], groups[0]["members"][0]
                    if label == "source": group["sha256"] = "0" * 64
                    elif label == "source_size": group["bytes"] += 1
                    elif label == "path": group["source"] = group["source"].replace("private-logs", "checkout")
                    elif label == "duplicate_source": groups.append(json.loads(json.dumps(group)))
                    elif label == "source_cycle":
                        member.update(offset=0, sha256=group["sha256"], bytes=group["bytes"])
                    elif label == "archive_cycle":
                        physical = changed["xz_members"][0]
                        member.update(offset=0, sha256=physical["sha256"], bytes=physical["bytes"])
                    elif label == "duplicate_target": group["members"].append(dict(member))
                    elif label == "offset": member["offset"] += 1
                    elif label == "offset_type": member["offset"] = True
                    elif label == "overlap": group["members"].append(dict(member, sha256="0" * 64))
                    elif label == "extent": member["bytes"] = group["bytes"] + 1
                    elif label == "hash": member["sha256"] = "0" * 64
                    elif label == "extra": member["unexpected"] = 0
                    elif label == "missing": groups.pop()
                    with self.subTest(batch_recipe_fault=label), self.assertRaises(_PIPELINE.Refused):
                        restore_v5(v5_archive(changed))
                derived = canonical[(hashlib.sha256(checkout_body).hexdigest(), len(checkout_body))]
                duplicate = tarfile.TarInfo(derived); duplicate.size = len(checkout_body)
                duplicate.mode = expected_files[derived][1]
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(v5_archive(header, lambda rows: rows + [(duplicate, checkout_body)]))
                physical_source = header["batch_members"][0]["source"]
                with self.assertRaises(_PIPELINE.Refused):
                    restore_v5(v5_archive(header, lambda rows: [row for row in rows if row[0].name != physical_source]))
                # Rehash actual malformed captures into a fresh complete
                # inventory, so tests reach framing/SHA1/EOF rather than only
                # rejecting a stale file hash. No tool is executed.
                capture_path = provision / "private-logs/0001.stdout"
                malformed = (b"invalid header\n", capture[:-1], capture + b"suffix",
                             b"0" * 40 + capture[40:],
                             b"0" * 40 + b" blob " + str(1024 * 1024 * 1024 + 1).encode() + b"\n")
                for content in malformed:
                    capture_path.write_bytes(content); capture_path.chmod(0o600)
                    rows, directories = _PIPELINE.scan_tree(_MATERIAL, folders["assembled"])
                    broken = dict(inventories)
                    broken[folders["assembled"].name] = {"role": "assembled", "files": list(rows.values()),
                                                          "directories": sorted(directories)}
                    with self.assertRaises(_PIPELINE.Refused):
                        _PIPELINE.batch_member_recipes(root, broken, folders["assembled"], _MATERIAL, excluded)
                capture_path.write_bytes(capture); capture_path.chmod(0o600)
                row = _MATERIAL.file_fact(capture_path)
                with _PIPELINE.batch_member_stream(capture_path, row, _MATERIAL) as stream:
                    observed = bytearray()
                    while True:
                        block = stream.read(31)
                        if not block: break
                        observed.extend(block)
                    self.assertEqual(bytes(observed), capture)
            # Inner XZ parsing is one stream, checksum/EOF-bound, supports
            # partial reads, and cannot accept a newly hashed invalid suffix.
            original = archive_paths[0].read_bytes()
            for content in (original, original[:-1], original + b"suffix", original + original):
                path = root / "xz-case.data"; path.write_bytes(content); path.chmod(0o600)
                row = {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
                stream_pins = {row["sha256"]: {"bytes": len(content), "name": "synthetic-stream",
                               "member_limit": 10000, "decoder_memory": 256 * 1024 * 1024}}
                if content != original:
                    with mock.patch.object(_PIPELINE, "XZ_MEMBER_ARCHIVES", stream_pins), \
                            self.assertRaises(_PIPELINE.Refused):
                        with _PIPELINE.xz_member_stream(path, row, _MATERIAL) as stream:
                            stream.payload = True
                            while stream.read(31): pass
                else:
                    with mock.patch.object(_PIPELINE, "XZ_MEMBER_ARCHIVES", stream_pins), \
                            _PIPELINE.xz_member_stream(path, row, _MATERIAL) as stream:
                        stream.payload = True; observed = bytearray()
                        while True:
                            block = stream.read(31)
                            if not block: break
                            observed.extend(block)
                        self.assertEqual(bytes(observed), raw_archives[0])
                    with mock.patch.object(_PIPELINE, "MAX_EXPANDED", len(raw_archives[0]) - 1), \
                            mock.patch.object(_PIPELINE, "XZ_MEMBER_ARCHIVES", stream_pins):
                        with self.assertRaises(_PIPELINE.Refused):
                            with _PIPELINE.xz_member_stream(path, row, _MATERIAL) as stream:
                                stream.payload = True
                                while stream.read(1024 * 1024): pass
                path.unlink()

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA pack")
    def test_actual_gnu_gzip_pack_reports_payload_footer_and_first_body_failure(self):
        def attempt(payload, cap, fail_fact=False):
            with TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                source = root / "fixture-source"; source.mkdir(mode=0o700)
                path = source / "fixture.data"; path.write_bytes(payload); path.chmod(0o600)
                row = {"path": "fixture.data", "bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest(), "mode": 0o600}
                inventories = {source.name: {"role": "fixture", "files": [row], "directories": ["."]}}
                fact = _MATERIAL.file_fact
                with mock.patch.object(_PIPELINE, "MAX_HANDOFF", cap), \
                        mock.patch.object(_MATERIAL, "file_fact", side_effect=_PIPELINE.Refused("opaque-fixture") if fail_fact else fact) as verify:
                    with self.assertRaises(_PIPELINE.Refused):
                        _PIPELINE.pack_handoff(root, {"schema": "synthetic", "roots": inventories}, inventories, _MATERIAL)
                self.assertIs(_PIPELINE.PACK_ACTIVE, False)
                self.assertTrue(verify.called)
                self.assertLessEqual((root / "native-webrtc-handoff/handoff.tar.gz").stat().st_size, cap)
                return _PIPELINE.pack_frame()
        try:
            payload = b"".join(hashlib.sha256(str(i).encode()).digest() for i in range(2048))
            self.assertEqual(attempt(payload, 4096), {"pack_operation": "file_add", "pack_failure": "compressed_capacity"})
            self.assertEqual(attempt(b"body", 10), {"pack_operation": "gzip_finish", "pack_failure": "compressed_capacity"})
            # The same real gzip footer also fails at cap10 after this earlier
            # file_fact refusal. Its cleanup must not replace the first record.
            self.assertEqual(attempt(b"body", 10, True), {"pack_operation": "file_fact", "pack_failure": "refused"})
        finally:
            _PIPELINE.reset_pack_capture()

    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only DATA pack")
    def test_actual_pack_success_and_fatal_framing_remain_closed_and_reset(self):
        try:
            with TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                source = root / "fixture-source"; source.mkdir(mode=0o700)
                payload = b"owned synthetic DATA"
                path = source / "fixture.data"; path.write_bytes(payload); path.chmod(0o600)
                row = {"path": "fixture.data", "bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest(), "mode": 0o600}
                inventories = {source.name: {"role": "fixture", "files": [row], "directories": ["."]}}
                handoff = {"schema": "synthetic", "roots": inventories}
                output = _PIPELINE.pack_handoff(root, handoff, inventories, _MATERIAL)
                self.assertEqual(_PIPELINE.pack_frame(), {})
                self.assertIs(_PIPELINE.PACK_ACTIVE, False)
                with tarfile.open(output, "r:gz") as archive:
                    self.assertEqual([m.name for m in archive], ["HANDOFF.json", "fixture-source", "fixture-source/fixture.data"])
                    self.assertEqual(json.loads(archive.extractfile("HANDOFF.json").read()), handoff)
                    self.assertEqual(archive.extractfile("fixture-source/fixture.data").read(), payload)
            states = ((["file_fact", "refused"], {}), (("opaque-fixture", "refused"), {}),
                      (("file_fact", "opaque-fixture"), {}), (({"opaque-fixture": 1}, "refused"), {}),
                      (("file_fact", "os_space"), {"pack_operation": "file_fact", "pack_failure": "os_space"}))
            for state, extra in states:
                def fail():
                    _PIPELINE.POST_ASSEMBLY_PHASE = "handoff_pack"
                    _PIPELINE.PACK_FAILURE = state
                    raise RuntimeError("opaque-fixture")
                output = io.StringIO()
                with mock.patch.object(_PIPELINE, "main", side_effect=fail), mock.patch.object(_PIPELINE.sys, "stderr", output):
                    with self.assertRaises(SystemExit) as caught: _PIPELINE.entry()
                self.assertEqual(caught.exception.code, 1)
                self.assertLess(len(output.getvalue()), 256)
                self.assertTrue("opaque-fixture" not in output.getvalue(), "closed pack frame disclosed synthetic input")
                self.assertEqual(json.loads(output.getvalue()), {"native_linux_producer": "failed",
                    "build_or_release_acceptance": False, "category": "handoff_pack", **extra})
            # Entry resets old pack state before any non-pack or new failure.
            for phase in ("final_inventory", None):
                def fail_other():
                    if phase is not None: _PIPELINE.POST_ASSEMBLY_PHASE = phase
                    raise RuntimeError("opaque-fixture")
                output = io.StringIO()
                with mock.patch.object(_PIPELINE, "main", side_effect=fail_other), mock.patch.object(_PIPELINE.sys, "stderr", output):
                    with self.assertRaises(SystemExit): _PIPELINE.entry()
                value = json.loads(output.getvalue())
                self.assertEqual(value["category"], phase or "unknown")
                self.assertNotIn("pack_operation", value)
                self.assertNotIn("pack_failure", value)
        finally:
            _PIPELINE.reset_pack_capture()


class NativeCacheDataBoundaryTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only producer gate")
    def test_runner_requires_owner_original_and_triggering_actor_before_api(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            environment = {
                "GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64",
                "GITHUB_REPOSITORY": "brk275/Morpheus",
                "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/main",
                "GITHUB_ACTOR": "brk275", "GITHUB_TRIGGERING_ACTOR": "brk275",
                "RUNNER_TEMP": str(root),
            }
            repository = {"full_name": "brk275/Morpheus", "private": False, "default_branch": "main"}
            with mock.patch.object(_PIPELINE.os, "uname", return_value=mock.Mock(sysname="Linux", machine="x86_64")), \
                    mock.patch.object(_PIPELINE, "api", return_value=repository) as api:
                with mock.patch.dict(_PIPELINE.os.environ, environment, clear=True):
                    self.assertEqual(_PIPELINE.runner(), root)
                api.assert_called_once_with("/repos/brk275/Morpheus")
                for actor, triggering in (("brk275", "other-maintainer"), ("brk275", None),
                                          ("other-maintainer", "brk275")):
                    with self.subTest(actor=actor, triggering=triggering):
                        selected = environment | {"GITHUB_ACTOR": actor}
                        if triggering is None:
                            selected.pop("GITHUB_TRIGGERING_ACTOR")
                        else:
                            selected["GITHUB_TRIGGERING_ACTOR"] = triggering
                        api.reset_mock()
                        with mock.patch.dict(_PIPELINE.os.environ, selected, clear=True):
                            with self.assertRaises(_PIPELINE.Refused):
                                _PIPELINE.runner()
                        api.assert_not_called()

    def test_flat_unique_response_refuses_nested_duplicate_or_wrong_typed_values(self):
        valid = (b'{"ok":true,"matched_key":"synthetic-key",'
                 b'"signed_download_url":"https://abc.blob.core.windows.net/a?q=1"}')
        self.assertEqual(_PIPELINE.flat_cache_response(valid)["matched_key"], "synthetic-key")
        for raw in (valid.replace(b'"ok":true', b'"ok":true,"ok":false'),
                    valid.replace(b'"matched_key":"synthetic-key"', b'"matched_key":{}'),
                    valid.replace(b'"ok":true', b'"ok":"true"'),
                    valid.replace(b'"ok":true', b'"ok":[' + b'[' * 1000 + b']' * 1000 + b']'),
                    valid + b' trailing'):
            with self.subTest(raw=raw[:24]):
                with self.assertRaises(_PIPELINE.Refused):
                    _PIPELINE.flat_cache_response(raw)

    def test_blob_url_requires_one_ascii_azure_account_and_unambiguous_origin(self):
        _PIPELINE.signed_blob_url("https://abc123.blob.core.windows.net/a?q=1")
        for value in ("https://a.b.blob.core.windows.net/a?q=1",
                      "https://ab.blob.core.windows.net/a?q=1",
                      "https://abc.blob.core.windows.net.evil.test/a?q=1",
                      "https://abc.blob.core.windows.net:0443/a?q=1",
                      "https://user@abc.blob.core.windows.net/a?q=1",
                      "https://abc.blob.core.windows.net/a?q=1#x",
                      "https://abc.blob.core.windows.net\\@evil.test/a?q=1",
                      "https://ébc.blob.core.windows.net/a?q=1"):
            with self.subTest(value=value):
                with self.assertRaises(_PIPELINE.Refused):
                    _PIPELINE.signed_blob_url(value)

    def test_outer_tar_hidden_metadata_has_a_cumulative_budget(self):
        import tarfile
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "outer.tar"
            # Each individually small GNU longname record is legal to parse,
            # but the chain is refused before the eventual file can be yielded.
            header = tarfile.TarInfo("././@LongLink")
            header.type, header.size = tarfile.GNUTYPE_LONGNAME, 2
            file = tarfile.TarInfo("file")
            path.write_bytes((header.tobuf() + b'x\0' + b'\0' * 510) * 80 +
                             file.tobuf() + b'\0' * 1024)
            with self.assertRaises(_PIPELINE.Refused):
                with _PIPELINE.bounded_tar(path) as archive:
                    next(iter(archive))

    def test_outer_payload_is_separately_bounded_and_parser_budget_resumes(self):
        import io
        import tarfile
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "outer.tar"
            payload = b"synthetic" * (256 * 1024)
            with tarfile.open(path, "w", format=tarfile.GNU_FORMAT) as archive:
                info = tarfile.TarInfo("file"); info.size = len(payload)
                archive.addfile(info, io.BytesIO(payload))
            with _PIPELINE.bounded_tar(path) as archive:
                member = next(iter(archive))
                stream = archive.extractfile(member)
                count = 0
                with _PIPELINE.outer_payload(archive):
                    while True:
                        data = stream.read(1024 * 1024)
                        if not data: break
                        count += len(data)
                self.assertEqual(count, len(payload))
                self.assertFalse(archive.fileobj.payload)
                with self.assertRaises(_PIPELINE.Refused):
                    archive.fileobj.read(65536)


_MAC_PATH = _SCRIPT.parent / "native_webrtc/macos_materials.py"
_MAC_SPEC = importlib.util.spec_from_file_location("morpheus_mac_material_tests", _MAC_PATH)
assert _MAC_SPEC is not None and _MAC_SPEC.loader is not None
_MAC = importlib.util.module_from_spec(_MAC_SPEC)
_MAC_SPEC.loader.exec_module(_MAC)


class NativeMacMaterialBoundaryTests(unittest.TestCase):
    def profile(self):
        return {"format": "tar.xz", "members": [
            {"path": "bin", "kind": "directory", "mode": 0o755, "bytes": 0},
            {"path": "bin/tool", "kind": "regular", "mode": 0o755, "bytes": 4,
             "sha256": "a" * 64, "output_mode": 0o700},
            {"path": "bin/alias", "kind": "symlink", "mode": 0o777, "bytes": 0, "linkname": "tool"}],
            "alias_copies": [{"path": "bin/alias", "kind": "symlink", "linkname": "tool", "source": "bin/tool",
                              "bytes": 4, "sha256": "a" * 64, "output_mode": 0o700}]}

    def test_declared_regular_alias_is_a_copy_and_escape_is_refused(self):
        row = self.profile()
        self.assertEqual(set(_MAC.paths(row, _MATERIAL)), {"bin", "bin/tool", "bin/alias"})
        row["members"][-1]["linkname"] = "../../outside"
        row["alias_copies"][0]["linkname"] = "../../outside"
        with self.assertRaises(_MAC.Refused):
            _MAC.paths(row, _MATERIAL)

    def test_unknown_alias_changed_bytes_and_case_collision_are_refused(self):
        for change in ("unknown", "bytes", "case"):
            with self.subTest(change=change):
                row = self.profile()
                if change == "unknown": row["alias_copies"] = []
                elif change == "bytes": row["alias_copies"][0]["sha256"] = "b" * 64
                else: row["members"].append({"path": "BIN", "kind": "directory", "mode": 0o755, "bytes": 0})
                with self.assertRaises(_MAC.Refused):
                    _MAC.paths(row, _MATERIAL)

    def test_real_mac_profiles_validate_original_runtime_archives_and_app_roles(self):
        python = _PIPELINE.module("python_stager.py")
        for target in ("mac-arm64", "mac-x64"):
            with self.subTest(target=target):
                path = _SCRIPT.parents[1] / "native/webrtc/macos" / target / "selected-mac-materials.json"
                plan = json.loads(path.read_bytes())
                self.assertIs(_MAC.validate(plan, target, _MATERIAL, python), plan)
                self.assertEqual(len(plan["materials"]), 35)
                self.assertEqual(sum(row["seed_cache"] for row in plan["materials"]), 30)
                self.assertEqual(plan["roles"]["vpython3"]["path"],
                    "runtime/vpython/vpython3.app/Contents/MacOS/vpython3")
                # Recognize the actual client family while preserving its exact
                # selected platform and pinned revision, plus the source binding.
                for package, version in (("infra/tools/cipd/mac-other", None),
                                         ("infra/tools/foreign/mac-arm64", None),
                                         (None, "git_revision:" + "0" * 40)):
                    changed = json.loads(json.dumps(plan))
                    client = next(row for row in changed["materials"] if row["name"] == "cipd-client")
                    if package is not None:
                        client["package"] = client["source"]["package"] = package
                    if version is not None:
                        client["version"] = client["source"]["version"] = version
                    with self.subTest(client_package=package, client_version=version), self.assertRaises(_MAC.Refused):
                        _MAC.validate(changed, target, _MATERIAL, python)
                plan["roles"]["vpython3"]["path"] = "runtime/vpython/vpython3"
                with self.assertRaises(_MAC.Refused):
                    _MAC.validate(plan, target, _MATERIAL, python)

    def test_real_mac_llvm_addon_pins_and_original_regular_copy_are_required(self):
        python = _PIPELINE.module("python_stager.py")
        pins = {
            "mac-arm64": ("Mac_arm64", "cace63bdf68ff4be9680c497a9d15482f52fe01ebeb0de65dce1828e5628df12", 5350540, 1763731858865476),
            "mac-x64": ("Mac", "65c229f8ac773e258bcf4ba67038d8db9f9100fea063c60263d7d37a8cee2374", 5611140, 1763731833330327),
        }
        for target, (prefix, digest, size, generation) in pins.items():
            with self.subTest(target=target):
                path = _SCRIPT.parents[1] / "native/webrtc/macos" / target / "selected-mac-materials.json"
                plan = json.loads(path.read_bytes())
                _MAC.validate(plan, target, _MATERIAL, python)
                row = next(r for r in plan["materials"] if r["name"] == "llvmobjdump")
                self.assertEqual(plan["schema"], "morpheus-native-macos-materials-v2")
                self.assertEqual((row["sha256"], row["bytes"], row["source"]["generation"]), (digest, size, generation))
                self.assertEqual(row["source"], {"kind": "gcs", "bucket": "chromium-browser-clang",
                    "object": prefix + "/llvmobjdump-llvmorg-22-init-14273-gea10026b-2.tar.xz", "generation": generation})
                self.assertEqual(len(row["members"]), 8)
                self.assertEqual(len(row["alias_copies"]), 1)
                copy = row["alias_copies"][0]
                regular = next(m for m in row["members"] if m["path"] == "bin/llvm-objdump")
                self.assertEqual((copy["path"], copy["source"], copy["linkname"]),
                                 ("bin/llvm-otool", "bin/llvm-objdump", "llvm-objdump"))
                self.assertEqual((copy["bytes"], copy["sha256"], copy["output_mode"]),
                                 (regular["bytes"], regular["sha256"], regular["output_mode"]))
                for field in ("missing", "platform", "generation", "alias"):
                    changed = json.loads(json.dumps(plan))
                    addon = next(r for r in changed["materials"] if r["name"] == "llvmobjdump")
                    if field == "missing": changed["materials"].remove(addon)
                    elif field == "platform": addon["source"]["object"] = "Linux_x64/" + addon["source"]["object"].split("/", 1)[1]
                    elif field == "generation": addon["source"]["generation"] += 1
                    else: addon["alias_copies"][0]["linkname"] = "../../outside"
                    with self.subTest(refusal=field), self.assertRaises(_MAC.Refused):
                        _MAC.validate(changed, target, _MATERIAL, python)


    def test_llvm_addon_projects_real_bytes_without_overwrite_or_marker_crossing(self):
        payload = b"synthetic original addon"
        digest = hashlib.sha256(payload).hexdigest()
        name = "runtime/llvmobjdump/bin/llvm-objdump"
        row = {"path": name, "bytes": len(payload), "sha256": digest, "mode": 0o700}
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            source = root / "source"; source.mkdir(mode=0o700)
            runtime = root / "runtime-data"
            original = runtime / name; original.parent.mkdir(mode=0o700, parents=True)
            original.write_bytes(payload); original.chmod(0o700)
            overlay = _MAC.project(source, {}, {"."}, [], (runtime, {}), (runtime, {name: row}), "mac-arm64", _MATERIAL)
            output = "third_party/llvm-build/Release+Asserts/bin/llvm-objdump"
            self.assertEqual(overlay, [{"path": output, "type": "file", "mode": "100755", "bytes": len(payload), "sha256": digest}])
            self.assertEqual((source / "checkout/src" / output).read_bytes(), payload)
            self.assertFalse((source / "checkout/src" / output).is_symlink())
            for marker in ("src/third_party/llvm-build/Release+Asserts", "src/third_party/llvm-build/Release+Asserts/bin/llvm-objdump"):
                with self.subTest(marker=marker), mock.patch.object(_MATERIAL, "file_fact", side_effect=AssertionError("copy before marker check")):
                    with self.assertRaises(_MAC.Refused):
                        _MAC.project(source, {}, {"."}, [marker], (runtime, {}), (runtime, {name: row}), "mac-arm64", _MATERIAL)
            with mock.patch.object(_MATERIAL, "file_fact", side_effect=AssertionError("copy before original collision check")):
                with self.assertRaises(_MAC.Refused):
                    _MAC.project(source, {"checkout/src/" + output: row}, {"."}, [], (runtime, {}), (runtime, {name: row}), "mac-arm64", _MATERIAL)


    def test_zip_implicit_parents_are_derived_without_accepting_aliases_or_file_parents(self):
        member = {"path": "app/Contents/bin/tool", "kind": "regular", "mode": 0o100755,
                  "bytes": 4, "sha256": "a" * 64, "output_mode": 0o700}
        row = {"format": "zip", "members": [member], "alias_copies": []}
        self.assertEqual(_MAC.paths(row, _MATERIAL), {member["path"]: member})
        for extra in ({**member, "path": "app"}, {**member, "path": "APP/other"},
                      {**member, "path": "app/../escape"}):
            with self.subTest(path=extra["path"]), self.assertRaises((_MAC.Refused, _MATERIAL.Refused)):
                _MAC.paths({**row, "members": [member, extra]}, _MATERIAL)
        # The same omission is not silently generalized to tar inventories.
        with self.assertRaises(_MAC.Refused):
            _MAC.paths({**row, "format": "tar.xz"}, _MATERIAL)

    def test_tar_exact_pax_data_is_checked_before_writing_its_payload(self):
        payload = b"synthetic bounded archive body"
        path = "bin/" + "x" * 110
        parent = {"path": "bin", "kind": "directory", "mode": 0o755, "bytes": 0,
                  "pax_headers": {"path": "bin/"}}
        member = {"path": path, "kind": "regular", "mode": 0o644, "bytes": len(payload),
                  "sha256": hashlib.sha256(payload).hexdigest(), "output_mode": 0o600,
                  "pax_headers": {"path": path, "mtime": "1762913607.25"}}
        row = {"format": "tar.xz", "members": [parent, member], "alias_copies": []}
        transport = _PIPELINE.module("linux_stager.py")
        class Stage:
            def __init__(self): self.files = []
            def create(self, path):
                path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                self.files.append(path)
                return path.open("xb")
            def parents(self, path): path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            def bytes(self, path, raw):
                with self.create(path) as stream: stream.write(raw)
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            archive = root / "synthetic.tar.xz"
            with tarfile.open(archive, "w:xz", format=tarfile.PAX_FORMAT) as output:
                info = tarfile.TarInfo("bin"); info.type = tarfile.DIRTYPE; info.mode = 0o755
                info.pax_headers = dict(parent["pax_headers"])
                output.addfile(info)
                info = tarfile.TarInfo(path); info.mode = 0o644; info.size = len(payload)
                info.pax_headers = dict(member["pax_headers"])
                output.addfile(info, io.BytesIO(payload))
            destination = root / "good"
            _MAC.extract(Stage(), row, archive, destination, _MATERIAL, transport)
            self.assertEqual((destination / path).read_bytes(), payload)
            wrong = json.loads(json.dumps(row)); wrong["members"][1]["pax_headers"]["mtime"] = "1762913608.25"
            with self.assertRaises(_MAC.Refused):
                _MAC.extract(Stage(), wrong, archive, root / "bad", _MATERIAL, transport)
            self.assertFalse((root / "bad" / path).exists())
        for fields in ({"path": "../escape"}, {"path": "bin/other"}, {"path": path + "/"},
                       {"linkpath": "bin/tool"},
                       {"size": "1"}, {"GNU.sparse.name": path}, {"mtime": 5}, {"mtime": "NaN"}):
            with self.subTest(keys=tuple(fields)), self.assertRaises(_MAC.Refused):
                _MAC.paths({**row, "members": [parent, {**member, "pax_headers": fields}]}, _MATERIAL)
        for value in ("bin//", "../bin/", "/bin/", "bin/../elsewhere/", "other/"):
            with self.subTest(directory_pax=value), self.assertRaises(_MAC.Refused):
                _MAC.paths({**row, "members": [{**parent, "pax_headers": {"path": value}}, member]}, _MATERIAL)

    def test_projection_refuses_original_gitlink_scope_before_copy(self):
        with TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            files = {"tools/gn/gn": {"path": "tools/gn/gn", "bytes": 4,
                                    "sha256": "a" * 64, "mode": 0o700}}
            for marker in ("src/buildtools/mac", "src/buildtools/mac/gn", "src/buildtools/mac/gn/child"):
                with self.subTest(marker=marker):
                    with mock.patch.object(_MATERIAL, "file_fact", side_effect=AssertionError("copy before marker check")):
                        with self.assertRaises(_MAC.Refused):
                            _MAC.project(base, {}, {"."}, [marker], (base, files), (base, {}), "mac-arm64", _MATERIAL)

    def test_mac_args_retain_both_real_declared_deployment_settings_and_native_targets(self):
        build = object.__new__(_BUILDER.Build)
        build.target_os, build.cpu = "mac", "arm64"
        args = build.args(False)
        self.assertIn('mac_deployment_target="10.15"', args)
        self.assertIn('mac_min_system_version="10.15"', args)
        self.assertIn('rtc_include_tests=false', args)
        self.assertIn('rtc_include_tests=true', build.args(True))
        self.assertEqual(build.production_targets(), [":default"] + list(_BUILDER.MAC_TARGETS))
        self.assertEqual(len(_BUILDER.TESTS), 12)

    def test_apple_sdk_material_cannot_be_copied_into_package(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            sdk = root / "sdk"; sdk.mkdir(); (sdk / "header.h").write_bytes(b"synthetic SDK")
            build = object.__new__(_BUILDER.Build)
            build.target_os, build.roots = "mac", {"sdk": sdk}
            with self.assertRaises(_BUILDER.Refused):
                build.copy(sdk / "header.h", root / "redistributed.h")
            self.assertFalse((root / "redistributed.h").exists())




@unittest.skipUnless(os.name == "posix" and hasattr(os, "waitid"), "Owned Unix process contract")
class NativeUnixOwnedCommandTests(unittest.TestCase):
    def fixture(self, root):
        root.chmod(0o700)
        (root / "logs").mkdir(mode=0o700)
        build = object.__new__(_BUILDER.Build)
        build.work, build.src, build.target, build.commands = root, root, "linux-x64", []
        build.env = {"PATH": str(Path(sys.executable).parent), "HOME": str(root),
                     "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1"}
        return build

    def test_success_retains_both_streams_and_proves_owned_group_join(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            build = self.fixture(root)
            log = build.command("ordinary", [sys.executable, "-I", "-B", "-c",
                                "import os; os.write(1,b'output'); os.write(2,b'error')"], 5)
            self.assertEqual(log.read_bytes(), b"outputerror")
            self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)
            capture = build.commands[0]["process_capture"]
            self.assertIs(capture["original_joined"], True)
            self.assertEqual(capture["process_contract"], "trusted_git_fresh_process_group")
            self.assertEqual(capture["stdout_bytes"] + capture["stderr_bytes"], log.stat().st_size)
            self.assertIsNone(build._native_command_runner.UNRETIRED_COMMAND)

    def test_successful_leader_cannot_leave_its_inherited_group_running(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            build = self.fixture(root)
            child = ("import pathlib,sys,time; pathlib.Path(sys.argv[1]).write_bytes(b'ready'); "
                     "time.sleep(0.4); pathlib.Path(sys.argv[2]).write_bytes(b'unretired')")
            leader = ("import pathlib,subprocess,sys,time; "
                      "subprocess.Popen([sys.executable,'-I','-B','-c',sys.argv[1],sys.argv[2],sys.argv[3]]); "
                      "deadline=time.monotonic()+3; "
                      "\nwhile not pathlib.Path(sys.argv[2]).exists():\n"
                      " if time.monotonic()>=deadline: raise SystemExit(2)\n time.sleep(0.01)\n")
            ready, late = root / "ready", root / "unretired"
            build.command("leader", [sys.executable, "-I", "-B", "-c", leader, child,
                                     str(ready), str(late)], 5)
            self.assertTrue(ready.is_file())
            time.sleep(0.6)
            self.assertFalse(late.exists(), "successful leader left its compiler child running")
            self.assertIsNone(build._native_command_runner.UNRETIRED_COMMAND)

    def test_output_limits_apply_before_writes_and_timeout_retires_the_group(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            build = self.fixture(root)
            runner = build.native_module("git_source_stager.py", _BUILDER.COMMAND_PROCESS_HELPER_SHA)
            build._native_command_runner = runner
            # Exercise the real readers and owner at a small synthetic budget;
            # production keeps the independently fixed256MiB combined limit.
            with mock.patch.object(_BUILDER, "MAX_LOG", 1024), \
                    mock.patch.object(runner, "MAX_METADATA", 1024):
                for fd in (1, 2):
                    label = "overflow-" + str(fd)
                    with self.subTest(fd=fd), self.assertRaises(runner.Refusal) as caught:
                        build.command(label, [sys.executable, "-I", "-B", "-c",
                            "import os; os.write(" + str(fd) + ",b'x'*4096)"], 5)
                    self.assertEqual(caught.exception.category, "command_output")
                    self.assertLessEqual(sum(path.stat().st_size for path in
                        (root / "logs").glob(label + ".capture.*")), 1024)
                    self.assertIsNone(runner.UNRETIRED_COMMAND)
                with self.assertRaises(runner.Refusal) as caught:
                    build.command("timeout", [sys.executable, "-I", "-B", "-c",
                                               "import time; time.sleep(30)"], 0.05)
                self.assertEqual(caught.exception.category, "command_timeout")
                self.assertIsNone(runner.UNRETIRED_COMMAND)
            self.assertEqual(build.commands, [])


class NativeMacPreflightDiscoveryTests(unittest.TestCase):
    def entry_result(self, scenario):
        # Execute the unchanged entry functions against controlled in-memory
        # filesystem DATA. No actual Xcode/SDK/host tool is read or executed.
        script = r"""
const realFs = require('node:fs');
const path = require('node:path').posix;
const crypto = require('node:crypto');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const source = realFs.readFileSync(process.argv[1], 'utf8');
const scenario = process.argv[2];
const bad = '/Applications/Xcode_bad.app', good = '/Applications/Xcode_good.app';
const sdkRoot = app => app + '/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs';
const version = app => app + '/Contents/version.plist';
const metadata = app => sdkRoot(app) + '/MacOSX15.1.sdk/System/Library/CoreServices/SystemVersion.plist';
const python = '/opt/hostedtoolcache/Python/3.13.7/arm64/bin/python3.13';
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const plan = {schema: 'morpheus-native-macos-pipeline-v1', target: 'mac-arm64',
  host_python: {path: python, bytes: 4, sha256: hash(Buffer.from('data'))}};
const planPath = '/synthetic/native/webrtc/macos/mac-arm64/pipeline-plan.json';
const files = new Map([[planPath, Buffer.from(JSON.stringify(plan))], [python, Buffer.from('data')]]);
for (const role of ['git', 'xcodebuild', 'xcrun', 'xcode-select', 'sw_vers']) files.set('/usr/bin/' + role, Buffer.from('data'));
for (const app of [bad, good]) { files.set(version(app), Buffer.from('version')); files.set(metadata(app), Buffer.from('sdk')); }
const descriptors = new Map(); let nextFd = 10, directoryReads = 0;
function info(name, second = false) {
  const directory = name === bad || name === good;
  const bytes = files.get(name);
  assert.ok(directory || bytes, 'unknown synthetic file');
  const isVersion = name === version(bad);
  const uid = (scenario === 'version_uid' && isVersion || scenario === 'sdk_metadata' && name === metadata(bad) ||
    scenario === 'required_tool' && name === '/usr/bin/git' || scenario === 'stage_fact' && name === python ||
    scenario === 'full_inventory' && name === bad) ? 501 : 0;
  return {isFile: () => !directory && !(scenario === 'version_nonregular' && isVersion),
    isDirectory: () => directory, isSymbolicLink: () => false, uid,
    mode: directory ? 0o40755 : scenario === 'version_mode' && isVersion ? 0o100666 : 0o100644,
    size: directory ? 0 : scenario === 'version_size' && isVersion ? 1048577 : bytes.length,
    dev: 1, ino: scenario === 'version_identity' && isVersion && second ? 2 : 1, mtimeMs: 1, ctimeMs: 1};
}
const fixtureFs = {
  constants: realFs.constants,
  realpathSync(name) {
    if (scenario === 'xcode_root' && name === bad) throw new Error('opaque-fixture');
    return name;
  },
  openSync(name, flags) {
    assert.ok(flags & realFs.constants.O_NOFOLLOW);
    assert.ok(files.has(name));
    const fd = nextFd++; descriptors.set(fd, {name, offset: 0, stats: 0}); return fd;
  },
  fstatSync(fd) { const owner = descriptors.get(fd); assert.ok(owner); return info(owner.name, ++owner.stats > 1); },
  statSync(name) { return info(name); },
  lstatSync(name) { return info(name); },
  closeSync(fd) { assert.ok(descriptors.delete(fd)); },
  readSync(fd, buffer) {
    const owner = descriptors.get(fd), bytes = files.get(owner.name);
    const size = Math.min(buffer.length, bytes.length - owner.offset);
    bytes.copy(buffer, 0, owner.offset, owner.offset + size); owner.offset += size; return size;
  },
  readFileSync(fd) { const owner = descriptors.get(fd); assert.ok(owner); return Buffer.from(files.get(owner.name)); },
  readdirSync(name) {
    directoryReads++;
    if (name === '/Applications') return ['Xcode_bad.app', 'Xcode_good.app'];
    if (name === sdkRoot(bad)) {
      if (scenario === 'sdk_listing') throw new Error('opaque-fixture');
      if (scenario === 'sdk_limit') return Array.from({length: 33}, (_, i) => 'MacOSX15.' + i + '.sdk');
      return ['MacOSX15.1.sdk'];
    }
    if (name === sdkRoot(good)) return ['MacOSX15.1.sdk'];
    if (['/Library/Frameworks/Python.framework/Versions', '/opt/hostedtoolcache/Python'].includes(name)) return [];
    throw new Error('unexpected-read');
  },
  existsSync() { return false; },
};
const output = [], errors = [];
const env = {INPUT_TARGET: 'mac-arm64', INPUT_PHASE: scenario === 'stage_fact' ? 'stage' : 'preflight',
  INPUT_PUBLISH_HANDOFF: 'false', INPUT_XCODE_APP: scenario === 'full_inventory' ? 'Xcode_bad.app' : '',
  INPUT_PLAN_SHA256: hash(files.get(planPath)), GITHUB_ACTIONS: 'true', RUNNER_ENVIRONMENT: 'github-hosted',
  GITHUB_REPOSITORY: 'brk275/Morpheus', GITHUB_REF: 'refs/heads/main', GITHUB_ACTOR: 'brk275',
  GITHUB_TRIGGERING_ACTOR: 'brk275', GITHUB_EVENT_NAME: 'workflow_dispatch', GITHUB_WORKSPACE: '/synthetic'};
const processFixture = {platform: 'darwin', arch: 'arm64', env,
  stderr: {write: value => errors.push(value)}, execve() { throw new Error('unexpected-execution'); }};
vm.runInNewContext(source, {Buffer, process: processFixture,
  console: {log: value => output.push(value)},
  require(name) { if (name === 'node:fs') return fixtureFs; if (name === 'node:path') return path;
    if (name === 'node:crypto') return crypto; throw new Error('unexpected-module'); }}, {timeout: 2000});
assert.equal(descriptors.size, 0);
assert.ok(!JSON.stringify({output, errors}).includes('opaque-fixture'));
console.log(JSON.stringify({output, errors, exitCode: processFixture.exitCode || 0, directoryReads}));
"""
        import subprocess
        entry = _SCRIPT.parent / "native_webrtc_macos/entry.cjs"
        environment = {key: value for key, value in os.environ.items()
                       if key in ("PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "TEMP", "TMP")}
        result = subprocess.run(["node", "-e", script, str(entry), scenario],
                                capture_output=True, text=True, timeout=10, env=environment)
        self.assertEqual(result.returncode, 0, "controlled entry regression failed")
        self.assertEqual(result.stderr, "")
        return json.loads(result.stdout)

    def test_optional_xcode_identities_are_unbound_and_other_candidates_continue(self):
        for scenario in ("version_uid", "version_mode", "version_nonregular", "version_size", "version_identity",
                         "xcode_root", "sdk_listing", "sdk_metadata", "sdk_limit", "valid"):
            with self.subTest(scenario=scenario):
                result = self.entry_result(scenario)
                self.assertEqual((result["exitCode"], result["errors"]), (0, []))
                self.assertEqual(len(result["output"]), 1)
                prefix = "MORPHEUS_NATIVE_MAC_HOST_INPUTS "
                self.assertTrue(result["output"][0].startswith(prefix))
                facts = json.loads(result["output"][0][len(prefix):])
                for flag in ("sdk_redistributed", "cache_write", "staged", "build_admission_granted",
                             "native_sources_complete", "build_ready", "distribution_ready"):
                    self.assertIs(facts[flag], False)
                self.assertEqual(facts["sdk_full_inventory"], "UNBOUND")
                self.assertEqual(len(facts["xcode_candidates"]), 2)
                self.assertLessEqual(len(facts["sdk_candidates"]), 32)
                bad, good = facts["xcode_candidates"]
                self.assertEqual((bad["app"], good["app"]), ("Xcode_bad.app", "Xcode_good.app"))
                self.assertEqual(good["failures"], [])
                self.assertTrue(any(row["app"] == "Xcode_good.app" for row in facts["sdk_metadata"]))
                if scenario == "valid":
                    self.assertEqual(len(facts["sdk_metadata"]), 2)
                    self.assertTrue(all(row["admissible_root_owned_identity"] for row in facts["sdk_candidates"]))
                else:
                    self.assertEqual([row["app"] for row in facts["sdk_metadata"]], ["Xcode_good.app"])
                if scenario.startswith("version_"):
                    self.assertEqual(bad["version_plist"], "UNBOUND")
                    self.assertEqual(bad["failures"], ["preflight_xcode_version"])
                    self.assertEqual(bad["sdk_listing"], "observed")
                    row = next(row for row in facts["sdk_candidates"] if row["app"] == "Xcode_bad.app")
                    self.assertEqual(row["category"], "preflight_xcode_version")
                    self.assertIs(row["admissible_root_owned_identity"], False)
                    self.assertNotEqual(row["sdk_system_version_plist"], "UNBOUND")
                elif scenario == "xcode_root":
                    self.assertEqual(bad["failures"], ["preflight_xcode_root"])
                elif scenario in ("sdk_listing", "sdk_limit"):
                    self.assertEqual(bad["sdk_listing"], "UNBOUND")
                    self.assertEqual(bad["failures"], ["preflight_sdk_listing"])
                elif scenario == "sdk_metadata":
                    row = next(row for row in facts["sdk_candidates"] if row["app"] == "Xcode_bad.app")
                    self.assertEqual(row["sdk_system_version_plist"], "UNBOUND")
                    self.assertEqual(row["category"], "preflight_sdk_metadata")
                    self.assertIs(row["admissible_root_owned_identity"], False)
                self.assertNotIn("opaque-fixture", json.dumps(facts))

    def test_required_tools_explicit_inventory_and_execution_facts_remain_fatal(self):
        for scenario, category in (("required_tool", "preflight_tools"),
                                   ("full_inventory", "preflight_sdk_inventory"),
                                   ("stage_fact", "python_admission")):
            with self.subTest(scenario=scenario):
                result = self.entry_result(scenario)
                self.assertEqual((result["exitCode"], result["output"]), (1, []))
                self.assertEqual(result["errors"], ["MORPHEUS_NATIVE_MAC_PRODUCER_ENTRY failed category=" + category + "\n"])
                if scenario == "stage_fact":
                    self.assertEqual(result["directoryReads"], 0)

class NativeMacSdkIntakeTests(unittest.TestCase):
    def intake_result(self, scenario, target="mac-arm64"):
        # Source-reviewed real entry against complete synthetic host DATA. No
        # native tool, installed SDK, network or owner filesystem is accessed.
        script = r"""
const fs = require('node:fs'), path = require('node:path').posix, crypto = require('node:crypto');
const vm = require('node:vm'), assert = require('node:assert/strict');
let source = fs.readFileSync(process.argv[1], 'utf8');
const scenario = process.argv[2], target = process.argv[3];
if (scenario === 'metadata_limit') {
  const fixed = 'metadata_bytes: 128 * 1024 ** 2'; assert.equal(source.split(fixed).length, 2);
  source = source.replace(fixed, 'metadata_bytes: 500');
}
const root = '/Applications/Xcode_fixture.app', planPath = '/synthetic/native/webrtc/macos/' + target + '/pipeline-plan.json';
const tree = new Map(); let inode = 1, outsideReads = 0, walks = 0, payloadReads = 0, calls = 0;
function add(name, type, bytes = '') { tree.set(name, {type, data: Buffer.from(bytes), ino: inode++}); }
add(root, 'directory');
function file(name, bytes) {
  let here = path.dirname(name); const parents = [];
  while (here !== root && here.startsWith(root + '/')) { parents.push(here); here = path.dirname(here); }
  for (const parent of parents.reverse()) if (!tree.has(parent)) add(parent, 'directory');
  add(name, 'file', bytes);
}
file(root + '/Contents/version.plist', 'version');
const sdkBase = root + '/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/';
const sdkName = scenario === 'plain' ? 'MacOSX.sdk' : 'MacOSX15.1.sdk';
file(sdkBase + sdkName + '/SDKSettings.plist', 'settings');
if (scenario === 'plain') { add(sdkBase + 'MacOSX15.1.sdk', 'symlink'); tree.get(sdkBase + 'MacOSX15.1.sdk').target = 'MacOSX.sdk'; }
if (scenario === 'component_local') {
  file(sdkBase + 'nested/MacOSX15.1.sdk/SDKSettings.plist', 'nested-settings');
  add(sdkBase + 'nested/directory', 'directory');
  add(sdkBase + 'pivot', 'symlink'); tree.get(sdkBase + 'pivot').target = 'nested/directory';
  add(sdkBase + 'MacOSX15.2.sdk', 'symlink'); tree.get(sdkBase + 'MacOSX15.2.sdk').target = 'pivot/../MacOSX15.1.sdk';
}
file(root + '/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang', 'binary');
add(root + '/Contents/link', 'symlink'); tree.get(root + '/Contents/link').target =
  scenario === 'escape' ? '/private/never-read' : scenario === 'cycle' ? 'link' :
    scenario === 'component_escape' ? 'pivot/../version.plist' : 'version.plist';
if (scenario === 'component_escape') { add(root + '/Contents/pivot', 'symlink'); tree.get(root + '/Contents/pivot').target = '/private/never-read'; }
if (scenario === 'metadata_limit') {
  for (const name of [...tree.keys()]) if (![root, root + '/Contents', root + '/Contents/version.plist'].includes(name)) tree.delete(name);
}
if (scenario === 'fifo') add(root + '/AAA_fifo', 'fifo');
if (scenario === 'oversize') { add(root + '/AAA_large', 'file'); tree.get(root + '/AAA_large').size = 2 * 1024 ** 3 + 1; }
const bytes = new Map([[planPath, Buffer.from(JSON.stringify({schema: 'morpheus-native-macos-pipeline-v1', target}))]]);
for (const role of ['git','xcodebuild','xcrun','xcode-select','sw_vers']) bytes.set('/usr/bin/' + role, Buffer.from('tool'));
const descriptors = new Map(), opened = []; let next = 10;
function info(name, big = false, changed = false) {
  if (name.startsWith('/private/')) { outsideReads++; throw new Error('opaque'); }
  const node = tree.get(name); assert.ok(node || bytes.has(name), 'unknown synthetic node');
  const type = node ? node.type : 'file', uid = node && scenario === 'owner' ? 501 : 0;
  const mode = type === 'directory' ? 0o40755 : type === 'symlink' ? 0o120777 :
    type === 'fifo' ? 0o10600 : scenario === 'mode' && node ? 0o100666 : 0o100644;
  const n = value => big ? BigInt(value) : value;
  return {isFile: () => type === 'file', isDirectory: () => type === 'directory', isSymbolicLink: () => type === 'symlink',
    dev: n(1), ino: n((node ? node.ino : 2000) + (changed ? 1000 : 0)), mode: n(mode), uid: n(uid), gid: n(0),
    nlink: n(1), size: n(node ? node.size || (type === 'file' ? node.data.length : 0) : bytes.get(name).length),
    mtimeNs: n(10), ctimeNs: n(20), mtimeMs: 1, ctimeMs: 1};
}
function names(name) {
  const prefix = name + '/', result = [];
  for (const key of tree.keys()) if (key.startsWith(prefix) && !key.slice(prefix.length).includes('/')) result.push(path.basename(key));
  if (scenario === 'directory_change' && name === root && payloadReads) result.push('added');
  return result;
}
const fake = {constants: fs.constants,
  realpathSync(name) { if (name === '/synthetic' || name === root || bytes.has(name)) return name; throw new Error('opaque'); },
  openSync(name, flags) { assert.ok(flags & fs.constants.O_NOFOLLOW); const node = tree.get(name);
    assert.ok(node || bytes.has(name)); assert.ok(!node || ['file','directory'].includes(node.type));
    const fd = next++; descriptors.set(fd, {name, position: 0}); opened.push(name); return fd; },
  closeSync(fd) { assert.ok(descriptors.delete(fd)); },
  fstatSync(fd, options) { const owner = descriptors.get(fd); assert.ok(owner);
    return info(owner.name, !!(options && options.bigint), scenario === 'identity' && owner.name === root + '/Contents/version.plist' && owner.position > 0); },
  lstatSync(name, options) { return info(name, !!(options && options.bigint)); },
  statSync(name) { return info(name); },
  readFileSync(fd) { const owner = descriptors.get(fd); return Buffer.from(bytes.get(owner.name)); },
  readSync(fd, buffer, offset = 0, length = buffer.length) {
    const owner = descriptors.get(fd), value = bytes.get(owner.name) || tree.get(owner.name).data;
    const count = Math.min(length, value.length - owner.position); value.copy(buffer, offset, owner.position, owner.position + count);
    owner.position += count; if (count && owner.name.startsWith(root)) payloadReads++; return count; },
  readlinkSync(name) { assert.equal(tree.get(name).type, 'symlink'); return tree.get(name).target; },
  opendirSync(name) { assert.equal(tree.get(name).type, 'directory'); const rows = names(name); walks++; let i = 0, closed = false;
    return {readSync() { assert.ok(!closed); return i < rows.length ? {name: rows[i++]} : null; },
      closeSync() { assert.ok(!closed); closed = true; walks--; }}; },
};
const output = [], errors = [];
const env = {INPUT_TARGET: target, INPUT_PHASE: scenario === 'stage_detail' ? 'stage' : 'preflight',
  INPUT_PREFLIGHT_DETAIL: 'host-sdk', INPUT_XCODE_APP: scenario === 'empty' || scenario === 'stage_detail' ? '' :
    scenario === 'traversal' ? '../private.app' : 'Xcode_fixture.app', INPUT_PUBLISH_HANDOFF: 'false',
  GITHUB_ACTIONS: 'true', RUNNER_ENVIRONMENT: 'github-hosted', GITHUB_REPOSITORY: 'brk275/Morpheus',
  GITHUB_REF: 'refs/heads/main', GITHUB_ACTOR: 'brk275', GITHUB_TRIGGERING_ACTOR: 'brk275',
  GITHUB_EVENT_NAME: 'workflow_dispatch', GITHUB_WORKSPACE: '/synthetic'};
const proc = {platform: 'darwin', arch: target === 'mac-arm64' ? 'arm64' : 'x64', env,
  hrtime: {bigint: () => BigInt(calls++)}, stderr: {write: value => errors.push(value)},
  execve() { throw new Error('unexpected-native-execution'); }};
vm.runInNewContext(source, {Buffer, process: proc, console: {log: value => output.push(value)},
  require(name) { if (name === 'node:fs') return fake; if (name === 'node:path') return path;
    if (name === 'node:crypto') return crypto; throw new Error('unexpected-module'); }}, {timeout: 2000});
assert.equal(descriptors.size, 0); assert.equal(walks, 0); assert.equal(outsideReads, 0);
assert.ok(!JSON.stringify({output, errors}).includes('/private/never-read'));
console.log(JSON.stringify({output, errors, exitCode: proc.exitCode || 0,
  selected_file_opened: opened.includes(root + '/Contents/version.plist'),
  oversized_payload_opened: opened.includes(root + '/AAA_large'), fifo_opened: opened.includes(root + '/AAA_fifo')}));
"""
        import subprocess
        entry = _SCRIPT.parent / "native_webrtc_macos/entry.cjs"
        environment = {key: value for key, value in os.environ.items()
                       if key in ("PATH", "SystemRoot", "SYSTEMROOT", "WINDIR", "TEMP", "TMP")}
        result = subprocess.run(["node", "-e", script, str(entry), scenario, target],
                                capture_output=True, text=True, timeout=10, env=environment)
        self.assertEqual(result.returncode, 0, "controlled SDK intake regression failed")
        self.assertTrue(result.stderr == "", "controlled SDK intake child stderr withheld")
        self.assertLess(len(result.stdout), 65536)
        return json.loads(result.stdout)

    def test_selected_sdk_intake_hashes_real_synthetic_material_without_admission(self):
        files = {"Contents/version.plist": b"version",
                 "Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX15.1.sdk/SDKSettings.plist": b"settings",
                 "Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang": b"binary"}
        for target in ("mac-arm64", "mac-x64"):
            for scenario in ("valid", "owner", "mode", "plain", "component_local"):
                with self.subTest(target=target, scenario=scenario):
                    inventory = [{"path": name.replace("MacOSX15.1.sdk/", "MacOSX.sdk/") if scenario == "plain" else name,
                                  "type": "file", "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
                                 for name, raw in files.items()]
                    inventory.append({"path": "Contents/link", "type": "symlink", "target": "version.plist"})
                    if scenario == "plain":
                        inventory.append({"path": "Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX15.1.sdk",
                                          "type": "symlink", "target": "MacOSX.sdk"})
                    if scenario == "component_local":
                        nested = "Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/"
                        inventory.extend([
                            {"path": nested + "nested/MacOSX15.1.sdk/SDKSettings.plist", "type": "file", "bytes": 15,
                             "sha256": hashlib.sha256(b"nested-settings").hexdigest()},
                            {"path": nested + "pivot", "type": "symlink", "target": "nested/directory"},
                            {"path": nested + "MacOSX15.2.sdk", "type": "symlink", "target": "pivot/../MacOSX15.1.sdk"}])
                    inventory.sort(key=lambda row: row["path"].encode("utf-8"))
                    expected = hashlib.sha256((json.dumps(inventory, sort_keys=True, separators=(",", ":")) + "\n").encode()).hexdigest()
                    result = self.intake_result(scenario, target)
                    self.assertEqual((result["exitCode"], result["errors"]), (0, []))
                    self.assertEqual(len(result["output"]), 1)
                    prefix = "MORPHEUS_NATIVE_MAC_SDK_INTAKE "
                    self.assertTrue(result["output"][0].startswith(prefix))
                    value = json.loads(result["output"][0][len(prefix):])
                    self.assertEqual(value["target"], target)
                    intake = value["intake"]
                    self.assertEqual(intake["file_inventory"]["sha256"], expected)
                    self.assertEqual(intake["files"], 5 if scenario == "plain" else 7 if scenario == "component_local" else 4)
                    self.assertEqual(intake["original_file_bytes"], sum(map(len, files.values())) + (15 if scenario == "component_local" else 0))
                    self.assertIs(intake["snapshot_complete"], True)
                    self.assertIs(intake["original_root_owned_nonwritable_policy_observed"], scenario in ("valid", "plain", "component_local"))
                    self.assertEqual(intake["owner_uids"], ["501"] if scenario == "owner" else ["0"])
                    self.assertEqual(intake["provenance"], "UNBOUND")
                    self.assertIs(intake["developer_directory_observed"], True)
                    self.assertEqual(len(intake["sdk_candidates"]), 2 if scenario in ("plain", "component_local") else 1)
                    if scenario == "component_local":
                        candidate = next(row for row in intake["sdk_candidates"] if row["path"].endswith("/MacOSX15.2.sdk"))
                        self.assertEqual(candidate["resolved_path"],
                                         "Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/nested/MacOSX15.1.sdk")
                    self.assertEqual(len(intake["metadata_roles"]), 2)
                    self.assertTrue(any(row["path"].endswith(("MacOSX.sdk/SDKSettings.plist" if scenario == "plain" else
                                                             "MacOSX15.1.sdk/SDKSettings.plist"))
                                        for row in intake["metadata_roles"]))
                    for row in (value, intake):
                        for flag in ("build_admission_granted", "build_ready", "distribution_ready", "sdk_redistributed"):
                            self.assertIs(row[flag], False)
                    self.assertIs(value["cache_write"], False)
                    self.assertIs(intake["sdk_bytes_copied"], False)
                    self.assertEqual(value["sdk_full_inventory"], "UNBOUND")

    def test_intake_refuses_escape_cycles_mutations_specials_caps_and_execution_mode(self):
        for scenario in ("escape", "component_escape", "cycle", "identity", "directory_change", "fifo", "oversize", "metadata_limit", "empty", "traversal", "stage_detail"):
            with self.subTest(scenario=scenario):
                result = self.intake_result(scenario)
                self.assertEqual((result["exitCode"], result["output"]), (1, []))
                category = "producer_input" if scenario == "stage_detail" else "preflight_sdk_intake"
                self.assertEqual(result["errors"], ["MORPHEUS_NATIVE_MAC_PRODUCER_ENTRY failed category=" + category + "\n"])
                self.assertIs(result["oversized_payload_opened"], False)
                self.assertIs(result["fifo_opened"], False)
                if scenario == "metadata_limit":
                    self.assertIs(result["selected_file_opened"], False)


if __name__ == "__main__":
    unittest.main()
