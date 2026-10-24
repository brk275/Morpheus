"""Focused scanner-boundary regressions; run only on fresh hosted runners."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from scripts.scan_history import MergeProvenance, classify, fixture_digest, git, load_manifest, scan


class HistoryScannerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="scanner-boundary-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "candidate"
        self.template = self.root / "template"
        self.template.mkdir()
        git("init", f"--template={self.template}", self.repo)
        git("-C", self.repo, "config", "user.name", "Morpheus fixture")
        git("-C", self.repo, "config", "user.email", "fixture@example.invalid")
        self.manifest = self.root / "reviewed.json"
        self.manifest.write_text('{"version":1,"fixtures":[]}')
        # Synthetic canary assembled here so the committed test has no key.
        self.canary = 'api_key = "' + "morpheusSynthetic" + "X7d9R2s8F4w6N3t5Q1v0" + '"\n'

    def commit(self, message):
        git("-C", self.repo, "add", ".")
        git("-C", self.repo, "commit", "-m", message)
        return git("-C", self.repo, "rev-parse", "HEAD").decode().strip()

    def execute(self, name):
        scanner = os.environ.get("MORPHEUS_REVIEWED_GITLEAKS")
        if not scanner:
            self.skipTest("real scanner boundary runs in the explicit hosted security job")
        return scan(self.repo, Path(scanner).resolve(), self.manifest, self.root / name)

    def test_candidate_suppressions_attributes_and_config_cannot_hide_canary(self):
        (self.repo / "fixture.txt").write_text(self.canary + "# gitleaks:allow\n")
        (self.repo / ".gitattributes").write_text("fixture.txt -diff\n")
        (self.repo / ".gitleaks.toml").write_text('[allowlist]\npaths = [".*"]\n')
        commit = self.commit("Synthetic detection fixture")
        (self.repo / ".gitleaksignore").write_text(f"{commit}:fixture.txt:generic-api-key:1\n")
        self.commit("Candidate suppression must be ignored")
        result = self.execute("untrusted-suppressions")
        self.assertGreater(result["unexpected_findings"], 0)
        self.assertEqual(result["reviewed_public_fixtures"], 0)

    def test_non_head_branch_history_is_scanned(self):
        (self.repo / "readme.txt").write_text("Synthetic history only\n")
        base = self.commit("Initial fixture")
        git("-C", self.repo, "checkout", "-b", "other-fixture")
        (self.repo / "fixture.txt").write_text(self.canary)
        self.commit("Synthetic canary on another ref")
        git("-C", self.repo, "checkout", "--detach", base)
        self.assertGreater(self.execute("other-ref")["unexpected_findings"], 0)

    def test_merge_only_introduction_is_scanned(self):
        (self.repo / "readme.txt").write_text("Synthetic merge history only\n")
        base = self.commit("Merge fixture base")
        git("-C", self.repo, "checkout", "-b", "side-fixture")
        (self.repo / "side.txt").write_text("Side parent has no credential\n")
        self.commit("Side parent")
        git("-C", self.repo, "checkout", "-b", "primary-fixture", base)
        (self.repo / "primary.txt").write_text("First parent has no credential\n")
        self.commit("First parent")
        git("-C", self.repo, "merge", "--no-ff", "--no-commit", "side-fixture")
        # Neither parent contains this file; only the merge introduces it.
        (self.repo / "fixture.txt").write_text(self.canary)
        merge = self.commit("Synthetic merge-only introduction")
        result = self.execute("merge-introduction")
        self.assertGreater(result["unexpected_findings"], 0)
        findings = json.loads((self.root / "merge-introduction/redacted-findings.json").read_text())
        self.assertTrue(any(f["Commit"] == merge and f["File"] == "fixture.txt" for f in findings))

    def test_exception_requires_exact_fingerprint_and_complete_blob_digest(self):
        (self.repo / "fixture.txt").write_text(self.canary)
        self.commit("Synthetic public fixture")
        first = self.execute("first")
        self.assertGreater(first["unexpected_findings"], 0)
        findings = json.loads((self.root / "first/redacted-findings.json").read_text())
        digest = hashlib.sha256((self.repo / "fixture.txt").read_bytes()).hexdigest()
        entries = [{"fingerprint": f["Fingerprint"], "blob_sha256": digest,
                    "provenance": "Generated disposable synthetic fixture"} for f in findings]
        self.manifest.write_text(json.dumps({"version": 1, "fixtures": entries}))
        accepted = self.execute("accepted")
        self.assertEqual(accepted["unexpected_findings"], 0)
        self.assertEqual(accepted["reviewed_public_fixtures"], len(findings))
        entries[0]["blob_sha256"] = "0" * 64
        self.manifest.write_text(json.dumps({"version": 1, "fixtures": entries}))
        self.assertGreater(self.execute("wrong-digest")["unexpected_findings"], 0)
        (self.repo / "fixture.txt").write_text(self.canary + "changed bytes\n")
        self.commit("New introduction must not inherit an exception")
        self.assertGreater(self.execute("new-introduction")["unexpected_findings"], 0)

    def test_digest_reuses_verified_blob_bytes_across_paths_and_commits(self):
        content = b"Reviewed synthetic public fixture bytes\n"
        (self.repo / "fixture.txt").write_bytes(content)
        (self.repo / "same.txt").write_bytes(content)
        original = self.commit("Identical fixture objects at separate paths")
        (self.repo / "readme.txt").write_text("Unrelated history only\n")
        later = self.commit("Same fixture object in another commit")
        expected = hashlib.sha256(content).hexdigest()
        proof = MergeProvenance(self.repo)
        with mock.patch("scripts.scan_history.fixture_digest", wraps=fixture_digest) as verify:
            self.assertEqual(proof.digest(original, "fixture.txt"), expected)
            self.assertEqual(proof.digest(original, "fixture.txt"), expected)
            self.assertEqual(proof.digest(original, "same.txt"), expected)
            self.assertEqual(proof.digest(later, "fixture.txt"), expected)
            verify.assert_called_once_with(self.repo, original, "fixture.txt", proof.query)
        self.assertEqual(len(proof.digests), 3)
        self.assertEqual(len(proof.blob_digests), 1)
        # One tree + the existing size/type/content queries for the first
        # object, then one tree per new location; the exact-key hit costs none.
        self.assertEqual(proof.queries, 6)

        # A byte cache is not path/commit admission. Reject malformed identities
        # before either cache, even after real verified content is present.
        for commit, path in (("HEAD", "fixture.txt"), ("A" * 40, "fixture.txt"),
                             (original, "../fixture.txt"), (original, "/fixture.txt"),
                             (original, "same.txt:other"), (original, "same.txt\n"),
                             (original, "")):
            with self.subTest(commit=commit, path=path), self.assertRaises(ValueError):
                proof.digest(commit, path)
        self.assertEqual(proof.queries, 6)

    def test_changed_blob_cannot_reuse_a_verified_digest_or_reviewed_exception(self):
        original_bytes = b"Reviewed synthetic fixture\n"
        (self.repo / "fixture.txt").write_bytes(original_bytes)
        original = self.commit("Reviewed original object")
        changed_bytes = original_bytes + b"Unreviewed additional bytes\n"
        (self.repo / "fixture.txt").write_bytes(changed_bytes)
        changed = self.commit("Changed whole-blob content")
        original_digest = hashlib.sha256(original_bytes).hexdigest()
        changed_digest = hashlib.sha256(changed_bytes).hexdigest()
        proof = MergeProvenance(self.repo)
        with mock.patch("scripts.scan_history.fixture_digest", wraps=fixture_digest) as verify:
            self.assertEqual(proof.digest(original, "fixture.txt"), original_digest)
            self.assertEqual(proof.digest(changed, "fixture.txt"), changed_digest)
            self.assertEqual(verify.call_count, 2)
        self.assertNotEqual(proof.blob(original, "fixture.txt")[1],
                            proof.blob(changed, "fixture.txt")[1])
        self.assertEqual(len(proof.blob_digests), 2)

        def finding(commit):
            return {"Commit": commit, "File": "fixture.txt", "RuleID": "generic-api-key",
                    "StartLine": 1, "Fingerprint": f"{commit}:fixture.txt:generic-api-key:1"}

        findings = [finding(original), finding(changed)]
        fixtures = {row["Fingerprint"]: {"blob_sha256": original_digest}
                    for row in findings}
        self.assertEqual(classify(self.repo, findings, fixtures),
                         {"findings": 2, "reviewed_public_fixtures": 1, "unexpected_findings": 1})

    def test_scanner_fingerprint_mismatch_fails_closed(self):
        with self.assertRaisesRegex(ValueError, "fingerprint"):
            classify(self.repo, [{"Commit": "0" * 40, "File": "fixture.txt",
                                 "RuleID": "generic-api-key", "StartLine": 1,
                                 "Fingerprint": "different"}], {})

    def report(self, name):
        return json.loads((self.root / name / "redacted-findings.json").read_text())

    def reviewed_branch(self):
        (self.repo / "readme.txt").write_text("Synthetic merge provenance only\n")
        base = self.commit("Provenance base")
        git("-C", self.repo, "checkout", "-b", "reviewed-fixture")
        (self.repo / "fixture.txt").write_text(self.canary)
        original = self.commit("Reviewed synthetic original")
        self.assertGreater(self.execute("seed")["unexpected_findings"], 0)
        seeds = self.report("seed")
        self.assertTrue(seeds)
        self.assertTrue(all(f["Commit"] == original and f["File"] == "fixture.txt" for f in seeds))
        digest = hashlib.sha256((self.repo / "fixture.txt").read_bytes()).hexdigest()
        self.manifest.write_text(json.dumps({"version": 1, "fixtures": [
            {"fingerprint": f["Fingerprint"], "blob_sha256": digest,
             "provenance": "Generated disposable synthetic merge fixture"} for f in seeds
        ]}))
        return base, original, seeds

    def begin_merge(self, base, side="reviewed-fixture", primary="primary-fixture"):
        git("-C", self.repo, "checkout", "-b", primary, base)
        (self.repo / "primary.txt").write_text("Unrelated first-parent change\n")
        self.commit("Divergent first parent")
        git("-C", self.repo, "merge", "--no-ff", "--no-commit", side)

    def test_merge_copy_requires_direct_same_scan_seed_and_exact_identity(self):
        base, original, seeds = self.reviewed_branch()
        self.begin_merge(base)
        merge = self.commit("Byte-identical reviewed merge copy")
        result = self.execute("merge-copy")
        findings = self.report("merge-copy")
        copies = [f for f in findings if f["Commit"] == merge]
        self.assertTrue(copies)
        self.assertEqual(result["unexpected_findings"], 0)
        self.assertEqual(result["reviewed_public_fixtures"], len(findings))
        fixtures = load_manifest(self.manifest)
        # A manifest entry alone cannot seed inheritance; its original finding
        # must also have been directly accepted in this same scanner result.
        self.assertEqual(classify(self.repo, copies, fixtures)["unexpected_findings"], len(copies))
        for field, value in (("File", "other.txt"), ("RuleID", "other-rule"), ("StartLine", 2)):
            changed = dict(copies[0], **{field: value})
            changed["Fingerprint"] = (
                f'{merge}:{changed["File"]}:{changed["RuleID"]}:{changed["StartLine"]}'
            )
            with self.subTest(field=field):
                self.assertEqual(classify(self.repo, seeds + [changed], fixtures)["unexpected_findings"], 1)
        self.assertNotEqual(original, merge)

    def test_changed_merge_blob_and_additional_secret_remain_unexpected(self):
        base, _, _ = self.reviewed_branch()
        self.begin_merge(base)
        (self.repo / "fixture.txt").write_text(self.canary + "Changed whole-blob bytes\n")
        (self.repo / "new-secret.txt").write_text(self.canary.replace("X7d9R2s8F4w6N3t5Q1v0", "V9k2T8p4J6c1W7n5B3z0"))
        merge = self.commit("Modified fixture and unreviewed merge-only secret")
        result = self.execute("changed-merge")
        copies = [f for f in self.report("changed-merge") if f["Commit"] == merge]
        self.assertTrue(any(f["File"] == "fixture.txt" for f in copies))
        self.assertTrue(any(f["File"] == "new-secret.txt" for f in copies))
        self.assertEqual(result["unexpected_findings"], len(copies))

    def test_single_parent_copy_cannot_inherit_a_reviewed_exception(self):
        base, _, _ = self.reviewed_branch()
        git("-C", self.repo, "checkout", "-b", "single-parent-copy", base)
        (self.repo / "fixture.txt").write_text(self.canary)
        copy = self.commit("Same bytes introduced without a merge")
        result = self.execute("single-parent")
        copies = [f for f in self.report("single-parent") if f["Commit"] == copy]
        self.assertTrue(copies)
        self.assertEqual(result["unexpected_findings"], len(copies))

    def test_merge_copy_at_another_path_remains_unexpected(self):
        base, _, _ = self.reviewed_branch()
        self.begin_merge(base)
        (self.repo / "fixture.txt").rename(self.repo / "other.txt")
        merge = self.commit("Same blob at an unreviewed path")
        result = self.execute("different-path")
        copies = [f for f in self.report("different-path") if f["Commit"] == merge]
        self.assertTrue(any(f["File"] == "other.txt" for f in copies))
        self.assertEqual(result["unexpected_findings"], len(copies))

    def test_identical_nonfirst_parent_requires_original_ancestry(self):
        base, _, seeds = self.reviewed_branch()
        git("-C", self.repo, "checkout", "-b", "unrelated-side", base)
        (self.repo / "fixture.txt").write_text(self.canary)
        self.commit("Identical bytes without reviewed original ancestry")
        self.begin_merge(base, side="unrelated-side")
        merge = self.commit("Merge an unrelated identical blob")
        self.assertGreater(self.execute("unrelated-original")["unexpected_findings"], 0)
        copies = [f for f in self.report("unrelated-original") if f["Commit"] == merge]
        self.assertTrue(copies)
        result = classify(self.repo, seeds + copies, load_manifest(self.manifest))
        self.assertEqual(result["reviewed_public_fixtures"], len(seeds))
        self.assertEqual(result["unexpected_findings"], len(copies))

    def test_nonregular_entries_and_first_parent_ownership_cannot_sponsor_a_copy(self):
        base, original, seeds = self.reviewed_branch()
        self.begin_merge(base)
        (self.repo / "fixture.txt").unlink()
        os.symlink(self.canary, self.repo / "fixture.txt")
        symlink_merge = self.commit("Symlink mode is not an inherited public fixture")
        fixtures = load_manifest(self.manifest)

        def copy_at(commit):
            finding = dict(seeds[0], Commit=commit)
            finding["Fingerprint"] = (
                f'{commit}:fixture.txt:{finding["RuleID"]}:{finding["StartLine"]}'
            )
            return finding

        # Real Git trees with scanner-derived identities exercise modes even
        # if a scanner version elects not to emit a symlink-target finding.
        self.assertEqual(classify(self.repo, seeds + [copy_at(symlink_merge)], fixtures)["unexpected_findings"], 1)
        git("-C", self.repo, "checkout", "-b", "symlink-side", original)
        (self.repo / "fixture.txt").unlink()
        os.symlink(self.canary, self.repo / "fixture.txt")
        side = self.commit("Nonfirst parent holds only a symlink")
        self.begin_merge(base, side=side, primary="regular-merge")
        (self.repo / "fixture.txt").unlink()
        (self.repo / "fixture.txt").write_text(self.canary)
        regular_merge = self.commit("Regular result has no regular nonfirst sponsor")
        self.assertEqual(classify(self.repo, seeds + [copy_at(regular_merge)], fixtures)["unexpected_findings"], 1)
        git("-C", self.repo, "checkout", "-b", "already-owned-side", original)
        (self.repo / "side.txt").write_text("No fixture change\n")
        self.commit("Side also has the original blob")
        self.begin_merge(original, side="already-owned-side", primary="already-owned-primary")
        owned_merge = self.commit("First parent already owns the identical blob")
        self.assertEqual(classify(self.repo, seeds + [copy_at(owned_merge)], fixtures)["unexpected_findings"], 1)

    def test_merge_proof_git_errors_malformed_fields_and_limits_fail_closed(self):
        base, _, seeds = self.reviewed_branch()
        self.begin_merge(base)
        merge = self.commit("Valid merge used for failure-boundary checks")
        self.assertEqual(self.execute("proof-limits")["unexpected_findings"], 0)
        findings = self.report("proof-limits")
        fixtures = load_manifest(self.manifest)
        self.assertTrue(any(f["Commit"] == merge for f in findings))
        with self.assertRaises(subprocess.CalledProcessError):
            classify(self.root / "missing-repository", findings, fixtures)
        for field, value in (("Commit", "not-a-commit"), ("File", "../fixture.txt"), ("StartLine", True)):
            malformed = dict(seeds[0], **{field: value})
            malformed["Fingerprint"] = (
                f'{malformed["Commit"]}:{malformed["File"]}:{malformed["RuleID"]}:{malformed["StartLine"]}'
            )
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "fingerprint"):
                classify(self.repo, [malformed], fixtures)
        for bound, value in (("MAX_FINDINGS", 0), ("MAX_PROVENANCE_QUERIES", 0),
                             ("MAX_MERGE_PARENTS", 1), ("MAX_PROVENANCE_COMMIT", 0),
                             ("MAX_MERGE_PROOFS", 0),
                             ("MAX_PROVENANCE_SECONDS", 0)):
            with self.subTest(bound=bound), mock.patch(f"scripts.scan_history.{bound}", value):
                with self.assertRaisesRegex(ValueError, "limit"):
                    classify(self.repo, findings, fixtures)
