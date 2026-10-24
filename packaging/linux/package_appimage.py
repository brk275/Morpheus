#!/usr/bin/env python3
"""Private Linux-only fixed-client AppImage wrapper; enablement is gated.

Run only after the installed normal-app capture adapter has been implemented and
reviewed. No runtime application override or capture diagnostic CLI is added.
"""

from __future__ import annotations

import argparse
import base64
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
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(ROOT / "scripts"))
import appimage_client as client
import appimage_runtime_build as runtime_builder
from appimage_process import OwnedRunner, download_package_input as download
import appimage_diagnostics as diagnostics
import element_process_namespace as containment
from package import package_config
from prepare_appimage_update import prepare_appimage, validate_metadata
from package_revision import expected_revision, verify_checkout, verify_versioned_export, UPDATE_VERSION, SourceRevisionError


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
               "GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_SHA", "GITHUB_ACTOR",
               "GITHUB_TRIGGERING_ACTOR", "MORPHEUS_PACKAGE_SOURCE_REVISION",
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
            runner, url=client.tool_download_url(pin), maximum=pin["size"], expected_size=pin["size"],
            expected_sha256=pin["sha256"], root=root, environment=environment,
            stage="download-linuxdeploy" if pin["cache_name"] == "linuxdeploy-x86_64.AppImage" else "download-appimage-plugin")
    archives = {}
    for key, (name, digest, maximum) in client.R.ARCHIVES.items():
        archives[key] = download(runner, url=client.R.BASE_URL + name, maximum=maximum,
                                 expected_sha256=digest, root=root, environment=environment, stage="download-pipewire-" + key)
    patch = download(runner, url="https://github.com/PipeWire/pipewire/commit/" + client.R.COMMIT + ".patch",
                     maximum=32768, expected_sha256=client.R.UPSTREAM_PATCH_SHA256,
                     root=root, environment=environment, stage="download-pipewire-patch")
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


