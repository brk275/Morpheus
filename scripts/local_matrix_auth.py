#!/usr/bin/env python3
"""Disposable Linux MAS/Synapse OAuth fixture; private data never enters the checkout.

``up --root DIR`` prints only the private oauth-accounts.json path on stdout.
Stderr contains fixed-enum progress/failure markers, never service diagnostics.
``down``
removes this fixture's labelled containers, then its private data. ``browser``
drives the pinned MAS login/consent forms over HTTPS, using JSON on stdin/stdout.
It tests the browser protocol, not an actual browser window or desktop launch.
"""

from __future__ import annotations

import argparse
import contextlib
from enum import Enum
import hashlib
import http.client
import http.cookiejar
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import stat
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

from fixture_paths import external_path, private_directory, write_private


# Published multi-architecture manifest digests verified against the registries.
# MAS release v1.26.0 is published under Docker tag 1.26.0 (without the v).
MAS_IMAGE = "ghcr.io/element-hq/matrix-authentication-service:1.26.0@sha256:e089f1048a1d4a9a492ed17b9fe759100f1bd619407b001f5927928d88b780c4"
SYNAPSE_IMAGE = "ghcr.io/element-hq/synapse:v1.162.0@sha256:6b84a7bbac36f080b2d2e51e0289cf1b08b349598ea44a558df38d558f2c2311"
POSTGRES_IMAGE = "docker.io/library/postgres:17.10-bookworm@sha256:9b18b78397054fce88a9552e9d5a3ad5bb7fd258c5b3cc1c5028e46373d6ea8f"
OWNER_LABEL = "io.github.brk275.morpheus.oauth-fixture"
STATE_FILE = "oauth-owner.json"
FIXTURE_FILE = "oauth-accounts.json"
MAX_BODY = 1024 * 1024
CA_DIRECTORY = Path("/usr/local/share/ca-certificates")
CA_UPDATE = "/usr/sbin/update-ca-certificates"


class Stage(str, Enum):
    ROOT = "fixture_root"
    PORTS = "ports"
    CONFIGURATION = "configuration"
    CERTIFICATE = "certificate"
    MAS_CONFIG = "mas_config"
    SYNAPSE_CONFIG = "synapse_config"
    MERGE_CONFIG = "merge_config"
    POSTGRES_START = "postgres_start"
    POSTGRES_READY = "postgres_ready"
    SYNAPSE_START = "synapse_start"
    MAS_START = "mas_start"
    MAS_READY = "mas_ready"
    SYNAPSE_READY = "synapse_ready"
    REGISTRATION = "registration"
    AUTHORIZE = "browser_authorize"
    LOGIN = "browser_login"
    CONSENT = "browser_consent"
    CALLBACK = "browser_callback"
    CLEANUP = "cleanup"


class Failure(str, Enum):
    FIXTURE_OPERATION = "fixture_operation"
    BROWSER_PROTOCOL = "browser_protocol"


_stage = Stage.ROOT
_failure = Failure.FIXTURE_OPERATION


def set_stage(stage: Stage):
    global _stage
    if not isinstance(stage, Stage):
        raise TypeError("stage must be a fixed enum")
    _stage = stage
    # Keep the last attempted step available even if a parent watchdog kills
    # this process mid-command. Stdout remains a machine-readable result only.
    print("MORPHEUS_OAUTH_STAGE " + stage.value, file=sys.stderr, flush=True)


class FixtureError(RuntimeError):
    """Internal descriptions never leave the CLI's fixed-enum failure boundary."""


class PrivateArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        # argparse's normal error includes argv; keep it within the same private
        # boundary as subprocess diagnostics and malformed JSON/form responses.
        raise FixtureError("invalid fixture arguments")


@contextlib.contextmanager
def private_output(path: Path):
    external_path(path)
    private_directory(path.parent)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as output:
        yield output


