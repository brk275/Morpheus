"""Focused regressions for the exact native source patch input contract."""
from pathlib import Path
import hashlib
import importlib.util
from tempfile import TemporaryDirectory
import unittest
import os
import sys

_SCRIPT = Path(__file__).resolve().parents[1] / "provision_native_webrtc_sources.py"
_SPEC = importlib.util.spec_from_file_location("morpheus_source_patch_tests", _SCRIPT)
assert _SPEC is not None and _SPEC.loader is not None
_SOURCE = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_SOURCE)
_FIXTURES = Path(__file__).resolve().parent / "fixtures/native_webrtc_source_patches"
_CANONICAL = {
    "07-h264-prefix-validation.patch": "1de7bf5402089c13f93f108f3008aaa8c025e2bdbd7b2375694417c186c86a17",
    "08-rbsp-decoded-iv.patch": "dbefa1b81cd8b24eb0bcd41ec996e6b14265d66ffdad38cf1112e646821d4e09",
}
_EXPECTED = [
    "api/crypto/frame_crypto_transformer.cc",
    "api/crypto/sif_payload_unittest.cc",
]
_GIT_PATCH = (
    b"diff --git a/example.cc b/example.cc\n"
    b"--- a/example.cc\n+++ b/example.cc\n"
    b"@@ -1 +1 @@\n-old\n+new\n"
)


class NativeSourcePatchTests(unittest.TestCase):
    def test_exact_reviewed_plain_security_artifacts_are_accepted(self):
        for name, expected_hash in _CANONICAL.items():
            with self.subTest(patch=name):
                data = (_FIXTURES / name).read_bytes()
                self.assertEqual(hashlib.sha256(data).hexdigest(), expected_hash)
                self.assertEqual(_SOURCE.patch_paths(data), _EXPECTED)

    def test_mutated_plain_artifacts_do_not_keep_canonical_admission(self):
        for name, expected_hash in _CANONICAL.items():
            with self.subTest(patch=name):
                data = (_FIXTURES / name).read_bytes()
                self.assertEqual(hashlib.sha256(data).hexdigest(), expected_hash)
                with self.assertRaises(_SOURCE.Refusal):
                    _SOURCE.patch_paths(data + b"\n")

    def test_unreviewed_plain_diff_is_refused(self):
        data = b"--- a/example.cc\n+++ b/example.cc\n@@ -1 +1 @@\n-old\n+new\n"
        with self.assertRaises(_SOURCE.Refusal):
            _SOURCE.patch_paths(data)

    def test_existing_git_format_still_accepts_its_declared_regular_path(self):
        self.assertEqual(_SOURCE.patch_paths(_GIT_PATCH), ["example.cc"])

    def test_unsafe_or_duplicate_git_paths_remain_refused(self):
        for data in (
            _GIT_PATCH.replace(b"example.cc", b"../escape.cc"),
            _GIT_PATCH.replace(b"example.cc", b".git/config"),
            _GIT_PATCH.replace(b"example.cc", b".GIT/config"),
            _GIT_PATCH + _GIT_PATCH,
            _GIT_PATCH + b"GIT binary patch\n",
            _GIT_PATCH + b"rename from example.cc\n",
            _GIT_PATCH + b"old mode 100644\nnew mode 120000\n",
            _GIT_PATCH + b"\0",
        ):
            with self.subTest(data=data):
                with self.assertRaises(_SOURCE.Refusal):
                    _SOURCE.patch_paths(data)



