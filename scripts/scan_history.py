#!/usr/bin/env python3
"""Scan Git data with default rules; trust exceptions only from reviewed inputs.

Invoke this script and its manifest from the PR base, never the candidate. Reports
stay in the private runner temp directory; the console contains counts only.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

MAX_REPORT = 64 * 1024 * 1024
MAX_FIXTURE = 4 * 1024 * 1024
MAX_FINDINGS = 4096
MAX_MERGE_PARENTS = 8
MAX_PROVENANCE_COMMIT = 64 * 1024
MAX_PROVENANCE_QUERIES = 512
MAX_MERGE_PROOFS = 512
MAX_PROVENANCE_SECONDS = 60
SCANNER_SHA256 = "88f91962aa2f93ac6ab281d553b9e125f5197bbbce38f9f2437f7299c32e5509"


def git_env():
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "GITLEAKS_"))}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
               GIT_TERMINAL_PROMPT="0", GIT_NO_REPLACE_OBJECTS="1",
               GIT_ATTR_NOSYSTEM="1", GIT_OPTIONAL_LOCKS="0")
    return env


def git(*args, cwd=None, timeout=90):
    return subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
         "-c", "core.attributesFile=/dev/null", *map(str, args)],
        cwd=cwd, env=git_env(), check=True, capture_output=True, timeout=timeout,
    ).stdout


def fixture_object(commit, path):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("invalid fixture commit")
    if not isinstance(path, str) or not path or path.startswith(("/", "-")) or any(
        part in ("", ".", "..") for part in path.split("/")
    ) or any(c in path for c in "\x00\r\n:"):
        raise ValueError("invalid fixture path")
    return f"{commit}:{path}"


def fixture_digest(repository, commit, path, query=git):
    obj = fixture_object(commit, path)
    size = int(query("-C", repository, "cat-file", "-s", obj))
    if not 0 < size <= MAX_FIXTURE:
        raise ValueError("fixture exceeds size bound")
    if query("-C", repository, "cat-file", "-t", obj).strip() != b"blob":
        raise ValueError("fixture is not a blob")
    return hashlib.sha256(query("-C", repository, "cat-file", "blob", obj)).hexdigest()


def load_manifest(path):
    data = json.loads(path.read_text(encoding="utf-8"))
    if set(data) != {"version", "fixtures"} or data["version"] != 1:
        raise ValueError("invalid reviewed fixture manifest")
    fixtures = {}
    for row in data["fixtures"]:
        if set(row) != {"fingerprint", "blob_sha256", "provenance"}:
            raise ValueError("invalid fixture fields")
        fp = row["fingerprint"]
        if (not isinstance(fp, str) or fp in fixtures
                or not re.fullmatch(r"[0-9a-f]{40}:[^\r\n:]+:[a-z0-9-]+:[1-9][0-9]*", fp)
                or not re.fullmatch(r"[0-9a-f]{64}", row["blob_sha256"])
                or not isinstance(row["provenance"], str) or not row["provenance"].strip()):
            raise ValueError("invalid or duplicate fixture")
        fixtures[fp] = row
    return fixtures


def finding_identity(finding):
    commit, path = finding["Commit"], finding["File"]
    rule, line = finding["RuleID"], finding["StartLine"]
    if (not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit)
            or not isinstance(path, str) or not 0 < len(path) <= 4096
            or path.startswith(("/", "-"))
            or any(part in ("", ".", "..") for part in path.split("/"))
            or any(c in path for c in "\x00\r\n:")
            or not isinstance(rule, str) or not re.fullmatch(r"[a-z0-9-]{1,128}", rule)
            or type(line) is not int or not 0 < line <= 2**31 - 1):
        raise ValueError("invalid scanner fingerprint fields")
    # Do not let a scanner-format change accidentally broaden exceptions.
    if finding.get("Fingerprint") != f"{commit}:{path}:{rule}:{line}":
        raise ValueError("scanner fingerprint mismatch")
    return commit, path, rule, line


class MergeProvenance:
    """Bound and cache proof queries; never accept a failed or partial proof."""

    def __init__(self, repository):
        self.repository = repository
        self.deadline = time.monotonic() + MAX_PROVENANCE_SECONDS
        self.queries = 0
        self.proofs = 0
        self.digests = {}
        self.blob_digests = {}
        self.parents = {}
        self.blobs = {}
        self.ancestors = {}

    def query(self, *args):
        self.queries += 1
        remaining = self.deadline - time.monotonic()
        if self.queries > MAX_PROVENANCE_QUERIES or remaining <= 0:
            raise ValueError("fixture provenance query limit")
        return git(*args, timeout=remaining)

    def digest(self, commit, path):
        # Validation is required even on a cache hit; a cached object never
        # authorizes symbolic commits or a different malformed path.
        fixture_object(commit, path)
        key = (commit, path)
        if key not in self.digests:
            entry = self.blob(commit, path)
            if entry is None:
                raise ValueError("fixture is not a blob")
            _, oid = entry
            if oid not in self.blob_digests:
                # The first object still requires the complete original
                # size/type/SHA256 checks. Only an exact parsed Git blob OID
                # reuses that verified content, never a path or fingerprint.
                self.blob_digests[oid] = fixture_digest(
                    self.repository, commit, path, self.query,
                )
            self.digests[key] = self.blob_digests[oid]
        return self.digests[key]

    def commit_parents(self, commit):
        if commit not in self.parents:
            size = int(self.query("-C", self.repository, "cat-file", "-s", commit))
            if not 0 < size <= MAX_PROVENANCE_COMMIT:
                raise ValueError("fixture commit size limit")
            if self.query("-C", self.repository, "cat-file", "-t", commit).strip() != b"commit":
                raise ValueError("fixture merge is not a commit")
            data = self.query("-C", self.repository, "rev-list", "--parents", "--no-walk", commit)
            if len(data) > 41 * (MAX_MERGE_PARENTS + 1):
                raise ValueError("fixture merge parent limit")
            words = data.decode("ascii").split()
            if (not words or words[0] != commit or len(words) > MAX_MERGE_PARENTS + 1
                    or any(not re.fullmatch(r"[0-9a-f]{40}", word) for word in words)
                    or len(set(words)) != len(words)):
                raise ValueError("invalid fixture merge parents")
            self.parents[commit] = tuple(words[1:])
        return self.parents[commit]

    def blob(self, commit, path):
        key = (commit, path)
        if key not in self.blobs:
            data = self.query("-C", self.repository, "--literal-pathspecs", "ls-tree", "-z",
                              commit, "--", path)
            if not data:
                self.blobs[key] = None
            else:
                if len(data) > 8192 or not data.endswith(b"\0") or data.count(b"\0") != 1:
                    raise ValueError("invalid fixture tree entry")
                metadata, name = data[:-1].split(b"\t", 1)
                mode, kind, oid = metadata.split()
                if (name != path.encode("utf-8") or not re.fullmatch(rb"[0-9a-f]{40}", oid)
                        or mode not in (b"100644", b"100755", b"120000", b"040000", b"160000")
                        or kind not in (b"blob", b"tree", b"commit")):
                    raise ValueError("invalid fixture tree entry")
                self.blobs[key] = (mode, oid) if kind == b"blob" else None
        return self.blobs[key]

    def ancestor(self, original, parent):
        key = (original, parent)
        if key not in self.ancestors:
            try:
                self.query("-C", self.repository, "merge-base", "--is-ancestor", original, parent)
                self.ancestors[key] = True
            except subprocess.CalledProcessError as error:
                if error.returncode != 1:
                    raise
                self.ancestors[key] = False
        return self.ancestors[key]

    def inherits(self, commit, path, original, digest):
        self.proofs += 1
        if self.proofs > MAX_MERGE_PROOFS or time.monotonic() >= self.deadline:
            raise ValueError("fixture merge proof limit")
        parents = self.commit_parents(commit)
        if len(parents) < 2:
            return False
        merged = self.blob(commit, path)
        source = self.blob(original, path)
        first = self.blob(parents[0], path)
        regular = (b"100644", b"100755")
        if (merged is None or merged[0] not in regular
                or source is None or source[0] not in regular or source[1] != merged[1]
                or (first is not None and first[1] == merged[1])
                or self.digest(commit, path) != digest):
            return False
        for parent in parents[1:]:
            entry = self.blob(parent, path)
            if (entry is not None and entry[0] in regular and entry[1] == merged[1]
                    and self.ancestor(original, parent)):
                return True
        return False


def classify(repository, findings, fixtures):
    if not isinstance(findings, list) or len(findings) > MAX_FINDINGS:
        raise ValueError("scanner finding limit")
    identities = [finding_identity(finding) for finding in findings]
    proof = MergeProvenance(repository)
    direct = set()
    seeds = {}
    # Only exact, whole-blob reviewed findings present in THIS scan are seeds.
    for index, (commit, path, rule, line) in enumerate(identities):
        row = fixtures.get(findings[index]["Fingerprint"])
        if row is not None and proof.digest(commit, path) == row["blob_sha256"]:
            direct.add(index)
            seeds.setdefault((path, rule, line), set()).add((commit, row["blob_sha256"]))
    known = len(direct)
    for index, (commit, path, rule, line) in enumerate(identities):
        if index not in direct and any(
            proof.inherits(commit, path, original, digest)
            for original, digest in sorted(seeds.get((path, rule, line), ()))
        ):
            # Inherited findings are deliberately never added to seeds.
            known += 1
    return {"findings": len(findings), "reviewed_public_fixtures": known,
            "unexpected_findings": len(findings) - known}


def scan(candidate, scanner, manifest, evidence):
    if hashlib.sha256(scanner.read_bytes()).hexdigest() != SCANNER_SHA256:
        raise ValueError("unreviewed scanner executable")
    fixtures = load_manifest(manifest)
    evidence.mkdir(mode=0o700, parents=True, exist_ok=False)
    # A bare mirror has no candidate worktree .gitleaksignore to autoload. An
    # explicit empty --gitleaks-ignore-path alone DOES NOT prevent that autoload.
    with tempfile.TemporaryDirectory(prefix="history-scan-") as temp:
        root = Path(temp)
        template = root / "empty-template"
        template.mkdir()
        mirror = root / "history.git"
        source_head = git("-C", candidate, "rev-parse", "HEAD").strip()
        if git("-C", candidate, "rev-parse", "--is-shallow-repository").strip() != b"false":
            raise ValueError("complete history required")
        git("clone", "--mirror", "--no-hardlinks", f"--template={template}", "--", candidate, mirror)
        if git("-C", mirror, "rev-parse", "HEAD").strip() != source_head:
            raise ValueError("mirror head mismatch")
        config = root / "default.toml"
        config.write_text("[extend]\nuseDefault = true\n", encoding="utf-8")
        ignore = root / "empty.ignore"
        ignore.write_text("", encoding="utf-8")
        report = evidence / "redacted-findings.json"
        with (evidence / "scanner.log").open("wb") as log:
            result = subprocess.run(
                [str(scanner), "git", str(mirror), "--config", str(config),
                 "--gitleaks-ignore-path", str(ignore), "--ignore-gitleaks-allow",
                 "--redact=100", "--no-banner", "--max-target-megabytes", "0",
                 "--report-format", "json", "--report-path", str(report),
                 "--log-opts=--all --full-history --text --no-ext-diff --no-textconv --diff-merges=first-parent"],
                cwd=root, env=git_env(), stdout=log, stderr=subprocess.STDOUT, timeout=240,
            )
        if result.returncode not in (0, 1) or not report.is_file() or report.stat().st_size > MAX_REPORT:
            raise ValueError("scanner failed or report exceeds bound")
        findings = json.loads(report.read_text(encoding="utf-8"))
        if not isinstance(findings, list) or (result.returncode == 0) != (not findings):
            raise ValueError("invalid scanner result")
        summary = classify(mirror, findings, fixtures)
        summary.update(source=source_head.decode("ascii"),
                       scanner_sha256=SCANNER_SHA256,
                       report_sha256=hashlib.sha256(report.read_bytes()).hexdigest())
        (evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--scanner", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        result = scan(args.candidate.resolve(), args.scanner.resolve(), args.manifest.resolve(), args.evidence.resolve())
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        # Never reflect server data, candidate paths or secret report contents.
        raise SystemExit("History scanner failed; private runner evidence requires review.") from None
    print(json.dumps(result, sort_keys=True))
    raise SystemExit(1 if result["unexpected_findings"] else 0)


if __name__ == "__main__":
    main()
