#!/usr/bin/env python3
"""Temporary owner-admitted upstream source check. Never a stock/beta gate."""
from pathlib import Path, PurePosixPath
import argparse
import hashlib
import json
import os
import re
import select
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

MAX_DOWNLOAD = 128 * 1024 * 1024
MAX_LOG = 8 * 1024 * 1024
SOURCE_PREFIX = "matrix-js-sdk-fa4772a4ddd335a5a8bfbf8bb21df49b70f7a999"
PROPOSAL = "cc8e316c77ebc086f3f400cca15a02392cebbe0014411ac086f9a3fada858a98"


def need(value):
    if not value:
        raise RuntimeError("refused")


def bootstrap_phase(name):
    need(name in ("guards", "inputs", "scanner", "download_source", "download_node", "download_pnpm",
                  "source_tree", "apply_patch", "node_binary", "pnpm_binary"))
    print(json.dumps({"sender_bootstrap": name}), flush=True)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def regular(path, limit):
    before = path.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid()
         and before.st_nlink == 1 and before.st_size <= limit)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        with os.fdopen(fd, "rb", closefd=False) as source:
            data = source.read(limit + 1)
        need(len(data) == before.st_size and len(data) <= limit
             and os.fstat(fd).st_ino == before.st_ino)
        return data
    finally:
        os.close(fd)


def write(path, data, executable=False):
    fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb", closefd=False) as out:
            need(out.write(data) == len(data))
        if executable:
            os.fchmod(fd, 0o700)
    finally:
        os.close(fd)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RuntimeError("redirect")


def download(spec, destination):
    need(spec["url"].startswith(("https://nodejs.org/download/release/",
                               "https://registry.npmjs.org/", "https://codeload.github.com/matrix-org/matrix-js-sdk/")))
    need(0 < spec["bytes"] <= MAX_DOWNLOAD)
    opener = urllib.request.build_opener(NoRedirect())
    with opener.open(urllib.request.Request(spec["url"], headers={"User-Agent": "Morpheus-source-check/1.0"}), timeout=60) as response:
        need(response.status == 200)
        body = response.read(spec["bytes"] + 1)
    need(len(body) == spec["bytes"] and digest(body) == spec["sha256"])
    write(destination, body)


def selected_binary(archive, spec, destination):
    count = 0
    with tarfile.open(archive, "r:*") as source:
        for index, member in enumerate(source):
            need(index < 20000)
            if member.name != spec["path"]:
                continue
            need(count == 0 and member.isfile() and not member.issparse() and member.size == spec["bytes"])
            stream = source.extractfile(member)
            need(stream is not None)
            data = stream.read(spec["bytes"] + 1)
            need(len(data) == spec["bytes"] and digest(data) == spec["sha256"])
            write(destination, data, True)
            count += 1
    need(count == 1)


def source_tree(archive, expected, destination):
    seen = set()
    total = 0
    with tarfile.open(archive, "r:gz") as source:
        for index, member in enumerate(source):
            need(index < 2000 and not member.issparse())
            if member.name == SOURCE_PREFIX:
                need(member.isdir())
                continue
            need(member.name.startswith(SOURCE_PREFIX + "/"))
            relative = member.name[len(SOURCE_PREFIX) + 1:]
            parts = PurePosixPath(relative)
            need(not parts.is_absolute() and all(p not in ("", ".", "..") for p in relative.split("/")))
            path = destination.joinpath(*parts.parts)
            if member.isdir():
                path.mkdir(mode=0o700, parents=True, exist_ok=True)
                continue
            need(relative in expected and relative not in seen)
            fact = expected[relative]
            if fact["mode"] == "120000":
                need(relative == "examples/voip/lib/matrix.js" and member.issym())
                data = member.linkname.encode()
                need(len(data) == fact["bytes"] and digest(data) == fact["sha256"])
                # This declared, irrelevant example link remains unmaterialized.
            else:
                need(member.isfile() and member.size == fact["bytes"]
                     and (member.size <= 4 * 1024 * 1024
                          or (relative == "spec/test-utils/test_indexeddb_cryptostore_dump/full_account/dump.json"
                              and member.size == 4_920_911
                              and fact["sha256"] == "e5b48b253421ac2f34e8f0c47c54dead056798f8537f7d7256017b4a57b9adc8")))
                need(fact["mode"] in ("100644", "100755"))
                data = source.extractfile(member).read(fact["bytes"] + 1)
                need(len(data) == fact["bytes"] and digest(data) == fact["sha256"])
                total += len(data)
                need(total <= 32 * 1024 * 1024)
                path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                write(path, data, fact["mode"] == "100755")
            seen.add(relative)
    need(seen == set(expected))