def build_unsigned(runner, root, *, tools, environment, notices, target=None, update=None):
    """Complete build stage; final capture gate is called separately by owner.

    ``notices`` is the reviewed inventory adapter. prepare must provide verified
    source/copyright files for all possible deployed dependencies before packing;
    verify rejects every final ELF component lacking matching source provenance
    and notices. It must not classify licenses by filename alone.
    """
    if update is not None:
        validate_update_parameters(update)
    check_versions(runner, tools, root, environment)
    payloads, archives, patch = download_inputs(runner, root, environment)
    runtime, runtime_metadata, runtime_notices = runtime_builder.prepare(runner, root, environment)
    package_environment = {**environment, **client.seed_tools(root, payloads, runtime)}
    package_environment["PATH"] = str(Path(tools["cargo"]).parent) + ":/usr/bin:/bin"
    stage, source_metadata = client.prepare_client(root, archives, patch, runner.run, variant="appimage")
    # Reuse the ordinary runner cache. Build output is not an installer or a
    # profile; only the fresh packaging/output directories must be private.
    target = ROOT / "target" if target is None else Path(target)
    if not target.is_absolute() or target.resolve() != target:
        raise PackageFailure("output")
    if target.exists() and (target.is_symlink() or target.resolve() != target
                            or target.stat().st_uid != os.getuid()):
        raise PackageFailure("output")
    receipt = client.build_receipt(stage, source_metadata, variant="appimage")
    native_environment = {**package_environment, "CARGO_TARGET_DIR": str(target),
                          "MORPHEUS_PIPEWIRE_CLIENT_VARIANT": "appimage",
                          "MORPHEUS_PIPEWIRE_CLIENT_RECEIPT": str(receipt)}
    if update is not None:
        native_environment.update(MORPHEUS_UPDATE_FEED=update["feed"], MORPHEUS_UPDATE_PUBLIC_KEY=update["public_key"])
    runner.run("native-build", [tools["cargo"], "build", "--release", "--locked", "-p", "morpheus-app",
               "--features", "native-calls"], cwd=ROOT, env=native_environment, timeout=3600,
               output_limit=8 * 1024 * 1024)
    binary = target / "release/morpheus-app"
    binary_bytes = client.read_regular(binary, MAX_APPIMAGE)
    if not binary_bytes.startswith(b"\x7fELF\x02\x01"):
        raise PackageFailure("identity")
    config = client.appimage_config(package_config(ROOT, "linux"), stage)
    if update is not None:
        config["version"] = update["version"]
    for path in runtime_notices.iterdir():
        destination = runtime_builder.NOTICE_DIR + "/" + path.name
        if destination in config["appimage"]["files"].values():
            raise PackageFailure("notices")
        config["appimage"]["files"][str(path)] = destination
    # Native build was explicitly owned/bounded above. Avoid spawning an
    # unbounded shell build inside Cargo Packager's before hook.
    config.pop("before-packaging-command", None)
    if any(name in config for name in ("before-each-package-command", "after-packaging-command")):
        raise PackageFailure("configuration")
    config["binaries-dir"] = str(target / "release")
    output = root / "packages"
    output.mkdir(mode=0o700)
    config["out-dir"] = str(output)
    notice_record = notices.prepare(runner, root, binary, stage, source_metadata, config, package_environment,
                                    runtime_metadata=runtime_metadata)
    runner.run("package", [tools["cargo"], "packager", "--release", "--formats", FORMAT,
               "--config", json.dumps(config)], cwd=ROOT, env=package_environment, timeout=900,
               output_limit=8 * 1024 * 1024)
    tool_receipts = client.verify_tools_after(root, payloads, runtime)
    artifact = exact_artifact(output)
    embedded_runtime = runtime_builder.verify_embedded(artifact, runtime)
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
              "source_built_runtime": runtime_metadata, "embedded_runtime": embedded_runtime,
              "installed_capture_passed": False, "distribution_ready": inventory.get("distribution_ready") is True,
              "system_capture_compatibility": "separate_unmodified_gate"}
    if update is not None:
        if metadata["version"] != update["version"]:
            raise PackageFailure("identity")
        record["source_variant"] = update["source_proof"]
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
    # An independently specified default configuration and empty ignore file
    # prevent checkout config, annotations or ambient ignores weakening this gate.
    with tempfile.TemporaryDirectory(prefix="default-scan-", dir=root) as temporary:
        scan = Path(temporary)
        client.write_new(scan / "gitleaks.toml", b"[extend]\nuseDefault = true\n")
        client.write_new(scan / "empty.ignore", b"")
        failed = False
        for path in sorted(root.rglob("*.log")):
            try:
                value = bounded_file_log(path)
                result = subprocess.run([scanner, "stdin", "--config", str(scan / "gitleaks.toml"),
                                        "--gitleaks-ignore-path", str(scan / "empty.ignore"),
                                        "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
                                        input=value, cwd=scan, stdout=subprocess.DEVNULL,
                                        stderr=subprocess.DEVNULL, timeout=30, check=False)
                if result.returncode:
                    failed = True
            except (PackageFailure, OSError, subprocess.SubprocessError):
                # Retained data after failed child cleanup must not prevent
                # attempts to scan later actual command logs. Every invalid
                # entry or scanner failure still makes the complete gate fail.
                failed = True
        if failed:
            raise PackageFailure("scan")


class NativeSourceCache:
    """The outer owner's anchored identity for one consumed Cargo cache.

    These non-inherited descriptors prevent inode reuse or a child-supplied
    marker from authorizing deletion. Command logs live beside this cache and
    are never removed by discard(). The caller must first drain all children.
    """
    def __init__(self, root):
        self.root = client.private_directory(root)
        self.descriptors = []
        try:
            flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
            self.root_fd = os.open(self.root, flags)
            self.descriptors.append(self.root_fd)
            self.check(self.root.lstat(), os.fstat(self.root_fd))
            os.mkdir("native-source-inputs", mode=0o700, dir_fd=self.root_fd)
            self.work_fd = os.open("native-source-inputs", flags, dir_fd=self.root_fd)
            self.descriptors.append(self.work_fd)
            os.mkdir("cargo", mode=0o700, dir_fd=self.work_fd)
            self.cache_fd = os.open("cargo", flags, dir_fd=self.work_fd)
            self.descriptors.append(self.cache_fd)
        except BaseException:
            self.close()
            raise

    @staticmethod
    def check(current, anchor):
        if (not stat.S_ISDIR(current.st_mode) or current.st_uid != os.getuid()
                or current.st_mode & 0o077
                or (current.st_dev, current.st_ino) != (anchor.st_dev, anchor.st_ino)):
            raise PackageFailure("cleanup")

    def discard(self):
        if not shutil.rmtree.avoids_symlink_attacks:
            raise PackageFailure("cleanup")
        self.check(self.root.lstat(), os.fstat(self.root_fd))
        self.check(os.stat("native-source-inputs", dir_fd=self.root_fd, follow_symlinks=False),
                   os.fstat(self.work_fd))
        self.check(os.stat("cargo", dir_fd=self.work_fd, follow_symlinks=False),
                   os.fstat(self.cache_fd))
        # Python's descriptor-relative, symlink-resistant implementation checks
        # each traversal. Only the exact consumed cache is removed; all command
        # and containment logs retain the full strict independent-default scan.
        shutil.rmtree("cargo", dir_fd=self.work_fd)
        try:
            os.stat("cargo", dir_fd=self.work_fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        raise PackageFailure("cleanup")

    def close(self):
        failed = False
        while self.descriptors:
            try:
                os.close(self.descriptors.pop())
            except OSError:
                failed = True
        if failed:
            raise PackageFailure("cleanup")


def assemble_native_sources(root, artifact, appdir, environment, *, progress=None):
    """Use this build's still-owned inputs; never rebuild, upload or retain them.

    The native package route was already explicitly selected. Complete Cargo
    sources use the existing Cargo-fetch-only acquisition, in this namespace and
    a new private cache. Source-only selection never reaches this function.
    """
    import corresponding_source as sources
    import source_bundle_qa as acquisition
    containment.assert_contained()
    acquisition.guard_native(environment)
    root = client.private_directory(root)
    if artifact != exact_artifact(root / "packages") or appdir != root / "extracted/squashfs-root":
        raise PackageFailure("identity")
    if appdir.resolve(strict=True) != appdir:
        raise PackageFailure("identity")
    # The outer owner created and anchored only the cache before spawning.
    # Reuse that private layout; never replace a preexisting identity marker.
    work = client.private_directory(root / "native-source-inputs")
    client.private_directory(work / "cargo")
    for name in ("home", "fetch", "logs"):
        (work / name).mkdir(mode=0o700)
    cargo, rustc = acquisition.source_tools(environment)
    controlled = acquisition.environment_for_acquisition(environment, work, cargo, rustc)
    acquisition.acquire_inputs(work, cargo, rustc, controlled, progress=progress)
    plan = sources.unique_json(sources.read_owned(work / "plan.json", 65536, private=True))
    sources.require(type(plan) is dict and set(plan) == {"schema", "registry_cache", "git_mirrors"}
                    and type(plan["schema"]) is int and plan["schema"] == 1, "input")
    plan.update(schema=2, native_inputs={
        "coverage": "known-components", "package_artifact": str(artifact),
        "package_receipt": str(root / "build-receipt.json"),
        "runtime_notices": str(appdir / sources.RUNTIME_DOC),
        "pipewire_notices": str(appdir / sources.PIPEWIRE_DOC),
    })
    inputs = root / "native-source-inputs.json"
    client.write_new(inputs, json.dumps(plan, sort_keys=True).encode() + b"\n")
    receipt = sources.build(OwnedRunner(root / "native-source-commands", progress=progress), root, ROOT,
                            expected_revision(environment), owned_tool("/usr/bin/git", root_owned=True),
                            environment, inputs)
    private_receipt(root / "source-receipt.json", receipt)
    return receipt


def verify_native_sources(root, receipt, environment):
    """After namespace exit/scanning, bind closed evidence to the exact owned bytes."""
    import corresponding_source as sources
    revision = expected_revision(environment)
    actual = sources.unique_json(sources.read_owned(root / "source-receipt.json", 32768, private=True))
    _, artifact_hash, _ = sources.hash_owned_file(exact_artifact(root / "packages"), MAX_APPIMAGE)
    _, build_hash, _ = sources.hash_owned_file(root / "build-receipt.json", sources.MAX_NATIVE_RECEIPT, private=True)
    sources.require(artifact_hash == receipt.get("appimage_sha256"), "identity")
    sources.validate_receipt(actual, revision, native_artifact=artifact_hash, native_receipt=build_hash)
    _, bundle_hash, _ = sources.hash_owned_file(root / sources.BUNDLE_NAME, sources.MAX_BUNDLE, private=True)
    sources.require(bundle_hash == actual["bundle_sha256"], "identity")
    return actual


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
        # Source assembly consumes this exact installed-QA build before the
        # namespace or notice inputs are removed. No source plan may select a
        # second native build, an alternative artifact or a replacement receipt.
        progress.wrapper("source")
        progress.command("none")
        assemble_native_sources(root, artifact, appdir, environment, progress=progress)
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
    source_cache = NativeSourceCache(root)
    process = None
    failed = None
    outer_reason = "unknown"
    cleanup_passed = True
    scan_passed = False
    log = root / "namespace.log"
    namespace_stopped = True
    try:
        command = containment.namespace_command(
            [str(Path(sys.executable).resolve()), str(Path(__file__).resolve()),
             "--inside", str(root), "--cargo", tools["cargo"]], root / "namespace", environment)
        descriptor = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        # No caller-controlled output goes to the terminal. The namespace
        # contains even detached AppImage/native descendants after failure.
        with os.fdopen(descriptor, "wb") as output:
            process = subprocess.Popen(command, cwd=ROOT, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            namespace_stopped = False
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
                namespace_stopped = True
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
            if namespace_stopped and cleanup_passed:
                source_cache.discard()
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
        finally:
            try:
                source_cache.close()
            except BaseException:
                cleanup_passed = False
                if failed is None:
                    failed = PackageFailure("cleanup")
                    outer_reason = "cleanup"
    if failed is None:
        try:
            receipt = json.loads(client.read_regular(root / "capture-receipt.json", 1024 * 1024))
            if receipt.get("installed_capture_passed") is not True:
                raise PackageFailure("capture")
            receipt["corresponding_source"] = verify_native_sources(root, receipt, environment)
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


def validate_update_parameters(update):
    """Bound public compile inputs; neither signer keys nor general config enter."""
    if (not isinstance(update, dict) or set(update) != {"version", "feed", "public_key", "source_proof"}
            or type(update["version"]) is not str or len(update["version"]) > 128
            or not UPDATE_VERSION.fullmatch(update["version"])):
        raise PackageFailure("configuration")
    feed, key = update["feed"], update["public_key"]
    if type(feed) is not str or len(feed) > 128 or type(key) is not str or not 1 <= len(key) <= 4096:
        raise PackageFailure("configuration")
    try:
        url = urlsplit(feed)
        if (url.scheme != "https" or url.hostname != "127.0.0.1" or url.username is not None
                or url.password is not None or url.path != "/feed.json" or url.query or url.fragment
                or url.port is None or not 1024 <= url.port <= 65535
                or url.netloc != "127.0.0.1:" + str(url.port)):
            raise ValueError()
        lines = base64.b64decode(key, validate=True).decode("ascii").splitlines()
        if (len(lines) != 2 or not lines[0].startswith("untrusted comment: minisign public key ")
                or len(lines[0]) > 256 or any(ord(c) < 32 or ord(c) > 126 for c in lines[0])):
            raise ValueError()
        raw = base64.b64decode(lines[1], validate=True)
        if len(raw) != 42 or raw[:2] not in (b"Ed", b"ED"):
            raise ValueError()
    except (ValueError, UnicodeError):
        raise PackageFailure("configuration") from None
    proof = update["source_proof"]
    if (not isinstance(proof, dict) or proof.get("variant") != "updateqa-version-only"
            or proof.get("version") != update["version"]
            or type(proof.get("inventory_sha256")) is not str
            or not re.fullmatch(r"[0-9a-f]{64}", proof["inventory_sha256"])):
        raise PackageFailure("identity")


def update_build(plan_path, scanner):
    """Private build-only capability for the unchanged installed update gate.

    Ordinary packaging never enters this path. It is not an installed-capture
    pass, signer, release path or permission to execute an arbitrary export.
    """
    public_hosted_environment(os.environ)
    containment.assert_contained()
    if os.environ.get("MORPHEUS_FREE_HOSTED_UPDATE_QA") != "1":
        raise PackageFailure("ineligible")
    event = json.loads(client.read_regular(Path(os.environ["GITHUB_EVENT_PATH"]), 1024 * 1024))
    owner = event.get("repository", {}).get("owner", {})
    sender = event.get("sender", {})
    if (owner.get("login") != "brk275" or sender.get("login") != "brk275"
            or type(owner.get("id")) is not int or not 0 < owner["id"] < 2 ** 63
            or type(sender.get("id")) is not int or sender.get("id") != owner["id"]
            or os.environ.get("GITHUB_ACTOR") != "brk275"
            or os.environ.get("GITHUB_TRIGGERING_ACTOR") != "brk275"
            or os.environ.get("GITHUB_ACTOR_ID") != str(owner["id"])):
        raise PackageFailure("ineligible")
    if any(name in os.environ for name in ("CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD",
            "MORPHEUS_UPDATE_FEED", "MORPHEUS_UPDATE_PUBLIC_KEY", "LD_PRELOAD", "LD_LIBRARY_PATH")):
        raise PackageFailure("configuration")
    plan_path = Path(plan_path)
    details = plan_path.lstat()
    if (not stat.S_ISREG(details.st_mode) or details.st_uid != os.getuid() or details.st_mode & 0o077
            or plan_path.resolve() != plan_path):
        raise PackageFailure("configuration")
    plan = json.loads(client.read_regular(plan_path, 32768))
    keys = {"schema", "reviewed_checkout", "revision", "target", "version", "feed", "public_key"}
    if (not isinstance(plan, dict) or set(plan) != keys or type(plan["schema"]) is not int or plan["schema"] != 1
            or any(type(plan[name]) is not str for name in keys - {"schema"})
            or any(len(plan[name]) > 4096 for name in ("reviewed_checkout", "target", "public_key"))
            or plan["revision"] != expected_revision(os.environ)):
        raise PackageFailure("identity")
    root = client.private_directory(plan_path.parent)
    if plan_path.name != "update-plan.json":
        raise PackageFailure("configuration")
    reference = Path(plan["reviewed_checkout"])
    target = Path(plan["target"])
    if (not reference.is_absolute() or reference.resolve() != reference or not reference.is_dir()
            or reference == ROOT or ROOT in reference.parents or reference in ROOT.parents
            or not target.is_absolute() or target.resolve() != target or not target.is_dir()
            or target == ROOT or ROOT in target.parents or target in ROOT.parents
            or target.stat().st_uid != os.getuid() or target.stat().st_mode & 0o022):
        raise PackageFailure("configuration")
    source_details = ROOT.lstat()
    if ROOT.resolve() != ROOT or not stat.S_ISDIR(source_details.st_mode) or source_details.st_uid != os.getuid() or source_details.st_mode & 0o077:
        raise PackageFailure("identity")
    environment = controlled_environment(os.environ)
    tools = command_tools(os.environ)
    runner = OwnedRunner(root / "command-logs")
    git = client.R.trusted_tool("git")
    git_env = {**environment, "PATH": "/usr/bin:/bin", "GIT_CONFIG_GLOBAL": "/dev/null",
               "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0", "GIT_OPTIONAL_LOCKS": "0"}
    safe_git = [git, "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
                "-c", "gc.auto=0", "-c", "maintenance.auto=false"]
    try:
        head = runner.run("source-head", [*safe_git, "rev-parse", "--verify", "HEAD^{commit}"],
                          cwd=reference, env=git_env, timeout=20, output_limit=128)
        if head != (plan["revision"] + "\n").encode("ascii"):
            raise PackageFailure("identity")
        clean = runner.run("source-clean", [*safe_git, "diff", "--no-ext-diff", "--no-textconv", "--quiet", "HEAD", "--"],
                           cwd=reference, env=git_env, timeout=20, output_limit=128)
        if clean:
            raise PackageFailure("identity")
        archive = runner.run("source-archive", [*safe_git, "archive", "--format=tar", plan["revision"]],
                             cwd=reference, env=git_env, timeout=60, output_limit=64 * 1024 * 1024, binary_output=True)
        proof = verify_versioned_export(ROOT, archive, plan["version"])
        update = {name: plan[name] for name in ("version", "feed", "public_key")}
        update["source_proof"] = proof
        validate_update_parameters(update)
        import appimage_notices as notices
        # Same original-SONAME client, fixed static runtime, notices/deployment
        # and embedded-byte checks as the normal canonical builder.
        build_unsigned(runner, root, tools=tools, environment=environment, notices=notices, target=target, update=update)
    finally:
        # Build-only is not scan-only: all child groups are already reaped by
        # OwnedRunner, and private output cannot bypass the independent scan.
        scan_private_logs(root, owned_tool(scanner))
    print('MORPHEUS_APPIMAGE_UPDATE_BUILD {"built":true,"installed_capture_passed":false}')


def qa_output_path(value):
    """Only the generic caller's original private dist may retain a QA image."""
    path = Path(value)
    expected = ROOT / "dist"
    if path != expected or path.resolve() != expected:
        raise PackageFailure("output")
    if not path.exists():
        path.mkdir(mode=0o700)
    details = path.lstat()
    if (not stat.S_ISDIR(details.st_mode) or details.st_uid != os.getuid()
            or details.st_mode & 0o077 or any(path.iterdir())):
        raise PackageFailure("output")
    return path


def qa_image_bytes(root, receipt):
    data = client.read_regular(exact_artifact(root / "packages"), MAX_APPIMAGE)
    if receipt.get("installed_capture_passed") is not True or hashlib.sha256(data).hexdigest() != receipt.get("appimage_sha256"):
        raise PackageFailure("identity")
    return data


def retain_qa_image(output, data, receipt):
    """Publish one private QA image atomically, without replacing any entry."""
    if not 64 < len(data) <= MAX_APPIMAGE or hashlib.sha256(data).hexdigest() != receipt.get("appimage_sha256") or receipt.get("installed_capture_passed") is not True:
        raise PackageFailure("identity")
    directory = os.open(output, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    name = ".morpheus-qa-" + os.urandom(16).hex()
    created = False
    published = False
    try:
        details = os.fstat(directory)
        if (details.st_uid != os.getuid() or details.st_mode & 0o077 or os.listdir(directory)):
            raise PackageFailure("output")
        descriptor = os.open(name, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=directory)
        created = True
        with os.fdopen(descriptor, "w+b") as stream:
            if stream.write(data) != len(data):
                raise PackageFailure("output")
            stream.flush()
            os.fsync(stream.fileno())
            stream.seek(0)
            if hashlib.sha256(stream.read(MAX_APPIMAGE + 1)).hexdigest() != receipt["appimage_sha256"]:
                raise PackageFailure("identity")
            os.fchmod(stream.fileno(), 0o700)
            identity = os.fstat(stream.fileno())
            if identity.st_size != len(data):
                raise PackageFailure("identity")
            # Same-directory link is atomic and refuses an existing name.
            os.link(name, "morpheus-qa.AppImage", src_dir_fd=directory, dst_dir_fd=directory, follow_symlinks=False)
            published = True
        return (identity.st_dev, identity.st_ino, len(data))
    except BaseException:
        if published:
            os.unlink("morpheus-qa.AppImage", dir_fd=directory)
            published = False
        raise
    finally:
        try:
            if created:
                os.unlink(name, dir_fd=directory)
        except BaseException:
            if published:
                os.unlink("morpheus-qa.AppImage", dir_fd=directory)
            raise
        finally:
            os.close(directory)


def remove_qa_image(output, identity):
    path = output / "morpheus-qa.AppImage"
    details = path.lstat()
    if (details.st_dev, details.st_ino, details.st_size) != identity or not stat.S_ISREG(details.st_mode):
        raise PackageFailure("cleanup")
    path.unlink()


def source_bundle_mode(args):
    """Explicit source evidence only; ordinary package/check/capture routes stay separate."""
    import corresponding_source as sources
    if (args.check_readiness or args.inside or args.cleanup or args.cargo or args.qa_output
            or args.update_build_plan or args.source_bundle_inputs is None):
        raise PackageFailure("configuration")
    public_hosted_environment(os.environ)
    environment = controlled_environment(os.environ)
    try:
        verify_checkout(ROOT, expected_revision(os.environ))
    except SourceRevisionError:
        raise PackageFailure("identity") from None
    if args.source_bundle_inside is not None:
        if args.source_bundle_output is not None or args.gitleaks:
            raise PackageFailure("configuration")
        containment.assert_contained()
        root = client.private_directory(args.source_bundle_inside)
        # This fixed plan was copied by the outer owner before namespace entry.
        if args.source_bundle_inputs != root / "source-inputs.json":
            raise PackageFailure("configuration")
        receipt = sources.build(OwnedRunner(root / "source-command-logs"), root, ROOT,
                                expected_revision(os.environ),
                                owned_tool("/usr/bin/git", root_owned=True),
                                environment, args.source_bundle_inputs)
        private_receipt(root / "source-receipt.json", receipt)
        return
    if args.source_bundle_output is None or not args.gitleaks:
        raise PackageFailure("configuration")
    output = sources.output_directory(args.source_bundle_output, ROOT)
    inputs = sources.read_owned(args.source_bundle_inputs, 65536, private=True)
    scanner = owned_tool(args.gitleaks)
    data = None
    receipt = None
    with tempfile.TemporaryDirectory(prefix="morpheus-source-bundle-", dir=os.environ.get("RUNNER_TEMP")) as temporary:
        root = Path(temporary).resolve()
        root.chmod(0o700)
        client.write_new(root / "source-inputs.json", inputs)
        containment.containment_check(root / "source-probe")
        command = containment.namespace_command(
            [str(Path(sys.executable).resolve()), str(Path(__file__).resolve()),
             "--source-bundle-inside", str(root),
             "--source-bundle-inputs", str(root / "source-inputs.json")],
            root / "source-namespace", environment)
        process = None
        descriptor = os.open(root / "source-namespace.log",
                             os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            with os.fdopen(descriptor, "wb") as log:
                process = subprocess.Popen(command, cwd=ROOT, env=environment,
                                           stdin=subprocess.DEVNULL, stdout=log,
                                           stderr=subprocess.STDOUT, start_new_session=True)
            if containment.wait_owned(process, 1800):
                raise PackageFailure("output")
        finally:
            try:
                if process is not None:
                    containment.stop_owned(process)
            finally:
                scan_private_logs(root, scanner)
        receipt = sources.unique_json(sources.read_owned(root / "source-receipt.json", 32768, private=True))
        sources.validate_receipt(receipt, expected_revision(os.environ))
        data = sources.read_owned(root / sources.BUNDLE_NAME, sources.MAX_BUNDLE, private=True)
        sources.require(sources.digest(data) == receipt["bundle_sha256"], "identity")
    # The namespace, child groups, log scan and temporary tree have completed
    # before any source artifact can leave the private owner root.
    handed = sources.publish(output, data, receipt)
    try:
        print(json.dumps({"check": "corresponding_source_bundle", **receipt}, sort_keys=True), flush=True)
    except BaseException:
        sources.remove_published(output, handed)
        raise


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-readiness", action="store_true")
    parser.add_argument("--gitleaks")
    parser.add_argument("--inside", type=Path)
    parser.add_argument("--cargo")
    parser.add_argument("--cleanup", type=Path)
    parser.add_argument("--update-build-plan", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--qa-output", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--source-bundle-inputs", type=Path,
                        help="Explicit offline registry-cache/Git-mirror plan; no downloads or hooks")
    parser.add_argument("--source-bundle-output", type=Path,
                        help="Existing empty private output directory for source evidence only")
    parser.add_argument("--source-bundle-inside", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if any(value is not None for value in
           (args.source_bundle_inputs, args.source_bundle_output, args.source_bundle_inside)):
        return source_bundle_mode(args)
    # Source enablement is required before any download or build. It does not
    # attest that this candidate has passed actual installed capture.
    require_readiness()
    if args.check_readiness:
        print("AppImage installed capture adapter is ready")
        return
    public_hosted_environment(os.environ)
    environment = controlled_environment(os.environ)
    if args.update_build_plan:
        if not args.gitleaks or args.inside or args.cleanup or args.cargo or args.qa_output:
            raise PackageFailure("configuration")
        return update_build(args.update_build_plan, args.gitleaks)
    if args.qa_output and (args.cleanup or args.inside or args.cargo):
        raise PackageFailure("configuration")
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
    # Every native entry, including the fixed internal worker, must retain the
    # selected-owner guard before it can start downloads or native commands.
    import source_bundle_qa as acquisition
    acquisition.guard_native(environment)
    if args.inside:
        containment.assert_contained()
        tools = {"cargo": owned_tool(args.cargo), "curl": owned_tool("/usr/bin/curl", root_owned=True),
                 "readelf": owned_tool("/usr/bin/readelf", root_owned=True)}
        return inside(args.inside, tools, environment)
    if args.cargo or not args.gitleaks:
        raise PackageFailure("configuration")
    tools = command_tools(os.environ)
    scanner = owned_tool(args.gitleaks)
    output = qa_output_path(args.qa_output) if args.qa_output else None
    data = None
    with tempfile.TemporaryDirectory(prefix="morpheus-appimage-capture-", dir=os.environ.get("RUNNER_TEMP")) as directory:
        root = Path(directory).resolve()
        root.chmod(0o700)
        containment.containment_check(root / "containment-probe")
        receipt = contained(root, scanner, tools, environment)
        if output is not None:
            data = qa_image_bytes(root, receipt)
    # Handoff happens after namespace/service cleanup, scan and temporary-root
    # deletion. It remains private and is consumed by the original smoke gate.
    handed = None
    try:
        if output is not None:
            handed = retain_qa_image(output, data, receipt)
        print(json.dumps({"format": FORMAT, "installed_capture_passed": True,
                          "distribution_ready": receipt.get("distribution_ready") is True,
                          "appimage_sha256": receipt["appimage_sha256"],
                          "corresponding_source": receipt["corresponding_source"]}, sort_keys=True), flush=True)
    except BaseException:
        if handed is not None:
            remove_qa_image(output, handed)
        raise


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
