"""Exact source identity for explicitly reviewed, manual package QA.

The workflow revision and built revision can differ. Keep them distinct; this
checks commit/checkout identity, never substitutes for maintainer source review.
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import stat
import tarfile
from pathlib import PurePosixPath


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
        # Git's single result line may use either platform line ending.
        lines = {(expected + "\n").encode("ascii"), (expected + "\r\n").encode("ascii")}
        if result.returncode or result.stdout not in lines or result.stderr:
            raise SourceRevisionError("checkout")
        clean = subprocess.run(["git", "diff", "--no-ext-diff", "--no-textconv", "--quiet", "HEAD", "--"],
                               cwd=Path(root), capture_output=True, timeout=20, check=False)
        if clean.returncode or clean.stdout or clean.stderr:
            raise SourceRevisionError("dirty")
    except (OSError, subprocess.SubprocessError):
        raise SourceRevisionError("checkout") from None
    return expected


UPDATE_VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+-updateqa\.[12]")
UPDATE_VERSION_PATHS = frozenset({"crates/morpheus-app/Cargo.toml", "Cargo.lock"})
MAX_EXPORT_BYTES = 64 * 1024 * 1024


def versioned_source_bytes(path, data, version):
    """Only the existing two persistence-cohort version exclusions may vary."""
    if type(version) is not str or len(version) > 128 or not UPDATE_VERSION.fullmatch(version):
        raise SourceRevisionError("checkout")
    if path not in UPDATE_VERSION_PATHS:
        return data
    try:
        text = data.decode("utf-8")
    except UnicodeError:
        raise SourceRevisionError("checkout") from None
    if path == "crates/morpheus-app/Cargo.toml":
        section = re.search(r"(?ms)^\[package\]\n(.*?)(?=^\[|\Z)", text)
        if section is None:
            raise SourceRevisionError("checkout")
        body, count = re.subn(r'^version = "[^"]+"$', 'version = "' + version + '"', section[1], flags=re.M)
        if count != 1:
            raise SourceRevisionError("checkout")
        text = text[:section.start(1)] + body + text[section.end(1):]
    else:
        blocks = text.split("[[package]]")
        count = 0
        for index, block in enumerate(blocks):
            if re.search(r'^name = "morpheus-app"$', block, re.M):
                blocks[index], changed = re.subn(r'^version = "[^"]+"$', 'version = "' + version + '"', block, flags=re.M)
                if changed != 1:
                    raise SourceRevisionError("checkout")
                count += 1
        if count != 1:
            raise SourceRevisionError("checkout")
        text = "[[package]]".join(blocks)
    return text.encode("utf-8")


def verify_versioned_export(root, archive, version):
    """Compare the complete export to authenticated Git archive bytes, not a flag.

    The caller must first authenticate the clean reviewed checkout/revision and
    obtain these bytes from Git itself. This allows no arbitrary file exclusion.
    """
    if type(archive) is not bytes or not 1 <= len(archive) <= MAX_EXPORT_BYTES:
        raise SourceRevisionError("checkout")
    root = Path(root)
    expected = {}
    expanded = 0
    try:
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as source:
            names = set()
            for member in source:
                pure = PurePosixPath(member.name)
                if (not member.name or "\\" in member.name or "\0" in member.name
                        or pure.is_absolute() or ".." in pure.parts
                        or str(pure) != member.name.rstrip("/") or pure in names
                        or not (member.isdir() or member.isfile())):
                    raise SourceRevisionError("checkout")
                names.add(pure)
                if len(names) > 5000 or member.size > 8 * 1024 * 1024:
                    raise SourceRevisionError("checkout")
                if member.isfile():
                    data = source.extractfile(member).read(member.size + 1)
                    expanded += len(data)
                    if len(data) != member.size or expanded > MAX_EXPORT_BYTES:
                        raise SourceRevisionError("checkout")
                    expected[str(pure)] = (versioned_source_bytes(str(pure), data, version), bool(member.mode & 0o111))
        if not UPDATE_VERSION_PATHS <= expected.keys():
            raise SourceRevisionError("checkout")
        actual = set()
        records = []
        for index, path in enumerate(root.rglob("*"), 1):
            if index > 5000:
                raise SourceRevisionError("checkout")
            info = path.lstat()
            if stat.S_ISDIR(info.st_mode):
                continue
            name = path.relative_to(root).as_posix()
            if not stat.S_ISREG(info.st_mode) or name not in expected:
                raise SourceRevisionError("checkout")
            data, executable = expected[name]
            if info.st_size != len(data) or bool(info.st_mode & 0o111) != executable or path.read_bytes() != data:
                raise SourceRevisionError("checkout")
            actual.add(name)
            records.append((name, "100755" if executable else "100644", hashlib.sha256(data).hexdigest()))
        if actual != expected.keys():
            raise SourceRevisionError("checkout")
    except (OSError, ValueError, tarfile.TarError):
        raise SourceRevisionError("checkout") from None
    return {"schema": 1, "files": len(actual), "version": version,
            "variant": "updateqa-version-only", "allowed_paths": sorted(UPDATE_VERSION_PATHS),
            "inventory_sha256": hashlib.sha256(json.dumps(sorted(records), separators=(",", ":")).encode()).hexdigest()}


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