def apply_patch(tree, patch, files):
    lines = patch.decode().splitlines(keepends=True)
    cursor = 0
    seen = set()
    while cursor < len(lines):
        need(lines[cursor].startswith("--- a/"))
        name = lines[cursor][6:].rstrip("\n")
        cursor += 1
        need(lines[cursor] == "+++ b/" + name + "\n" and name in files and name not in seen)
        cursor += 1
        fact = files[name]
        path = tree / name
        before = regular(path, 4 * 1024 * 1024)
        need(digest(before) == fact["before_sha256"])
        original, output, position = before.decode().splitlines(keepends=True), [], 0
        while cursor < len(lines) and not lines[cursor].startswith("--- a/"):
            match = re.fullmatch(r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@[^\n]*\n", lines[cursor])
            need(match is not None)
            start, old_count, new_count = int(match[1]) - 1, int(match[2] or "1"), int(match[4] or "1")
            need(position <= start <= len(original))
            output.extend(original[position:start])
            position, consumed, produced = start, 0, 0
            cursor += 1
            while cursor < len(lines) and not lines[cursor].startswith(("@@ ", "--- a/")):
                line = lines[cursor]
                need(line[:1] in (" ", "+", "-"))
                if line[0] in " -":
                    need(position < len(original) and original[position] == line[1:])
                    position += 1
                    consumed += 1
                if line[0] in " +":
                    output.append(line[1:])
                    produced += 1
                cursor += 1
            need(consumed == old_count and produced == new_count)
        data = "".join(output + original[position:]).encode()
        need(digest(data) == fact["after_sha256"] and len(data) == fact["after_bytes"])
        path.write_bytes(data)
        seen.add(name)
    need(seen == set(files))


def checked_command(name, arguments, cwd, environment, root, scanner, seconds):
    print(json.dumps({"sender_stage": name, "status": "started"}), flush=True)
    logs = root / name
    logs.mkdir(mode=0o700)
    log = logs / "job.log"
    size = 0
    child = subprocess.Popen(arguments, cwd=cwd, env=environment, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, start_new_session=True)
    deadline = time.monotonic() + seconds
    try:
        with log.open("xb") as out:
            while True:
                remaining = deadline - time.monotonic()
                need(remaining > 0)
                readable, _, _ = select.select([child.stdout], [], [], remaining)
                need(readable)
                block = os.read(child.stdout.fileno(), 65536)
                if not block:
                    break
                size += len(block)
                need(size <= MAX_LOG)
                out.write(block)
        # Keep the leader unreaped until its owned group is retired, so a
        # recycled numeric PID cannot authorize cleanup of another group.
        while True:
            result = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT | os.WNOHANG)
            if result is not None:
                status = result.si_status if result.si_code == os.CLD_EXITED else -result.si_status
                break
            need(time.monotonic() < deadline)
            time.sleep(0.02)
    finally:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=5)
        child.stdout.close()
    report = root / (name + "-scan.json")
    scan = subprocess.run([str(scanner), "dir", str(logs), "--config", str(root / "scanner.toml"),
        "--gitleaks-ignore-path", str(root / "scanner.ignore"), "--ignore-gitleaks-allow", "--redact=100",
        "--max-target-megabytes", "0", "--report-format", "json", "--report-path", str(report), "--no-banner"],
        cwd=root, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=60)
    need(scan.returncode == 0 and json.loads(report.read_text()) == [])
    print(json.dumps({"sender_stage": name, "status": "success" if status == 0 else "failure",
                      "log_sha256": digest(log.read_bytes()), "log_bytes": size, "findings": 0}), flush=True)
    if status:
        # Only a bounded, independently scanned tail is exposed on failure.
        sys.stdout.write(log.read_text(errors="replace")[-65536:])
    need(status == 0)


