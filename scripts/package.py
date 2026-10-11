#!/usr/bin/env python3
"""Build local QA packages with the matching platform's package configuration."""

import argparse
from email.utils import parseaddr
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
LINUX_KEYS = {"deb", "linux", "appimage", "pacman"}


def package_config(root=ROOT, platform=sys.platform):
    with (root / "Packager.toml").open("rb") as source:
        config = tomllib.load(source)
    if platform.startswith("linux"):
        with (root / "packaging/linux.toml").open("rb") as source:
            overlay = tomllib.load(source)
        if not set(overlay) <= LINUX_KEYS:
            raise ValueError("Linux package overlay may only contain Linux-specific tables")
        if set(config) & set(overlay):
            raise ValueError("Platform package tables must not also appear in Packager.toml")
        config.update(overlay)
    return config


def preflight_deb_authors(config, formats=None, platform=sys.platform):
    """Check required public metadata before spending time building a DEB."""
    if not platform.startswith("linux"):
        return
    selected = formats.split(",") if formats else config.get("formats", ["default"])
    if not any(value.strip().lower() in {"deb", "all", "default"} for value in selected):
        return
    authors = config.get("authors", [])
    if not isinstance(authors, list) or len(authors) != 1:
        raise ValueError("DEB packaging requires one configured public maintainer in top-level authors before building")
    # Cargo Packager joins authors into Debian's single Maintainer field.
    first = authors[0]
    if not isinstance(first, str) or any(character in first for character in "\r\n\x00"):
        raise ValueError("DEB maintainer must be a single public Name <email> author")
    name, address = parseaddr(first)
    if not name.strip() or not re.fullmatch(r"[^\s<>@]+@[^\s<>@]+\.[^\s<>@]+", address):
        raise ValueError("DEB maintainer requires an owner-approved public Name <email> author")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--formats", help="Cargo Packager formats, such as deb,appimage")
    parser.add_argument("--check", action="store_true", help="Validate configuration without building")
    args = parser.parse_args()
    config = package_config()
    if args.check:
        print(f"Package configuration is valid for {sys.platform}")
        return
    preflight_deb_authors(config, args.formats)
    if sys.platform.startswith("linux") and os.environ.get("CARGO_PACKAGER_SIGN_PRIVATE_KEY"):
        raise ValueError("Package unsigned first, prepare AppImage metadata, then use the official Cargo Packager signer on the final artifact")
    output = ROOT / config.get("out-dir", "dist")
    before = {
        path: (path.stat().st_mtime_ns, path.stat().st_size, path.stat().st_ino)
        for path in output.rglob("*.AppImage")
    } if sys.platform.startswith("linux") else {}
    # Cargo Packager 0.11.8 accepts inline JSON. Passing an argv item avoids
    # shell interpolation and keeps resource paths relative to the repo root.
    command = ["cargo", "packager", "--release", "--config", json.dumps(config)]
    if args.formats:
        command.extend(["--formats", args.formats])
    subprocess.run(command, check=True, cwd=ROOT)
    if sys.platform.startswith("linux"):
        if __package__:
            from .prepare_appimage_update import binary_metadata, prepare_appimage
        else:
            from prepare_appimage_update import binary_metadata, prepare_appimage

        changed = [
            path for path in output.rglob("*.AppImage")
            if before.get(path) != (path.stat().st_mtime_ns, path.stat().st_size, path.stat().st_ino)
        ]
        if changed:
            metadata = binary_metadata(ROOT / config.get("binaries-dir", "target/release") / "morpheus-app")
            for artifact in changed:
                prepare_appimage(artifact, metadata)
            print(f"Prepared compatibility metadata for {len(changed)} unsigned AppImage package(s)")


if __name__ == "__main__":
    main()
