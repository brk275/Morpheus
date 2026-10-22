"""Explicit fresh-host acquisition/finalization for private source evidence.

Acquisition owns one PID namespace and finishes it before the workflow invokes
the genuine outer package.py command. Only the finalizer emits a closed result,
after checking the private bundle and removing every acquired/output file.
"""
from __future__ import annotations

import argparse
import contextlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import corresponding_source as S
import package_appimage as P
from appimage_process import OwnedRunner

ROOT = HERE.parents[1]
PINS = frozenset({
    "git+https://github.com/element-hq/matrix-rust-rtc?tag=v0.5.0-rc.2#53880ef1ef66b62df555ce71ccd1d6c85b813596",
    "git+https://github.com/jplatte/const_panic?rev=e0b317a9a7bde2d48a7d15b6a60d70e4a41d3b5f#e0b317a9a7bde2d48a7d15b6a60d70e4a41d3b5f",
    "git+https://github.com/matrix-org/matrix-rust-sdk?rev=c6ad52ecc09eaa36601f55effb0a48a533eb87dd#c6ad52ecc09eaa36601f55effb0a48a533eb87dd",
})


def guard(environment, *, checkout=True):
    P.public_hosted_environment(environment)
    S.require(environment.get("GITHUB_ACTOR") == "brk275"
              and environment.get("GITHUB_TRIGGERING_ACTOR") == "brk275")
    event = S.unique_json(P.client.read_regular(Path(environment["GITHUB_EVENT_PATH"]), 1024 * 1024))
    owner = event.get("repository", {}).get("owner", {}).get("id")
    S.require(type(owner) is int and owner > 0 and event.get("sender", {}).get("id") == owner)
    selected = event.get("inputs", {})
    def boolean(value, expected):
        return value is expected or type(value) is str and value == str(expected).lower()
    S.require(boolean(selected.get("source_bundle"), True)
              and selected.get("platform") == "linux" and selected.get("linux_formats") == "appimage"
              and boolean(selected.get("appimage_updates"), False)
              and boolean(selected.get("appimage_capture"), False))
    if checkout:
        P.verify_checkout(ROOT, P.expected_revision(environment))


def environment_for_acquisition(environment, root, cargo, rustc):
    result = S.clean_git_environment(P.controlled_environment(environment))
    # No account, credential helper, ambient Cargo configuration, shared cache,
    # compiler wrapper, signing material or inherited network proxy is supplied.
    result.update(HOME=str(root / "home"), CARGO_HOME=str(root / "cargo"),
                  CARGO_TARGET_DIR=str(root / "target"), RUSTC=rustc,
                  PATH=str(Path(cargo).parent) + ":/usr/bin:/bin",
                  CARGO_NET_GIT_FETCH_WITH_CLI="false", CARGO_NET_RETRY="0",
                  CARGO_HTTP_TIMEOUT="60", CARGO_REGISTRIES_CRATES_IO_PROTOCOL="sparse",
                  CARGO_REGISTRIES_CRATES_IO_INDEX="sparse+https://index.crates.io/",
                  GIT_ALLOW_PROTOCOL="https", GITHUB_ACTOR=environment["GITHUB_ACTOR"],
                  GITHUB_TRIGGERING_ACTOR=environment["GITHUB_TRIGGERING_ACTOR"])
    return result


def source_inputs(files):
    packages = S.lock_packages(files)
    S.require({p["source"] for p in packages if p.get("source", "").startswith("git+")} == PINS, "lock")
    catalog = S.package_catalog(files)
    documents = {"Cargo.toml"} | {record["manifest"] for record in catalog.values()}
    expected = set()
    for source in PINS:
        url, query = source[4:].split("?", 1)
        kind, value = query.split("#", 1)[0].split("=", 1)
        expected.add((url, kind, value))
    def inspect(value):
        if type(value) is dict:
            if "git" in value:
                kinds = [key for key in ("rev", "tag", "branch") if key in value]
                S.require(len(kinds) == 1 and (value["git"], kinds[0], value[kinds[0]]) in expected, "lock")
            S.require("registry" not in value or value["registry"] == "crates-io", "lock")
            S.require("registry-index" not in value, "lock")
            for child in value.values():
                inspect(child)
        elif type(value) is list:
            for child in value:
                inspect(child)
    for name in documents:
        document = S.toml_file(files, name)
        S.require(not document.get("replace"), "lock")
        S.require(set(document.get("patch", {})) <= {"crates-io", *(item[0] for item in expected)}, "lock")
        inspect(document)
    return packages