class Docker:
    def __init__(self, root: Path):
        self.root = root

    def run(self, *args: str, output: Path | None = None, check: bool = True,
            timeout: int = 120) -> subprocess.CompletedProcess:
        # Never interpolate subprocess exceptions, commands or diagnostics into
        # public output: register-user's argv includes a disposable password.
        log = self.root / ("docker-" + secrets.token_hex(8) + ".log")
        try:
            with contextlib.ExitStack() as stack:
                stderr = stack.enter_context(private_output(log))
                stdout = stack.enter_context(private_output(output)) if output else subprocess.PIPE
                result = subprocess.run(
                    ["docker", *args], stdin=subprocess.DEVNULL,
                    stdout=stdout, stderr=stderr, timeout=timeout, check=False, umask=0o077,
                )
        except (OSError, subprocess.SubprocessError):
            raise FixtureError("OAuth fixture Docker command could not complete") from None
        if check and result.returncode:
            raise FixtureError("OAuth fixture Docker command failed; inspect private diagnostics")
        return result


def private_command(root: Path, args: list[str]):
    """Bounded certificate tooling; output never leaves the private fixture."""
    log = root / ("certificate-" + secrets.token_hex(8) + ".log")
    try:
        with private_output(log) as output:
            process = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=output,
                                       stderr=subprocess.STDOUT, start_new_session=True, umask=0o077)
            deadline = time.monotonic() + 30
            interrupted = None
            while True:
                try:
                    status = process.wait(timeout=max(0, deadline - time.monotonic()))
                    break
                except (KeyboardInterrupt, SystemExit) as error:
                    # Do not let up/down start a competing trust operation
                    # while this retained sudo handle still owns its command.
                    # The separate session avoids inherited terminal signals.
                    interrupted = interrupted or error
                except subprocess.TimeoutExpired:
                    # Privileged commands have their own 20s+2s watchdog;
                    # killing only sudo here is never their containment policy.
                    process.kill()
                    process.wait(timeout=3)
                    raise FixtureError("OAuth fixture certificate command timed out") from None
            if interrupted is not None:
                raise interrupted
    except (OSError, subprocess.SubprocessError):
        raise FixtureError("OAuth fixture certificate command could not complete") from None
    if status:
        raise FixtureError("OAuth fixture certificate command failed")


def privileged_command(root: Path, args: list[str]):
    # GNU timeout runs with the same privilege as the trusted system command,
    # so its process-group TERM/KILL also reaches update-ca-certificates hooks.
    # No --foreground: that option would disable descendant group signalling.
    private_command(root, ["/usr/bin/sudo", "--non-interactive", "/usr/bin/timeout",
                           "--signal=TERM", "--kill-after=2s", "20s", *args])


def certificate_digest(path: Path) -> str:
    # Never follow a replaced symlink while deciding which system trust entry
    # this fixture owns. The contents are public certificates, not private keys.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise FixtureError("OAuth fixture certificate is not a regular file")
        content = source.read(MAX_BODY + 1)
    if not content or len(content) > MAX_BODY:
        raise FixtureError("OAuth fixture certificate has an invalid size")
    return hashlib.sha256(content).hexdigest()


def trusted_certificate(owner: str) -> Path:
    if not isinstance(owner, str) or not re.fullmatch(r"[0-9a-f]{32}", owner):
        raise FixtureError("OAuth fixture certificate owner is invalid")
    return CA_DIRECTORY / ("morpheus-oauth-" + owner + ".crt")