class GitlinkInputTests(unittest.TestCase):
    def row(self):
        return {"mount": "src/third_party/grpc/src", "path": "third_party/abseil-cpp",
                "mode": "160000", "type": "commit", "oid": "a" * 40,
                "action": "uninitialized-directory"}

    def specs(self):
        return {"src": {}, "src/third_party": {}, "src/third_party/grpc/src": {}}

    def test_exact_explicit_marker_allows_its_owning_and_ancestor_grafts(self):
        row = self.row()
        policy = _SOURCE.gitlink_policy([row], self.specs())
        name = row["mount"] + "/" + row["path"]
        self.assertEqual(policy["entries"], {name: row})
        self.assertEqual(policy["roots"], {name})
        _SOURCE.outside_gitlinks("src/ordinary.cc", policy)

    def test_marker_aliases_and_overlapping_selected_grafts_are_refused(self):
        row = self.row()
        for other in (
            {**row, "path": row["path"].upper()},
            {**row, "path": row["path"] + "/nested"},
        ):
            with self.subTest(other=other):
                with self.assertRaises(_SOURCE.Refusal):
                    _SOURCE.gitlink_policy([row, other], self.specs())
        for graft in (
            row["mount"] + "/" + row["path"],
            row["mount"] + "/" + row["path"] + "/nested",
            row["mount"] + "/third_party",
        ):
            with self.subTest(graft=graft):
                with self.assertRaises(_SOURCE.Refusal):
                    _SOURCE.gitlink_policy([row], {**self.specs(), graft: {}})

    def test_unknown_actions_modes_objects_and_unsafe_paths_are_refused(self):
        row = self.row()
        for changed in (
            {"action": "initialize"}, {"mode": "120000"}, {"type": "blob"},
            {"oid": "not-an-object-id"}, {"path": "../escape"},
            {"mount": "unselected"}, {"extra": True},
        ):
            with self.subTest(changed=changed):
                with self.assertRaises(_SOURCE.Refusal):
                    _SOURCE.gitlink_policy([{**row, **changed}], self.specs())

    def test_regular_or_patch_files_cannot_claim_marker_scope(self):
        row = self.row()
        policy = _SOURCE.gitlink_policy([row], self.specs())
        marker = row["mount"] + "/" + row["path"]
        for name in (marker, marker + "/file.cc", row["mount"] + "/third_party"):
            with self.subTest(name=name):
                with self.assertRaises(_SOURCE.Refusal):
                    _SOURCE.outside_gitlinks(name, policy)

    def test_actual_materialization_must_remain_an_empty_private_directory(self):
        row = self.row()
        policy = _SOURCE.gitlink_policy([row], self.specs())
        with TemporaryDirectory() as temporary:
            checkout = Path(temporary).resolve()
            marker = checkout / row["mount"] / row["path"]
            _SOURCE.private_parents(marker.parent, checkout)
            _SOURCE.private_directory(marker)
            _SOURCE.verify_gitlinks(checkout, policy["entries"])
            child = marker / "unexpected.cc"
            child.write_bytes(b"synthetic fixture only\n")
            with self.assertRaises(_SOURCE.Refusal):
                _SOURCE.verify_gitlinks(checkout, policy["entries"])
            child.unlink()
            marker.rmdir()
            marker.write_bytes(b"not a directory\n")
            with self.assertRaises(_SOURCE.Refusal):
                _SOURCE.verify_gitlinks(checkout, policy["entries"])
            marker.unlink()
            target = checkout / "synthetic-target"
            target.write_bytes(b"fixture\n")
            marker.symlink_to(target)
            with self.assertRaises(_SOURCE.Refusal):
                _SOURCE.verify_gitlinks(checkout, policy["entries"])


