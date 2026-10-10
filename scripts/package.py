#!/usr/bin/env python3
"""Build local QA packages with the matching platform's package configuration."""

import argparse
import json
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--formats", help="Cargo Packager formats, such as deb,appimage")
    parser.add_argument("--check", action="store_true", help="Validate configuration without building")
    args = parser.parse_args()
    config = package_config()
    if args.check:
        print(f"Package configuration is valid for {sys.platform}")
        return
    # Cargo Packager 0.11.8 accepts inline JSON. Passing an argv item avoids
    # shell interpolation and keeps resource paths relative to the repo root.
    command = ["cargo", "packager", "--release", "--config", json.dumps(config)]
    if args.formats:
        command.extend(["--formats", args.formats])
    subprocess.run(command, check=True, cwd=ROOT)


if __name__ == "__main__":
    main()
