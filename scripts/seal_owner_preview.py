#!/usr/bin/env python3
"""One-off owner preview delivery; produces encrypted bytes, never a release.

Run only on a fresh standard Apple Silicon runner after exact-source review.
The recipient private key remains on the owner's computer. The public recipient
certificate and this script come from the immutable delivery workflow checkout.
The application source is a separate, exact checkout. Never run this on a PR.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tarfile

SOURCE = "7ec8c2acc1e8abb1117d20f53584d58bedfd75a4"
OPENSSL = "/opt/homebrew/opt/openssl@3/bin/openssl"
MAX_FILES = 20_000
MAX_BYTES = 1024 * 1024 * 1024


def require(condition):
    if not condition:
        raise ValueError("Owner preview delivery checks failed")


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def command(arguments, log):
    # Logs remain private on the ephemeral runner, including compiler/signing
    # diagnostics. No app, account, vault or permission work is performed.
    with log.open("ab") as stream:
        result = subprocess.run(arguments, stdin=subprocess.DEVNULL, stdout=stream,
                                stderr=stream, timeout=180)
    require(result.returncode == 0)


def check_bundle(bundle):
    require(bundle.is_dir() and not bundle.is_symlink())
    root_info = bundle.lstat()
    require(root_info.st_uid == os.getuid() and not root_info.st_mode & 0o022)
    size = 0
    count = 0
    for directory, dirs, files in os.walk(bundle, followlinks=False):
        require(len(Path(directory).relative_to(bundle).parts) <= 24)
        for name in dirs + files:
            count += 1
            require(count <= MAX_FILES)
            entry = Path(directory) / name
            info = entry.lstat()
            require(info.st_uid == os.getuid())
            if stat.S_ISLNK(info.st_mode):
                # Framework aliases may be relative and entirely in-bundle.
                # No absolute, dangling, cyclic or escaping target is admitted.
                require(not Path(os.readlink(entry)).is_absolute())
                require(entry.resolve(strict=True).is_relative_to(bundle))
            elif stat.S_ISREG(info.st_mode):
                require(info.st_nlink == 1 and not info.st_mode & 0o022)
                size += info.st_size
                require(size <= MAX_BYTES)
            else:
                require(stat.S_ISDIR(info.st_mode) and not info.st_mode & 0o022)
    return count, size


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--certificate", required=True, type=Path)
    parser.add_argument("--certificate-sha256", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    require(sys.platform == "darwin" and os.uname().machine == "arm64")
    require(all(os.environ.get(key) == value for key, value in (
        ("GITHUB_ACTIONS", "true"), ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "macOS"), ("RUNNER_ARCH", "ARM64"))))
    require(os.environ.get("ImageOS") in {"macos15", "macos15-arm64"})
    require(os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus")
    require(os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch")
    os.umask(0o077)
    source = args.source.resolve(strict=True)
    certificate = args.certificate.resolve(strict=True)
    require(certificate.stat().st_size <= 16 * 1024)
    require(digest(certificate) == args.certificate_sha256)
    data = certificate.read_bytes()
    require(data.startswith(b"-----BEGIN CERTIFICATE-----\n")
            and data.rstrip().endswith(b"-----END CERTIFICATE-----")
            and b"PRIVATE KEY" not in data)
    revision = subprocess.run(["/usr/bin/git", "-C", str(source), "rev-parse", "HEAD"],
                              capture_output=True, text=True, timeout=10, check=True)
    require(revision.stdout.strip() == SOURCE)
    dist = (source / "dist").resolve(strict=True)
    bundles = list(dist.rglob("Morpheus.app"))
    require(len(bundles) == 1 and not bundles[0].is_symlink())
    bundle = bundles[0].resolve(strict=True)
    require(bundle.is_relative_to(dist))
    check_bundle(bundle)
    binary = bundle / "Contents/MacOS/morpheus-app"
    license_file = bundle / "Contents/Resources/LICENSE"
    require(binary.is_file() and license_file.is_file())
    require(digest(license_file) == digest(source / "LICENSE"))
    # A new, private temporary directory; nothing supplied by an app profile.
    require(args.output.is_absolute() and not args.output.exists())
    output = args.output
    output.mkdir(mode=0o700)
    require(not output.resolve().is_relative_to(source))
    log = output / "producer-private.log"
    architecture = subprocess.run(["/usr/bin/lipo", "-archs", str(binary)],
                                  capture_output=True, text=True, timeout=10, check=True)
    require(architecture.stdout.strip() == "arm64")
    # Ad-hoc signing is solely for this private developer preview. It is not
    # Developer ID signing or notarization and grants no system permissions.
    command(["/usr/bin/codesign", "--force", "--sign", "-",
             "--timestamp=none", str(bundle)], log)
    command(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(bundle)], log)
    count, size = check_bundle(bundle)
    archive = output / "preview-private.tar"
    with tarfile.open(archive, "w", format=tarfile.PAX_FORMAT, dereference=False) as tar:
        tar.add(bundle, arcname="Morpheus.app", recursive=True)
    require(archive.stat().st_size <= MAX_BYTES + 32 * 1024 * 1024)
    upload = output / "upload"
    upload.mkdir(mode=0o700)
    sealed = upload / "morpheus-arm64.cms"
    command([OPENSSL, "cms", "-encrypt", "-aes-256-gcm", "-binary", "-outform", "DER",
             "-in", str(archive), "-out", str(sealed), "-recip", str(certificate),
             "-keyopt", "rsa_padding_mode:oaep", "-keyopt", "rsa_oaep_md:sha256"], log)
    receipt = {"v": 1, "source": SOURCE, "architecture": "arm64",
               "workflow": os.environ["GITHUB_SHA"], "run": os.environ["GITHUB_RUN_ID"],
               "certificate_sha256": digest(certificate), "archive_sha256": digest(archive),
               "binary_sha256": digest(binary), "ciphertext_sha256": digest(sealed),
               "files": count, "bundle_bytes": size, "ciphertext_bytes": sealed.stat().st_size,
               "encryption": "CMS-AES256GCM-RSA3072-OAEP-SHA256",
               "ad_hoc_signed": True, "notarized": False,
               "build_profile": "release", "publication_approved": False}
    (upload / "receipt.json").write_text(json.dumps(receipt, sort_keys=True) + "\n")
    print(json.dumps({"check": "owner_preview_sealed", "status": "passed",
                      "source": SOURCE, "binary_sha256": receipt["binary_sha256"],
                      "ciphertext_sha256": receipt["ciphertext_sha256"],
                      "certificate_sha256": receipt["certificate_sha256"],
                      "publication_approved": False}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print('{"check":"owner_preview_sealed","status":"failed"}', flush=True)
        raise SystemExit(1) from None
