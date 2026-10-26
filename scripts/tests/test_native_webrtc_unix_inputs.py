"""Input traversal regressions for the native Unix builder; synthetic files only."""
from pathlib import Path
import hashlib
import io
import json
import importlib.util
import os
import sys
from tempfile import TemporaryDirectory
import unittest
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


if __name__ == "__main__":
    unittest.main()