@unittest.skipUnless(os.name == "posix" and sys.platform == "linux", "Linux-only source assembly")
class InstrumentedAssemblyTests(unittest.TestCase):
    def fixture(self, root):
        source = root / "input"
        _SOURCE.private_directory(source)
        directories = {".", "checkout", "checkout/src", "checkout/src/empty-marker",
                       "objects", "objects/000.git"}
        for name in sorted(directories - {"."}, key=lambda name: (name.count("/"), name)):
            _SOURCE.private_directory(source / name)
        files = {}
        for name, value, mode in (("checkout/src/example.cc", b"synthetic source only\n", 0o644),
                                  ("objects/000.git/HEAD", b"synthetic original metadata\n", 0o600)):
            path = source / name
            with _SOURCE.private_file(path) as stream:
                stream.write(value)
            path.chmod(mode)
            files[name] = {"path": name, "bytes": len(value), "sha256": hashlib.sha256(value).hexdigest(),
                           "mode": mode}
        return source, files, directories

    def test_assembly_copies_actual_bytes_modes_and_empty_directories_without_aliasing(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            source, files, directories = self.fixture(root)
            output = root / "output"
            _SOURCE.private_directory(output)
            originals = {name: (source / name).read_bytes() for name in files}
            _SOURCE.assembly_tree(source, files, directories, output)
            _SOURCE.assembly_tree(output, files, directories)
            for name, row in files.items():
                self.assertEqual((output / name).read_bytes(), originals[name])
                self.assertEqual((source / name).read_bytes(), originals[name])
                self.assertEqual((output / name).stat().st_mode & 0o777, row["mode"])
                self.assertEqual((output / name).stat().st_nlink, 1)
                self.assertNotEqual((output / name).stat().st_ino, (source / name).stat().st_ino)
            self.assertEqual(list((output / "checkout/src/empty-marker").iterdir()), [])

    def test_assembly_refuses_links_special_files_unbound_topology_and_changed_identity(self):
        for kind in ("symlink", "hardlink", "fifo", "changed", "mode", "extra-directory",
                     "unreadable-directory"):
            with self.subTest(kind=kind), TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                source, files, directories = self.fixture(root)
                path = source / "checkout/src/example.cc"
                if kind == "symlink":
                    target = root / "outside"
                    target.write_bytes(path.read_bytes())
                    path.unlink()
                    path.symlink_to(target)
                elif kind == "hardlink":
                    os.link(path, root / "outside-alias")
                elif kind == "fifo":
                    path.unlink()
                    os.mkfifo(path)
                elif kind == "changed":
                    path.write_bytes(b"x" * files["checkout/src/example.cc"]["bytes"])
                elif kind == "mode":
                    path.chmod(0o600)
                else:
                    _SOURCE.private_directory(source / "unexpected")
                    if kind == "unreadable-directory":
                        (source / "unexpected").chmod(0)
                try:
                    with self.assertRaises(_SOURCE.Refusal):
                        _SOURCE.assembly_tree(source, files, directories)
                finally:
                    if kind == "unreadable-directory":
                        (source / "unexpected").chmod(0o700)

    def test_assembly_inventory_refuses_case_prefix_escape_and_unsafe_mode_conflicts(self):
        row = {"path": "a", "bytes": 0, "sha256": hashlib.sha256(b"").hexdigest(), "mode": 0o600}
        for files, directories in (
            ({"a": row, "A": {**row, "path": "A"}}, {"."}),
            ({"a": row}, {".", "a"}),
            ({"../escape": {**row, "path": "../escape"}}, {".", ".."}),
            ({"a": {**row, "mode": 0o777}}, {"."}),
        ):
            with self.subTest(files=files):
                with self.assertRaises(_SOURCE.Refusal):
                    _SOURCE.assembly_layout(files, directories)



class SourceAliasInputTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        script = Path(__file__).resolve().parents[1] / "native_webrtc/git_source_stager.py"
        spec = importlib.util.spec_from_file_location("morpheus_actual_source_alias_tests", script)
        assert spec is not None and spec.loader is not None
        cls.source = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.source)

    def fixture(self, root, tree=False, missing=False):
        source = self.source
        for name in ("checkout", "checkout/src", "materials", "materials/original-alias-blobs"):
            source.private_directory(root / name)
        payload = b"synthetic original source fixture\n"
        blob = hashlib.sha1(b"blob " + str(len(payload)).encode() + b"\0" + payload).hexdigest()
        target = "original/one.h" if tree else "original.h"
        mode = "100644"
        if not missing:
            source.private_parents((root / "checkout/src" / target).parent, root / "checkout")
            (root / "checkout/src" / target).write_bytes(payload)
            os.chmod(root / "checkout/src" / target, 0o644)
        raw = b"absent.h" if missing else b"original" if tree else b"original.h"
        row = {"mount": "src", "path": "alias", "mode": "120000", "type": "blob",
            "oid": hashlib.sha1(b"blob " + str(len(raw)).encode() + b"\0" + raw).hexdigest(),
            "target": raw.decode(), "target_bytes": len(raw), "target_sha256": hashlib.sha256(raw).hexdigest(),
            "target_path": raw.decode(), "target_type": "missing" if missing else "tree" if tree else "file",
            "target_mode": None if missing else "040000" if tree else mode,
            "target_oid": None if missing else source.alias_tree([{"path": "one.h", "mode": mode, "oid": blob}]) if tree else blob,
            "action": "retain-dangling" if missing else "copy-tree" if tree else "copy-file"}
        original = {} if missing else {target: {"path": target, "mode": mode, "blob": blob,
            "bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()}}
        nodes = {name: {"mode": fact["mode"], "oid": fact["blob"]} for name, fact in original.items()}
        nodes["alias"] = {"mode": "120000", "oid": row["oid"]}
        spec = {"mount": "src", "revision": "a" * 40, "tree": source.alias_tree([
            {"path": name, **value} for name, value in nodes.items()])}
        owner = object.__new__(source.Git)
        owner.root = root
        owner.command = lambda argv, cap: raw
        receipts, files = owner.aliases(spec, [], root / "checkout", [row], nodes, original)
        return row, receipts, files, nodes

    def test_selected_original_profile_declares_all_aliases_and_markers(self):
        import json
        plan = json.loads((Path(__file__).resolve().parents[2] / "native/webrtc/linux/source-plan.json").read_bytes())
        self.assertEqual(plan["schema"], "morpheus-native-webrtc-sources-v3")
        specs = {row["mount"]: row for row in plan["repositories"]}
        markers = self.source.gitlink_policy(plan["gitlinks"], specs)
        aliases = self.source.alias_policy(plan["aliases"], specs, markers)
        self.assertEqual(len(markers["entries"]), 22)
        self.assertEqual(len(aliases), 69)
        self.assertEqual(sum(row["action"] == "copy-tree" for row in aliases.values()), 8)
        self.assertEqual(sum(row["action"] == "retain-dangling" for row in aliases.values()), 1)
        self.assertEqual(aliases["outer-depot/cros_sdk"]["target"], "cros")
        self.assertEqual(aliases["outer-depot/cros_sdk"]["oid"], "a876afe9bd699fefe0c719400b4b744582432854")

    def test_actual_copy_retains_original_blob_and_private_closed_projection(self):
        for tree in (False, True):
            with self.subTest(tree=tree), TemporaryDirectory() as temporary:
                root = Path(temporary)
                row, receipts, files, _ = self.fixture(root, tree=tree)
                self.source.verify_aliases(root / "checkout", receipts, root)
                self.assertEqual(len(files), 1)
                output = receipts[0]["outputs"][0]
                self.assertEqual((root / "checkout" / output["path"]).read_bytes(),
                                 (root / "checkout" / output["source_path"]).read_bytes())
                self.assertFalse((root / "checkout" / output["path"]).is_symlink())
                self.assertEqual((root / receipts[0]["original_blob_path"]).read_bytes(), row["target"].encode())
                (root / "checkout" / output["path"]).write_bytes(b"changed")
                with self.assertRaises(self.source.Refusal):
                    self.source.verify_aliases(root / "checkout", receipts, root)

    def test_copy_budget_refusal_precedes_every_alias_write_and_blob_command(self):
        for kind in ("files", "bytes"):
            with self.subTest(kind=kind), TemporaryDirectory() as temporary:
                root = Path(temporary)
                row, receipts, _, nodes = self.fixture(root)
                receipt, output = receipts[0], receipts[0]["outputs"][0]
                (root / "checkout" / output["path"]).unlink()
                (root / receipt["original_blob_path"]).unlink()
                source_path = output["source_path"].removeprefix("src/")
                originals = {source_path: {"path": source_path,
                    **{key: output[key] for key in ("mode", "blob", "bytes", "sha256")}}}
                owner = object.__new__(self.source.Git)
                owner.root = root
                calls = []
                owner.command = lambda argv, cap: calls.append(argv) or row["target"].encode()
                limits = {"file_budget": 1} if kind == "files" else {"byte_budget": output["bytes"]}
                with self.assertRaises(self.source.Refusal) as caught:
                    owner.aliases({"mount": "src", "revision": "a" * 40,
                        "tree": receipt["repository_tree"]}, [], root / "checkout", [row], nodes, originals, **limits)
                self.assertEqual(caught.exception.category, "resource")
                self.assertEqual(calls, [])
                self.assertFalse((root / "checkout" / output["path"]).exists())
                self.assertFalse((root / receipt["original_blob_path"]).exists())

    def test_copy_tree_refuses_extra_link_and_nested_original_modes(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            row, receipts, _, nodes = self.fixture(root, tree=True)
            extra = root / "checkout/src/alias/extra"
            extra.write_bytes(b"undeclared")
            with self.assertRaises(self.source.Refusal):
                self.source.verify_aliases(root / "checkout", receipts, root)
            extra.unlink()
            extra.symlink_to("one.h")
            with self.assertRaises(self.source.Refusal):
                self.source.verify_aliases(root / "checkout", receipts, root)
            extra.unlink()
            os.link(root / "checkout/src/alias/one.h", extra)
            with self.assertRaises(self.source.Refusal):
                self.source.verify_aliases(root / "checkout", receipts, root)
            extra.unlink()
            for mode in ("120000", "160000"):
                with self.subTest(mode=mode), self.assertRaises(self.source.Refusal):
                    self.source.alias_sources(row, {**nodes, "original/nested": {"mode": mode, "oid": "a" * 40}})

    def test_dangling_alias_remains_absent_and_original_target_is_retained(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            row, receipts, files, _ = self.fixture(root, missing=True)
            self.assertEqual(files, {})
            self.assertFalse((root / "checkout/src/alias").exists())
            self.source.verify_aliases(root / "checkout", receipts, root)
            (root / "checkout/src/absent.h").write_bytes(b"fabricated target")
            with self.assertRaises(self.source.Refusal):
                self.source.verify_aliases(root / "checkout", receipts, root)

    def test_alias_plan_refuses_escape_chain_graft_marker_and_changed_scope(self):
        with TemporaryDirectory() as temporary:
            row, _, _, _ = self.fixture(Path(temporary))
        source = self.source
        specs = {"src": {}}
        markers = source.gitlink_policy([], specs)
        aliases = source.alias_policy([row], specs, markers)
        for name in ("src/alias", "src/alias/child", "src/original.h", "src"):
            with self.subTest(name=name), self.assertRaises(source.Refusal):
                source.outside_aliases(name, aliases)
        raw = b"../outside"
        escape = {**row, "target": raw.decode(), "target_bytes": len(raw),
                  "target_sha256": hashlib.sha256(raw).hexdigest(),
                  "oid": hashlib.sha1(b"blob " + str(len(raw)).encode() + b"\0" + raw).hexdigest(),
                  "target_path": "outside"}
        with self.assertRaises(source.Refusal) as caught:
            source.alias_policy([escape], specs, markers)
        self.assertEqual(caught.exception.category, "alias_escape")
        with self.assertRaises(source.Refusal):
            source.alias_policy([row, {**row, "path": "ALIAS"}], specs, markers)
        for changed in ({"target_sha256": "0" * 64}, {"extra": True}):
            with self.subTest(changed=changed), self.assertRaises(source.Refusal):
                source.alias_policy([{**row, **changed}], specs, markers)
        with self.assertRaises(source.Refusal):
            source.alias_policy([row], {**specs, "src/alias/child": {}}, markers)
        marker = {"mount": "src", "path": "original.h", "mode": "160000", "type": "commit",
                  "oid": "b" * 40, "action": "uninitialized-directory"}
        with self.assertRaises(source.Refusal):
            source.alias_policy([row], specs, source.gitlink_policy([marker], specs))

if __name__ == "__main__":
    unittest.main()
