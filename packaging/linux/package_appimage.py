#!/usr/bin/env python3
"""Private Linux-only fixed-client AppImage wrapper; enablement is gated.

Run only after the installed normal-app capture adapter has been implemented and
reviewed. No runtime application override or capture diagnostic CLI is added.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(ROOT / "scripts"))
import appimage_client as client
from appimage_process import OwnedRunner, download
import appimage_diagnostics as diagnostics
import element_process_namespace as containment
from package import package_config
from prepare_appimage_update import prepare_appimage, validate_metadata
from package_revision import expected_revision, verify_checkout, SourceRevisionError


# Source-review gate only, never an environment bypass or runtime proof. The
# normal-app capture/peer/maps adapter and ownership/cleanup path were reviewed;
# actual installed capture and distribution readiness remain separate outcomes.
INSTALLED_CAPTURE_READY = True
FORMAT = "appimage"
MAX_APPIMAGE = 512 * 1024 * 1024
DIAGNOSTICS_PUBLISHED = False


class PackageFailure(RuntimeError):
    def __init__(self, reason):
        allowed = {"not_ready", "ineligible", "tool", "configuration", "output", "identity",
                   "notices", "capture", "scan", "cleanup", "unexpected"}
        self.reason = reason if reason in allowed else "unexpected"
        super().__init__("AppImage packaging failed: " + self.reason)


def require_readiness():
    if not INSTALLED_CAPTURE_READY:
        raise PackageFailure("not_ready")


def public_hosted_environment(environment):
    expected = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                "RUNNER_OS": "Linux", "GITHUB_REPOSITORY": "brk275/Morpheus",
                "GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "workflow_dispatch"}
    if any(environment.get(name) != value for name, value in expected.items()):
        raise PackageFailure("ineligible")
    client.require_build_platform()
    event = json.loads(client.read_regular(Path(environment["GITHUB_EVENT_PATH"]), 1024 * 1024))
    if event.get("repository", {}).get("private") is not False:
        raise PackageFailure("ineligible")


def controlled_environment(environment):
    # Preserve required runner/Cargo context and HOME; discard every compiler,
    # dynamic-loader, package plugin and inherited signing/capture override.
    allowed = {"HOME", "USER", "LOGNAME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME", "RUNNER_TEMP",
               "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS", "GITHUB_REPOSITORY", "GITHUB_REF",
               "GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_SHA", "MORPHEUS_PACKAGE_SOURCE_REVISION",
               "MORPHEUS_ELEMENT_NAMESPACE_CONTROL"}
    result = client.R.clean_environment({name: value for name, value in environment.items() if name in allowed})
    result["APPIMAGE_EXTRACT_AND_RUN"] = "1"
    result["RUSTUP_TOOLCHAIN"] = "1.99.0"
    result["RUST_LOG"] = "warn"
    return result


def owned_tool(path, *, root_owned=False):
    path = Path(path).absolute()
    resolved = path.resolve(strict=True)
    for item in (resolved, *resolved.parents):
        info = item.stat()
        if info.st_uid not in ({0} if root_owned else {0, os.getuid()}) or info.st_mode & 0o022:
            raise PackageFailure("tool")
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise PackageFailure("tool")
    # Keep the invocation name for rustup's cargo multicall symlink.
    return str(path)


def command_tools(environment):
    cargo = shutil.which("cargo", path=environment.get("PATH", ""))
    if not cargo:
        raise PackageFailure("tool")
    return {"cargo": owned_tool(cargo),
            "curl": owned_tool("/usr/bin/curl", root_owned=True),
            "readelf": owned_tool("/usr/bin/readelf", root_owned=True)}


def check_versions(runner, tools, root, environment):
    cargo = runner.run("cargo-version", [tools["cargo"], "--version"], cwd=root, env=environment,
                       timeout=20, output_limit=4096)
    packager = runner.run("packager-version", [tools["cargo"], "packager", "--version"], cwd=root,
                          env=environment, timeout=20, output_limit=4096)
    if (not re.fullmatch(rb"cargo 1\.99\.0(?: \([^\r\n]+\))?\n", cargo)
            or not re.fullmatch(rb"cargo-packager 0\.11\.8\n", packager)):
        raise PackageFailure("tool")


def download_inputs(runner, root, environment):
    payloads = {}
    for pin in client.pinned_tools():
        payloads[pin["cache_name"]] = download(
            runner, url=pin["url"], maximum=pin["size"], expected_size=pin["size"],
            expected_sha256=pin["sha256"], root=root, environment=environment)
    archives = {}
    for key, (name, digest, maximum) in client.R.ARCHIVES.items():
        archives[key] = download(runner, url=client.R.BASE_URL + name, maximum=maximum,
                                 expected_sha256=digest, root=root, environment=environment)
    patch = download(runner, url="https://github.com/PipeWire/pipewire/commit/" + client.R.COMMIT + ".patch",
                     maximum=32768, expected_sha256=client.R.UPSTREAM_PATCH_SHA256,
                     root=root, environment=environment)
    return payloads, archives, patch


def private_receipt(path, value):
    client.write_new(path, json.dumps(value, sort_keys=True, indent=2).encode() + b"\n")


def exact_artifact(directory):
    candidates = list(Path(directory).glob("*.AppImage"))
    if len(candidates) != 1:
        raise PackageFailure("output")
    artifact = candidates[0]
    info = artifact.lstat()
    if (not stat.S_ISREG(info.st_mode) or artifact.is_symlink() or info.st_uid != os.getuid()
            or info.st_nlink != 1 or not 64 < info.st_size <= MAX_APPIMAGE):
        raise PackageFailure("output")
    return artifact


def build_unsigned(runner, root, *, tools, environment, notices):
    """Complete build stage; final capture gate is called separately by owner.

    ``notices`` is the reviewed inventory adapter. prepare must provide verified
    source/copyright files for all possible deployed dependencies before packing;
    verify rejects every final ELF component lacking matching source provenance
    and notices. It must not classify licenses by filename alone.
    """
    check_versions(runner, tools, root, environment)
    payloads, archives, patch = download_inputs(runner, root, environment)
    package_environment = {**environment, **client.seed_tools(root, payloads)}
    package_environment["PATH"] = str(Path(tools["cargo"]).parent) + ":/usr/bin:/bin"
    stage, source_metadata = client.prepare_client(root, archives, patch, runner.run, variant="appimage")
    # Reuse the ordinary runner cache. Build output is not an installer or a
    # profile; only the fresh packaging/output directories must be private.
    target = ROOT / "target"
    if target.exists() and (target.is_symlink() or target.resolve() != target
                            or target.stat().st_uid != os.getuid()):
        raise PackageFailure("output")
    receipt = client.build_receipt(stage, source_metadata, variant="appimage")
    native_environment = {**package_environment, "CARGO_TARGET_DIR": str(target),
                          "MORPHEUS_PIPEWIRE_CLIENT_VARIANT": "appimage",
                          "MORPHEUS_PIPEWIRE_CLIENT_RECEIPT": str(receipt)}
    runner.run("native-build", [tools["cargo"], "build", "--release", "--locked", "-p", "morpheus-app",
               "--features", "native-calls"], cwd=ROOT, env=native_environment, timeout=3600,
               output_limit=8 * 1024 * 1024)
    binary = target / "release/morpheus-app"
    binary_bytes = client.read_regular(binary, MAX_APPIMAGE)
    if not binary_bytes.startswith(b"\x7fELF\x02\x01"):
        raise PackageFailure("identity")
    config = client.appimage_config(package_config(ROOT, "linux"), stage)
    # Native build was explicitly owned/bounded above. Avoid spawning an
    # unbounded shell build inside Cargo Packager's before hook.
    config.pop("before-packaging-command", None)
    if any(name in config for name in ("before-each-package-command", "after-packaging-command")):
        raise PackageFailure("configuration")
    config["binaries-dir"] = str(target / "release")
    output = root / "packages"
    output.mkdir(mode=0o700)
    config["out-dir"] = str(output)
    notice_record = notices.prepare(runner, root, binary, stage, source_metadata, config, package_environment)
    runner.run("package", [tools["cargo"], "packager", "--release", "--formats", FORMAT,
               "--config", json.dumps(config)], cwd=ROOT, env=package_environment, timeout=900,
               output_limit=8 * 1024 * 1024)
    tool_receipts = client.verify_tools_after(root, payloads)
    artifact = exact_artifact(output)
    extract = root / "extracted"
    extract.mkdir(mode=0o700)
    runner.run("extract", [str(artifact), "--appimage-extract"], cwd=extract, env=package_environment,
               timeout=120, output_limit=8 * 1024 * 1024)
    appdir = extract / "squashfs-root"
    shipped = client.verify_deployed_client(appdir, source_metadata)
    inventory = notices.verify(runner, appdir, notice_record, shipped, package_environment)
    # Footer generation uses the just-built ordinary metadata-only app command,
    # which exits before stores/network/UI. No key material is passed.
    raw_metadata = runner.run("update-metadata", [str(binary), "--morpheus-update-metadata"], cwd=root,
                              env=native_environment, timeout=20, output_limit=4096)
    metadata = json.loads(raw_metadata)
    validate_metadata(metadata)
    prepare_appimage(artifact, metadata)
    final_bytes = client.read_regular(artifact, MAX_APPIMAGE)
    record = {"schema": 1, "format": FORMAT, "unsigned": True,
              "source_revision": expected_revision(os.environ), "appimage_sha256": hashlib.sha256(final_bytes).hexdigest(),
              "native_executable_sha256": hashlib.sha256(binary_bytes).hexdigest(),
              "pipewire": shipped, "packager_tools": tool_receipts, "inventory": inventory,
              "installed_capture_passed": False, "distribution_ready": inventory.get("distribution_ready") is True,
              "system_capture_compatibility": "separate_unmodified_gate"}
    private_receipt(root / "build-receipt.json", record)
    return artifact, appdir, record


def bounded_file_log(path, maximum=8 * 1024 * 1024):
    """Read only regular current-user private logs; never print their bytes."""
    info = path.lstat()
    if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
            or info.st_mode & 0o077 or info.st_size > maximum):
        raise PackageFailure("scan")
    return client.read_regular(path, maximum)


def scan_private_logs(root, scanner):
    # Child logs are private even after a clean scan. A scan does not prove an
    # arbitrary server error is safe to print; publish only our fixed receipt.
    for path in sorted(root.rglob("*.log")):
        value = bounded_file_log(path)
        result = subprocess.run([scanner, "stdin", "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
                                input=value, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=30, check=False)
        if result.returncode:
            raise PackageFailure("scan")


def inside(root, tools, environment):
    containment.assert_contained()
    client.private_directory(root)
    import appimage_notices as notices
    import appimage_capture as capture
    progress = diagnostics.Progress(root)
    runner = OwnedRunner(root / "command-logs", progress=progress)
    try:
        progress.wrapper("build")
        artifact, appdir, record = build_unsigned(runner, root, tools=tools,
                                                 environment=environment, notices=notices)
        # Normal installed UI plus receiving peer; no test ELF may substitute
        # for the actual sender process or its loaded-library identity.
        progress.wrapper("capture")
        capture_environment = {**environment, "PATH": str(Path(tools["cargo"]).parent) + ":/usr/bin:/bin",
                               "CARGO_TARGET_DIR": str(ROOT / "target"),
                               "MORPHEUS_PIPEWIRE_CLIENT_VARIANT": "appimage",
                               "MORPHEUS_PIPEWIRE_CLIENT_RECEIPT": str(root / "stage/build-client.txt")}
        evidence = capture.run(runner, root, artifact, appdir, record, capture_environment, diagnostics=progress)
        if evidence.get("installed_capture_passed") is not True:
            raise PackageFailure("capture")
        record["installed_capture"] = evidence
        record["installed_capture_passed"] = True
        private_receipt(root / "capture-receipt.json", record)
        progress.wrapper("complete")
    except BaseException as error:
        progress.failed(error)
        raise


def cleanup_owned_fixture(root, environment):
    if not (root / "fixture-owned.json").exists():
        return
    if client.read_regular(root / "fixture-owned.json", 64) != b'{"owned":true}\n':
        raise PackageFailure("cleanup")
    command = containment.namespace_command(
        [str(Path(sys.executable).resolve()), str(Path(__file__).resolve()), "--cleanup", str(root)],
        root / "cleanup-namespace", environment)
    descriptor = os.open(root / "cleanup.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    process = None
    try:
        with os.fdopen(descriptor, "wb") as output:
            process = subprocess.Popen(command, cwd=ROOT, env=environment, stdin=subprocess.DEVNULL,
                                       stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        if containment.wait_owned(process, 320):
            raise PackageFailure("cleanup")
    finally:
        if process is not None:
            containment.stop_owned(process)


def contained(root, scanner, tools, environment):
    global DIAGNOSTICS_PUBLISHED
    diagnostics.Progress(root, outer=True)
    command = containment.namespace_command(
        [str(Path(sys.executable).resolve()), str(Path(__file__).resolve()),
         "--inside", str(root), "--cargo", tools["cargo"]], root / "namespace", environment)
    process = None
    failed = None
    outer_reason = "unknown"
    cleanup_passed = True
    scan_passed = False
    log = root / "namespace.log"
    descriptor = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        # No caller-controlled output goes to the terminal. The namespace
        # contains even detached AppImage/native descendants after failure.
        with os.fdopen(descriptor, "wb") as output:
            process = subprocess.Popen(command, cwd=ROOT, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=True)
        deadline = time.monotonic() + 7200
        while (code := containment.status(process)) is None:
            if time.monotonic() >= deadline:
                outer_reason = "namespace_deadline"
                raise PackageFailure("capture")
            if log.stat().st_size > 8 * 1024 * 1024:
                outer_reason = "namespace_output"
                raise PackageFailure("capture")
            time.sleep(0.1)
        if code:
            outer_reason = "namespace_exit"
            raise PackageFailure("capture")
    except BaseException as error:
        failed = error
        if outer_reason == "unknown":
            outer_reason = diagnostics.reason(error)
    finally:
        if process is not None:
            try:
                containment.stop_owned(process)
            except BaseException:
                cleanup_passed = False
                if failed is None:
                    failed = PackageFailure("cleanup")
                    outer_reason = "cleanup"
        try:
            cleanup_owned_fixture(root, environment)
        except BaseException:
            cleanup_passed = False
            if failed is None:
                failed = PackageFailure("cleanup")
                outer_reason = "cleanup"
        try:
            scan_private_logs(root, scanner)
            scan_passed = True
        except BaseException:
            if failed is None:
                failed = PackageFailure("scan")
    if failed is None:
        try:
            receipt = json.loads(client.read_regular(root / "capture-receipt.json", 1024 * 1024))
            if receipt.get("installed_capture_passed") is not True:
                raise PackageFailure("capture")
        except BaseException as error:
            failed = error
            outer_reason = diagnostics.reason(error)
    if failed is not None:
        # Reduction validates the entire bounded schema after all private logs
        # were scanned; arbitrary exceptions, paths and server text stay private.
        facts = diagnostics.public_failure(root, scan_passed=scan_passed,
                    outer_cleanup="passed" if cleanup_passed else "failed", outer_reason=outer_reason)
        print("MORPHEUS_APPIMAGE_RESULT " + json.dumps(facts, sort_keys=True), file=sys.stderr)
        DIAGNOSTICS_PUBLISHED = True
        raise failed
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-readiness", action="store_true")
    parser.add_argument("--gitleaks")
    parser.add_argument("--inside", type=Path)
    parser.add_argument("--cargo")
    parser.add_argument("--cleanup", type=Path)
    args = parser.parse_args()
    # Source enablement is required before any download or build. It does not
    # attest that this candidate has passed actual installed capture.
    require_readiness()
    if args.check_readiness:
        print("AppImage installed capture adapter is ready")
        return
    public_hosted_environment(os.environ)
    environment = controlled_environment(os.environ)
    if args.cleanup:
        # Source/diagnostic failures must never prevent owned-service cleanup.
        # This path retains hosted, containment and private-directory guards.
        containment.assert_contained()
        client.private_directory(args.cleanup)
        import appimage_capture as capture
        return capture.cleanup(OwnedRunner(args.cleanup / "cleanup-command-logs"), args.cleanup, environment)
    try:
        verify_checkout(ROOT, expected_revision(os.environ))
    except SourceRevisionError:
        raise PackageFailure("identity") from None
    if args.inside:
        containment.assert_contained()
        tools = {"cargo": owned_tool(args.cargo), "curl": owned_tool("/usr/bin/curl", root_owned=True),
                 "readelf": owned_tool("/usr/bin/readelf", root_owned=True)}
        return inside(args.inside, tools, environment)
    if args.cargo or not args.gitleaks:
        raise PackageFailure("configuration")
    tools = command_tools(os.environ)
    scanner = owned_tool(args.gitleaks)
    with tempfile.TemporaryDirectory(prefix="morpheus-appimage-capture-", dir=os.environ.get("RUNNER_TEMP")) as directory:
        root = Path(directory).resolve()
        root.chmod(0o700)
        containment.containment_check(root / "containment-probe")
        receipt = contained(root, scanner, tools, environment)
        # A private capture candidate can have unresolved static runtime source
        # obligations. It is never uploaded; this is not a release invocation.
        print(json.dumps({"format": FORMAT, "installed_capture_passed": True,
                          "distribution_ready": receipt.get("distribution_ready") is True,
                          "appimage_sha256": receipt["appimage_sha256"]}, sort_keys=True))


if __name__ == "__main__":
    def interrupted(_signal, _frame):
        raise PackageFailure("cleanup")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except BaseException:
        if not DIAGNOSTICS_PUBLISHED:
            print('MORPHEUS_APPIMAGE_RESULT {"diagnostics":"withheld","passed":false}', file=sys.stderr)
        raise SystemExit(1)
