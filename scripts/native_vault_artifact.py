#!/usr/bin/env python3
"""Select one exact core-vault test from successful Cargo JSON; never execute it."""
import hashlib
import json
import os
from pathlib import Path
import stat
import sys


class ArtifactError(RuntimeError):
    pass


def require(value):
    if not value:
        raise ArtifactError("Invalid native vault build evidence")


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        require(key not in value)
        value[key] = item
    return value


def checked_file(raw):
    require(type(raw) is str and not any(ord(c) < 32 for c in raw))
    path = Path(raw)
    require(path.is_absolute())
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and not stat.S_ISLNK(info.st_mode)
            and not getattr(info, "st_file_attributes", 0) & 0x400)
    return path.resolve(strict=True)


def bounded_read(path, limit):
    require(path.stat().st_size <= limit)
    with path.open("rb") as stream:
        raw = stream.read(limit + 1)
    require(len(raw) <= limit)
    return raw


def select_artifact(repo, metadata_raw, messages_raw):
    repo = Path(repo).resolve(strict=True)
    target = repo / "target"
    info = target.lstat()
    require(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode)
            and not getattr(info, "st_file_attributes", 0) & 0x400
            and target.resolve(strict=True) == target)
    require(len(metadata_raw) <= 4 * 1024 * 1024 and len(messages_raw) <= 16 * 1024 * 1024)
    metadata = json.loads(metadata_raw, object_pairs_hook=unique_object)
    require(type(metadata) is dict and type(metadata.get("version")) is int and metadata["version"] == 1
            and type(metadata.get("packages")) is list
            and type(metadata.get("workspace_members")) is list)
    require(Path(metadata["target_directory"]).resolve(strict=True) == target)
    manifest = repo / "crates/morpheus-matrix/Cargo.toml"
    source = repo / "crates/morpheus-matrix/tests/native_vault.rs"
    candidates = [p for p in metadata["packages"]
                  if type(p) is dict and p.get("name") == "morpheus-matrix"]
    require(len(candidates) == 1)
    package = candidates[0]
    require(package.get("source") is None and "source" in package
            and checked_file(package.get("manifest_path")) == manifest
            and type(package.get("id")) is str and 0 < len(package["id"]) <= 4096
            and package["id"] in metadata["workspace_members"])
    records = [json.loads(line, object_pairs_hook=unique_object)
               for line in messages_raw.splitlines() if line.strip()]
    require(records and all(type(item) is dict for item in records))
    finished = [i for i, item in enumerate(records) if item.get("reason") == "build-finished"]
    require(finished == [len(records) - 1] and records[-1].get("success") is True)
    artifacts = []
    for item in records:
        if item.get("reason") != "compiler-artifact" or item.get("package_id") != package["id"]:
            continue
        cargo_target = item.get("target")
        require(type(cargo_target) is dict)
        if cargo_target.get("name") != "native_vault":
            continue
        require(cargo_target.get("kind") == ["test"]
                and checked_file(cargo_target.get("src_path")) == source
                and checked_file(item.get("manifest_path")) == manifest
                and type(item.get("profile")) is dict and item["profile"].get("test") is True
                and type(item.get("filenames")) is list
                and item.get("executable") in item["filenames"])
        executable = checked_file(item.get("executable"))
        require(executable.is_relative_to(target) and executable.stat().st_size <= 512 * 1024 * 1024)
        artifacts.append(executable)
    require(len(artifacts) == 1)
    digest = hashlib.sha256()
    with artifacts[0].open("rb") as stream:
        before = os.fstat(stream.fileno())
        remaining = 512 * 1024 * 1024
        while block := stream.read(min(1024 * 1024, remaining + 1)):
            require(len(block) <= remaining)
            digest.update(block)
            remaining -= len(block)
        after = os.fstat(stream.fileno())
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns)
                == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns))
    return artifacts[0], digest.hexdigest()


def main():
    try:
        require(os.environ.get("GITHUB_ACTIONS") == "true"
                and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
                and os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus"
                and os.environ.get("GITHUB_REF") == "refs/heads/main")
        repo = Path(__file__).resolve().parents[1]
        require(Path(os.environ["GITHUB_WORKSPACE"]).resolve(strict=True) == repo)
        metadata = checked_file(str(repo / "target/native-vault-metadata.json"))
        messages = checked_file(str(repo / "target/native-vault-build.json"))
        binary, digest = select_artifact(repo, bounded_read(metadata, 4 * 1024 * 1024),
                                         bounded_read(messages, 16 * 1024 * 1024))
        require(not any(ord(c) < 32 for c in str(binary)))
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(f"binary={binary}\nbinary_sha256={digest}\n")
        print(json.dumps({"check": "native_vault_artifact", "status": "passed", "binary_sha256": digest}))
        return 0
    except Exception:
        print("Native vault artifact selection failed; no executable was authorized.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
