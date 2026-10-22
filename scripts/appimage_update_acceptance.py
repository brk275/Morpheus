#!/usr/bin/env python3
"""Explicit free-hosted Linux AppImage update QA; never a public release job.

Private keys, installers, exported source, profiles and logs stay in an external
0700 temporary directory. Only fixed stage/check codes leave the harness.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import re
import runpy
import shutil
import signal
import ssl
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import tomllib
import urllib.request
import uuid

from fixture_paths import external_path, private_directory, write_private
from package_revision import expected_revision, SourceRevisionError
from element_process_namespace import namespace_command, stop_owned, wait_owned

ROOT = Path(__file__).resolve().parents[1]
CA_DIRECTORY = Path("/usr/local/share/ca-certificates")
CA_UPDATE = "/usr/sbin/update-ca-certificates"
CASES = ("invalid_signature", "truncated_download", "discard", "signed_invalid_elf_rollback", "upgrade")
CHECKS = frozenset((*CASES, "cohort_equal", "https_system_trust", "official_signer", "private_cleanup"))
STAGES = frozenset(("preflight", "source_export", "key_generation", "certificate", "build_old", "build_new",
                    "signing", "containment", "log_scan", "cleanup", "complete", *CASES))
STATUS_PREFIX = "Application update status: "
AVAILABLE = "An application update is available to download."
STAGED = "The signed update is ready. Leave any call before restarting."
DOWNLOAD_FAILED = "The update could not be downloaded and verified."
IDLE = "Check for a signed application update."
FOOTER = b"\nMORPHEUS-APPIMAGE-METADATA-V1\n"
MAX_IMAGE = 512 * 1024 * 1024
FAILURES = frozenset(("acceptance", "command_failed", "command_deadline", "ui_deadline", "ui_action",
                      "log_scan", "cohort_mismatch", "artifact_identity", "case_evidence"))
OPERATIONS = frozenset(("command", "source_revision", "source_export", "signer_version", "key_generation",
                        "signing", "openssl", "certificate_install", "certificate_refresh", "certificate_remove",
                        "package_build", "metadata", "containment", "ui_case", "accessibility"))
SELECTORS = {
    "Open interactive preview without signing in": "preview", "Open settings": "settings",
    "Check for updates": "check", "Download update": "download", "Discard update": "discard",
    "Restart to update": "restart", STATUS_PREFIX + AVAILABLE: "available_status",
    STATUS_PREFIX + STAGED: "staged_status", STATUS_PREFIX + DOWNLOAD_FAILED: "download_error_status",
    STATUS_PREFIX + IDLE: "idle_status",
}
SELECTOR_CODES = frozenset((*SELECTORS.values(), "candidate_version", "unknown_selector"))


class QAError(RuntimeError):
    def __init__(self, reason="acceptance", *, operation=None, selector=None, exit_status=None, stage=None):
        self.reason = reason if reason in FAILURES else "acceptance"
        self.operation = operation if operation in OPERATIONS else None
        self.selector = selector if selector in SELECTOR_CODES else None
        self.exit_status = exit_status if type(exit_status) is int and -64 <= exit_status <= 255 and exit_status != 0 else None
        self.stage = stage if stage in STAGES else None
        super().__init__(self.reason)

    def facts(self):
        return {key: value for key, value in {"reason": self.reason, "operation": self.operation,
                "selector": self.selector, "exit_status": self.exit_status, "stage": self.stage}.items() if value is not None}


def child_failure(output):
    found = None
    for line in output.splitlines():
        if not line.startswith("MORPHEUS_APPIMAGE_QA_FAILURE "):
            continue
        try:
            value = json.loads(line.removeprefix("MORPHEUS_APPIMAGE_QA_FAILURE "))
            if isinstance(value, dict):
                found = QAError(**{key: item for key, item in value.items()
                                   if key in {"reason", "operation", "selector", "exit_status", "stage"}
                                   and (isinstance(item, str) or type(item) is int)})
        except (ValueError, TypeError):
            continue
    return found


def operation_code(args):
    name = Path(args[0]).name
    if name == "git":
        return "source_export" if args[1:2] == ["archive"] else "source_revision"
    if name == "cargo":
        if args[1:3] == ["packager", "--version"]:
            return "signer_version"
        if args[1:4] == ["packager", "signer", "generate"]:
            return "key_generation"
        if args[1:4] == ["packager", "signer", "sign"]:
            return "signing"
        return "package_build"
    if name == "openssl":
        return "openssl"
    if name == "gdbus":
        return "accessibility"
    if name == "morpheus-app":
        return "metadata"
    if name == "sudo":
        if "/usr/bin/unshare" in args:
            return "ui_case"
        for executable, code in (("/usr/bin/install", "certificate_install"),
                                 (CA_UPDATE, "certificate_refresh"), ("/usr/bin/rm", "certificate_remove")):
            if executable in args:
                return code
    if any(Path(arg).name == "element_process_namespace.py" for arg in args):
        return "containment"
    return "command"


def selector_code(value):
    if value.startswith("Available version: "):
        return "candidate_version"
    return SELECTORS.get(value, "unknown_selector")


def require(value, reason="acceptance"):
    if not value:
        raise QAError(reason)


def sha256(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def identity_record(revision, artifacts, invalid_hash, public_key_hash):
    require(re.fullmatch(r"[0-9a-f]{40}", revision) and len(artifacts) == 2)
    require(all(set(item) == {"version", "sha256", "persistence"} for item in artifacts))
    for index, item in enumerate(artifacts, 1):
        require(re.fullmatch(r"\d+\.\d+\.\d+-updateqa\." + str(index), item["version"]))
        require(all(re.fullmatch(r"[0-9a-f]{64}", item[key]) for key in ("sha256", "persistence")))
    require(artifacts[0]["persistence"] == artifacts[1]["persistence"])
    require(all(re.fullmatch(r"[0-9a-f]{64}", value) for value in (invalid_hash, public_key_hash)))
    return {"source_revision": revision, "signer": "cargo-packager-0.11.8",
            "runtime": "appimage_extract_and_run", "display": "x11_xvfb",
            "genuine_old": artifacts[0], "genuine_new": artifacts[1],
            "signed_invalid_elf_fixture_sha256": invalid_hash,
            "disposable_public_key_sha256": public_key_hash}


def hosted_guard(explicit):
    require(explicit and sys.platform == "linux" and platform.machine() == "x86_64")
    require(all(os.environ.get(key) == value for key, value in {
        "GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64",
        "RUNNER_ENVIRONMENT": "github-hosted", "MORPHEUS_FREE_HOSTED_UPDATE_QA": "1",
    }.items()))
    require(os.getuid() != 0 and os.getuid() == os.geteuid())


def app_versions(source):
    manifest = tomllib.loads((source / "crates/morpheus-app/Cargo.toml").read_text())
    version = manifest["package"]["version"]
    match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version)
    require(match is not None)
    return match[1] + "-updateqa.1", match[1] + "-updateqa.2"


def replace_app_version(source, version):
    """Only the two exclusions explicitly allowed by build.rs may vary."""
    from package_revision import UPDATE_VERSION_PATHS, versioned_source_bytes
    for name in sorted(UPDATE_VERSION_PATHS):
        path = source / name
        try:
            replacement = versioned_source_bytes(name, path.read_bytes(), version)
        except SourceRevisionError:
            raise QAError("artifact_identity", operation="source_revision") from None
        path.write_bytes(replacement)


def image_metadata(path):
    path = Path(path)
    require(path.is_file() and not path.is_symlink() and 64 < path.stat().st_size <= MAX_IMAGE)
    with path.open("rb") as source:
        header = source.read(64)
        require(header[:6] == b"\x7fELF\x02\x01" and header[8:11] == b"AI\x02" and header[18:20] == b"\x3e\x00")
        source.seek(-len(FOOTER) - 4, os.SEEK_END)
        tail = source.read()
        length = struct.unpack("<I", tail[:4])[0]
        require(tail[4:] == FOOTER and 0 < length <= 4096 and length + len(tail) + 64 < path.stat().st_size)
        source.seek(-len(tail) - length, os.SEEK_END)
        metadata = json.loads(source.read(length))
    require(set(metadata) == {"protocol", "product", "version", "arch", "persistence"})
    require(metadata["protocol"] == 1 and metadata["product"] == "io.github.brk275.morpheus"
            and metadata["arch"] == "x86_64" and re.fullmatch(r"[0-9a-f]{64}", metadata["persistence"]))
    return metadata


def invalid_elf_copy(source, destination):
    """Signed *negative* fixture: Linux refuses e_phentsize=0 before execution.

    The genuine newer package remains untouched. This is not a real candidate
    build or evidence for arbitrary failures after the trial app starts.
    """
    shutil.copyfile(source, destination)
    with Path(destination).open("r+b") as output:
        output.seek(54)  # ELF64 e_phentsize; load_elf_phdrs requires sizeof(Elf64_Phdr).
        output.write(b"\x00\x00")
    Path(destination).chmod(0o700)
    require(image_metadata(source) == image_metadata(destination))


class Commands:
    def __init__(self, root, env=None):
        self.root = private_directory(root)
        self.logs = private_directory(root / "logs")
        self.env = dict(os.environ) if env is None else dict(env)
        self.env.update({"RUST_LOG": "warn", "RUST_BACKTRACE": "0"})
        self.deadline = time.monotonic() + 6900
        self.sequence = 0
        self.stage = "preflight"

    def mark(self, stage):
        require(stage in STAGES)
        self.stage = stage
        print("MORPHEUS_APPIMAGE_QA_STAGE " + stage, flush=True)

    def run(self, args, *, env=None, cwd=None, seconds=180, operation=None):
        require(operation is None or operation in OPERATIONS)
        operation = operation or operation_code(args)
        self.sequence += 1
        timeout = min(seconds, self.deadline - time.monotonic())
        require(timeout > 0)
        log = self.logs / f"{self.sequence:03d}.log"
        fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as output:
            process = subprocess.Popen(args, cwd=cwd, env=self.env | (env or {}), stdin=subprocess.DEVNULL,
                                       stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            try:
                result = wait_owned(process, timeout)
            except subprocess.TimeoutExpired:
                raise QAError("command_deadline", operation=operation) from None
        finally:
            stop_owned(process)
        if result != 0:
            # Only an allowlisted child's fixed diagnostics may cross this
            # boundary; raw compiler/server/app messages remain private.
            with log.open("rb") as source:
                source.seek(max(0, log.stat().st_size - 65536))
                nested = child_failure(source.read().decode(errors="replace"))
            error = nested or QAError("command_failed", operation=operation)
            error.exit_status = result if -64 <= result <= 255 else None
            raise error
        return log.read_text(errors="replace")

    def scan(self, scanner):
        for path in sorted(self.root.rglob("*.log")):
            with path.open("rb") as source:
                result = subprocess.run([scanner, "stdin", "--redact=100", "--ignore-gitleaks-allow", "--no-banner"],
                                        stdin=source, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                        timeout=30, cwd=self.root)
            require(result.returncode == 0, "log_scan")


def export_source(commands, source):
    commands.mark("source_export")
    revision = commands.run(["git", "rev-parse", "HEAD"], cwd=source).strip()
    try:
        expected = expected_revision(os.environ)
    except SourceRevisionError:
        raise QAError("artifact_identity", operation="source_revision") from None
    if revision != expected:
        raise QAError("artifact_identity", operation="source_revision")
    archive = commands.root / "source.tar"
    commands.run(["git", "archive", "--format=tar", "--output=" + str(archive), revision], cwd=source)
    destination = private_directory(commands.root / "source")
    with tarfile.open(archive) as tar:
        for item in tar.getmembers():
            require(not item.name.startswith("/") and ".." not in Path(item.name).parts
                    and (item.isfile() or item.isdir()))
        tar.extractall(destination, filter="data")
    archive.unlink()
    commands.reviewed_source = Path(source).resolve()
    commands.reviewed_revision = revision
    return destination, revision


def generate_key(commands, name):
    key = commands.root / name
    commands.run(["cargo", "packager", "signer", "generate", "--ci", "--path", str(key)],
                 env={"CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD": ""})
    for path in (key, key.with_name(key.name + ".pub")):
        require(path.is_file() and not path.is_symlink())
        path.chmod(0o600)
    return key


def sign(commands, image, key):
    commands.run(["cargo", "packager", "signer", "sign", str(image)],
                 env={"CARGO_PACKAGER_SIGN_PRIVATE_KEY": str(key), "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD": ""})
    signature = image.with_name(image.name + ".sig")
    signature.chmod(0o600)
    text = signature.read_text().strip()
    require(0 < len(text) < 8192 and re.fullmatch(r"[A-Za-z0-9+/=]+", text))
    return text


class Certificate:
    def __init__(self, commands):
        self.commands = commands
        self.directory = private_directory(commands.root / "tls")
        self.trusted = CA_DIRECTORY / ("morpheus-update-" + uuid.uuid4().hex + ".crt")
        self.installed = False

    def create(self):
        root, run = self.directory, self.commands.run
        run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-subj", "/CN=Morpheus disposable update QA CA", "-keyout", str(root / "ca.key"), "-out", str(root / "ca.crt"),
             "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"])
        run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
             "-keyout", str(root / "server.key"), "-out", str(root / "server.csr")])
        write_private(root / "server.ext", "subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n"
                      "extendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\n", create=True)
        run(["openssl", "x509", "-req", "-in", str(root / "server.csr"), "-CA", str(root / "ca.crt"),
             "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-days", "1", "-out", str(root / "server.crt"),
             "-extfile", str(root / "server.ext")])
        for path in root.iterdir():
            path.chmod(0o600)
        require(not self.trusted.exists() and not self.trusted.is_symlink())
        # Install only this public certificate, never the disposable private key.
        self.installed = True  # Cleanup must also cover a command interrupted after copying.
        run(["/usr/bin/sudo", "--non-interactive", "/usr/bin/install", "-m", "644", str(root / "ca.crt"), str(self.trusted)])
        run(["/usr/bin/sudo", "--non-interactive", CA_UPDATE])

    def close(self):
        if self.installed:
            if self.trusted.exists():
                require(not self.trusted.is_symlink() and sha256(self.trusted) == sha256(self.directory / "ca.crt"))
                self.commands.run(["/usr/bin/sudo", "--non-interactive", "/usr/bin/rm", "--", str(self.trusted)])
            self.commands.run(["/usr/bin/sudo", "--non-interactive", CA_UPDATE])
            self.installed = False


class Feed:
    def __init__(self, certificate):
        self.image = None
        self.signature = self.version = self.mode = ""
        self.counts = {"feed": 0, "download": 0}
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                try:
                    if self.path == "/feed.json":
                        owner.counts["feed"] += 1
                        if owner.image is None:
                            self.send_response(204)
                            self.end_headers()
                            return
                        content = json.dumps({"version": owner.version, "url": owner.url + "/app.AppImage",
                                              "signature": owner.signature, "format": "appimage"}).encode()
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(content)))
                        self.end_headers()
                        self.wfile.write(content)
                    elif self.path == "/app.AppImage" and owner.image is not None:
                        owner.counts["download"] += 1
                        self.send_response(200)
                        self.send_header("Content-Length", str(owner.image.stat().st_size))
                        self.end_headers()
                        with owner.image.open("rb") as source:
                            if owner.mode == "truncated_download":
                                self.wfile.write(source.read(4096))
                                self.wfile.flush()
                                self.close_connection = True
                                return
                            shutil.copyfileobj(source, self.wfile, 256 * 1024)
                    else:
                        self.send_response(404)
                        self.end_headers()
                except (BrokenPipeError, ConnectionResetError, OSError):
                    self.close_connection = True

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def get_request(self):
                connection, address = super().get_request()
                connection.settimeout(10)
                try:
                    return self.tls.wrap_socket(connection, server_side=True), address
                except BaseException:
                    connection.close()
                    raise

            def handle_error(self, *_args):
                pass  # Raw TLS/request errors never reach public job output.

        self.server = Server(("127.0.0.1", 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        context.load_cert_chain(certificate.directory / "server.crt", certificate.directory / "server.key")
        self.server.tls = context
        self.url = f"https://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def select(self, case, image, signature, version):
        require(case in CASES)
        self.mode, self.image, self.signature, self.version = case, image, signature, version
        self.counts = {"feed": 0, "download": 0}

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def package_build_environment(environment):
    # Namespace control contains public runner/source context, not the broad
    # fixture environment or generated signer/private-feed inputs.
    allowed = {"PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME", "RUNNER_TEMP",
               "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS", "RUNNER_ARCH", "GITHUB_REPOSITORY",
               "GITHUB_REF", "GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_SHA", "GITHUB_ACTOR",
               "GITHUB_ACTOR_ID", "GITHUB_TRIGGERING_ACTOR", "MORPHEUS_PACKAGE_SOURCE_REVISION",
               "MORPHEUS_FREE_HOSTED_UPDATE_QA"}
    return {name: value for name, value in environment.items() if name in allowed}


def build_image(commands, source, target, version, feed, public_key):
    replace_app_version(source, version)
    build_root = private_directory(commands.root / "artifacts" / version)
    plan = {"schema": 1, "reviewed_checkout": str(commands.reviewed_source),
            "revision": commands.reviewed_revision, "target": str(target),
            "version": version, "feed": feed, "public_key": public_key}
    plan_path = build_root / "update-plan.json"
    write_private(plan_path, json.dumps(plan, sort_keys=True), create=True)
    # One existing owned namespace and the unchanged3300s per-image bound.
    # The canonical entry authenticates the whole exported source itself; only
    # these two exact version substitutions are allowed, with no signer env.
    environment = package_build_environment(commands.env)
    invocation = namespace_command(["/usr/bin/python3", "-I", "-B",
                  str(source / "packaging/linux/package_appimage.py"),
                  "--update-build-plan", str(plan_path), "--gitleaks", commands.gitleaks],
                  build_root / "namespace", environment)
    commands.run(invocation, cwd=source, seconds=3300, env=environment, operation="package_build")
    destination = build_root / "packages"
    helper = runpy.run_path(str(source / "scripts/prepare_appimage_update.py"))
    # Inspect the just-built local binary before any signing/download, using its
    # supported metadata-only entry point, never an untrusted downloaded probe.
    metadata = json.loads(commands.run([str(target / "release/morpheus-app"), "--morpheus-update-metadata"], seconds=20))
    helper["validate_metadata"](metadata)
    images = list(destination.glob("*.AppImage"))
    require(len(images) == 1 and metadata["version"] == version)
    helper["prepare_appimage"](images[0], metadata)
    require(image_metadata(images[0]) == metadata)
    images[0].chmod(0o700)
    return images[0], metadata


class NativeUI:
    def __init__(self, nonce):
        import pyatspi
        from gi.repository import GLib
        self.api = pyatspi
        self.transient_error = GLib.Error
        self.nonce = nonce.encode()
        pyatspi.setTimeout(1500, 1500)

    def owned(self, application):
        try:
            pid = application.get_process_id()
            values = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
            return b"MORPHEUS_UPDATE_ACCEPTANCE_NONCE=" + self.nonce in values
        except (OSError, AttributeError, self.transient_error):
            return False

    def find(self, expected, enabled=None):
        matches = []
        desktop = self.api.Registry.getDesktop(0)
        for application in desktop:
            if not self.owned(application):
                continue
            pending, count = [application], 0
            while pending:
                node = pending.pop()
                count += 1
                require(count <= 5000)
                try:
                    node.clearCache()
                    texts = [node.name]
                    with contextlib.suppress(NotImplementedError):
                        texts.append(node.queryText().getText(0, -1))
                    if expected in texts:
                        active = node.getState().contains(self.api.STATE_ENABLED)
                        if enabled is None or active == enabled:
                            matches.append((node, application.get_process_id()))
                    pending.extend(list(node))
                except (LookupError, self.transient_error):
                    continue  # An app can leave AT-SPI while restarting.
        require(len(matches) <= 1)
        return matches[0] if matches else None

    def wait(self, expected, *, enabled=None, seconds=150, previous_pid=None):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            found = self.find(expected, enabled)
            if found and (previous_pid is None or found[1] != previous_pid):
                return found
            time.sleep(0.1)
        raise QAError("ui_deadline", selector=selector_code(expected))

    def click(self, expected):
        node, pid = self.wait(expected, enabled=True)
        action = node.queryAction()
        if not (action.nActions > 0 and action.doAction(0)):
            raise QAError("ui_action", selector=selector_code(expected))
        return pid

    def settings(self, previous_pid=None):
        self.wait("Open interactive preview without signing in", previous_pid=previous_pid)
        self.click("Open interactive preview without signing in")
        self.click("Open settings")
        return self.wait("Check for updates", enabled=True)[1]


def ui_case(plan_path):
    hosted_guard(True)
    # This mode must be under the guarded, capability-free namespace wrapper.
    from element_process_namespace import read_control
    control = read_control(Path(os.environ["MORPHEUS_ELEMENT_NAMESPACE_CONTROL"]))
    require(os.readlink("/proc/self/ns/pid") != control["outer_namespace"])
    plan = read_control(plan_path)
    case, installed = plan["case"], Path(plan["installed"])
    require(case in CASES and sha256(installed) == plan["old_hash"])
    root = external_path(plan_path.parent)
    commands = Commands(root / "driver")
    for setting in ("IsEnabled", "ScreenReaderEnabled"):
        commands.run(["gdbus", "call", "--session", "--dest", "org.a11y.Bus", "--object-path", "/org/a11y/bus",
                      "--method", "org.freedesktop.DBus.Properties.Set", "org.a11y.Status", setting, "<true>"], seconds=20)
    env = dict(os.environ)
    for name in ("MORPHEUS_UPDATE_FEED", "MORPHEUS_UPDATE_PUBLIC_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY",
                 "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD", "MORPHEUS_QA_PROFILE_DIR", "SSL_CERT_FILE", "SSL_CERT_DIR"):
        env.pop(name, None)
    profile = private_directory(root / "profile")
    env.update({"XDG_CONFIG_HOME": str(private_directory(profile / "config")),
                "XDG_DATA_HOME": str(private_directory(profile / "data")), "XDG_CACHE_HOME": str(private_directory(profile / "cache")),
                "MORPHEUS_UPDATE_ACCEPTANCE_NONCE": plan["nonce"], "SLINT_BACKEND": "winit-software",
                "WINIT_UNIX_BACKEND": "x11", "APPIMAGE_EXTRACT_AND_RUN": "1", "RUST_BACKTRACE": "0"})
    sentinel = profile / "data" / "acceptance-sentinel"
    write_private(sentinel, "disposable unrelated user-data sentinel\n", create=True)
    before = sha256(sentinel)
    fd = os.open(root / "application.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as output:
        process = subprocess.Popen([str(installed)], env=env, stdin=subprocess.DEVNULL, stdout=output,
                                   stderr=subprocess.STDOUT, start_new_session=True)
    try:
        ui = NativeUI(plan["nonce"])
        original_pid = ui.settings()
        ui.click("Check for updates")
        ui.wait(STATUS_PREFIX + AVAILABLE)
        ui.wait("Available version: " + plan["new_version"])
        ui.click("Download update")
        state = installed.parent / ("." + installed.name + ".morpheus-update")
        journal = state / "journal.json"
        if case in ("invalid_signature", "truncated_download"):
            ui.wait(STATUS_PREFIX + DOWNLOAD_FAILED)
            ui.wait("Restart to update", enabled=False)
            require(sha256(installed) == plan["old_hash"] and not journal.exists())
            require(not list(Path(env["TMPDIR"]).glob("morpheus-verified-update-*")))
        else:
            ui.wait(STATUS_PREFIX + STAGED)
            if case == "discard":
                ui.click("Discard update")
                ui.wait(STATUS_PREFIX + IDLE)
                ui.wait("Restart to update", enabled=False)
                require(sha256(installed) == plan["old_hash"] and not journal.exists())
                require(not list(Path(env["TMPDIR"]).glob("morpheus-verified-update-*")))
            else:
                ui.click("Restart to update")
                ui.settings(previous_pid=original_pid)
                if case == "upgrade":
                    observed = json.loads(journal.read_text())
                    require(observed["phase"] == "Committed" and observed["new_hash"] == plan["new_hash"]
                            and observed["old_hash"] == plan["old_hash"]
                            and observed["candidate"]["version"] == plan["new_version"]
                            and sha256(installed) == plan["new_hash"])
                else:
                    require(sha256(installed) == plan["old_hash"] and not journal.exists())
        require(sha256(sentinel) == before)
        write_private(root / "result.json", json.dumps({"case": case, "passed": True, "sentinel_unchanged": True,
                                                        "runtime": "appimage_extract_and_run"}), create=True)
    finally:
        # Retain the original wrapper as a waitable anchor. Namespace exit also
        # tears down supervisor/new-app groups that intentionally detached.
        stop_owned(process)


def run_case(commands, case, old, new, signature, feed, target_version):
    commands.mark(case)
    root = private_directory(commands.root / "cases" / case)
    install = private_directory(root / "install") / "Morpheus.AppImage"
    shutil.copyfile(old, install)
    install.chmod(0o700)
    plan = {"case": case, "installed": str(install), "old_hash": sha256(old), "new_hash": sha256(new),
            "new_version": target_version, "nonce": uuid.uuid4().hex}
    write_private(root / "plan.json", json.dumps(plan), create=True)
    feed.select(case, new, signature, target_version)
    # Xvfb, D-Bus and their clients live in the same PID namespace, so AT-SPI's
    # process identifiers refer to the same /proc seen by the UI driver.
    child_env = commands.env | {"TMPDIR": str(private_directory(root / "tmp"))}
    args = namespace_command(["/usr/bin/xvfb-run", "-a", "--server-args=-screen 0 1440x1000x24",
                              "/usr/bin/dbus-run-session", "--", "/usr/bin/python3", str(Path(__file__).resolve()),
                              "--ui-case", str(root / "plan.json")], root / "namespace", child_env)
    commands.run(args, seconds=420)
    result = json.loads((root / "result.json").read_text())
    require(result == {"case": case, "passed": True, "sentinel_unchanged": True, "runtime": "appimage_extract_and_run"}, "case_evidence")
    require(feed.counts["feed"] >= 1 and feed.counts["download"] >= 1)


def execute(args):
    hosted_guard(args.execute_on_free_hosted_runner)
    os.umask(0o077)
    for name in ("CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD", "MORPHEUS_UPDATE_FEED",
                 "MORPHEUS_UPDATE_PUBLIC_KEY", "SSL_CERT_FILE", "SSL_CERT_DIR", "LD_PRELOAD"):
        require(name not in os.environ)
    for command in ("cargo", "git", "openssl", "gdbus", "/usr/bin/xvfb-run", "/usr/bin/dbus-run-session", args.gitleaks):
        require(shutil.which(command))
    target = Path(args.target_dir).resolve()
    target.mkdir(parents=True, exist_ok=True)
    checks = set()
    with tempfile.TemporaryDirectory(prefix="morpheus-installed-update-") as temporary:
        commands = Commands(Path(temporary))
        commands.gitleaks = args.gitleaks
        certificate, feed = Certificate(commands), None
        try:
            commands.mark("preflight")
            require("0.11.8" in commands.run(["cargo", "packager", "--version"]).split())
            source, revision = export_source(commands, Path(args.source).resolve())
            versions = app_versions(source)
            commands.mark("key_generation")
            key, wrong_key = generate_key(commands, "signing.key"), generate_key(commands, "wrong.key")
            public_key = key.with_name(key.name + ".pub").read_text().strip()
            commands.mark("certificate")
            certificate.create()
            feed = Feed(certificate)
            # Ordinary system-root validation, without custom client CA options.
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(feed.url + "/feed.json", timeout=10) as response:
                require(response.status == 204)
            checks.add("https_system_trust")
            commands.mark("build_old")
            old, old_meta = build_image(commands, source, target, versions[0], feed.url + "/feed.json", public_key)
            commands.mark("build_new")
            new, new_meta = build_image(commands, source, target, versions[1], feed.url + "/feed.json", public_key)
            require({k: v for k, v in old_meta.items() if k != "version"} == {k: v for k, v in new_meta.items() if k != "version"}, "cohort_mismatch")
            artifact_evidence = [{"version": metadata["version"], "sha256": sha256(image),
                                  "persistence": metadata["persistence"]}
                                 for image, metadata in ((old, old_meta), (new, new_meta))]
            checks.add("cohort_equal")
            commands.mark("signing")
            sign(commands, old, key)
            valid_signature = sign(commands, new, key)
            wrong = commands.root / "wrong-key.AppImage"
            shutil.copyfile(new, wrong)
            wrong_signature = sign(commands, wrong, wrong_key)
            invalid = commands.root / "signed-invalid-elf.AppImage"
            invalid_elf_copy(new, invalid)
            invalid_signature = sign(commands, invalid, key)
            identity = identity_record(revision, artifact_evidence, sha256(invalid),
                                       hashlib.sha256(public_key.encode()).hexdigest())
            checks.add("official_signer")
            commands.mark("containment")
            commands.run(["/usr/bin/python3", str(ROOT / "scripts/element_process_namespace.py"), "--check",
                          str(commands.root / "containment")], seconds=60)
            for case in CASES:
                image = invalid if case == "signed_invalid_elf_rollback" else new
                signature = wrong_signature if case == "invalid_signature" else invalid_signature if case == "signed_invalid_elf_rollback" else valid_signature
                run_case(commands, case, old, image, signature, feed, versions[1])
                checks.add(case)
        except BaseException as error:
            safe = error if isinstance(error, QAError) else QAError()
            safe.stage = commands.stage
            raise safe from None
        finally:
            commands.mark("cleanup")
            # Teardown retains its own finite budget even if the run deadline
            # was exhausted. Never leave the runner's temporary CA installed
            # merely because a build or UI operation consumed its allowance.
            commands.deadline = max(commands.deadline, time.monotonic() + 300)
            try:
                if feed is not None:
                    feed.close()
            finally:
                certificate.close()
        commands.mark("log_scan")
        commands.scan(args.gitleaks)
        checks.add("private_cleanup")
    require(checks == CHECKS)
    print("MORPHEUS_APPIMAGE_QA_IDENTITY " + json.dumps(identity, separators=(",", ":")), flush=True)
    for check in sorted(checks):
        print("MORPHEUS_APPIMAGE_QA_CHECK " + check + "=true", flush=True)
    print("MORPHEUS_APPIMAGE_QA_STAGE complete", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--gitleaks")
    parser.add_argument("--execute-on-free-hosted-runner", action="store_true")
    parser.add_argument("--ui-case", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.ui_case:
        ui_case(args.ui_case)
    else:
        require(args.gitleaks)
        execute(args)


if __name__ == "__main__":
    try:
        main()
    except BaseException as error:
        safe = error if isinstance(error, QAError) else QAError()
        print("MORPHEUS_APPIMAGE_QA_FAILURE " + json.dumps(safe.facts(), separators=(",", ":")), flush=True)
        print("AppImage update acceptance failed; raw diagnostics withheld", file=sys.stderr)
        raise SystemExit(1)
