#!/usr/bin/env python3
"""Optional owner-recipient encryption after ordinary hosted ARM package acceptance."""

import argparse
import hashlib
import io
import json
import os
import platform
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import time
import urllib.parse
import urllib.request
from pathlib import Path


ARCHIVE_URL = "https://github.com/FiloSottile/age/releases/download/v1.3.2/age-v1.3.2-darwin-arm64.tar.gz"
ARCHIVE_BYTES = 18106798
ARCHIVE_SHA256 = "e2020b073c44f692685a24d6abc378817eb81ffaaf49fd0531ef8565f767f2f5"
EXECUTABLE_BYTES = 6502258
EXECUTABLE_SHA256 = "4012dfc2725883beafb710894af4f599b7a94f8c8e0f51f02cc96ab8df33915e"
MAX_DMG = 512 * 1024 * 1024
ALPHABET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"


class Failure(Exception):
    pass


def require(value):
    if not value:
        raise Failure("Owner package encryption requirements were not met")


def recipient(value):
    require(type(value) is str and re.fullmatch(r"age1[qpzry9x8gf2tvdw0s3jn54khce6mua7l]{58}", value))
    # Bech32 checksum for a native age X25519 recipient; SSH/plugins are excluded.
    values = [ord(c) >> 5 for c in "age"] + [0] + [ord(c) & 31 for c in "age"]
    values += [ALPHABET.index(c) for c in value[4:]]
    check = 1
    for item in values:
        top = check >> 25
        check = ((check & 0x1ffffff) << 5) ^ item
        for index, generator in enumerate((0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3)):
            if (top >> index) & 1:
                check ^= generator
    require(check == 1 and (ALPHABET.index(value[-7]) & 15) == 0)
    return value


def selected_revision(requested, workflow_revision):
    def valid(value):
        return (type(value) is str and re.fullmatch(r"[0-9a-f]{40}", value) is not None
                and value != "0" * 40)
    require(valid(workflow_revision))
    selected = workflow_revision if requested == "" else requested
    require(valid(selected))
    return selected


def selection(inputs, workflow_revision):
    require(type(inputs) is dict and inputs.get("platform") == "macos-arm64")
    revision = selected_revision(inputs.get("source_revision", ""), workflow_revision)
    for name in ("native_updates", "appimage_updates", "appimage_capture", "source_bundle"):
        value = inputs.get(name)
        require((type(value) is bool and value is False) or (type(value) is str and value == "false"))
    return recipient(inputs.get("owner_age_recipient")), revision


def context(*, worker):
    env = os.environ
    require(env.get("GITHUB_ACTIONS") == "true" and env.get("RUNNER_ENVIRONMENT") == "github-hosted")
    require(env.get("GITHUB_REPOSITORY") == "brk275/Morpheus"
            and env.get("GITHUB_REF") == "refs/heads/main"
            and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
            and env.get("GITHUB_ACTOR") == env.get("GITHUB_TRIGGERING_ACTOR") == "brk275")
    event = json.loads(Path(env["GITHUB_EVENT_PATH"]).read_bytes())
    require(event["repository"]["private"] is False
            and event["sender"]["id"] == event["repository"]["owner"]["id"])
    key, revision = selection(event["inputs"], env.get("GITHUB_SHA", ""))
    if worker:
        require(platform.system() == "Darwin" and platform.machine() == "arm64"
                and env.get("RUNNER_OS") == "macOS" and env.get("ImageOS") == "macos15")
        require(os.getuid() == os.geteuid() != 0)
    return key, revision


def identity(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_nlink,
            info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def regular(path, limit):
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_nlink == 1
            and before.st_uid == os.getuid() and 0 < before.st_size <= limit)
    return before


def copy_dmg(dist, destination):
    require(dist.is_absolute() and dist.resolve(strict=True) == dist)
    candidates = list(dist.glob("*.dmg"))
    require(len(candidates) == 1)
    source = candidates[0]
    before = regular(source, MAX_DMG)
    checksum = hashlib.sha256()
    with os.fdopen(os.open(source, os.O_RDONLY | os.O_NOFOLLOW), "rb") as stream:
        require(identity(os.fstat(stream.fileno())) == identity(before))
        with destination.open("xb") as output:
            os.fchmod(output.fileno(), 0o600)
            copied = 0
            while chunk := stream.read(65536):
                copied += len(chunk)
                require(copied <= before.st_size)
                checksum.update(chunk)
                output.write(chunk)
            require(copied == before.st_size)
            output.flush()
            os.fsync(output.fileno())
        require(identity(os.fstat(stream.fileno())) == identity(before))
    require(identity(source.lstat()) == identity(before))
    return before.st_size, checksum.hexdigest()


class Redirect(urllib.request.HTTPRedirectHandler):
    def __init__(self):
        super().__init__()
        self.remaining = 3

    def redirect_request(self, request, fp, code, message, headers, newurl):
        self.remaining -= 1
        parsed = urllib.parse.urlsplit(newurl)
        require(self.remaining >= 0 and parsed.scheme == "https"
                and parsed.hostname in {"github.com", "release-assets.githubusercontent.com"}
                and parsed.port in (None, 443) and not parsed.username and not parsed.password)
        return super().redirect_request(request, fp, code, message, headers, newurl)


