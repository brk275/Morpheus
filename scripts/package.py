#!/usr/bin/env python3
"""Build local QA packages with the matching platform's package configuration."""

import argparse
from email.utils import parseaddr
import json
import os
import platform
import re
import subprocess
import sys
import tempfile
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


def linux_appimage(formats, scanner, *, source_bundle=None):
    """Keep an explicit AppImage-only QA route; never redefine default/all/DEB."""
    if formats is None or [value.strip().lower() for value in formats.split(",")] != ["appimage"]:
        raise ValueError("Linux fixed-client packaging requires explicit --formats appimage; DEB/default/all remain unsupported")
    if not scanner:
        raise ValueError("Linux AppImage QA requires the reviewed --gitleaks executable")
    if any(name in os.environ for name in ("CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD")):
        raise ValueError("Package unsigned first; signing and update acceptance remain separate gates")
    # Replace this controller so canonical __main__ retains its SIGTERM handler
    # as well as public/manual/source/namespace/scanner/installed-capture gates.
    command = [sys.executable, "-I", "-B",
               str(ROOT / "packaging/linux/package_appimage.py"), "--gitleaks", scanner]
    if source_bundle is None:
        command += ["--qa-output", str(ROOT / "dist")]
    else:
        inputs, output = source_bundle
        command += ["--source-bundle-inputs", str(inputs), "--source-bundle-output", str(output)]
    return os.execv(sys.executable, command)


def macos_update_formats(config, formats=None):
    """Retain the one existing .app build needed by the bounded Mac payload."""
    if any(name in os.environ for name in ("CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD")):
        raise ValueError("Prepare the bounded unsigned Mac payload first; sign its final bytes separately")
    if __package__:
        from .prepare_macos_update import RESOURCES
    else:
        from prepare_macos_update import RESOURCES
    if (config.get("identifier") != "io.github.brk275.morpheus"
            or config.get("product-name") != "Morpheus"
            or config.get("resources") != list(RESOURCES.values())):
        raise ValueError("Mac payload configuration must retain the reviewed identity and all required resources")
    selected = formats.split(",") if formats is not None else config.get("formats", ["default"])
    if selected in (["default"], ["all"]):
        selected = ["app", "dmg"]
    if (not isinstance(selected, list) or not selected or len(selected) != len(set(selected))
            or any(kind not in {"app", "dmg"} for kind in selected)):
        raise ValueError("Mac packaging requires app, dmg or app,dmg; other platform formats are not remapped")
    # Packager already builds .app for DMG. Including App explicitly prevents
    # its cleanup step from deleting the exact bundle; no second build occurs.
    return "app,dmg" if "dmg" in selected else "app"


def prepare_macos_payload(config, output, notice_contents):
    if __package__:
        from .prepare_macos_update import RESOURCES, prepare_update
    else:
        from prepare_macos_update import RESOURCES, prepare_update
    if (config.get("identifier") != "io.github.brk275.morpheus"
            or config.get("product-name") != "Morpheus"
            or config.get("resources") != list(RESOURCES.values())):
        raise ValueError("Mac payload configuration must retain the reviewed identity and all required resources")
    arch = {"arm64": "aarch64", "x86_64": "x86_64"}.get(platform.machine())
    if arch is None:
        raise ValueError("Mac update payload requires a supported native Mac architecture")
    output = output.resolve(strict=True)
    receipt = prepare_update(output / "Morpheus.app", output / "Morpheus.app.tar.gz",
                             version=config["version"], arch=arch, root=ROOT, notice_contents=notice_contents)
    print("MORPHEUS_MAC_UPDATE " + json.dumps(receipt, sort_keys=True))
    return receipt


