"""Focused scanner-boundary regressions; run only on fresh hosted runners."""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

from scripts.scan_history import classify, git, scan


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

    def test_scanner_fingerprint_mismatch_fails_closed(self):
        with self.assertRaisesRegex(ValueError, "fingerprint"):
            classify(self.repo, [{"Commit": "0" * 40, "File": "fixture.txt",
                                 "RuleID": "generic-api-key", "StartLine": 1,
                                 "Fingerprint": "different"}], {})
