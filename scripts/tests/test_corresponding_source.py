"""Bounded source/archive checks with mocked Git; no child, hooks or downloads."""

import gzip
import io
import json
import os
import struct
from pathlib import Path
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "packaging/linux"))
import corresponding_source as S
import source_bundle_qa as Q
from scripts import package as P


def crate(members):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for name, data, link in members:
            info = tarfile.TarInfo(name)
            if link:
                info.type, info.linkname = tarfile.SYMTYPE, link
            else:
                info.size = len(data)
            archive.addfile(info, None if link else io.BytesIO(data))
    return output.getvalue()


def native_fixture(root):
    """Synthetic byte identities only; no upstream archive, build or app runs."""
    root = Path(root)
    runtime_dir, pipewire_dir = root / "runtime", root / "pipewire"
    runtime_dir.mkdir(mode=0o700)
    pipewire_dir.mkdir(mode=0o700)
    runtime_files, pipewire_files, files = {}, {}, {}
    archives = {}
    for component in ("runtime", "squashfuse", "musl", "fuse", "zlib", "zstd", "mimalloc"):
        name = component + ".tar.gz"
        data, license_text = ("synthetic source " + component).encode(), ("license " + component).encode()
        runtime_files[name], runtime_files[component + "-LICENSE"] = data, license_text
        archives[component] = {"filename": name, "size": len(data), "sha256": S.digest(data),
                               "licenses": {"LICENSE": {"bytes": len(license_text), "sha256": S.digest(license_text)}}}
    uapi = b"synthetic SPDX header\n"
    host = {"packages": [], "files": {"/usr/include/linux/fixture.h": {"bytes": len(uapi), "sha256": S.digest(uapi)}}}
    host_bytes = json.dumps(host, sort_keys=True).encode()
    patch, regression = b"synthetic patch", b"synthetic C fixture"
    spec = {"schema": 1, "archives": archives, "host_manifest_sha256": S.digest(host_bytes),
            "kernel_headers": {"linux/fixture.h": "/usr/include/linux/fixture.h"},
            "patches": {"runtime.patch": {"size": len(patch), "sha256": S.digest(patch)}},
            "regressions": {"type2-runtime-path-regression.c": {"size": len(regression), "sha256": S.digest(regression)}},
            "runtime_revision": "a" * 40, "version_label": "synthetic-runtime", "security_backports": {}}
    spec_bytes = json.dumps(spec, sort_keys=True).encode()
    recipe = ('MANIFEST_SHA256 = "' + S.digest(spec_bytes) + '"\n'
              'client.write_new(stage / "RELINKING.txt", ("synthetic relink explanation").encode())\n').encode()
    for name, value in (("runtime-build.json", spec_bytes), ("runtime-build-host.json", host_bytes), ("appimage_runtime_build.py", recipe)):
        files["packaging/linux/" + name] = value
        runtime_files[name] = value
    for name, value in (("runtime.patch", patch), ("type2-runtime-path-regression.c", regression)):
        files["packaging/linux/runtime-patches/" + name] = value
        runtime_files[name] = value
    headers = io.BytesIO()
    with tarfile.open(fileobj=headers, mode="w") as archive:
        member = tarfile.TarInfo("linux/fixture.h")
        member.mode, member.size = 0o644, len(uapi)
        archive.addfile(member, io.BytesIO(uapi))
    runtime_files["linux-uapi-headers.tar.gz"] = gzip.compress(headers.getvalue(), mtime=0)
    relink = bytearray(64)
    relink[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HH", relink, 16, 1, 62)
    runtime_files["runtime-relink.o.gz"] = gzip.compress(bytes(relink), mtime=0)
    runtime_files["RELINKING.txt"] = b"synthetic relink explanation"
    prefix = bytearray(1024)
    prefix[:7], prefix[8:11] = b"\x7fELF\x02\x01\x01", b"AI\x02"
    struct.pack_into("<HHI", prefix, 16, 3, 62, 1)
    struct.pack_into("<QQ", prefix, 32, 64, 256)
    struct.pack_into("<HHHHHH", prefix, 52, 64, 56, 2, 64, 3, 1)
    struct.pack_into("<IIQQQQQQ", prefix, 64, 1, 6, 0, 0, 0, 1024, 1024, 4096)
    struct.pack_into("<IIQQQQQQ", prefix, 120, 0x6474e551, 6, 0, 0, 0, 0, 0, 16)
    strings = b"\0.shstrtab\0.digest_md5\0"
    prefix[512:512 + len(strings)] = strings
    struct.pack_into("<IIQQQQIIQQ", prefix, 320, 1, 3, 0, 0, 512, len(strings), 0, 0, 1, 0)
    struct.pack_into("<IIQQQQIIQQ", prefix, 384, 11, 1, 2, 192, 192, 16, 0, 0, 1, 0)
    identity = {"sha256": S.digest(prefix), "size": len(prefix), "static_pie": True,
                "needed": [], "interpreter": None, "md5_slot": [192, 16]}
    prefix[192:208] = b"m" * 16
    embedded = {"runtime_sha256": identity["sha256"], "embedded_prefix_sha256": S.digest(prefix),
                "runtime_bytes": len(prefix), "only_mutable_region": ".digest_md5:16",
                "code_and_all_other_runtime_bytes_exact": True}
    runtime = {"schema": 1, "runtime": identity, "runtime_revision": spec["runtime_revision"],
               "version_label": spec["version_label"], "sources": archives, "patches": spec["patches"],
               "security_backports": {}, "host_inputs": {"manifest_sha256": S.digest(host_bytes), "packages": [],
                   "hashed_inputs": 1, "actual_bytes_match_pinned_packages": True,
                   "base_interpreter_and_coreutils": "standard_fresh_ubuntu_host_not_a_bit_reproducibility_claim"},
               "tool_version_output_sha256": {name: "b" * 64 for name in ("gcc-13", "make", "ar", "ranlib", "strip", "readelf", "patch", "meson", "ninja", "autoreconf", "pkg-config")},
               "runtime_object_sha256": S.digest(relink), "link_map_sha256": "c" * 64,
               "malformedtest_returned_zero": True, "runtime_path_regression_returned_zero": True,
               "runtime_path_regression": spec["regressions"]["type2-runtime-path-regression.c"],
               "malformedtest_sha256": "d" * 64,
               "commands": [{"stage": "runtime-compile", "argv": ["/usr/bin/cc"], "cwd": "$BUILD", "maximum_seconds": 1, "maximum_output": 1024}],
               "linked_inputs": [{"path": "$BUILD/runtime.o", "sha256": S.digest(relink), "bytes": len(relink)}],
               "fresh_private_prefix": True, "upstream_runtime_binary_downloaded": False,
               "reproducible_build_proof": False, "distribution_ready": False}
    for name in ("libsquashfuse.a", "libsquashfuse_ll.a", "libzstd.a", "libz.a", "libfuse3.a", "libmimalloc.a", "libc.a", "rcrt1.o"):
        runtime["linked_inputs"].append({"path": "$BUILD/prefix/lib/" + name, "sha256": "e" * 64, "bytes": 32})
    runtime_files["SOURCE.json"] = json.dumps(runtime, sort_keys=True).encode()
    archive_pins = {}
    for kind in ("dsc", "original", "debian"):
        name, data = "pipewire-" + kind + ".source", ("synthetic pipewire " + kind).encode()
        pipewire_files[name] = data
        archive_pins[kind] = (name, S.digest(data), 1024)
    pipewire_files.update({"LICENSE": b"synthetic MIT license", "COPYING": b"synthetic license notices", "exported-node-startup.patch": b"synthetic upstream change"})
    patch_sha = S.digest(pipewire_files["exported-node-startup.patch"])
    pipewire_recipe = ('VERSION = "1.0.5-fixture"\nCOMMIT = "' + "f" * 40 + '"\nUPSTREAM_PATCH_SHA256 = "' + patch_sha + '"\nPATCHED_SOURCE_SHA256 = "' + "1" * 64 + '"\nCONFIGURE = ("--fixture",)\nSOURCES = ("impl-node",)\nARCHIVES = ' + repr(archive_pins) + '\n').encode()
    files["scripts/pipewire_startup_probe.py"] = pipewire_recipe
    files["packaging/linux/appimage_client.py"] = ('LICENSES = ' + repr({name: S.digest(pipewire_files[name]) for name in ("LICENSE", "COPYING")}) + '\n').encode()
    files["vendor/webrtc-sys-build/src/pins.rs"] = ('"linux-x64-release" => (123, "' + "2" * 64 + '",)\n').encode()
    client = {"schema": 1, "source_version": "1.0.5-fixture", "upstream_commit": "f" * 40,
              "upstream_patch_sha256": patch_sha, "patched_source_sha256": "1" * 64,
              "source_archives": {key: pin[1] for key, pin in archive_pins.items()},
              "source_client_sha256": "3" * 64, "build_id": "4" * 40, "needed": ["libc.so.6"],
              "objects": {"impl-node.c.o": "5" * 64},
              "tool_version_output_sha256": {name: "b" * 64 for name in ("patch", "meson", "ninja", "cc", "pkg-config")},
              "configure": ["--fixture"], "config_sha256": "6" * 64, "recipe_sha256": "7" * 64}
    pipewire_files["SOURCE.json"] = json.dumps(client, sort_keys=True).encode()
    notice_sha = {}
    for directory, items, doc in ((runtime_dir, runtime_files, S.RUNTIME_DOC), (pipewire_dir, pipewire_files, S.PIPEWIRE_DOC)):
        for name, value in items.items():
            (directory / name).write_bytes(value)
            notice_sha[doc + "/" + name] = S.digest(value)
    artifact = bytes(prefix) + b"synthetic filesystem bytes"
    artifact_path = root / "synthetic.AppImage"
    artifact_path.write_bytes(artifact)
    receipt = {"schema": 1, "format": "appimage", "source_revision": "8" * 40, "unsigned": True,
               "distribution_ready": False, "appimage_sha256": S.digest(artifact), "native_executable_sha256": "9" * 64,
               "source_built_runtime": runtime, "embedded_runtime": embedded,
               "pipewire": {"source_client_sha256": client["source_client_sha256"], "build_id": client["build_id"], "needed": client["needed"], "sha256": "a" * 64},
               "inventory": {"schema": 1, "distribution_ready": False, "notice_sha256": notice_sha,
                   "runtime_sources": archives, "runtime_source_patches": spec["patches"], "runtime_input_sha256": identity["sha256"],
                   "deployed": [{"relative_path": "usr/bin/morpheus-app", "kind": "project", "source_sha256": "9" * 64},
                                {"relative_path": "usr/lib/libpipewire-0.3.so.0", "kind": "pipewire", "source_sha256": "3" * 64, "deployed_sha256": "a" * 64}]}}
    receipt_path = root / "build-receipt.json"
    receipt_path.write_text(json.dumps(receipt, sort_keys=True))
    receipt_path.chmod(0o600)
    plan = {"schema": 2, "registry_cache": str(root / "cache"), "git_mirrors": {}, "native_inputs": {
        "coverage": "known-components", "package_artifact": str(artifact_path), "package_receipt": str(receipt_path),
        "runtime_notices": str(runtime_dir), "pipewire_notices": str(pipewire_dir)}}
    return plan, files, receipt, spec, host


class SourceTests(unittest.TestCase):
    def test_native_assembly_is_bound_to_the_actual_package_and_stays_partial(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            root = Path(directory)
            plan, files, receipt, _, _ = native_fixture(root)
            # A checked owned receipt can contain private diagnostic arguments.
            # Its raw text must not become source-bundle content or provenance.
            secret = "synthetic-ambient-path-that-must-remain-private"
            receipt["source_built_runtime"]["commands"][0]["argv"].append(secret)
            raw_source = json.dumps(receipt["source_built_runtime"], sort_keys=True).encode()
            (root / "runtime/SOURCE.json").write_bytes(raw_source)
            receipt["inventory"]["notice_sha256"][S.RUNTIME_DOC + "/SOURCE.json"] = S.digest(raw_source)
            (root / "build-receipt.json").write_text(json.dumps(receipt, sort_keys=True))
            budget = S.Budget()
            material, coverage = S.native_material(plan, files, receipt["source_revision"], budget)
            self.assertNotIn(secret, json.dumps(coverage))
            self.assertFalse(any(secret.encode() in value for value in material.values()))
            self.assertEqual(material["native/runtime/musl.tar.gz"], (root / "runtime/musl.tar.gz").read_bytes())
            self.assertEqual(material["native/pipewire/exported-node-startup.patch"], (root / "pipewire/exported-node-startup.patch").read_bytes())
            self.assertTrue(coverage["runtime_prefix_bound"])
            self.assertTrue(coverage["runtime_relink_object_bound"])
            self.assertTrue(coverage["uapi_headers_bound"])
            self.assertFalse(coverage["native_sources_complete"])
            self.assertFalse(coverage["actual_native_build_inputs_verified"])
            self.assertFalse(coverage["sdk"]["complete_matching_source_verified"])
            self.assertFalse(coverage["sdk"]["actual_linked_input_binding"])
            self.assertEqual(coverage["package_artifact_sha256"], receipt["appimage_sha256"])
            self.assertEqual(set(material), {key for key, value in coverage["files"].items() if value["included_in_bundle"]})
            self.assertEqual(budget.inputs, sum(value["bytes"] for value in coverage["files"].values()))
            self.assertNotIn("native/runtime/SOURCE.json", material)
            self.assertNotIn("native/pipewire/SOURCE.json", material)
            self.assertNotIn("native/runtime/appimage_runtime_build.py", material)
            self.assertFalse(coverage["raw_receipts_included"])
            self.assertFalse(coverage["native_lifecycle_acceptance"])
            self.assertNotIn(str(root), json.dumps(coverage))
            self.assertEqual(S.native_material({"schema": 1}, {}, "a" * 40, S.Budget()),
                             ({}, {"coverage": "rust-only", "native_sources_complete": False,
                                   "actual_native_build_inputs_verified": False}))

    def test_native_plan_never_admits_complete_or_unknown_source_claims(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            root = Path(directory)
            plan, _, _, _, _ = native_fixture(root)
            path = root / "plan.json"
            path.write_text(json.dumps(plan))
            path.chmod(0o600)
            self.assertEqual(S.input_plan(path, [])["native_inputs"]["coverage"], "known-components")
            for change in ({"coverage": "complete-native"}, {"coverage": "complete"}, {"sdk_sources": "/unbound"}):
                selected = json.loads(json.dumps(plan))
                selected["native_inputs"].update(change)
                path.write_text(json.dumps(selected))
                with self.assertRaises(S.SourceFailure):
                    S.input_plan(path, [])

    def test_native_assembly_rejects_missing_unknown_and_rehashed_unpinned_source(self):
        for mode in ("archive", "extra", "missing", "receipt", "sdk", "relink_text"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
                root = Path(directory)
                plan, files, receipt, _, _ = native_fixture(root)
                if mode == "archive":
                    data = b"different source even if inventory is updated"
                    (root / "runtime/musl.tar.gz").write_bytes(data)
                    receipt["inventory"]["notice_sha256"][S.RUNTIME_DOC + "/musl.tar.gz"] = S.digest(data)
                elif mode == "extra":
                    (root / "runtime/unbound-source.tar.gz").write_bytes(b"unbound")
                elif mode == "missing":
                    (root / "pipewire/LICENSE").unlink()
                elif mode == "receipt":
                    receipt["source_revision"] = "0" * 40
                elif mode == "sdk":
                    files.pop("vendor/webrtc-sys-build/src/pins.rs")
                else:
                    data = b"unbound relink instructions"
                    (root / "runtime/RELINKING.txt").write_bytes(data)
                    receipt["inventory"]["notice_sha256"][S.RUNTIME_DOC + "/RELINKING.txt"] = S.digest(data)
                (root / "build-receipt.json").write_text(json.dumps(receipt, sort_keys=True))
                with self.assertRaises(S.SourceFailure):
                    S.native_material(plan, files, "8" * 40, S.Budget())

    def test_runtime_normalization_cannot_expand_beyond_its_noncode_md5_slot(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            root = Path(directory)
            _, _, receipt, _, _ = native_fixture(root)
            original = (root / "synthetic.AppImage").read_bytes()[:1024]
            runtime, embedded = receipt["source_built_runtime"]["runtime"], receipt["embedded_runtime"]
            S.runtime_prefix_identity(bytearray(original), runtime, embedded)
            for mode in ("code", "exec_section", "exec_segment", "slot_size", "other_section_alias"):
                value = bytearray(original)
                if mode == "code":
                    value[220] ^= 1
                elif mode == "exec_section":
                    struct.pack_into("<Q", value, 384 + 8, 6)
                elif mode == "exec_segment":
                    struct.pack_into("<I", value, 64 + 4, 7)
                elif mode == "slot_size":
                    struct.pack_into("<Q", value, 384 + 32, 32)
                else:
                    struct.pack_into("<IIQQQQIIQQ", value, 256, 1, 1, 2, 192, 192, 16, 0, 0, 1, 0)
                altered = dict(embedded, embedded_prefix_sha256=S.digest(value))
                with self.subTest(mode=mode), self.assertRaises(S.SourceFailure):
                    S.runtime_prefix_identity(value, runtime, altered)

    def test_native_relink_uapi_and_actual_link_inventory_fail_closed(self):
        for mode in ("relink", "headers", "unknown_link"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
                root = Path(directory)
                plan, files, receipt, _, _ = native_fixture(root)
                if mode in ("relink", "headers"):
                    name = "runtime-relink.o.gz" if mode == "relink" else "linux-uapi-headers.tar.gz"
                    data = gzip.compress(b"not the actual recorded input", mtime=0)
                    (root / "runtime" / name).write_bytes(data)
                    receipt["inventory"]["notice_sha256"][S.RUNTIME_DOC + "/" + name] = S.digest(data)
                else:
                    receipt["source_built_runtime"]["linked_inputs"].append(
                        {"path": "$BUILD/prefix/lib/unbound.a", "sha256": "a" * 64, "bytes": 12})
                    data = json.dumps(receipt["source_built_runtime"], sort_keys=True).encode()
                    (root / "runtime/SOURCE.json").write_bytes(data)
                    receipt["inventory"]["notice_sha256"][S.RUNTIME_DOC + "/SOURCE.json"] = S.digest(data)
                (root / "build-receipt.json").write_text(json.dumps(receipt, sort_keys=True))
                with self.assertRaises(S.SourceFailure):
                    S.native_material(plan, files, "8" * 40, S.Budget())

    def test_runtime_link_map_repeated_load_rows_require_the_same_exact_identity(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            _, _, receipt, spec, host = native_fixture(Path(directory))
            runtime = receipt["source_built_runtime"]
            original = next(entry for entry in runtime["linked_inputs"] if entry["path"].endswith("/libc.a"))
            runtime["linked_inputs"].append(dict(original))
            inventory = S.runtime_linked_sources(runtime, spec, host)
            matches = [entry for entry in inventory if entry["logical_input"] == "libc.a"]
            self.assertEqual(len(matches), 1)
            self.assertEqual(matches[0]["load_occurrences"], 2)
            self.assertNotIn("path", matches[0])
            runtime["linked_inputs"][-1]["sha256"] = "0" * 64
            with self.assertRaises(S.SourceFailure):
                S.runtime_linked_sources(runtime, spec, host)

    def test_large_package_hashing_streams_without_the_full_file_reader(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            path = Path(directory) / "artifact"
            data = b"synthetic package block" * 100000
            path.write_bytes(data)
            with mock.patch.object(S, "read_owned", side_effect=AssertionError("must stream artifact")):
                length, sha, prefix = S.hash_owned_file(path, S.MAX_BUNDLE, prefix_bytes=1024)
            self.assertEqual((length, sha, bytes(prefix)), (len(data), S.digest(data), data[:1024]))
            alias = Path(directory) / "alias"
            os.link(path, alias)
            with self.assertRaises(S.SourceFailure):
                S.hash_owned_file(path, S.MAX_BUNDLE)
            alias.unlink()
            alias.symlink_to(path)
            with self.assertRaises(S.SourceFailure):
                S.hash_owned_file(alias, S.MAX_BUNDLE)

    def test_declared_native_build_interfaces_are_inert_source_metadata(self):
        files = {"Cargo.toml": b'[package]\nname="fixture"\nversion="1.0.0"\nlinks="fixture_native"\nbuild="native/build.rs"\n',
                 "native/build.rs": b'fn main() { panic!("must never execute"); }'}
        record = S.package_catalog(files)[("fixture", "1.0.0")]["declared_build_interface"]
        self.assertEqual(record["links"], "fixture_native")
        self.assertEqual(record["build_script"], {"path": "native/build.rs", "sha256": S.digest(files["native/build.rs"])})
        self.assertFalse(record["executed_here"])
        self.assertFalse(record["actual_linked_inputs_verified"])
        self.assertFalse(record["separately_fetched_inputs_covered"])

    def test_native_recipe_ast_never_evaluates_expressions_or_imports(self):
        files = {"recipe.py": b'PIN = __import__("untrusted").execute()\n'}
        with self.assertRaises(S.SourceFailure):
            S.source_assignment(files, "recipe.py", "PIN")
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            _, files, _, _, _ = native_fixture(Path(directory))
            recipe = files["scripts/pipewire_startup_probe.py"]
            files["scripts/pipewire_startup_probe.py"] = recipe.replace(b"1024", b"__import__('untrusted').execute()")
            with self.assertRaises(S.SourceFailure):
                S.native_recipe(files)

    def test_known_native_bundle_is_deterministic_nonclobbering_and_never_complete(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            root = Path(directory)
            plan, files, receipt, _, _ = native_fixture(root)
            files.update({"Cargo.toml": b'[package]\nname="fixture"\nversion="1.0.0"\n',
                          "Cargo.lock": b'version=4\n[[package]]\nname="fixture"\nversion="1.0.0"\n'})
            (root / "cache").mkdir(mode=0o700)
            plan_path = root / "plan.json"
            plan_path.write_text(json.dumps(plan))
            plan_path.chmod(0o600)
            identities = []
            for name in ("first", "second"):
                output = root / name
                output.mkdir(mode=0o700)
                identity = {"commit": "8" * 40, "tree": "b" * 40}
                with mock.patch.object(S, "git_export", return_value=(b"synthetic authenticated workspace", identity, dict(files))):
                    result = S.build(None, output, root, "8" * 40, "/usr/bin/git", {}, plan_path)
                self.assertFalse(result["distribution_ready"])
                identities.append(result["bundle_sha256"])
                with tarfile.open(output / S.BUNDLE_NAME, mode="r:gz") as archive:
                    manifest = json.loads(archive.extractfile("MANIFEST.json").read())
                    self.assertFalse(manifest["license_compliance_verified"])
                    self.assertFalse(manifest["native_sources"]["native_sources_complete"])
                    self.assertEqual(archive.extractfile("native/runtime/musl.tar.gz").read(), b"synthetic source musl")
                original = (output / S.BUNDLE_NAME).read_bytes()
                with mock.patch.object(S, "git_export", return_value=(b"synthetic authenticated workspace", identity, dict(files))), self.assertRaises(FileExistsError):
                    S.build(None, output, root, "8" * 40, "/usr/bin/git", {}, plan_path)
                self.assertEqual((output / S.BUNDLE_NAME).read_bytes(), original)
            self.assertEqual(identities[0], identities[1])

    def test_git_uses_only_bounded_authenticated_raw_objects(self):
        blob = b"export me even when .gitattributes says export-ignore\n"
        entries = {"README": {"mode": "100644", "oid": S.object_id("blob", blob), "bytes": len(blob)}}
        tree = S.tree_identity(entries)
        commit = ("tree " + tree + "\nauthor Fixture <fixture@example.org> 0 +0000\n\nfixture\n").encode()
        revision = S.object_id("commit", commit)
        listing = ("100644 blob " + entries["README"]["oid"] + " " + str(len(blob)) + "\tREADME\0").encode()
        batch = (entries["README"]["oid"] + " blob " + str(len(blob)) + "\n").encode() + blob + b"\n"
        runner = SimpleNamespace(run=mock.Mock(side_effect=[commit, listing, batch]))
        with mock.patch.object(S, "safe_git", return_value=["/usr/bin/git"]):
            payload, identity, files = S.git_export(runner, Path("/mirror"), revision,
                                                    "/usr/bin/git", {"GIT_CONFIG_COUNT": "1", "SSH_AUTH_SOCK": "/secret"}, S.Budget())
        self.assertEqual(identity["commit"], revision)
        self.assertEqual(identity["tree"], tree)
        self.assertFalse(identity["git_attributes_applied"])
        self.assertEqual(identity["git_identity_schema"], 2)
        raw_commit = bytes.fromhex(identity["commit_object_hex"])
        self.assertLessEqual(len(raw_commit), 1024 * 1024)
        self.assertEqual(S.object_id("commit", raw_commit), identity["commit"])
        self.assertEqual(raw_commit.split(b"\n", 1)[0], ("tree " + identity["tree"]).encode("ascii"))
        self.assertEqual(files, {"README": blob})
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            self.assertEqual(archive.extractfile("README").read(), blob)
        for call in runner.run.call_args_list:
            self.assertEqual(call.kwargs["timeout"], 60)
            self.assertEqual(call.kwargs["stderr_limit"], 65536)
            self.assertNotIn("GIT_CONFIG_COUNT", call.kwargs["env"])
            self.assertNotIn("SSH_AUTH_SOCK", call.kwargs["env"])
            self.assertEqual(call.kwargs["env"]["GIT_NO_LAZY_FETCH"], "1")
            self.assertNotIn("archive", call.args[1])
        self.assertEqual(runner.run.call_args.kwargs["stdin_bytes"], (entries["README"]["oid"] + "\n").encode())
        for tampered in (b"different commit", listing.replace(b"README", b"omitted"), batch.replace(blob, b"x" * len(blob))):
            sequence = [commit, listing, batch]
            sequence[0 if tampered == b"different commit" else 1 if b"omitted" in tampered else 2] = tampered
            with mock.patch.object(S, "safe_git", return_value=["/usr/bin/git"]), self.assertRaises(S.SourceFailure):
                S.git_export(SimpleNamespace(run=mock.Mock(side_effect=sequence)), Path("/mirror"), revision,
                             "/usr/bin/git", {}, S.Budget())

    def test_git_environment_and_configuration_cannot_select_hooks_or_transports(self):
        result = S.clean_git_environment({"HOME": "/fixture", "GIT_DIR": "/other", "GIT_CONFIG_SYSTEM": "/evil",
                                          "GIT_SSH_COMMAND": "evil", "SSH_ASKPASS": "evil"})
        self.assertEqual(result["HOME"], "/fixture")
        self.assertNotIn("GIT_DIR", result)
        self.assertNotIn("GIT_SSH_COMMAND", result)
        self.assertNotIn("SSH_ASKPASS", result)
        self.assertEqual(result["GIT_CONFIG_NOSYSTEM"], "1")
        self.assertEqual(result["GIT_CONFIG_GLOBAL"], "/dev/null")
        self.assertEqual(result["GIT_NO_REPLACE_OBJECTS"], "1")
        self.assertEqual(result["GIT_ALLOW_PROTOCOL"], "")
        with mock.patch.object(Path, "resolve", side_effect=lambda strict: Path("/mirror")), \
                mock.patch.object(Path, "stat", return_value=SimpleNamespace(st_mode=0o40700, st_uid=os.getuid())):
            command = S.safe_git("/usr/bin/git", "/mirror")
        for setting in ("core.hooksPath=/dev/null", "core.fsmonitor=false", "diff.external=", "core.attributesFile=/dev/null"):
            self.assertIn(setting, command)

    def test_registry_identity_traversal_links_and_expansion_limits(self):
        manifest = b'[package]\nname="fixture"\nversion="1.0.0"\nlicense="MIT"\n'
        good = [("fixture-1.0.0/Cargo.toml", manifest, None)]
        for members, maximum in ((good, S.MAX_GIT_CONTENT),
                                 (good + [("fixture-1.0.0/../../escape", b"x", None)], S.MAX_GIT_CONTENT),
                                 (good + [("fixture-1.0.0/link", b"", "../../escape")], S.MAX_GIT_CONTENT),
                                 (good, 512)):
            payload = crate(members)
            package = {"name": "fixture", "version": "1.0.0", "checksum": S.digest(payload)}
            with self.subTest(members=members, maximum=maximum), mock.patch.object(S, "MAX_GIT_CONTENT", maximum):
                if members == good and maximum == S.MAX_GIT_CONTENT and maximum > 512:
                    self.assertEqual(S.registry_contents(payload, package, S.Budget())["Cargo.toml"], manifest)
                else:
                    with self.assertRaises(S.SourceFailure):
                        S.registry_contents(payload, package, S.Budget())
        with self.assertRaises(S.SourceFailure):
            S.registry_contents(crate(good), {"name": "fixture", "version": "1.0.0", "checksum": "0" * 64}, S.Budget())

    def test_git_links_are_preserved_only_when_the_resolved_graph_exists_inside(self):
        with self.assertRaises(S.SourceFailure):
            S.source_path(".")
        entries = {"LICENSE": {"mode": "100644"}, "docs/link": {"mode": "120000"}}
        S.check_links(entries, {"LICENSE": b"license", "docs/link": b"../LICENSE"})
        for target in (b"../../escape", b"/absolute", b"../missing", b"link", b"../LICENSE/../LICENSE"):
            with self.subTest(target=target), self.assertRaises(S.SourceFailure):
                S.check_links(entries, {"LICENSE": b"license", "docs/link": target})

    def test_catalog_preserves_inherited_license_source_and_published_archive_identity(self):
        files = {"Cargo.toml": b'[workspace]\nmembers=["member"]\n[workspace.package]\nversion="1.0.0"\nlicense-file="LICENSE"\n',
                 "member/Cargo.toml": b'[package]\nname="fixture"\nversion.workspace=true\nlicense-file.workspace=true\n',
                 "LICENSE": b"license text"}
        record = S.package_catalog(files)[("fixture", "1.0.0")]
        self.assertEqual(record["license_text_candidates"]["LICENSE"], S.digest(b"license text"))
        self.assertFalse(record["license_obligations_verified"])
        published = {"Cargo.toml": b'[package]\nname="fixture"\nversion="1.0.0"\n[dev-dependencies.unpublished]\npath="../unpublished"\n'}
        self.assertIn(("fixture", "1.0.0"), S.package_catalog(published, published=True))
        with self.assertRaises(S.SourceFailure):
            S.package_catalog(published)

    def test_missing_git_input_and_unknown_lock_sources_fail_closed(self):
        source = "git+https://github.com/fixture/source?rev=" + "a" * 40 + "#" + "a" * 40
        plan = json.dumps({"schema": 1, "registry_cache": "/cache", "git_mirrors": {}}).encode()
        with mock.patch.object(S, "read_owned", return_value=plan), self.assertRaises(S.SourceFailure):
            S.input_plan(Path("/plan"), [{"source": source}])
        for raw in (b'version=4\n[[package]]\nname="fixture"\nversion="1.0.0"\nsource="registry+https://other.invalid"\n',
                    b'version=4\n[[package]]\nname="fixture"\nversion="1.0.0"\nsource="registry+https://github.com/rust-lang/crates.io-index"\n'):
            with self.assertRaises(S.SourceFailure):
                S.lock_packages({"Cargo.lock": raw})

    def test_atomic_publication_is_private_nonreplacing_and_removable_by_exact_identity(self):
        payload = b"source evidence"
        receipt = {"bundle_sha256": S.digest(payload), "distribution_ready": False}
        with tempfile.TemporaryDirectory() as directory:
            identity = S.publish(Path(directory), payload, receipt)
            target = Path(directory) / S.BUNDLE_NAME
            self.assertEqual(target.read_bytes(), payload)
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(S.SourceFailure):
                S.publish(Path(directory), payload, receipt)
            self.assertEqual(target.read_bytes(), payload)
            with self.assertRaises(S.SourceFailure):
                S.remove_published(Path(directory), (identity[0], identity[1] + 1, identity[2]))
            S.remove_published(Path(directory), identity)
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_source_route_is_explicit_and_retains_unsigned_and_scanner_guards(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(P.os, "execv") as execute:
            P.linux_appimage("appimage", "/reviewed/scanner", source_bundle=(Path("/plan"), Path("/out")))
        args = execute.call_args.args[1]
        self.assertEqual(args[-4:], ["--source-bundle-inputs", "/plan", "--source-bundle-output", "/out"])
        self.assertNotIn("--qa-output", args)
        for scanner, environment in ((None, {}), ("/reviewed/scanner", {"CARGO_PACKAGER_SIGN_PRIVATE_KEY": "fixture"})):
            with mock.patch.dict(os.environ, environment, clear=True), mock.patch.object(P.os, "execv") as execute, self.assertRaises(ValueError):
                P.linux_appimage("appimage", scanner, source_bundle=(Path("/plan"), Path("/out")))
            execute.assert_not_called()

    def test_acquisition_rejects_unreviewed_manifest_origins_before_fetch(self):
        lock = 'version=4\n' + ''.join('[[package]]\nname="fixture' + str(index) + '"\nversion="1.0.0"\nsource="' + source + '"\n'
                                             for index, source in enumerate(sorted(Q.PINS)))
        root = b'[package]\nname="fixture"\nversion="1.0.0"\n'
        files = {"Cargo.toml": root, "Cargo.lock": lock.encode()}
        self.assertEqual(len(Q.source_inputs(files)), 3)
        for extra in (b'[dependencies.bad]\ngit="https://unreviewed.invalid/repository"\nrev="abc"\n',
                      b'[dependencies.bad]\nversion="1"\nregistry="unreviewed"\n'):
            with self.assertRaises(S.SourceFailure):
                Q.source_inputs({**files, "Cargo.toml": root + extra})

    def test_acquisition_environment_uses_private_cache_and_no_ambient_credentials(self):
        environment = Q.environment_for_acquisition({"HOME": "/old-home", "GITHUB_ACTOR": "brk275",
            "GITHUB_TRIGGERING_ACTOR": "brk275", "SSH_AUTH_SOCK": "/socket", "CARGO_REGISTRY_TOKEN": "fixture",
            "HTTP_PROXY": "http://unreviewed.invalid"}, Path("/private/owned"), "/toolchain/bin/cargo", "/toolchain/bin/rustc")
        self.assertEqual(environment["HOME"], "/private/owned/home")
        self.assertEqual(environment["CARGO_HOME"], "/private/owned/cargo")
        self.assertEqual(environment["RUSTC"], "/toolchain/bin/rustc")
        self.assertEqual(environment["CARGO_NET_GIT_FETCH_WITH_CLI"], "false")
        self.assertEqual(environment["GIT_ALLOW_PROTOCOL"], "https")
        for name in ("SSH_AUTH_SOCK", "CARGO_REGISTRY_TOKEN", "HTTP_PROXY"):
            self.assertNotIn(name, environment)

    def test_real_outer_receipt_parser_accepts_only_its_fixed_containment_line_and_json(self):
        self.assertEqual(Q.package_receipt(b'MORPHEUS_ELEMENT_CONTAINMENT passed\n{"schema":1}\n'), {"schema": 1})
        for data in (b'{}\n', b'private data\n{}\n', b'MORPHEUS_ELEMENT_CONTAINMENT passed\n{}\nprivate tail\n'):
            with self.assertRaises(S.SourceFailure):
                Q.package_receipt(data)

    def test_finalizer_removes_owned_inputs_even_if_preparation_or_package_receipt_is_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "owned"
            root.mkdir(mode=0o700)
            (root / "canonical-temp").mkdir(mode=0o700)
            (root / "logs").mkdir(mode=0o700)
            with mock.patch.object(Q, "private_root", return_value=root), \
                    mock.patch.object(Q.P, "scan_private_logs"), mock.patch.object(Q.P, "owned_tool", return_value="/scanner"), \
                    self.assertRaises(S.SourceFailure):
                Q.finish(root, "/scanner", {})
            self.assertFalse(root.exists())


class NativeLifecycleTests(unittest.TestCase):
    def receipt(self, artifact, build):
        return {"schema": 1, "source_revision": "8" * 40, "source_tree": "a" * 40,
                "bundle_sha256": S.digest(b"synthetic source bundle"), "manifest_sha256": "b" * 64,
                "registry_packages": 1, "git_sources": 1, "locked_packages": 2,
                "distribution_ready": False, "unresolved_release_requirements": list(S.UNRESOLVED),
                "native_sources": {"coverage": "receipt-bound-known-native-components-only",
                    "package_artifact_sha256": S.digest(artifact), "build_receipt_sha256": S.digest(build),
                    "native_sources_complete": False, "actual_native_build_inputs_verified": False,
                    "raw_receipts_included": False, "native_lifecycle_acceptance": False}}

    def test_native_guard_never_expands_source_only_or_update_selection(self):
        event = {"repository": {"owner": {"id": 123}}, "sender": {"id": 123}, "inputs": {
            "source_bundle": False, "platform": "linux", "linux_formats": "appimage",
            "appimage_updates": False, "appimage_capture": False}}
        environment = {"GITHUB_ACTOR": "brk275", "GITHUB_TRIGGERING_ACTOR": "brk275",
                       "GITHUB_EVENT_PATH": "/synthetic-event"}
        with mock.patch.object(Q.P, "public_hosted_environment"), \
                mock.patch.object(Q.P.client, "read_regular", side_effect=lambda *_: json.dumps(event).encode()):
            Q.guard_native(environment)
            for key, value in (("source_bundle", True), ("appimage_updates", True),
                               ("platform", "windows"), ("linux_formats", "all"),
                               ("appimage_capture", "unrecognized")):
                previous = event["inputs"][key]
                event["inputs"][key] = value
                with self.subTest(key=key), self.assertRaises(S.SourceFailure):
                    Q.guard_native(environment)
                event["inputs"][key] = previous
            event["inputs"].update(platform="all", appimage_capture=True)
            Q.guard_native(environment)
            with self.assertRaises(S.SourceFailure):
                Q.guard_native({**environment, "GITHUB_ACTOR": "outside-contributor"})
            event["sender"]["id"] = 456
            with self.assertRaises(S.SourceFailure):
                Q.guard_native(environment)

    def test_native_assembly_consumes_this_live_package_and_deployed_notice_paths_once(self):
        M = Q.P
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            root = Path(directory).resolve()
            root.chmod(0o700)
            (root / "packages").mkdir(mode=0o700)
            artifact = root / "packages/fixture.AppImage"
            artifact_bytes, build_bytes = b"x" * 128, b"synthetic private build receipt"
            artifact.write_bytes(artifact_bytes)
            (root / "build-receipt.json").write_bytes(build_bytes)
            appdir = root / "extracted/squashfs-root"
            appdir.mkdir(parents=True, mode=0o700)
            (root / "native-source-inputs/cargo").mkdir(parents=True, mode=0o700)
            (root / "native-source-inputs").chmod(0o700)
            receipt, calls = self.receipt(artifact_bytes, build_bytes), []
            source_progress = object()

            def acquire(work, cargo, rustc, environment, *, progress=None):
                calls.append("acquire")
                self.assertIs(progress, source_progress)
                self.assertEqual(work, root / "native-source-inputs")
                self.assertEqual(environment["CARGO_HOME"], str(work / "cargo"))
                self.assertEqual(environment["HOME"], str(work / "home"))
                self.assertNotIn("CARGO_REGISTRY_TOKEN", environment)
                self.assertTrue(artifact.exists())
                M.client.write_new(work / "plan.json", json.dumps({"schema": 1,
                    "registry_cache": str(work / "cargo/registry/cache/fixed"), "git_mirrors": {}}).encode())

            def assemble(runner, owned, repository, revision, git, environment, plan_path):
                calls.append("assemble")
                self.assertIs(runner.progress, source_progress)
                self.assertEqual(owned, root)
                self.assertEqual(revision, "8" * 40)
                plan = S.unique_json(S.read_owned(plan_path, 65536, private=True))
                self.assertEqual(plan["schema"], 2)
                self.assertEqual(plan["native_inputs"], {"coverage": "known-components",
                    "package_artifact": str(artifact), "package_receipt": str(root / "build-receipt.json"),
                    "runtime_notices": str(appdir / S.RUNTIME_DOC),
                    "pipewire_notices": str(appdir / S.PIPEWIRE_DOC)})
                self.assertTrue(artifact.exists())
                return receipt

            environment = {"HOME": "/old-home", "GITHUB_ACTOR": "brk275",
                "GITHUB_TRIGGERING_ACTOR": "brk275", "CARGO_REGISTRY_TOKEN": "synthetic-removed"}
            with mock.patch.object(M.containment, "assert_contained"), \
                    mock.patch.object(Q, "guard_native"), \
                    mock.patch.object(Q, "source_tools", return_value=("/pinned/bin/cargo", "/pinned/bin/rustc")), \
                    mock.patch.object(Q, "acquire_inputs", side_effect=acquire), \
                    mock.patch.object(M, "expected_revision", return_value="8" * 40), \
                    mock.patch.object(M, "owned_tool", return_value="/usr/bin/git"), \
                    mock.patch.object(M, "build_unsigned") as build_native, \
                    mock.patch.object(S, "build", side_effect=assemble):
                self.assertEqual(M.assemble_native_sources(root, artifact, appdir, environment,
                                                          progress=source_progress), receipt)
                build_native.assert_not_called()
                self.assertEqual(calls, ["acquire", "assemble"])
                self.assertEqual(S.unique_json(S.read_owned(root / "source-receipt.json", 32768, private=True)), receipt)
                with self.assertRaises(M.PackageFailure):
                    M.assemble_native_sources(root, root / "unrelated.AppImage", appdir, environment)

    def test_after_namespace_exit_native_receipt_is_bound_to_all_three_owned_outputs(self):
        M = Q.P
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            root = Path(directory).resolve()
            root.chmod(0o700)
            (root / "packages").mkdir(mode=0o700)
            artifact, build = b"x" * 128, b"synthetic private build receipt"
            (root / "packages/fixture.AppImage").write_bytes(artifact)
            (root / "build-receipt.json").write_bytes(build)
            (root / "build-receipt.json").chmod(0o600)
            (root / S.BUNDLE_NAME).write_bytes(b"synthetic source bundle")
            (root / S.BUNDLE_NAME).chmod(0o600)
            receipt = self.receipt(artifact, build)
            record = {"appimage_sha256": S.digest(artifact)}
            with mock.patch.object(M, "expected_revision", return_value="8" * 40):
                for path, replacement in ((None, None), ("packages/fixture.AppImage", b"y" * 128),
                                          ("build-receipt.json", b"changed receipt"),
                                          (S.BUNDLE_NAME, b"changed bundle")):
                    if path is not None:
                        original = (root / path).read_bytes()
                        (root / path).write_bytes(replacement)
                    M.private_receipt(root / "source-receipt.json", receipt)
                    if path is None:
                        self.assertEqual(M.verify_native_sources(root, record, {}), receipt)
                    else:
                        with self.subTest(path=path), self.assertRaises(S.SourceFailure):
                            M.verify_native_sources(root, record, {})
                        (root / path).write_bytes(original)
                    (root / "source-receipt.json").unlink()
                plain = {key: value for key, value in receipt.items() if key != "native_sources"}
                M.private_receipt(root / "source-receipt.json", plain)
                with self.assertRaises(S.SourceFailure):
                    M.verify_native_sources(root, record, {})

    def test_streamed_private_evidence_keeps_the_original_owner_only_mode_gate(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
            path = Path(directory).resolve() / "private-source"
            path.write_bytes(b"synthetic source")
            path.chmod(0o644)
            with self.assertRaises(S.SourceFailure):
                S.hash_owned_file(path, 4096, private=True)
            path.chmod(0o600)
            self.assertEqual(S.hash_owned_file(path, 4096, private=True)[:2],
                             (len(b"synthetic source"), S.digest(b"synthetic source")))

    def test_closed_native_receipt_cannot_publish_paths_or_promote_completeness(self):
        valid = self.receipt(b"artifact", b"receipt")
        self.assertEqual(S.validate_receipt(valid, "8" * 40), valid)
        for key, value in (("native_sources_complete", True), ("native_lifecycle_acceptance", True),
                           ("raw_receipts_included", True), ("ambient_path", "/private/diagnostic"),
                           ("package_artifact_sha256", "unbound")):
            changed = {**valid, "native_sources": {**valid["native_sources"], key: value}}
            with self.subTest(key=key), self.assertRaises(S.SourceFailure):
                S.validate_receipt(changed, "8" * 40)
        with self.assertRaises(S.SourceFailure):
            S.validate_receipt(valid, "9" * 40)

    def test_source_failure_fails_native_package_after_capture_and_before_completion(self):
        M = Q.P
        for fail in (False, True):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
                root = Path(directory).resolve()
                root.chmod(0o700)
                calls = []
                capture = SimpleNamespace(run=lambda *_args, **_kwargs: calls.append("capture") or {"installed_capture_passed": True})
                def assemble(*_args, **_kwargs):
                    calls.append("source")
                    if fail:
                        raise S.SourceFailure("missing")
                with mock.patch.dict(sys.modules, {"appimage_capture": capture, "appimage_notices": SimpleNamespace()}), \
                        mock.patch.object(M.containment, "assert_contained"), \
                        mock.patch.object(M, "build_unsigned", return_value=(Path("/owned.AppImage"), Path("/owned-appdir"), {})), \
                        mock.patch.object(M, "assemble_native_sources", side_effect=assemble), \
                        mock.patch.object(M, "private_receipt", side_effect=lambda *_: calls.append("receipt")), \
                        mock.patch.object(M.diagnostics, "Progress"):
                    if fail:
                        with self.assertRaises(S.SourceFailure):
                            M.inside(root, {"cargo": "/pinned/cargo"}, {})
                        self.assertEqual(calls, ["capture", "source"])
                    else:
                        M.inside(root, {"cargo": "/pinned/cargo"}, {})
                        self.assertEqual(calls, ["capture", "source", "receipt"])

    def test_native_outer_verification_waits_for_stop_cleanup_and_default_scan(self):
        M = Q.P
        for scan_fails in (False, True):
            with self.subTest(scan_fails=scan_fails), tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
                root = Path(directory).resolve()
                (root / "capture-receipt.json").write_text('{"installed_capture_passed":true}')
                calls = []
                def scan(*_args):
                    calls.append("scan")
                    if scan_fails:
                        raise M.PackageFailure("scan")
                with mock.patch.object(M.containment, "namespace_command", return_value=["/inert"]), \
                        mock.patch.object(M.containment, "status", return_value=0), \
                        mock.patch.object(M.containment, "stop_owned", side_effect=lambda *_: calls.append("stop")), \
                        mock.patch.object(M, "cleanup_owned_fixture", side_effect=lambda *_: calls.append("cleanup")), \
                        mock.patch.object(M, "scan_private_logs", side_effect=scan), \
                        mock.patch.object(M, "verify_native_sources", side_effect=lambda *_: calls.append("verify") or {}), \
                        mock.patch.object(M.subprocess, "Popen"), \
                        mock.patch.object(M.diagnostics, "Progress"), \
                        mock.patch.object(M.diagnostics, "public_failure", return_value={"passed": False}), \
                        mock.patch("sys.stderr", io.StringIO()):
                    if scan_fails:
                        with self.assertRaises(M.PackageFailure):
                            M.contained(root, "/scanner", {"cargo": "/cargo"}, {})
                        self.assertEqual(calls, ["stop", "cleanup", "scan"])
                    else:
                        M.contained(root, "/scanner", {"cargo": "/cargo"}, {})
                        self.assertEqual(calls, ["stop", "cleanup", "scan", "verify"])

    def test_consumed_cache_disposal_preserves_all_actual_logs_and_anchored_identity(self):
        M = Q.P
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            root.chmod(0o700)
            cache = M.NativeSourceCache(root)
            try:
                self.assertTrue(all(not os.get_inheritable(fd) for fd in cache.descriptors))
                work = root / "native-source-inputs"
                source = work / "cargo/registry/src/fixture/tests/data/sample_session.log"
                source.parent.mkdir(parents=True)
                source.write_bytes(b"synthetic authenticated source-data fixture")
                source.chmod(0o755)
                acquisition = work / "logs/commands/fetch.stderr.log"
                assembly = root / "native-source-commands/source.stderr.log"
                for log in (acquisition, assembly):
                    log.parent.mkdir(parents=True, exist_ok=True)
                    log.write_bytes(b"actual private command output")
                    log.chmod(0o600)
                cache.discard()
                self.assertFalse((work / "cargo").exists())
                for log in (acquisition, assembly):
                    self.assertEqual(log.read_bytes(), b"actual private command output")
                    self.assertEqual(log.stat().st_mode & 0o777, 0o600)
            finally:
                cache.close()
            self.assertEqual(cache.descriptors, [])

    def test_cache_disposal_refuses_replaced_directory_or_symlink_without_deleting_any_target(self):
        M = Q.P
        for replacement in ("directory", "symlink", "foreign-mode"):
            with self.subTest(replacement=replacement), tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                root.chmod(0o700)
                cache = M.NativeSourceCache(root)
                work, path = root / "native-source-inputs", root / "native-source-inputs/cargo"
                try:
                    if replacement == "foreign-mode":
                        path.chmod(0o755)
                    else:
                        path.rename(work / "original-cargo")
                        if replacement == "directory":
                            path.mkdir(mode=0o700)
                        else:
                            target = root / "unrelated-owned-data"
                            target.mkdir(mode=0o700)
                            (target / "preserve").write_bytes(b"unrelated")
                            path.symlink_to(target, target_is_directory=True)
                    with self.assertRaises(M.PackageFailure):
                        cache.discard()
                    self.assertTrue(path.exists())
                    if replacement == "symlink":
                        self.assertEqual((target / "preserve").read_bytes(), b"unrelated")
                finally:
                    cache.close()

    def test_stopped_native_cache_source_fixture_cannot_enter_the_real_command_log_scan(self):
        M = Q.P
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            root.chmod(0o700)
            (root / "capture-receipt.json").write_text('{"installed_capture_passed":true}')
            events, scanned = [], []
            source = root / "native-source-inputs/cargo/registry/src/fixture/sample_session.log"
            def spawn(*_args, **_kwargs):
                source.parent.mkdir(parents=True)
                source.write_bytes(b"synthetic crate source-data fixture")
                source.chmod(0o755)
                for name, value in (("native-source-inputs/logs/fetch.stderr.log", b"fetch diagnostic"),
                                    ("native-source-commands/objects.stderr.log", b"assembly diagnostic")):
                    log = root / name
                    log.parent.mkdir(parents=True, exist_ok=True)
                    log.write_bytes(value)
                    log.chmod(0o600)
                return SimpleNamespace(pid=42)
            def stop(*_args):
                events.append("stop")
                self.assertTrue(source.exists())
            def fixture(*_args):
                events.append("fixture")
                self.assertTrue(source.exists())
            def scanner(_argv, **kwargs):
                self.assertEqual(events, ["stop", "fixture"])
                self.assertFalse((root / "native-source-inputs/cargo").exists())
                scanned.append(kwargs["input"])
                return SimpleNamespace(returncode=0)
            with mock.patch.object(M.containment, "namespace_command", return_value=["/inert"]), \
                    mock.patch.object(M.containment, "status", return_value=0), \
                    mock.patch.object(M.containment, "stop_owned", side_effect=stop), \
                    mock.patch.object(M, "cleanup_owned_fixture", side_effect=fixture), \
                    mock.patch.object(M, "verify_native_sources", return_value={}), \
                    mock.patch.object(M.subprocess, "Popen", side_effect=spawn), \
                    mock.patch.object(M.subprocess, "run", side_effect=scanner), \
                    mock.patch.object(M.diagnostics, "Progress"):
                receipt = M.contained(root, "/reviewed-scanner", {"cargo": "/cargo"}, {})
            self.assertTrue(receipt["installed_capture_passed"])
            self.assertIn(b"fetch diagnostic", scanned)
            self.assertIn(b"assembly diagnostic", scanned)
            self.assertNotIn(b"synthetic crate source-data fixture", scanned)

    def test_failed_namespace_stop_retains_cache_but_attempts_every_actual_log_scan(self):
        M = Q.P
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            root.chmod(0o700)
            child, scanned = SimpleNamespace(pid=42), []
            def spawn(*_args, **_kwargs):
                work = root / "native-source-inputs"
                source = work / "cargo/registry/src/fixture/source.log"
                source.parent.mkdir(parents=True)
                source.write_bytes(b"source fixture is not a private command log")
                source.chmod(0o755)
                for name, value in (("native-source-inputs/logs/fetch.stderr.log", b"acquisition output"),
                                    ("native-source-commands/objects.stderr.log", b"assembly output")):
                    log = root / name
                    log.parent.mkdir(parents=True, exist_ok=True)
                    log.write_bytes(value)
                    log.chmod(0o600)
                return child
            def scanner(_argv, **kwargs):
                scanned.append(kwargs["input"])
                return SimpleNamespace(returncode=0)
            with mock.patch.object(M.containment, "namespace_command", return_value=["/inert"]), \
                    mock.patch.object(M.containment, "status", return_value=0), \
                    mock.patch.object(M.containment, "stop_owned", side_effect=RuntimeError("private stop failure")), \
                    mock.patch.object(M, "cleanup_owned_fixture"), \
                    mock.patch.object(M, "verify_native_sources") as verified, \
                    mock.patch.object(M.subprocess, "Popen", side_effect=spawn), \
                    mock.patch.object(M.subprocess, "run", side_effect=scanner), \
                    mock.patch.object(M.diagnostics, "Progress"), \
                    mock.patch("sys.stderr", io.StringIO()), self.assertRaises(M.PackageFailure):
                M.contained(root, "/reviewed-scanner", {"cargo": "/cargo"}, {})
            self.assertTrue((root / "native-source-inputs/cargo/registry/src/fixture/source.log").exists())
            self.assertIn(b"acquisition output", scanned)
            self.assertIn(b"assembly output", scanned)
            verified.assert_not_called()

    def test_private_log_scan_uses_independent_defaults_and_cleans_config_on_failure(self):
        M = Q.P
        for status in (0, 1):
            with self.subTest(status=status), tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", str(ROOT))) as directory:
                root = Path(directory).resolve()
                log = root / "private.log"
                log.write_bytes(b"synthetic private diagnostic")
                log.chmod(0o600)
                def scanner(argv, **kwargs):
                    config = Path(argv[argv.index("--config") + 1])
                    ignore = Path(argv[argv.index("--gitleaks-ignore-path") + 1])
                    self.assertEqual(config.read_bytes(), b"[extend]\nuseDefault = true\n")
                    self.assertEqual(ignore.read_bytes(), b"")
                    self.assertEqual(kwargs["cwd"], config.parent)
                    self.assertEqual(kwargs["input"], b"synthetic private diagnostic")
                    self.assertIn("--ignore-gitleaks-allow", argv)
                    return SimpleNamespace(returncode=status)
                with mock.patch.object(M.subprocess, "run", side_effect=scanner):
                    if status:
                        with self.assertRaises(M.PackageFailure):
                            M.scan_private_logs(root, "/reviewed-scanner")
                    else:
                        M.scan_private_logs(root, "/reviewed-scanner")
                self.assertEqual(list(root.iterdir()), [log])


if __name__ == "__main__":
    unittest.main()
