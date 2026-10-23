#!/usr/bin/env python3
"""Append bounded update compatibility metadata BEFORE official updater signing.

This does not sign or make an artifact trusted. Cargo Packager's official
minisign signer must sign the complete final AppImage after this step.
"""

import argparse
import json
import os
import stat
import struct
import subprocess
import tempfile
from pathlib import Path

MAGIC = b"\nMORPHEUS-APPIMAGE-METADATA-V1\n"
MAX_BYTES = 512 * 1024 * 1024


def validate_metadata(metadata):
    if not isinstance(metadata, dict) or set(metadata) != {
        "protocol", "product", "version", "arch", "persistence"
    }:
        raise ValueError("Invalid update metadata fields")
    if type(metadata["protocol"]) is not int or metadata["protocol"] != 2 or metadata["product"] != "io.github.brk275.morpheus":
        raise ValueError("Unsupported update metadata protocol or product")
    if metadata["arch"] != "x86_64":
        raise ValueError("Only the supported Linux x86_64 AppImage can be prepared")
    if not isinstance(metadata["version"], str) or not 0 < len(metadata["version"]) <= 128:
        raise ValueError("Invalid application version")
    cohort = metadata["persistence"]
    if not isinstance(cohort, str) or len(cohort) != 64 or any(c not in "0123456789abcdef" for c in cohort):
        raise ValueError("Invalid persistence compatibility fingerprint")
    result = json.dumps(metadata, separators=(",", ":"), sort_keys=True).encode()
    if len(result) > 4096:
        raise ValueError("Update metadata exceeds its limit")
    return result


def prepare_appimage(artifact, metadata):
    artifact = Path(artifact)
    encoded = validate_metadata(metadata)
    details = artifact.lstat()
    if not stat.S_ISREG(details.st_mode) or artifact.is_symlink():
        raise ValueError("AppImage must be a regular file, not a symbolic link")
    # Never quietly invalidate a signature produced before the footer step.
    for suffix in [".sig", ".minisig"]:
        if artifact.with_name(artifact.name + suffix).exists():
            raise ValueError("Prepare the AppImage before signing; an existing signature must not be reused")
    with artifact.open("rb") as source:
        header = source.read(64)
        if len(header) != 64 or header[:6] != b"\x7fELF\x02\x01" or header[8:11] != b"AI\x02" or header[18:20] != b"\x3e\x00":
            raise ValueError("Not a supported x86_64 Type 2 AppImage")
        end = source.seek(0, os.SEEK_END)
        if end + len(encoded) + 4 + len(MAGIC) > MAX_BYTES:
            raise ValueError("AppImage exceeds updater size limit")
        source.seek(-len(MAGIC), os.SEEK_END)
        if source.read() == MAGIC:
            source.seek(-len(MAGIC) - 4, os.SEEK_END)
            old_len = struct.unpack("<I", source.read(4))[0]
            if old_len == 0 or old_len > 4096 or old_len + 4 + len(MAGIC) > end - 64:
                raise ValueError("Existing metadata footer is malformed")
            end -= old_len + 4 + len(MAGIC)
        source.seek(0)
        temp_path = None
        try:
            with tempfile.NamedTemporaryFile(dir=artifact.parent, prefix=".morpheus-appimage-", delete=False) as target:
                temp_path = Path(target.name)
                remaining = end
                while remaining:
                    block = source.read(min(1024 * 1024, remaining))
                    if not block:
                        raise ValueError("AppImage changed while preparing metadata")
                    target.write(block)
                    remaining -= len(block)
                target.write(encoded)
                target.write(struct.pack("<I", len(encoded)))
                target.write(MAGIC)
                os.fchmod(target.fileno(), stat.S_IMODE(details.st_mode))
                target.flush()
                os.fsync(target.fileno())
            os.replace(temp_path, artifact)
            temp_path = None
            directory = os.open(artifact.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            if temp_path is not None:
                temp_path.unlink(missing_ok=True)


def binary_metadata(binary):
    # This is the just-built native binary, not a downloaded update. The flag
    # is handled before UI, credential vault or any account storage is opened.
    result = subprocess.run(
        [str(Path(binary).resolve()), "--morpheus-update-metadata"],
        check=True, capture_output=True, timeout=20,
    )
    if len(result.stdout) > 4096:
        raise ValueError("Application returned oversized update metadata")
    metadata = json.loads(result.stdout)
    validate_metadata(metadata)
    return metadata


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifact", type=Path)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    prepare_appimage(args.artifact, binary_metadata(args.binary))
    print("AppImage compatibility metadata prepared; artifact is still unsigned")


if __name__ == "__main__":
    main()
