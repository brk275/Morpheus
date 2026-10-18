"""Exact source identity for explicitly reviewed, manual package QA.

The workflow revision and built revision can differ. Keep them distinct; this
checks commit/checkout identity, never substitutes for maintainer source review.
"""

import argparse
import os
from pathlib import Path
import re
import subprocess


class SourceRevisionError(ValueError):
    def __init__(self, reason):
        self.reason = reason if reason in {"commit", "checkout", "dirty"} else "checkout"
        super().__init__("Package QA source verification failed: " + self.reason)


def valid_revision(value):
    return (type(value) is str and re.fullmatch(r"[0-9a-f]{40}", value) is not None
            and value != "0" * 40)


def select_revision(requested, workflow_revision):
    if not valid_revision(workflow_revision):
        raise SourceRevisionError("commit")
    selected = workflow_revision if requested == "" else requested
    if not valid_revision(selected):
        raise SourceRevisionError("commit")
    return selected


def expected_revision(environment):
    return select_revision(environment.get("MORPHEUS_PACKAGE_SOURCE_REVISION", ""),
                           environment.get("GITHUB_SHA", ""))


def verify_checkout(root, expected):
    if not valid_revision(expected):
        raise SourceRevisionError("commit")
    try:
        result = subprocess.run(["git", "rev-parse", "--verify", "HEAD^{commit}"], cwd=Path(root),
                                capture_output=True, timeout=20, check=False)
        if result.returncode or result.stdout != (expected + "\n").encode("ascii") or result.stderr:
            raise SourceRevisionError("checkout")
        clean = subprocess.run(["git", "diff", "--no-ext-diff", "--no-textconv", "--quiet", "HEAD", "--"],
                               cwd=Path(root), capture_output=True, timeout=20, check=False)
        if clean.returncode or clean.stdout or clean.stderr:
            raise SourceRevisionError("dirty")
    except (OSError, subprocess.SubprocessError):
        raise SourceRevisionError("checkout") from None
    return expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-checkout", required=True, type=Path)
    args = parser.parse_args()
    try:
        revision = verify_checkout(args.verify_checkout, expected_revision(os.environ))
    except SourceRevisionError as error:
        raise SystemExit(str(error)) from None
    print("Package QA source verified: " + revision)


if __name__ == "__main__":
    main()
