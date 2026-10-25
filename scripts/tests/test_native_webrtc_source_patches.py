"""Focused regressions for the exact native source patch input contract."""
from pathlib import Path
import hashlib
import importlib.util
from tempfile import TemporaryDirectory
import unittest

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


if __name__ == "__main__":
    unittest.main()
