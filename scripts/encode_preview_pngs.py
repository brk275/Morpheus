#!/usr/bin/env python3
"""Losslessly encode the native preview CLI's exact RGB snapshots as PNG.

This encoder does not render, draw, crop, resize, or alter pixels. Raw snapshots
and native logs remain outside the upload directory. Only four fixed PNG names
and a path-free provenance document are produced in a newly created directory.
"""

import argparse
import hashlib
import json
import os
import re
import stat
import struct
import sys
import zlib
from pathlib import Path

WIDTH, HEIGHT = 1560, 1040
HEADER = b"P6\n1560 1040\n255\n"
PIXEL_BYTES = WIDTH * HEIGHT * 3
NAMES = ("desktop-preview", "account-switcher", "sign-in", "incoming-call")


def require(condition):
    if not condition:
        raise ValueError("Native preview input or output validation failed")


def regular_bytes(path, limit):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "rb") as source:
        metadata = os.fstat(source.fileno())
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_size <= limit)
        data = source.read(limit + 1)
    require(len(data) <= limit)
    return data


def parse_ppm(data):
    require(len(data) == len(HEADER) + PIXEL_BYTES and data.startswith(HEADER))
    # Never skip pixel whitespace: the header ends at this exact byte offset.
    return data[len(HEADER):]


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def encode_png(pixels):
    require(len(pixels) == PIXEL_BYTES)
    stride = WIDTH * 3
    rows = b"".join(b"\x00" + pixels[start:start + stride] for start in range(0, len(pixels), stride))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 9))
            + chunk(b"IEND", b""))


def binary_digest(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "rb") as source:
        metadata = os.fstat(source.fileno())
        require(stat.S_ISREG(metadata.st_mode) and 0 < metadata.st_size <= 2 * 1024**3)
        return hashlib.file_digest(source, "sha256").hexdigest()


def convert(input_dir, output_dir, source_commit, binary, run_id, run_attempt):
    require(input_dir.is_absolute() and output_dir.is_absolute())
    require(input_dir.is_dir() and not input_dir.is_symlink())
    require(not output_dir.exists() and not output_dir.is_symlink())
    require(re.fullmatch(r"[0-9a-f]{40}", source_commit) is not None)
    require(all(re.fullmatch(r"[1-9][0-9]{0,19}", value) is not None for value in (run_id, run_attempt)))
    require({path.name for path in input_dir.iterdir()} == {name + ".ppm" for name in NAMES})
    executable_sha256 = binary_digest(binary)
    images, facts = {}, {}
    # Validate the whole batch before creating an upload directory.
    for name in NAMES:
        raw = regular_bytes(input_dir / (name + ".ppm"), len(HEADER) + PIXEL_BYTES)
        pixels = parse_ppm(raw)
        png = encode_png(pixels)
        images[name + ".png"] = png
        facts[name + ".png"] = {
            "ppm_sha256": hashlib.sha256(raw).hexdigest(),
            "rgb_sha256": hashlib.sha256(pixels).hexdigest(),
            "png_sha256": hashlib.sha256(png).hexdigest(),
        }
    provenance = {
        "schema": 1,
        "source_commit": source_commit,
        "binary_sha256": executable_sha256,
        "workflow_run_id": run_id,
        "workflow_run_attempt": run_attempt,
        "capture_cli": "--morpheus-capture-preview",
        "image_source": "native Slint window snapshot of fictional preview fixtures",
        "width": WIDTH,
        "height": HEIGHT,
        "pixel_format": "RGB8",
        "encoding": "lossless PNG; unchanged RGB pixels",
        "images": facts,
    }
    output_dir.mkdir(mode=0o700)
    for name, data in images.items():
        with (output_dir / name).open("xb") as destination:
            destination.write(data)
        (output_dir / name).chmod(0o600)
    with (output_dir / "provenance.json").open("x", encoding="utf-8") as destination:
        json.dump(provenance, destination, indent=2, sort_keys=True)
        destination.write("\n")
    (output_dir / "provenance.json").chmod(0o600)
    return provenance


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    args = parser.parse_args()
    try:
        convert(args.input_dir, args.output_dir, args.source_commit, args.binary, args.run_id, args.run_attempt)
    except (OSError, ValueError):
        print("Native preview PNG conversion failed validation", file=sys.stderr)
        return 1
    print("Native preview PNG conversion passed: 4 lossless images and provenance")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
