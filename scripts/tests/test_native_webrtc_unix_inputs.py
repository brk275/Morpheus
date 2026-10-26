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
                self.assertEqual(len(plan["materials"]), 34)
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

if __name__ == "__main__":
    unittest.main()
