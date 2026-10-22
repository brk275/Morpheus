#!/usr/bin/env python3
"""Explicit Ubuntu24/x64 fixed-client preparation for normal native builds.

Nothing is installed system-wide. The new owned prefix must remain present for
its resulting executables. Cargo hooks only validate/link this prepared input;
they never invoke this driver, download sources, or compile native code.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "packaging/linux"))
sys.path.insert(0, str(ROOT / "scripts"))
import appimage_client as client
from appimage_process import OwnedRunner, download
import element_process_namespace as containment


class BuildRunner(OwnedRunner):
    def assert_containment(self):
        # Hosted source compilation uses the existing private PID namespace.
        # An ordinary developer invocation is unprivileged, bounded source
        # compilation, with no claim that process groups form a sandbox.
        client.require_build_platform()
        if os.environ.get("GITHUB_ACTIONS") == "true":
            super().assert_containment()


def clean_environment(environment):
    result = client.R.clean_environment(dict(environment))
    for name in list(result):
        if (name in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                     "MORPHEUS_PIPEWIRE_CLIENT_RECEIPT", "MORPHEUS_PIPEWIRE_CLIENT_VARIANT",
                     "RUSTC", "RUSTDOC", "RUSTC_BOOTSTRAP", "CARGO_BUILD_RUSTFLAGS",
                     "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_ENCODED_RUSTDOCFLAGS"}
                or name.startswith("CARGO_TARGET_") and name.endswith(("_RUSTFLAGS", "_LINKER"))):
            result.pop(name)
    return result


def prepare(prefix, *, variant):
    if variant != "native":
        raise client.PackageFailure("configuration")
    client.require_build_platform()
    prefix = client.private_directory(prefix)
    environment = clean_environment(os.environ)
    runner = BuildRunner(prefix / "command-logs")
    archives = {key: download(runner, url=client.R.BASE_URL + name, maximum=maximum,
                            expected_sha256=digest, root=prefix, environment=environment)
                for key, (name, digest, maximum) in client.R.ARCHIVES.items()}
    patch = download(runner, url="https://github.com/PipeWire/pipewire/commit/" + client.R.COMMIT + ".patch",
                     maximum=32768, expected_sha256=client.R.UPSTREAM_PATCH_SHA256,
                     root=prefix, environment=environment)
    stage, metadata = client.prepare_client(prefix, archives, patch, runner.run, variant=variant)
    receipt = client.build_receipt(stage, metadata, variant=variant)
    # This is a private source-build receipt, not a deployed-library identity.
    client.write_new(prefix / "prepared.json", json.dumps({
        "schema": 1, "variant": variant, "receipt": str(receipt),
        "source_client_sha256": metadata["source_client_sha256"],
        "build_id": metadata["build_id"], "soname": client.R.NATIVE_SONAME,
    }, sort_keys=True).encode("ascii") + b"\n", 0o400)
    return receipt


def cargo_environment(receipt, environment):
    result = clean_environment(environment)
    cargo = shutil.which("cargo", path=environment.get("PATH", ""))
    if cargo is None:
        raise client.PackageFailure("tool")
    resolved = Path(cargo).resolve(strict=True)
    for item in (resolved, *resolved.parents):
        info = item.stat()
        if info.st_uid not in {0, os.getuid()} or info.st_mode & 0o022:
            raise client.PackageFailure("tool")
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise client.PackageFailure("tool")
    # Preserve rustup's invocation path, not its resolved multicall filename.
    result.update(PATH=str(Path(cargo).parent) + ":/usr/bin:/bin",
                  MORPHEUS_PIPEWIRE_CLIENT_VARIANT="native",
                  MORPHEUS_PIPEWIRE_CLIENT_RECEIPT=str(receipt))
    return str(Path(cargo).absolute()), result


def github_environment(receipt):
    if (os.environ.get("GITHUB_ACTIONS") != "true"
            or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted"):
        raise client.PackageFailure("configuration")
    path = Path(os.environ["GITHUB_ENV"])
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
    with os.fdopen(fd, "ab") as output:
        info = os.fstat(output.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o022 or info.st_size > 1024 * 1024):
            raise client.PackageFailure("input")
        encoded = str(receipt)
        if any(c in encoded for c in "\r\n\0"):
            raise client.PackageFailure("input")
        output.write(("MORPHEUS_PIPEWIRE_CLIENT_VARIANT=native\n"
                      "MORPHEUS_PIPEWIRE_CLIENT_RECEIPT=" + encoded + "\n").encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", required=True, type=Path)
    parser.add_argument("--variant", required=True, choices=("native",))
    parser.add_argument("--github-env", action="store_true")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("cargo", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    selected_cargo = args.cargo[1:] if args.cargo[:1] == ["--"] else args.cargo
    if (selected_cargo and (selected_cargo[0] not in {"build", "check", "test"} or "--locked" not in selected_cargo)
            or selected_cargo and os.environ.get("GITHUB_ACTIONS") == "true"
            or args.github_env and os.environ.get("GITHUB_ACTIONS") != "true"):
        raise client.PackageFailure("configuration")
    client.require_build_platform()
    prefix = args.prefix.absolute()
    if any(c in str(prefix) for c in "\r\n\0:,$\\"):
        raise client.PackageFailure("input")
    if args.inside:
        containment.assert_contained()
        prepare(prefix, variant=args.variant)
        return 0
    prefix.mkdir(mode=0o700)  # Refuse any old directory, alias or reused prefix.
    client.private_directory(prefix)
    if os.environ.get("GITHUB_ACTIONS") == "true":
        if (os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted"
                or os.environ.get("RUNNER_OS") != "Linux"):
            raise client.PackageFailure("configuration")
        containment.containment_check(prefix / "containment-probe")
        environment = clean_environment(os.environ)
        command = containment.namespace_command(
            [str(Path(sys.executable).resolve()), "-I", str(Path(__file__).resolve()),
             "--prefix", str(prefix), "--variant", args.variant, "--inside"],
            prefix / "namespace", environment)
        log = prefix / "preparation.log"
        with log.open("xb") as output:
            log.chmod(0o600)
            process = subprocess.Popen(command, env=environment, stdin=subprocess.DEVNULL,
                                       stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                # Four <=125s downloads plus the existing 600s whole build and
                # namespace cleanup. No capture or existing test deadline moves.
                status = containment.wait_owned(process, 1120)
            finally:
                containment.stop_owned(process)
        if status:
            raise client.PackageFailure("command")
        receipt = prefix / "stage/build-client.txt"
        client.read_regular(receipt, 4096)
    else:
        receipt = prepare(prefix, variant=args.variant)
    if args.github_env:
        github_environment(receipt)
    command = args.cargo
    if command and command[0] == "--":
        command = command[1:]
    if command:
        if command[0] not in {"build", "check", "test"} or "--locked" not in command:
            raise client.PackageFailure("configuration")
        cargo, environment = cargo_environment(receipt, os.environ)
        runner = BuildRunner(prefix / "cargo-logs")
        # Hosted callers prepare once with --github-env, then use their existing
        # workload owner/deadlines. This convenience path is for source builds.
        if os.environ.get("GITHUB_ACTIONS") == "true":
            raise client.PackageFailure("configuration")
        runner.run("native-cargo", [cargo, *command], cwd=ROOT, env=environment,
                   timeout=3600, output_limit=8 * 1024 * 1024)
    print('MORPHEUS_NATIVE_PIPEWIRE_PREPARED {"prepared":true,"variant":"native"}')
    return 0


if __name__ == "__main__":
    def interrupted(_signal, _frame):
        raise RuntimeError("native source preparation interrupted; stopping owned children")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        raise SystemExit(main())
    except Exception:
        print("Native PipeWire source preparation failed; private build logs retained.", file=sys.stderr)
        raise SystemExit(1) from None