def main():
    bootstrap_phase("guards")
    parser = argparse.ArgumentParser()
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--patch", type=Path, required=True)
    parser.add_argument("--scanner", type=Path, required=True)
    args = parser.parse_args()
    need(sys.platform == "linux" and os.environ.get("GITHUB_ACTIONS") == "true"
         and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
         and os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus"
         and os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
         and os.environ.get("GITHUB_ACTOR") == "brk275"
         and os.environ.get("GITHUB_TRIGGERING_ACTOR") == "brk275")
    bootstrap_phase("inputs")
    raw = regular(args.inputs, 512 * 1024)
    need(digest(raw) == os.environ.get("MORPHEUS_SETUP_INPUTS_SHA256"))
    inputs = json.loads(raw)
    patch = regular(args.patch, 128 * 1024)
    need(digest(patch) == PROPOSAL == inputs["proposal_sha256"])
    bootstrap_phase("scanner")
    need(digest(regular(args.scanner, 64 * 1024 * 1024)) == inputs["scanner_binary_sha256"])
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="morpheus-sender-source-", dir=runner_temp))
    os.chmod(root, 0o700)
    tree, tools, local_store = root / "source", root / "tools", root / "pnpm-store"
    for path in (tree, tools, local_store, root / "temp", root / "cache", root / "config"):
        path.mkdir(mode=0o700)
    write(root / "scanner.toml", b"[extend]\nuseDefault = true\n")
    write(root / "scanner.ignore", b"")
    write(root / "npmrc", b"registry=https://registry.npmjs.org/\nignore-scripts=true\n")
    for name in ("source", "node", "pnpm"):
        bootstrap_phase("download_" + name)
        download(inputs[name], root / (name + ".archive"))
    bootstrap_phase("source_tree")
    source_tree(root / "source.archive", inputs["source_files"], tree)
    bootstrap_phase("apply_patch")
    apply_patch(tree, patch, inputs["proposal_files"])
    bootstrap_phase("node_binary")
    selected_binary(root / "node.archive", inputs["node"]["binary"], tools / "node")
    bootstrap_phase("pnpm_binary")
    selected_binary(root / "pnpm.archive", inputs["pnpm"]["binary"], tools / "pnpm")
    environment = {"PATH": str(tools) + ":/usr/bin:/bin", "CI": "true", "TZ": "UTC", "LANG": "C.UTF-8",
        "TMPDIR": str(root / "temp"), "XDG_CACHE_HOME": str(root / "cache"), "XDG_CONFIG_HOME": str(root / "config"), "PNPM_HOME": str(tools),
        "npm_config_userconfig": str(root / "npmrc"), "npm_config_globalconfig": str(root / "npmrc"),
        "npm_config_cache": str(root / "cache"), "npm_config_registry": "https://registry.npmjs.org/",
        "npm_config_ignore_scripts": "true", "npm_config_enable_pre_post_scripts": "false",
        "npm_config_manage_package_manager_versions": "false", "npm_config_update_notifier": "false"}
    pnpm = str(tools / "pnpm")
    lock = regular(tree / "pnpm-lock.yaml", 512 * 1024)
    checked_command("install", [pnpm, "install", "--frozen-lockfile", "--ignore-scripts", "--store-dir",
        str(local_store), "--reporter", "append-only"], tree, environment, root, args.scanner, 300)
    need(regular(tree / "pnpm-lock.yaml", 512 * 1024) == lock)
    paths = list(inputs["proposal_files"])
    checked_command("format", [pnpm, "exec", "oxfmt", "--check", *paths], tree, environment, root, args.scanner, 120)
    checked_command("lint", [pnpm, "exec", "oxlint", *paths], tree, environment, root, args.scanner, 120)
    checked_command("typecheck", [pnpm, "run", "lint:types"], tree, environment, root, args.scanner, 600)
    checked_command("manager_session_tests", [pnpm, "exec", "vitest", "run", "--environment=node", "--no-api",
        "--no-ui", "--no-cache", "spec/unit/matrixrtc/MembershipManager.spec.ts", "spec/unit/matrixrtc/MatrixRTCSession.spec.ts"],
        tree, environment, root, args.scanner, 300)
    print(json.dumps({"sender_source_checks": "success", "proposal_sha256": PROPOSAL,
                      "stock_interoperability": False, "beta_ready": False}), flush=True)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        print('{"sender_source_checks":"failed","stock_interoperability":false,"beta_ready":false}', file=sys.stderr)
        raise SystemExit(1)