def tool(directory):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), Redirect())
    request = urllib.request.Request(ARCHIVE_URL, headers={"User-Agent": "Morpheus-owner-package"})
    deadline = time.monotonic() + 90
    chunks = []
    size = 0
    with opener.open(request, timeout=20) as response:
        require(response.status == 200)
        while chunk := response.read(65536):
            size += len(chunk)
            require(size <= ARCHIVE_BYTES and time.monotonic() < deadline)
            chunks.append(chunk)
    archive = b"".join(chunks)
    require(len(archive) == ARCHIVE_BYTES and hashlib.sha256(archive).hexdigest() == ARCHIVE_SHA256)
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as package:
        members = package.getmembers()
        require(len(members) <= 16 and len({m.name for m in members}) == len(members))
        require(all((m.isdir() and m.name == "age") or
                    (m.isfile() and m.name.startswith("age/") and ".." not in m.name.split("/")
                     and 0 < m.size <= 8 * 1024 * 1024) for m in members))
        require(sum(m.size for m in members) <= 40 * 1024 * 1024)
        selected = package.getmember("age/age")
        binary = package.extractfile(selected).read(EXECUTABLE_BYTES + 1)
    require(len(binary) == EXECUTABLE_BYTES and hashlib.sha256(binary).hexdigest() == EXECUTABLE_SHA256)
    # Mach-O 64-bit little-endian ARM64 header; the binary itself is separately hash pinned.
    require(binary[:8] == b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01")
    executable = directory / "age"
    with executable.open("xb") as output:
        output.write(binary)
        os.fchmod(output.fileno(), 0o500)
    return executable


def cleanup(directory):
    if directory is None:
        return True
    try:
        shutil.rmtree(directory)
        return True
    except OSError:
        try:
            print('{"check":"owner_encryption_cleanup","passed":false}', flush=True)
        except OSError:
            pass
        return False


def encrypt(dist, root, key):
    require(root.is_absolute() and root.resolve(strict=True) == root)
    require(not any(c in str(root) for c in "\r\n\x00"))
    info = root.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid() and not info.st_mode & 0o022)
    output = Path(tempfile.mkdtemp(prefix="owner-ciphertext-", dir=root))
    work = None
    try:
        os.chmod(output, 0o700)
        work = Path(tempfile.mkdtemp(prefix="owner-encryption-", dir=root))
        source = work / "Morpheus.dmg"
        size, checksum = copy_dmg(dist, source)
        executable = tool(work)
        ciphertext = output / "Morpheus.dmg.age"
        # No age identity, configuration, plugins or inherited agent variables.
        env = {"PATH": "/usr/bin:/bin", "HOME": str(work), "TMPDIR": str(work), "LANG": "C"}
        subprocess.run([str(executable), "--encrypt", "--recipient", key, "--output", str(ciphertext), str(source)],
                       env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, check=True, timeout=90)
        cipher_info = regular(ciphertext, MAX_DMG + 1024 * 1024)
        cipher_hash = hashlib.sha256()
        with os.fdopen(os.open(ciphertext, os.O_RDONLY | os.O_NOFOLLOW), "rb") as stream:
            require(identity(os.fstat(stream.fileno())) == identity(cipher_info))
            while chunk := stream.read(65536):
                cipher_hash.update(chunk)
            require(identity(os.fstat(stream.fileno())) == identity(cipher_info))
        require(identity(ciphertext.lstat()) == identity(cipher_info))
        require(list(output.iterdir()) == [ciphertext])
        receipt = {"plaintext_bytes": size, "plaintext_sha256": checksum,
                   "ciphertext_bytes": cipher_info.st_size, "ciphertext_sha256": cipher_hash.hexdigest(),
                   "recipient_sha256": hashlib.sha256(key.encode("ascii")).hexdigest()}
        require(cleanup(work))
        return output, receipt
    except BaseException:
        cleanup(work)
        cleanup(output)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--dist", type=Path)
    args = parser.parse_args()
    key, revision = context(worker=not args.preflight)
    if args.preflight:
        require(args.dist is None)
        print('{"check":"owner_package_recipient","passed":true}')
        return
    require(args.dist is not None and os.environ["MORPHEUS_PACKAGE_SOURCE_REVISION"] == revision)
    root = Path(os.environ["RUNNER_TEMP"])
    output, receipt = encrypt(args.dist, root, key)
    receipt.update(check="encrypted_owner_package", source=revision, controller=os.environ["GITHUB_SHA"],
                   run=os.environ["GITHUB_RUN_ID"], attempt=os.environ["GITHUB_RUN_ATTEMPT"])
    try:
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            stream.write("ciphertext=" + str(output / "Morpheus.dmg.age") + "\n")
        print(json.dumps(receipt, sort_keys=True), flush=True)
    except BaseException:
        cleanup(output)
        raise


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print('{"check":"encrypted_owner_package","passed":false}')
        raise SystemExit(1)