def certificate_preflight():
    # Refuse before creating ownership/key material if these ordinary runner
    # tools are absent. No package installation or paid runner upgrade is needed.
    if (sys.platform != "linux" or os.environ.get("GITHUB_ACTIONS") != "true"
            or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted"):
        raise FixtureError("OAuth fixture trust requires a disposable hosted Linux runner")
    if any(name in os.environ for name in ("SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE")):
        raise FixtureError("OAuth fixture requires ordinary system certificate trust")
    for name in ("/usr/bin/openssl", CA_UPDATE, "/usr/bin/sudo", "/usr/bin/timeout", "/usr/bin/install", "/usr/bin/rm"):
        if not Path(name).is_file() or not os.access(name, os.X_OK):
            raise FixtureError("OAuth fixture certificate tooling is unavailable")


def install_certificate(root: Path, state: dict):
    # System trust changes belong only to this disposable hosted fixture. The
    # actual SDK and urllib clients retain their ordinary certificate validation.
    certificate_preflight()
    tls = private_directory(root / "tls")
    run = lambda args: private_command(root, args)
    run(["/usr/bin/openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
         "-subj", "/CN=Morpheus disposable OAuth QA CA", "-keyout", str(tls / "ca.key"),
         "-out", str(tls / "ca.crt"), "-addext", "basicConstraints=critical,CA:TRUE",
         "-addext", "keyUsage=critical,keyCertSign,cRLSign"])
    run(["/usr/bin/openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
         "-keyout", str(tls / "server.key"), "-out", str(tls / "server.csr")])
    write_private(tls / "server.ext", "subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n"
                  "extendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\n", create=True)
    run(["/usr/bin/openssl", "x509", "-req", "-in", str(tls / "server.csr"), "-CA", str(tls / "ca.crt"),
         "-CAkey", str(tls / "ca.key"), "-CAcreateserial", "-days", "1", "-out", str(tls / "server.crt"),
         "-extfile", str(tls / "server.ext")])
    for path in tls.iterdir():
        path.chmod(0o600)
    trusted = trusted_certificate(state["owner"])
    if trusted.exists() or trusted.is_symlink():
        raise FixtureError("OAuth fixture refused an existing system trust entry")
    # Persist the exact certificate identity BEFORE sudo can copy it. A separate
    # down process can therefore clean up interrupted install/refresh commands.
    state["certificate_sha256"] = certificate_digest(tls / "ca.crt")
    save_state(root, state)
    privileged_command(root, ["/usr/bin/install", "-m", "644", str(tls / "ca.crt"), str(trusted)])
    privileged_command(root, [CA_UPDATE])


def remove_certificate(root: Path, state: dict):
    expected = state.get("certificate_sha256")
    if expected is None:
        return
    if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise FixtureError("OAuth fixture certificate ownership is invalid")
    trusted = trusted_certificate(state["owner"])
    if trusted.exists() or trusted.is_symlink():
        if certificate_digest(trusted) != expected:
            raise FixtureError("OAuth fixture refused an unowned system trust entry")
        privileged_command(root, ["/usr/bin/rm", "--", str(trusted)])
    # Run even after a prior partial cleanup removed the source file: generated
    # trust links/bundle may still contain the CA until refresh succeeds.
    privileged_command(root, [CA_UPDATE])
    del state["certificate_sha256"]
    save_state(root, state)


class PortReservations:
    """Hold all chosen loopback ports until their individual services start."""

    def __init__(self):
        self.sockets: dict[str, socket.socket] = {}
        try:
            for name in ("postgres", "synapse", "mas", "mas_internal"):
                sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                self.sockets[name] = sock
                sock.bind(("127.0.0.1", 0))
                sock.listen(1)
        except OSError:
            self.close()
            raise FixtureError("OAuth fixture could not reserve loopback ports") from None

    def ports(self) -> dict[str, int]:
        return {name: sock.getsockname()[1] for name, sock in self.sockets.items()}

    def release(self, name: str):
        self.sockets.pop(name).close()

    def close(self):
        for sock in self.sockets.values():
            sock.close()
        self.sockets.clear()


def fixture_configuration(root: Path, nonce: str, ports: dict[str, int]) -> dict:
    """Write secrets and explicit loopback overlays for generated vendor config."""
    mas_url = f"https://127.0.0.1:{ports['mas']}/"
    mas_internal_url = f"http://127.0.0.1:{ports['mas_internal']}/"
    synapse_url = f"http://127.0.0.1:{ports['synapse']}"
    server_name = f"localhost:{ports['synapse']}"
    shared_secret = secrets.token_hex(32)
    write_private(root / "mas-synapse.secret", shared_secret + "\n", create=True)
    write_private(root / "postgres.secret", secrets.token_urlsafe(48) + "\n", create=True)
    for name in ("synapse", "postgres"):
        private_directory(root / name)
    write_private(root / "postgres.env", "POSTGRES_USER=mas\nPOSTGRES_DB=mas\n"
                  "POSTGRES_PASSWORD_FILE=/fixture/postgres.secret\n"
                  "POSTGRES_INITDB_ARGS=--auth-host=scram-sha-256 --auth-local=trust\n",
                  create=True)
    mas = {
        "http": {
            "public_base": mas_url, "issuer": mas_url, "trusted_proxies": [],
            "listeners": [{"name": "fixture", "binds": [{"host": "127.0.0.1", "port": ports["mas"]}],
                           "tls": {"certificate_file": "/fixture/tls/server.crt", "key_file": "/fixture/tls/server.key"},
                           "resources": [{"name": name} for name in
                                         ("discovery", "human", "oauth", "compat", "assets", "health")]},
                          # Synapse derives both its metadata and introspection
                          # URLs from this private endpoint, not the public issuer.
                          {"name": "synapse-internal", "binds": [{"host": "127.0.0.1", "port": ports["mas_internal"]}],
                           "resources": [{"name": "discovery"}, {"name": "oauth"}]}],
        },
        "database": {"host": "127.0.0.1", "port": ports["postgres"], "username": "mas",
                     "database": "mas", "password_file": "/fixture/postgres.secret",
                     "ssl_mode": "disable", "max_connections": 5},
        "matrix": {"kind": "synapse", "homeserver": server_name, "endpoint": synapse_url,
                   "secret_file": "/fixture/mas-synapse.secret"},
        "account": {"password_registration_enabled": False, "password_recovery_enabled": False},
        "experimental": {"access_token_ttl": 60},
    }
    synapse = {
        "server_name": server_name, "public_baseurl": synapse_url + "/",
        "report_stats": False, "enable_registration": False, "enable_3pid_changes": False,
        "password_config": {"enabled": False},
        "listeners": [{"port": ports["synapse"], "type": "http", "tls": False,
                       "bind_addresses": ["127.0.0.1"],
                       "resources": [{"names": ["client"], "compress": False}]}],
        "database": {"name": "sqlite3", "args": {"database": "/fixture/synapse/homeserver.db"}},
        "media_store_path": "/fixture/synapse/media", "trusted_key_servers": [],
        "matrix_authentication_service": {"enabled": True, "endpoint": mas_internal_url,
                                          "secret_path": "/fixture/mas-synapse.secret"},
        # No outbound federation or URL-preview traffic belongs to this fixture.
        "federation_domain_whitelist": [], "url_preview_enabled": False,
    }
    write_private(root / "mas-overlay.json", json.dumps(mas), create=True)
    write_private(root / "synapse-overlay.json", json.dumps(synapse), create=True)
    account = {
        "homeserver_url": synapse_url, "issuer_url": mas_url,
        "username": "morpheus_" + nonce, "password": secrets.token_urlsafe(48),
        "user_id": f"@morpheus_{nonce}:{server_name}", "access_token_ttl_seconds": 60,
    }
    write_private(root / FIXTURE_FILE, json.dumps(account) + "\n", create=True)
    return account


# Use PyYAML already shipped in the pinned Synapse image, not a host dependency.
# Replace overlay sections whole so generated database.uri/default public
# listeners cannot survive a recursive merge. Preserve MAS generated keys,
# policy, templates, and all other vendor defaults.
MERGE_CONFIG = r"""
import json, os
from pathlib import Path
import yaml
os.umask(0o077)
for name in ('mas', 'synapse'):
    source = Path('/fixture') / (name + '.generated.yaml')
    config = yaml.safe_load(source.read_text())
    config.update(json.loads((Path('/fixture') / (name + '-overlay.json')).read_text()))
    destination = Path('/fixture') / (name + '.yaml')
    with destination.open('x') as output:
        json.dump(config, output)
    destination.chmod(0o600)
"""

SYNAPSE_COMMAND = (
    "import os,runpy,sys; os.umask(0o077); os.chdir('/fixture/synapse'); "
    "sys.argv=['synapse.app.homeserver',*sys.argv[1:]]; "
    "runpy.run_module('synapse.app.homeserver',run_name='__main__')"
)


def save_state(root: Path, state: dict):
    write_private(root / STATE_FILE, json.dumps(state) + "\n")


def create_container(docker: Docker, state: dict, role: str, image: str,
                     command: list[str], *, network: str = "none",
                     options: tuple[str, ...] = ()) -> str:
    name = "morpheus-oauth-" + state["owner"] + "-" + role
    cidfile = docker.root / (role + ".cid")
    # Record intent first. Label reconciliation in down also covers interruption
    # between Docker creating a container and the CLI saving its returned ID.
    state["containers"].append({"role": role, "name": name, "id": None})
    save_state(docker.root, state)
    result = docker.run(
        "create", "--name", name, "--cidfile", str(cidfile),
        "--label", OWNER_LABEL + "=" + state["owner"],
        "--network", network, "--log-driver", "none",
        "--user", f"{os.getuid()}:{os.getgid()}",
        "--mount", f"type=bind,src={docker.root},dst=/fixture",
        *options, image, *command, timeout=300,
    )
    cid = result.stdout.decode().strip()
    if not re.fullmatch(r"[0-9a-f]{64}", cid):
        raise FixtureError("OAuth fixture received an invalid container identity")
    if cidfile.exists():
        os.chmod(cidfile, 0o600)
    state["containers"][-1]["id"] = cid
    save_state(docker.root, state)
    return cid


def one_shot(docker: Docker, state: dict, role: str, image: str, command: list[str],
             *, output: Path | None = None, options: tuple[str, ...] = ()):
    cid = create_container(docker, state, role, image, command, options=options)
    docker.run("start", "--attach", cid, output=output, timeout=180)
    # `docker start --attach` need not propagate every engine exit condition.
    result = docker.run("inspect", "--format", "{{.State.ExitCode}}", cid)
    if result.stdout.strip() != b"0":
        raise FixtureError("OAuth fixture initialization failed")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def http_opener(*, cookies: bool = False):
    # Loopback must never pass through a CI host's ambient HTTP proxy.
    handlers = [urllib.request.ProxyHandler({}), NoRedirect()]
    if cookies:
        handlers.append(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
    return urllib.request.build_opener(*handlers)


def wait_ready(url: str, *, expected_issuer: str | None = None):
    opener = http_opener()
    deadline = time.monotonic() + 120
    while (remaining := deadline - time.monotonic()) > 0:
        try:
            with opener.open(url, timeout=min(3, remaining)) as response:
                data = json.loads(response.read(MAX_BODY))
                if expected_issuer is None and isinstance(data, dict) and data.get("versions"):
                    return
                if expected_issuer is not None and isinstance(data, dict) and data.get("issuer") == expected_issuer:
                    return
        except (urllib.error.URLError, ConnectionError, TimeoutError, ValueError, http.client.HTTPException):
            pass
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    raise FixtureError("OAuth fixture service did not become ready")


def up(root: Path) -> Path:
    set_stage(Stage.ROOT)
    if sys.platform != "linux":
        raise FixtureError("OAuth fixture services require Linux Docker host networking")
    certificate_preflight()
    root = external_path(root)
    if root.exists() and any(root.iterdir()):
        raise FixtureError("OAuth fixture requires a fresh empty private directory")
    private_directory(root)
    state = {"schema": 1, "owner": secrets.token_hex(16), "containers": []}
    save_state(root, state)
    docker = Docker(root)
    set_stage(Stage.PORTS)
    reservations = PortReservations()
    try:
        ports = reservations.ports()
        set_stage(Stage.CERTIFICATE)
        install_certificate(root, state)
        set_stage(Stage.CONFIGURATION)
        account = fixture_configuration(root, state["owner"], ports)
        set_stage(Stage.MAS_CONFIG)
        one_shot(docker, state, "mas-config", MAS_IMAGE, ["config", "generate"],
                 output=root / "mas.generated.yaml")
        set_stage(Stage.SYNAPSE_CONFIG)
        one_shot(docker, state, "synapse-config", SYNAPSE_IMAGE,
                 ["-c", SYNAPSE_COMMAND, "--server-name", account["user_id"].split(":", 1)[1],
                  "--config-path", "/fixture/synapse.generated.yaml", "--generate-config",
                  "--report-stats=no", "--data-directory", "/fixture/synapse"],
                 options=("--entrypoint", "python3"))
        set_stage(Stage.MERGE_CONFIG)
        one_shot(docker, state, "merge-config", SYNAPSE_IMAGE, ["-c", MERGE_CONFIG],
                 options=("--entrypoint", "python3"))
        # The official PostgreSQL entrypoint supports an arbitrary user ID.
        # Its generated database stays owned by the runner in our private root.
        set_stage(Stage.POSTGRES_START)
        postgres = create_container(
            docker, state, "postgres", POSTGRES_IMAGE,
            # Keep the image's internal Unix socket directory: its entrypoint
            # uses it to create the database before enabling TCP. The container
            # shares host networking, not the host filesystem/socket directory.
            ["postgres", "-c", "listen_addresses=127.0.0.1", "-p", str(ports["postgres"])],
            network="host", options=("--env-file", str(root / "postgres.env"),
                                     "--env", "PGDATA=/fixture/postgres/data"),
        )
        reservations.release("postgres")
        docker.run("start", postgres)
        set_stage(Stage.POSTGRES_READY)
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if docker.run("exec", postgres, "pg_isready", "-h", "127.0.0.1", "-p",
                          str(ports["postgres"]), "-U", "mas", "-d", "mas", check=False).returncode == 0:
                break
            time.sleep(1)
        else:
            raise FixtureError("OAuth fixture database did not become ready")
        set_stage(Stage.SYNAPSE_START)
        synapse = create_container(
            docker, state, "synapse", SYNAPSE_IMAGE,
            ["-c", SYNAPSE_COMMAND, "--config-path", "/fixture/synapse.yaml"],
            network="host", options=("--entrypoint", "python3"),
        )
        reservations.release("synapse")
        docker.run("start", synapse)
        set_stage(Stage.MAS_START)
        mas = create_container(docker, state, "mas", MAS_IMAGE,
                               ["--config", "/fixture/mas.yaml", "server"], network="host")
        reservations.release("mas")
        reservations.release("mas_internal")
        docker.run("start", mas)
        set_stage(Stage.MAS_READY)
        wait_ready(account["issuer_url"] + ".well-known/openid-configuration",
                   expected_issuer=account["issuer_url"])
        set_stage(Stage.SYNAPSE_READY)
        wait_ready(account["homeserver_url"] + "/_matrix/client/versions")
        # The modern MAS CLI has a positional username. --yes avoids interactive
        # command echo; output and diagnostics remain private and are not uploaded.
        set_stage(Stage.REGISTRATION)
        docker.run("exec", mas, "/usr/local/bin/mas-cli", "--config", "/fixture/mas.yaml",
                   "manage", "register-user", account["username"], "--password", account["password"],
                   "--yes", "--no-admin", output=root / "registration.log")
        return root / FIXTURE_FILE
    except BaseException:
        # Preserve private diagnostics on failure, but never leave live services.
        failed_stage = _stage
        try:
            down(root, remove_data=False)
        except FixtureError:
            raise FixtureError("OAuth fixture failed and cleanup needs retry with down") from None
        finally:
            # Report the original failed operation even if cleanup also failed.
            # The harness separately invokes down and reports its own outcome.
            set_stage(failed_stage)
        raise
    finally:
        reservations.close()


def down(root: Path, *, remove_data: bool = True):
    set_stage(Stage.CLEANUP)
    root = external_path(root)
    if not root.exists():
        return
    try:
        state = json.loads((root / STATE_FILE).read_text())
        owner = state["owner"]
        if state["schema"] != 1 or not re.fullmatch(r"[0-9a-f]{32}", owner):
            raise ValueError
    except (OSError, ValueError, KeyError, TypeError):
        raise FixtureError("OAuth fixture ownership record is missing or invalid") from None
    docker = Docker(root)
    cleanup_error = None
    # Leave at least 60s for the two bounded CA-removal commands before the
    # parent's 120s cleanup deadline, even when Docker is unresponsive.
    docker_deadline = time.monotonic() + 45

    def cleanup_run(*args):
        remaining = docker_deadline - time.monotonic()
        if remaining <= 0:
            raise FixtureError("OAuth fixture container cleanup deadline expired")
        return docker.run(*args, timeout=min(10, remaining))
    # Attempt CA removal even when Docker fails; retain all ownership evidence
    # if either cleanup fails so another down invocation can retry safely.
    try:
        found = cleanup_run("ps", "--all", "--quiet", "--no-trunc", "--filter", "label=" + OWNER_LABEL + "=" + owner)
        ids = found.stdout.decode().split()
        for cid in reversed(ids):
            try:
                if not re.fullmatch(r"[0-9a-f]{64}", cid):
                    raise FixtureError("OAuth fixture cleanup received an invalid container identity")
                result = cleanup_run("inspect", "--format", "{{json .Config.Labels}}", cid)
                if json.loads(result.stdout).get(OWNER_LABEL) != owner:
                    raise FixtureError("OAuth fixture cleanup refused an unowned container")
                cleanup_run("rm", "--force", "--volumes", cid)
            except BaseException as error:
                cleanup_error = cleanup_error or error
    except BaseException as error:
        cleanup_error = cleanup_error or error
    try:
        remove_certificate(root, state)
    except BaseException as error:
        cleanup_error = cleanup_error or error
    if isinstance(cleanup_error, (KeyboardInterrupt, SystemExit)):
        raise cleanup_error
    if cleanup_error is not None:
        raise FixtureError("OAuth fixture owned-resource cleanup failed")
    if remove_data:
        shutil.rmtree(root)


def loopback_url(value: str) -> urllib.parse.SplitResult:
    try:
        parsed = urllib.parse.urlsplit(value)
        valid = (isinstance(value, str) and not any(ord(c) < 33 for c in value)
                 and "\\" not in value and parsed.scheme in ("http", "https")
                 and parsed.hostname == "127.0.0.1" and parsed.port is not None
                 and parsed.port > 0 and parsed.username is None and parsed.password is None)
        if valid:
            return parsed
    except (ValueError, TypeError, AttributeError):
        pass
    raise FixtureError("OAuth form driver requires explicit HTTP(S) loopback URLs")


def origin(value: str) -> tuple[str, str, int]:
    parsed = loopback_url(value)
    return parsed.scheme, parsed.hostname, parsed.port


class Forms(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.forms: list[dict] = []
        self.current: dict | None = None

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "form":
            if self.current is not None:
                raise FixtureError("OAuth form driver received nested forms")
            self.current = {"method": attrs.get("method", "GET").upper(),
                            "action": attrs.get("action", ""), "inputs": {}}
        elif tag == "input" and self.current is not None and "disabled" not in attrs:
            name = attrs.get("name")
            if name:
                if name in self.current["inputs"]:
                    raise FixtureError("OAuth form driver received duplicate fields")
                self.current["inputs"][name] = attrs.get("value", "")

    def handle_endtag(self, tag):
        if tag == "form" and self.current is not None:
            self.forms.append(self.current)
            self.current = None


def form_body(html: str, current_url: str, username: str, password: str,
              *, logged_in: bool) -> bytes:
    parser = Forms()
    parser.feed(html)
    forms = [form for form in parser.forms if form["method"] == "POST"
             and urllib.parse.urljoin(current_url, form["action"]) == current_url
             and form["inputs"].get("csrf")]
    expected = {"csrf"} if logged_in else {"csrf", "username", "password"}
    forms = [form for form in forms if set(form["inputs"]) == expected]
    if len(forms) != 1:
        raise FixtureError("OAuth form driver did not find the expected MAS form")
    fields = {"csrf": forms[0]["inputs"]["csrf"]}
    if not logged_in:
        fields.update(username=username, password=password)
    return urllib.parse.urlencode(fields).encode()


def browser(request: dict, *, opener=None) -> dict:
    set_stage(Stage.AUTHORIZE)
    authorization = request["authorization_url"]
    redirect = request["redirect_url"]
    issuer = request["issuer_url"]
    auth_parts, redirect_parts = loopback_url(authorization), loopback_url(redirect)
    if (auth_parts.scheme != "https" or redirect_parts.scheme != "http" or auth_parts.fragment
            or redirect_parts.query or redirect_parts.fragment or origin(authorization) != origin(issuer)):
        raise FixtureError("OAuth form driver received an invalid authorization target")
    query = urllib.parse.parse_qs(auth_parts.query, strict_parsing=True)
    if query.get("redirect_uri") != [redirect] or len(query.get("state", [])) != 1 or not query["state"][0]:
        raise FixtureError("OAuth form driver received inconsistent authorization parameters")
    username, password = request["username"], request["password"]
    if not isinstance(username, str) or not username or not isinstance(password, str) or not password:
        raise FixtureError("OAuth form driver requires fixture credentials")
    opener = opener or http_opener(cookies=True)
    current, body, logged_in, consented = authorization, None, False, False
    for _ in range(24):
        if origin(current) != origin(issuer) or urllib.parse.urlsplit(current).fragment:
            raise FixtureError("OAuth form driver refused an off-origin navigation")
        headers = {"Accept": "text/html"}
        if body is not None:
            headers["Content-Type"] = "application/x-www-form-urlencoded"
            # The origin header contains only scheme and authority, never query.
            parts = loopback_url(issuer)
            headers["Origin"] = f"{parts.scheme}://{parts.netloc}"
        http_request = urllib.request.Request(current, data=body, headers=headers)
        try:
            response = opener.open(http_request, timeout=10)
        except urllib.error.HTTPError as error:
            response = error
        except (urllib.error.URLError, OSError, http.client.HTTPException):
            raise FixtureError("OAuth form driver request failed") from None
        with response:
            if response.status in (301, 302, 303, 307, 308):
                location = response.headers.get("Location")
                if not location:
                    raise FixtureError("OAuth form driver received an empty redirect")
                target = urllib.parse.urljoin(current, location)
                parts = loopback_url(target)
                if origin(target) == origin(redirect) and parts.path == redirect_parts.path:
                    set_stage(Stage.CALLBACK)
                    callback = urllib.parse.parse_qs(parts.query, strict_parsing=True)
                    if (parts.fragment or callback.get("state") != query["state"]
                            or len(callback.get("code", [])) != 1 or not callback["code"][0]
                            or "error" in callback):
                        raise FixtureError("OAuth form driver received an invalid callback")
                    return {"callback_url": target}
                if origin(target) != origin(issuer):
                    raise FixtureError("OAuth form driver refused an off-origin redirect")
                if response.status in (301, 302, 303):
                    body = None
                current = target
                set_stage(Stage.CONSENT if logged_in else Stage.LOGIN)
                continue
            if response.status != 200 or response.headers.get_content_type() != "text/html":
                raise FixtureError("OAuth form driver received an unexpected response")
            raw = response.read(MAX_BODY + 1)
            if len(raw) > MAX_BODY or consented:
                raise FixtureError("OAuth form driver received an unexpected page")
            set_stage(Stage.CONSENT if logged_in else Stage.LOGIN)
            body = form_body(raw.decode("utf-8"), current, username, password, logged_in=logged_in)
            if logged_in:
                consented = True
            logged_in = True
    raise FixtureError("OAuth form driver exceeded its navigation limit")


def main():
    global _failure
    parser = PrivateArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("up", "down", "browser"))
    parser.add_argument("--root", type=Path)
    parser.add_argument("--keep-data", action="store_true",
                        help="down only: retain private diagnostics after removing owned containers")
    args = parser.parse_args()
    if args.command == "browser":
        _failure = Failure.BROWSER_PROTOCOL
        set_stage(Stage.AUTHORIZE)
    elif args.command == "down":
        set_stage(Stage.CLEANUP)
    if args.keep_data and args.command != "down":
        raise FixtureError("OAuth fixture keep-data is only valid with down")
    if args.command == "browser":
        raw = sys.stdin.buffer.read(MAX_BODY + 1)
        if len(raw) > MAX_BODY:
            raise FixtureError("OAuth form driver input is too large")
        print(json.dumps(browser(json.loads(raw))))
    else:
        if args.root is None:
            raise FixtureError("OAuth fixture requires an explicit private root")
        if args.command == "up":
            print(up(args.root))
        else:
            down(args.root, remove_data=not args.keep_data)
            print("OAuth fixture containers removed" if args.keep_data else "OAuth fixture removed")


def run_cli() -> int:
    global _failure
    set_stage(Stage.ROOT)
    _failure = Failure.FIXTURE_OPERATION
    try:
        main()
    except (Exception, KeyboardInterrupt):
        # JSON, HTTP and OS exception text may contain credentials or callback
        # URLs. Only literal enum values reach the public job log.
        print("MORPHEUS_OAUTH_FAILURE " + _failure.value, file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(run_cli())
