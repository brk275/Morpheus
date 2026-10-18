#!/usr/bin/env python3
"""Select explicitly requested standard package runners without dispatching work."""

import json
import os

if __package__:
    from .package_revision import select_revision
else:
    from package_revision import select_revision


def package_matrix(platform, linux_formats, appimage_updates=False, appimage_capture=False):
    if appimage_capture and (platform != "linux" or linux_formats != "appimage" or appimage_updates):
        raise ValueError("Installed capture QA requires Linux/AppImage alone and excludes update QA")
    if appimage_updates and (platform not in {"linux", "all"} or linux_formats != "appimage"):
        raise ValueError("AppImage update QA requires Linux (or all) and the appimage format")
    try:
        formats = {"both": "deb,appimage", "appimage": "appimage", "deb": "deb"}[linux_formats]
    except KeyError:
        raise ValueError("Unknown Linux package format selection") from None
    options = {
        "linux": [{"os": "ubuntu-24.04", "label": "linux-x64", "formats": formats}],
        "windows": [{"os": "windows-2025", "label": "windows-x64", "formats": "nsis"}],
        "macos": [
            {"os": "macos-15", "label": "macos-arm64", "formats": "dmg"},
            {"os": "macos-15-intel", "label": "macos-x64", "formats": "dmg"},
        ],
    }
    options["all"] = options["macos"] + options["windows"] + options["linux"]
    try:
        return {"include": options[platform]}
    except KeyError:
        raise ValueError("Unknown package platform selection") from None


def main():
    update_input = os.environ["APPIMAGE_UPDATES"]
    if update_input not in {"true", "false"}:
        raise ValueError("AppImage update selection must be an explicit boolean")
    capture_input = os.environ.get("APPIMAGE_CAPTURE", "false")
    if capture_input not in {"true", "false"}:
        raise ValueError("AppImage capture selection must be an explicit boolean")
    matrix = package_matrix(
        os.environ["PACKAGE_PLATFORM"], os.environ["LINUX_FORMATS"], update_input == "true", capture_input == "true"
    )
    # Resolve/validate before emitting any runner or cache outputs. Only a
    # complete commit ID is accepted; branches and abbreviated refs can move.
    workflow_revision = os.environ["GITHUB_SHA"]
    source_revision = select_revision(os.environ.get("PACKAGE_SOURCE_REVISION", ""), workflow_revision)
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write("matrix=" + json.dumps(matrix) + "\n")
        output.write("source_revision=" + source_revision + "\n")
        output.write("cache_write=" + ("true" if source_revision == workflow_revision else "false") + "\n")


if __name__ == "__main__":
    main()