def native_package(config, args):
    """Stage exact source notices before the one ordinary native package build."""
    if __package__:
        from . import native_notices as notices
        from .package_revision import expected_revision
    else:
        import native_notices as notices
        from package_revision import expected_revision
    if args.native_source_bundle is None or args.native_source_receipt is None:
        raise ValueError("Mac/Windows packages require explicit candidate-bound source bundle and receipt")
    if any(name in os.environ for name in ("CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD")):
        raise ValueError("Prepare unsigned packages and data payloads before separate signing acceptance")
    if sys.platform == "win32" and args.formats != "nsis":
        raise ValueError("Windows marked packages require explicit --formats nsis")
    formats = macos_update_formats(config, args.formats) if sys.platform == "darwin" else None
    output = ROOT / config.get("out-dir", "dist")
    final_names = (*notices.NAMES, "Morpheus.app.tar.gz" if sys.platform == "darwin" else "Morpheus.windows-payload-v2.bin")
    if any((output / name).exists() or (output / name).is_symlink() for name in final_names):
        raise ValueError("Preserve previous native payload/notices before packaging; no output is overwritten")
    (ROOT / "target").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="native-notices-", dir=ROOT / "target") as temporary:
        directory = Path(temporary)
        contents = notices.prepare(args.native_source_bundle, args.native_source_receipt, directory,
                                   revision=expected_revision(os.environ), platform=sys.platform,
                                   version=config["version"], working=ROOT)
        if sys.platform == "win32":
            if __package__:
                from .prepare_windows_update import prepare
            else:
                from prepare_windows_update import prepare
            environment = {**os.environ, "MORPHEUS_PACKAGE_FORMAT": "nsis-payload-v2"}
            subprocess.run(["cargo", "build", "--release", "--locked", "-p", "morpheus-app", "--features", "native-calls"],
                           check=True, cwd=ROOT, env=environment)
            with tempfile.TemporaryDirectory(prefix="windows-payload-", dir=ROOT / "target") as payload_dir:
                prepared, payload = prepare(config, ROOT, Path(payload_dir), notice_contents=contents)
                subprocess.run(["cargo", "packager", "--release", "--config", json.dumps(prepared), "--formats", "nsis"],
                               check=True, cwd=ROOT, env=environment)
                output.mkdir(parents=True, exist_ok=True)
                with (output / payload.name).open("xb") as destination, payload.open("rb") as source:
                    while chunk := source.read(65536):
                        destination.write(chunk)
                    destination.flush(); os.fsync(destination.fileno())
        else:
            prepared = dict(config)
            prepared["resources"] = list(config["resources"]) + [
                {"src": str(directory / name), "target": name} for name in notices.NAMES]
            subprocess.run(["cargo", "packager", "--release", "--config", json.dumps(prepared), "--formats", formats],
                           check=True, cwd=ROOT)
            prepare_macos_payload(config, output, contents)
        # Keep exact generated comparison data beside private QA artifacts, never upload.
        for name in notices.NAMES:
            with (output / name).open("xb") as stream:
                if os.name != "nt": os.fchmod(stream.fileno(), 0o644)
                stream.write(contents[name]); stream.flush(); os.fsync(stream.fileno())
        print("MORPHEUS_NATIVE_NOTICES " + json.dumps({"schema": 1,
              "files": {name: {"bytes": len(contents[name]), "sha256": notices.digest(contents[name])}
                        for name in notices.NAMES}, "distribution_ready": False}, sort_keys=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--formats", help="Cargo Packager formats, such as deb,appimage")
    parser.add_argument("--check", action="store_true", help="Validate configuration without building")
    parser.add_argument("--gitleaks", help="Reviewed scanner for explicit Linux AppImage QA")
    parser.add_argument("--source-bundle-inputs", type=Path,
                        help="Explicit offline Cargo source-input plan; source bundle only, no installer")
    parser.add_argument("--source-bundle-output", type=Path,
                        help="Existing empty private directory outside the checkout for source evidence")
    parser.add_argument("--native-source-bundle", type=Path, help="Authenticated existing corresponding-source archive for Mac/Windows notices")
    parser.add_argument("--native-source-receipt", type=Path, help="Matching private source archive receipt; no fetch or legal acceptance")
    args = parser.parse_args()
    if args.source_bundle_inputs is not None or args.source_bundle_output is not None:
        if (args.check or not sys.platform.startswith("linux")
                or args.native_source_bundle is not None or args.native_source_receipt is not None
                or args.source_bundle_inputs is None or args.source_bundle_output is None):
            raise ValueError("Source evidence requires both explicit paths and Linux/AppImage; --check stays separate")
        return linux_appimage(args.formats, args.gitleaks,
                              source_bundle=(args.source_bundle_inputs, args.source_bundle_output))
    config = package_config()
    if args.check:
        print(f"Package configuration is valid for {sys.platform}")
        return
    preflight_deb_authors(config, args.formats)
    if sys.platform.startswith("linux"):
        if args.native_source_bundle is not None or args.native_source_receipt is not None:
            raise ValueError("Native notice inputs are for Mac/Windows only; Linux retains its existing source route")
        return linux_appimage(args.formats, args.gitleaks)
    if sys.platform not in {"darwin", "win32"}:
        raise ValueError("Unsupported native package platform")
    return native_package(config, args)


if __name__ == "__main__":
    main()