def check_cache_bounds(home):
    count = total = 0
    for directory, directories, files in os.walk(home, followlinks=False):
        for name in directories + files:
            info = (Path(directory) / name).lstat()
            count += 1
            total += info.st_size if not stat.S_ISDIR(info.st_mode) else 0
            S.require(info.st_uid == os.getuid() and count <= 300000 and total <= 4 * 1024**3
                      and info.st_size <= S.MAX_BUNDLE, "bounds")
            S.require(stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode), "path")


def input_plan(home, packages, runner, environment):
    cache_root, git_root = home / "registry/cache", home / "git/db"
    caches = list(cache_root.iterdir())
    S.require(len(caches) == 1 and re.fullmatch(r"index\.crates\.io-[0-9a-f]{16}", caches[0].name), "input")
    cache = caches[0]
    expected = {p["name"] + "-" + p["version"] + ".crate" for p in packages if p.get("source") == S.REGISTRY}
    S.require({p.name for p in cache.iterdir()} == expected, "missing")
    compressed = 0
    for package in packages:
        if package.get("source") != S.REGISTRY:
            continue
        data = S.read_owned(cache / (package["name"] + "-" + package["version"] + ".crate"), S.MAX_ARCHIVE)
        compressed += len(data)
        S.require(compressed <= S.MAX_BUNDLE and S.digest(data) == package["checksum"], "identity")
    mirrors = []
    for path in git_root.iterdir():
        info = path.lstat()
        if path.name == ".cargo-lock" and stat.S_ISREG(info.st_mode):
            continue
        S.require(stat.S_ISDIR(info.st_mode) and re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", path.name), "path")
        mirrors.append(path)
    S.require(0 < len(mirrors) <= 16, "bounds")
    git = P.owned_tool("/usr/bin/git", root_owned=True)
    plan = {}
    for source in sorted(PINS):
        revision = source.rsplit("#", 1)[1]
        matches = []
        for mirror in mirrors:
            data = runner.run("cache-git-commit", [*S.safe_git(git, mirror), "cat-file", "commit", revision],
                              cwd=mirror, env=S.clean_git_environment(environment), timeout=30,
                              output_limit=1024 * 1024, stderr_limit=65536,
                              expected_status=(0, 1, 128), binary_output=True)
            if S.object_id("commit", data) == revision:
                matches.append(str(mirror))
        S.require(len(matches) == 1, "missing")
        plan[source] = matches[0]
    return {"schema": 1, "registry_cache": str(cache), "git_mirrors": plan}


def private_root(value, environment):
    root = P.client.private_directory(value)
    parent = Path(environment["RUNNER_TEMP"]).resolve(strict=True)
    S.require(root.parent == parent and re.fullmatch(r"morpheus-source-qa-[a-z0-9_]{8}", root.name), "path")
    marker = S.unique_json(S.read_owned(root / "owner.json", 4096, private=True))
    info = root.lstat()
    S.require(type(marker) is dict and set(marker) == {"schema", "device", "inode", "source_revision"}
              and marker["schema"] == 1 and marker["device"] == info.st_dev and marker["inode"] == info.st_ino
              and marker["source_revision"] == P.expected_revision(environment), "identity")
    return root


def acquire(root, cargo, rustc, environment):
    P.containment.assert_contained()
    root = private_root(root, environment)
    cargo, rustc = P.owned_tool(cargo), P.owned_tool(rustc)
    S.require(Path(cargo).parent == Path(rustc).parent and Path(cargo).name == "cargo"
              and Path(rustc).name == "rustc", "input")
    runner = OwnedRunner(root / "logs/commands")
    for label, tool, prefix in (("source-cargo-version", cargo, b"cargo 1.99.0 "),
                                ("source-rustc-version", rustc, b"rustc 1.99.0 ")):
        version = runner.run(label, [tool, "--version"], cwd=root / "fetch", env=environment,
                             timeout=30, output_limit=256, stderr_limit=65536)
        S.require(version.startswith(prefix) and version.endswith(b"\n") and version.count(b"\n") == 1, "identity")
    _, source, files = S.git_export(runner, ROOT, P.expected_revision(environment),
                                    P.owned_tool("/usr/bin/git", root_owned=True), environment, S.Budget())
    packages = source_inputs(files)
    # Cargo discovers configuration from cwd's ancestors and the new Cargo
    # home, not the separate --manifest-path. Reject ambient ancestor config.
    for directory in (root / "fetch", root, *root.parents):
        S.require(not any((directory / ".cargo" / name).exists() for name in ("config", "config.toml")), "input")
    runner.run("source-cargo-fetch", [cargo, "--config", "net.git-fetch-with-cli=false",
               "--config", "net.retry=0", "--config", "registry.global-credential-providers=[]",
               "fetch", "--locked", "--manifest-path", str(ROOT / "Cargo.toml")],
               cwd=root / "fetch", env=environment, timeout=1200,
               output_limit=8 * 1024 * 1024, stderr_limit=8 * 1024 * 1024)
    P.verify_checkout(ROOT, P.expected_revision(environment))
    check_cache_bounds(root / "cargo")
    plan = input_plan(root / "cargo", packages, runner, environment)
    P.client.write_new(root / "plan.json", json.dumps(plan, sort_keys=True).encode() + b"\n")
    P.client.write_new(root / "prepared.json", json.dumps({
        "schema": 1, "source_revision": source["commit"], "source_tree": source["tree"],
        "registry_packages": sum(p.get("source") == S.REGISTRY for p in packages),
        "git_sources": len(plan["git_mirrors"]), "locked_packages": len(packages),
    }, sort_keys=True).encode() + b"\n")


def github_root(root, environment):
    descriptor = os.open(environment["GITHUB_ENV"], os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "ab") as stream:
        info = os.fstat(stream.fileno())
        S.require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid() and info.st_nlink == 1
                  and not info.st_mode & 0o022 and info.st_size < 1024 * 1024, "output")
        S.require(not any(c in str(root) for c in "\r\n\0"), "path")
        stream.write(("MORPHEUS_SOURCE_QA_ROOT=" + str(root) + "\n").encode())


