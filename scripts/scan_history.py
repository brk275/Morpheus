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

MAX_REPORT = 64 * 1024 * 1024
MAX_FIXTURE = 4 * 1024 * 1024
SCANNER_SHA256 = "88f91962aa2f93ac6ab281d553b9e125f5197bbbce38f9f2437f7299c32e5509"


def git_env():
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "GITLEAKS_"))}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
               GIT_TERMINAL_PROMPT="0", GIT_NO_REPLACE_OBJECTS="1",
               GIT_ATTR_NOSYSTEM="1", GIT_OPTIONAL_LOCKS="0")
    return env


def git(*args, cwd=None):
    return subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
         "-c", "core.attributesFile=/dev/null", *map(str, args)],
        cwd=cwd, env=git_env(), check=True, capture_output=True, timeout=90,
    ).stdout


def fixture_digest(repository, commit, path):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("invalid fixture commit")
    if not isinstance(path, str) or not path or path.startswith(("/", "-")) or any(
        part in ("", ".", "..") for part in path.split("/")
    ) or any(c in path for c in "\x00\r\n:"):
        raise ValueError("invalid fixture path")
    obj = f"{commit}:{path}"
    size = int(git("-C", repository, "cat-file", "-s", obj))
    if not 0 < size <= MAX_FIXTURE:
        raise ValueError("fixture exceeds size bound")
    if git("-C", repository, "cat-file", "-t", obj).strip() != b"blob":
        raise ValueError("fixture is not a blob")
    return hashlib.sha256(git("-C", repository, "cat-file", "blob", obj)).hexdigest()


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


def classify(repository, findings, fixtures):
    known = 0
    digests = {}
    for finding in findings:
        commit, path = finding["Commit"], finding["File"]
        expected = f'{commit}:{path}:{finding["RuleID"]}:{finding["StartLine"]}'
        # Do not let a scanner-format change accidentally broaden exceptions.
        if finding.get("Fingerprint") != expected:
            raise ValueError("scanner fingerprint mismatch")
        row = fixtures.get(expected)
        if row is None:
            continue
        key = (commit, path)
        if key not in digests:
            digests[key] = fixture_digest(repository, commit, path)
        if digests[key] == row["blob_sha256"]:
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
