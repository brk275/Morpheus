#!/usr/bin/env python3
"""Select only standard hosted runners for explicit native-vault acceptance."""
import json
import os


MACOS = (
    {"os": "macos-15", "label": "macos-arm64", "arch": "aarch64"},
    {"os": "macos-15-intel", "label": "macos-x64", "arch": "x86_64"},
)
WINDOWS = ({"os": "windows-2025", "label": "windows-x64", "arch": "x86_64"},)


def select(platform):
    if type(platform) is not str or platform not in {"macos", "macos-arm64", "macos-x64", "windows", "all"}:
        raise ValueError("Unsupported native vault platform selection")
    if platform in {"macos-arm64", "macos-x64"}:
        selected = MACOS[:1] if platform == "macos-arm64" else MACOS[1:]
    else:
        selected = MACOS if platform == "macos" else WINDOWS if platform == "windows" else MACOS + WINDOWS
    return {"include": [dict(item) for item in selected]}


def main():
    matrix = select(os.environ.get("NATIVE_VAULT_PLATFORM"))
    encoded = json.dumps(matrix, separators=(",", ":"), sort_keys=True)
    print(encoded)
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write("matrix=" + encoded + "\n")


if __name__ == "__main__":
    main()
