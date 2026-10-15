"""Cargo evidence parsing on inert files only; no Rust or native execution."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from scripts.native_vault_artifact import select_artifact


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name).resolve()
        (self.repo / "target/debug/deps").mkdir(parents=True)
        crate = self.repo / "crates/morpheus-matrix"
        (crate / "tests").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname="morpheus-matrix"\nversion="0.1.0"\n')
        (crate / "tests/native_vault.rs").write_text("// inert source fixture\n")
        self.binary = self.repo / "target/debug/deps/native_vault-inert"
        self.binary.write_bytes(b"inert fixture, never executable")
        self.metadata = {"version": 1, "packages": [{
            "name": "morpheus-matrix", "source": None, "id": "local-package-identity",
            "manifest_path": str(crate / "Cargo.toml"),
        }], "workspace_members": ["local-package-identity"], "target_directory": str(self.repo / "target")}
        self.artifact = {
            "reason": "compiler-artifact", "package_id": "local-package-identity",
            "manifest_path": str(crate / "Cargo.toml"),
            "target": {"name": "native_vault", "kind": ["test"], "src_path": str(crate / "tests/native_vault.rs")},
            "profile": {"test": True}, "executable": str(self.binary), "filenames": [str(self.binary)],
        }
        self.finished = {"reason": "build-finished", "success": True}

    def select(self, records=None, metadata=None):
        records = [self.artifact, self.finished] if records is None else records
        return select_artifact(self.repo, json.dumps(self.metadata if metadata is None else metadata).encode(),
                               b"\n".join(json.dumps(item).encode() for item in records))

    def test_one_exact_test_artifact_is_hashed_without_execution(self):
        self.assertEqual(self.select(), (self.binary, hashlib.sha256(self.binary.read_bytes()).hexdigest()))

    def test_missing_duplicate_or_incomplete_build_is_rejected(self):
        for records in ([self.finished], [self.artifact, self.artifact, self.finished],
                        [self.artifact], [self.artifact, {"reason": "build-finished", "success": False}],
                        [self.artifact, self.finished, self.finished], [self.finished, self.artifact]):
            with self.subTest(records=records), self.assertRaises(Exception):
                self.select(records)

    def test_wrong_package_target_source_profile_or_manifest_is_rejected(self):
        edits = [lambda a: a.update(package_id="foreign-package"),
                 lambda a: a["target"].update(kind=["bin"]),
                 lambda a: a["target"].update(name="another_test"),
                 lambda a: a["target"].update(src_path=str(self.repo / "Cargo.toml")),
                 lambda a: a["profile"].update(test=1),
                 lambda a: a.update(manifest_path=str(self.binary)),
                 lambda a: a.update(filenames=[])]
        for edit in edits:
            artifact = copy.deepcopy(self.artifact)
            edit(artifact)
            with self.assertRaises(Exception):
                self.select([artifact, self.finished])

    def test_registry_or_nonmember_package_metadata_cannot_authorize_local_file(self):
        for edit in (lambda m: m["packages"][0].update(source="registry+foreign"),
                     lambda m: m.update(workspace_members=[]),
                     lambda m: m["packages"].append(copy.deepcopy(m["packages"][0])),
                     lambda m: m.update(target_directory=str(self.repo))):
            metadata = copy.deepcopy(self.metadata)
            edit(metadata)
            with self.assertRaises(Exception):
                self.select(metadata=metadata)

    def test_foreign_or_symlink_executable_cannot_be_selected(self):
        foreign = self.repo / "foreign-inert"
        foreign.write_bytes(b"not executable")
        artifact = copy.deepcopy(self.artifact)
        artifact.update(executable=str(foreign), filenames=[str(foreign)])
        with self.assertRaises(Exception):
            self.select([artifact, self.finished])
        linked = self.binary.with_name("linked-inert")
        try:
            linked.symlink_to(self.binary)
        except OSError:
            self.skipTest("symlinks unavailable for this unprivileged test")
        artifact.update(executable=str(linked), filenames=[str(linked)])
        with self.assertRaises(Exception):
            self.select([artifact, self.finished])

    def test_duplicate_json_keys_and_raw_build_noise_are_rejected(self):
        metadata = json.dumps(self.metadata).encode()
        raw = b"\n".join(json.dumps(v).encode() for v in [self.artifact, self.finished])
        for malformed in (raw.replace(b'"success": true', b'"success": true,"success": false'),
                          b"unstructured build output\n" + raw):
            with self.assertRaises(Exception):
                select_artifact(self.repo, metadata, malformed)


if __name__ == "__main__":
    unittest.main()