def prepare(environment):
    toolchain = Path(environment.get("RUSTUP_HOME", str(Path(environment["HOME"]) / ".rustup"))) / "toolchains/1.99.0-x86_64-unknown-linux-gnu/bin"
    cargo, rustc = P.owned_tool(toolchain / "cargo"), P.owned_tool(toolchain / "rustc")
    root = Path(tempfile.mkdtemp(prefix="morpheus-source-qa-", dir=environment["RUNNER_TEMP"])).resolve()
    try:
        root.chmod(0o700)
        info = root.lstat()
        P.client.write_new(root / "owner.json", json.dumps({"schema": 1, "device": info.st_dev,
            "inode": info.st_ino, "source_revision": P.expected_revision(environment)}).encode() + b"\n")
        github_root(root, environment)
    except BaseException:
        shutil.rmtree(root)
        raise
    for name in ("home", "cargo", "fetch", "logs", "output", "canonical-temp"):
        (root / name).mkdir(mode=0o700)
    controlled = environment_for_acquisition(environment, root, cargo, rustc)
    process = None
    with (root / "logs/containment.log").open("x") as log, contextlib.redirect_stdout(log):
        (root / "logs/containment.log").chmod(0o600)
        P.containment.containment_check(root / "logs/probe")
    command = P.containment.namespace_command([
        str(Path(sys.executable).resolve()), "-I", "-B", str(Path(__file__).resolve()),
        "--inside", str(root), "--cargo", cargo, "--rustc", rustc,
    ], root / "acquisition-namespace", controlled)
    descriptor = os.open(root / "logs/acquisition.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as log:
            process = subprocess.Popen(command, cwd=ROOT, env=controlled, stdin=subprocess.DEVNULL,
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        S.require(P.containment.wait_owned(process, 1500) == 0, "input")
    finally:
        if process is not None:
            P.containment.stop_owned(process)


def package_receipt(data):
    # The real outer controller first emits its fixed containment proof. Keep
    # it private, then parse exactly one closed JSON result; no arbitrary tail.
    lines = data.splitlines()
    S.require(len(lines) == 2 and lines[0] == b"MORPHEUS_ELEMENT_CONTAINMENT passed", "input")
    return S.unique_json(lines[1])


def finish(root, scanner, environment):
    root = private_root(root, environment)
    info = root.lstat()
    result = None
    failed = None
    try:
        prepared = S.unique_json(S.read_owned(root / "prepared.json", 32768, private=True))
        result = package_receipt(S.read_owned(root / "logs/package.log", 32768, private=True))
        fields = {"schema", "source_revision", "source_tree", "registry_packages", "git_sources", "locked_packages"}
        S.require(type(prepared) is dict and set(prepared) == fields and prepared["schema"] == 1, "identity")
        S.require(type(result) is dict and set(result) == fields | {"check", "bundle_sha256", "manifest_sha256",
                  "distribution_ready", "unresolved_release_requirements"}, "identity")
        S.require(result["check"] == "corresponding_source_bundle" and result["distribution_ready"] is False
                  and result["unresolved_release_requirements"] == list(S.UNRESOLVED), "identity")
        S.require(all(result[key] == value for key, value in prepared.items())
                  and result["source_revision"] == P.expected_revision(environment)
                  and type(result["source_tree"]) is str and S.OID.fullmatch(result["source_tree"]), "identity")
        for key in ("bundle_sha256", "manifest_sha256"):
            S.require(type(result[key]) is str and S.HASH.fullmatch(result[key]), "identity")
        for key in ("registry_packages", "git_sources", "locked_packages"):
            S.require(type(result[key]) is int and 0 < result[key] <= 4096, "identity")
        S.require({p.name for p in (root / "output").iterdir()} == {S.BUNDLE_NAME}, "output")
        data = S.read_owned(root / "output" / S.BUNDLE_NAME, S.MAX_BUNDLE, private=True)
        S.require(S.digest(data) == result["bundle_sha256"], "identity")
        del data
    except BaseException as error:
        failed = error
    try:
        # The genuine outer package controller publishes its result only after
        # its namespace is stopped and its complete private temp tree deleted.
        S.require(not any((root / "canonical-temp").iterdir()), "output")
        P.scan_private_logs(root / "logs", P.owned_tool(scanner))
    except BaseException as error:
        failed = failed or error
    finally:
        current = root.lstat()
        S.require((current.st_dev, current.st_ino) == (info.st_dev, info.st_ino)
                  and shutil.rmtree.avoids_symlink_attacks, "identity")
        shutil.rmtree(root)
        S.require(not root.exists(), "output")
    if failed is not None:
        raise S.SourceFailure("input") from None
    print(json.dumps({"check": "corresponding_source_qa", "passed": True, "cleanup_passed": True,
          "distribution_ready": False, **{key: result[key] for key in
          ("source_revision", "source_tree", "bundle_sha256", "manifest_sha256", "registry_packages", "git_sources", "locked_packages")}}, sort_keys=True), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--prepare", action="store_true")
    mode.add_argument("--finish", type=Path)
    mode.add_argument("--inside", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--gitleaks")
    parser.add_argument("--cargo", help=argparse.SUPPRESS)
    parser.add_argument("--rustc", help=argparse.SUPPRESS)
    args = parser.parse_args()
    # Cleanup retains the owner/event/root identity guard; a dirty checkout or
    # failed acquisition must not prevent removing this job's private files.
    guard(os.environ, checkout=args.finish is None)
    if args.inside:
        S.require(args.cargo and args.rustc and not args.gitleaks)
        return acquire(args.inside, args.cargo, args.rustc, dict(os.environ))
    S.require(not args.cargo and not args.rustc)
    if args.prepare:
        S.require(not args.gitleaks)
        return prepare(dict(os.environ))
    S.require(args.gitleaks)
    return finish(args.finish, args.gitleaks, dict(os.environ))


if __name__ == "__main__":
    def interrupted(_signal, _frame):
        raise S.SourceFailure("input")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except BaseException:
        print('MORPHEUS_SOURCE_QA {"passed":false,"diagnostics":"withheld"}', file=sys.stderr)
        raise SystemExit(1)
