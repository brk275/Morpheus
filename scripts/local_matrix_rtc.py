#!/usr/bin/env python3
"""Run a disposable loopback MatrixRTC backend for the local Synapse A server.

The official LiveKit and lk-jwt-service images are used. API keys are generated
into the private temp test directory; no secret belongs in the repository.
Run ``scripts/local_matrix_env.py up`` first. This script only modifies the
disposable Synapse A volume and never changes Synapse B.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import re
import secrets
import selectors
import shutil
import signal
import stat
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

import local_matrix_env as matrix_env
from fixture_paths import external_path, write_private


FIXTURE_HOME = matrix_env.FIXTURE_HOME
STATE = FIXTURE_HOME / "rtc"
SECRET_FILE = STATE / "secrets.json"
SFU_CONFIG = STATE / "livekit.yaml"
NATIVE_SFU_CONFIG = STATE / "livekit-native.yaml"
NATIVE_SFU_PID = STATE / "livekit-native.pid"
NATIVE_SFU_LOG = STATE / "livekit-native.log"
SFU_OWNER = STATE / "sfu-owner.json"
SFU_OWNER_LABEL = "io.github.brk275.morpheus.fixture-owner"
SFU_RUN_LABEL = "io.github.brk275.morpheus.fixture-run"
SFU_SOURCE_LABEL = "io.github.brk275.morpheus.fixture-source"
AUTH_ENV = STATE / "auth.env"
SFU_LOOPBACK_SCRIPT = STATE / "sfu_loopback.py"
FEDERATION_PROXY = STATE / "federation_proxy.py"
FEDERATION_CERT = STATE / "federation.crt"
FEDERATION_KEY = STATE / "federation.key"
NETWORK = "morpheus-matrix-test"
SYNAPSE = "morpheus-synapse-a"
SFU = "morpheus-rtc-livekit"
AUTH = "morpheus-rtc-auth"
SFU_LOOPBACK = "morpheus-rtc-sfu-loopback"
FEDERATION = "morpheus-rtc-federation"
SFU_IMAGE = "livekit/livekit-server@sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154"
AUTH_IMAGE = "ghcr.io/element-hq/lk-jwt-service@sha256:fe787619877b9c12d4370ed39dc8edecf99b1a71fa8958c0fdcb687c1d31c775"
DISCOVERY_URL = "http://127.0.0.1:18010"
SFU_URL = "ws://127.0.0.1:18011"

# The pinned JWT service uses LIVEKIT_URL both for its server-side room API
# and for URLs returned to clients. Its private network namespace therefore
# needs its own loopback route to the SFU. This transparent TCP bridge keeps
# JWT responses unchanged and never handles Matrix/LiveKit credentials itself.
SFU_LOOPBACK_SOURCE = '''"""Private loopback route for the local JWT service only."""
import select
import socket
import socketserver


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            with socket.create_connection(self.server.target, timeout=5) as upstream:
                upstream.settimeout(10)
                self.request.settimeout(10)
                peers = {self.request: upstream, upstream: self.request}
                while peers:
                    readable, _, _ = select.select(list(peers), [], [], 30)
                    if not readable:
                        return
                    for source in readable:
                        destination = peers[source]
                        chunk = source.recv(65536)
                        if chunk:
                            destination.sendall(chunk)
                        else:
                            # Preserve half-close: the SFU may send its final
                            # response only after receiving the entire request.
                            destination.shutdown(socket.SHUT_WR)
                            del peers[source]
        except OSError:
            # Network failures close the connection. Never log request bytes,
            # headers, OpenID tokens, JWTs, or native API credentials.
            return


class Forwarder(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, target=("morpheus-rtc.localhost", 18011), port=18011):
        self.target = target
        super().__init__(("127.0.0.1", port), Handler)

    def handle_error(self, request, client_address):
        pass


if __name__ == "__main__":
    with Forwarder() as server:
        server.serve_forever()
'''


def docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(["docker", *args], text=True, capture_output=True)
    if check and result.returncode:
        raise matrix_env.DockerFailure(result)
    return result



def recovery_identity() -> dict[str, str] | None:
    """Only the fresh hosted owner creates this unguessable per-run identity."""
    run = os.environ.get("MORPHEUS_MATRIX_SFU_RUN_ID")
    if run is None:
        return None
    source = os.environ.get("GITHUB_SHA", "")
    if (os.environ.get("GITHUB_ACTIONS") != "true"
            or os.environ.get("RUNNER_OS") != "Linux"
            or os.environ.get("GITHUB_REPOSITORY") != "brk275/Morpheus"
            or re.fullmatch(r"[0-9a-f]{32}", run) is None
            or re.fullmatch(r"[0-9a-f]{40}", source) is None):
        raise RuntimeError("Owned SFU recovery identity unavailable")
    return {SFU_OWNER_LABEL: "matrixrtc", SFU_RUN_LABEL: run, SFU_SOURCE_LABEL: source}


# At most the failed command's actual unreaped client is retained until this
# short-lived fixture helper exits under the mandatory owned PID namespace.
# This is terminal uncertainty, never a retryable or successful cleanup receipt.
_UNREAPED_DOCKER = []


def owned_docker(*args: str) -> subprocess.CompletedProcess[str]:
    """Bound both actual client pipes while retaining its real process owner."""
    command = ["docker", *args]
    process = None
    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    limit = 131072
    deadline = time.monotonic() + 30
    try:
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        with selectors.DefaultSelector() as ready:
            for name in buffers:
                stream = getattr(process, name)
                os.set_blocking(stream.fileno(), False)
                ready.register(stream, selectors.EVENT_READ, name)
            while ready.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError("Owned SFU operation deadline exceeded")
                for key, _ in ready.select(min(0.1, remaining)):
                    total = sum(len(value) for value in buffers.values())
                    chunk = os.read(key.fd, min(65536, limit + 1 - total))
                    if not chunk:
                        ready.unregister(key.fileobj)
                        continue
                    buffers[key.data].extend(chunk)
                    if sum(len(value) for value in buffers.values()) > limit:
                        raise RuntimeError("Owned SFU operation output exceeds limit")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError("Owned SFU operation deadline exceeded")
        status = process.wait(timeout=remaining)
        if status:
            raise RuntimeError("Owned SFU operation failed")
        return subprocess.CompletedProcess(command, status, buffers["stdout"].decode("utf-8"),
                                            buffers["stderr"].decode("utf-8"))
    except (OSError, subprocess.SubprocessError, UnicodeError):
        raise RuntimeError("Owned SFU operation completion unproved") from None
    finally:
        if process is not None:
            cleanup_unproved = False
            try:
                if process.poll() is None:
                    process.kill()  # Actual retained, unreaped Popen only.
            except ProcessLookupError:
                pass
            except OSError:
                cleanup_unproved = True
            try:
                process.wait(timeout=10)
            except (OSError, subprocess.SubprocessError):
                _UNREAPED_DOCKER.append(process)
                cleanup_unproved = True
            for name in buffers:
                stream = getattr(process, name)
                if stream is not None:
                    try:
                        stream.close()
                    except OSError:
                        cleanup_unproved = True
            if cleanup_unproved:
                raise RuntimeError("Owned SFU client cleanup unproved") from None


def private_bytes(path: Path, limit: int) -> tuple[bytes, os.stat_result, os.stat_result]:
    if path.parent != STATE or path.name not in (SFU_CONFIG.name, SFU_OWNER.name):
        raise RuntimeError("Owned SFU private path rejected")
    external_path(path)
    root_fd = os.open(FIXTURE_HOME, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        root = os.fstat(root_fd)
        if not stat.S_ISDIR(root.st_mode) or root.st_uid != os.getuid() or root.st_mode & 0o077:
            raise RuntimeError("Owned SFU fixture root rejected")
        state_fd = os.open("rtc", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=root_fd)
        try:
            state = os.fstat(state_fd)
            if not stat.S_ISDIR(state.st_mode) or state.st_uid != os.getuid() or state.st_mode & 0o077:
                raise RuntimeError("Owned SFU private directory rejected")
            fd = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=state_fd)
            try:
                before = os.fstat(fd)
                if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid()
                        or before.st_nlink != 1 or before.st_mode & 0o077 or before.st_size > limit):
                    raise RuntimeError("Owned SFU private input rejected")
                data = bytearray()
                while len(data) <= limit:
                    chunk = os.read(fd, min(65536, limit + 1 - len(data)))
                    if not chunk:
                        break
                    data.extend(chunk)
                after = os.fstat(fd)
                if (len(data) > limit or len(data) != before.st_size
                        or (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns)
                        != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns)):
                    raise RuntimeError("Owned SFU private input changed")
                return bytes(data), before, root
            finally:
                os.close(fd)
        finally:
            os.close(state_fd)
    finally:
        os.close(root_fd)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate owned SFU field")
        result[key] = value
    return result


def inspect_owned_sfu(container: str, labels: dict[str, str], *, image: str | None = None) -> dict:
    if re.fullmatch(r"[0-9a-f]{64}", container) is None:
        raise RuntimeError("Owned SFU container identity rejected")
    response = owned_docker("container", "inspect", container)
    if len(response.stdout) > 131072:
        raise RuntimeError("Owned SFU inspection exceeds limit")
    try:
        records = json.loads(response.stdout, object_pairs_hook=unique_object)
        if not isinstance(records, list) or len(records) != 1:
            raise ValueError
        item = records[0]
        mounts = item["Mounts"]
        expected_ports = {f"{port}/{protocol}": [{"HostIp": "127.0.0.1", "HostPort": str(port)}]
                          for port, protocol in ((18011, "tcp"), (18012, "udp"), (18013, "tcp"))}
        if (item["Id"] != container or item["Name"] != "/" + SFU
                or item["Config"]["Image"] != SFU_IMAGE
                or any(item["Config"]["Labels"].get(key) != value for key, value in labels.items())
                or item["Config"]["Cmd"] != ["--config", "/etc/livekit.yaml"]
                or item["HostConfig"]["NetworkMode"] != NETWORK
                or item["HostConfig"]["PortBindings"] != expected_ports
                or re.fullmatch(r"sha256:[0-9a-f]{64}", item["Image"]) is None
                or (image is not None and item["Image"] != image)
                or len(mounts) != 1 or mounts[0]["Type"] != "bind"
                or mounts[0]["Source"] != str(SFU_CONFIG)
                or mounts[0]["Destination"] != "/etc/livekit.yaml"
                or mounts[0]["RW"] is not False
                or type(item["State"]["Running"]) is not bool
                or type(item["State"]["Pid"]) is not int):
            raise ValueError
    except (ValueError, TypeError, KeyError, IndexError, AttributeError):
        raise RuntimeError("Owned SFU inspection rejected") from None
    return item


def remember_sfu(container: str, labels: dict[str, str]) -> None:
    config, meta, root = private_bytes(SFU_CONFIG, 65536)
    if not stat.S_ISDIR(root.st_mode) or root.st_uid != os.getuid() or root.st_mode & 0o077:
        raise RuntimeError("Owned SFU fixture root rejected")
    item = inspect_owned_sfu(container, labels)
    if not item["State"]["Running"] or item["State"]["Pid"] <= 0:
        raise RuntimeError("Owned SFU did not start")
    record = {"protocol": 1, "container": container, "image": item["Image"], "labels": labels,
              "root": [root.st_dev, root.st_ino], "config": [meta.st_dev, meta.st_ino],
              "config_sha256": hashlib.sha256(config).hexdigest()}
    write_private(SFU_OWNER, json.dumps(record, sort_keys=True) + "\n", create=True)


def recovery_sfu() -> tuple[dict, dict]:
    labels = recovery_identity()
    if labels is None:
        raise RuntimeError("Owned SFU recovery is not admitted")
    data, _, owned_root = private_bytes(SFU_OWNER, 16384)
    try:
        record = json.loads(data, object_pairs_hook=unique_object)
        if (set(record) != {"protocol", "container", "image", "labels", "root", "config", "config_sha256"}
                or record["protocol"] != 1 or record["labels"] != labels):
            raise ValueError
        config, meta, root = private_bytes(SFU_CONFIG, 65536)
        if (not stat.S_ISDIR(root.st_mode) or root.st_uid != os.getuid() or root.st_mode & 0o077
                or (root.st_dev, root.st_ino) != (owned_root.st_dev, owned_root.st_ino)
                or record["root"] != [root.st_dev, root.st_ino]
                or record["config"] != [meta.st_dev, meta.st_ino]
                or record["config_sha256"] != hashlib.sha256(config).hexdigest()):
            raise ValueError
        item = inspect_owned_sfu(record["container"], labels, image=record["image"])
    except (ValueError, TypeError, KeyError):
        raise RuntimeError("Owned SFU recovery capability rejected") from None
    return record, item


def sfu_outage() -> None:
    record, before = recovery_sfu()
    if not before["State"]["Running"] or before["State"]["Pid"] <= 0:
        raise RuntimeError("Owned SFU was not live before outage")
    owned_docker("container", "kill", "--signal", "KILL", record["container"])
    _, after = recovery_sfu()
    if after["State"]["Running"] or after["State"]["Pid"] != 0:
        raise RuntimeError("Owned SFU outage is unproved")
    print("MORPHEUS_SFU_CONTROL stopped", flush=True)


def sfu_resume() -> None:
    record, before = recovery_sfu()
    if before["State"]["Running"] or before["State"]["Pid"] != 0:
        raise RuntimeError("Owned SFU was not stopped before restart")
    owned_docker("container", "start", record["container"])
    _, after = recovery_sfu()
    if (not after["State"]["Running"] or after["State"]["Pid"] <= 0
            or after["State"]["StartedAt"] == before["State"]["StartedAt"]):
        raise RuntimeError("Owned SFU restart is unproved")
    wait_http("http://127.0.0.1:18011/", duration=20)
    print("MORPHEUS_SFU_CONTROL restarted", flush=True)


def exists(kind: str, name: str) -> bool:
    return docker(kind, "inspect", name, check=False).returncode == 0


def running(name: str) -> bool:
    result = docker("inspect", "--format", "{{.State.Running}}", name, check=False)
    return result.returncode == 0 and result.stdout.strip() == "true"


def private_file(path: Path, content: str) -> None:
    write_private(path, content)


def secrets_data() -> dict[str, str]:
    external_path(SECRET_FILE)
    if SECRET_FILE.exists():
        return json.loads(SECRET_FILE.read_text())
    data = {"key": "morpheus" + secrets.token_hex(8), "secret": secrets.token_urlsafe(36)}
    private_file(SECRET_FILE, json.dumps(data) + "\n")
    return data


def write_configs() -> None:
    credentials = secrets_data()
    key, secret = credentials["key"], credentials["secret"]
    private_file(SFU_CONFIG, f"""port: 18011
rtc:
  use_external_ip: false
  node_ip: 127.0.0.1
  # Docker publishes packets to the container's network interface, not lo.
  # Including both creates duplicate NAT-rewritten 127.0.0.1 candidates whose
  # loopback socket cannot receive the host's media packets.
  enable_loopback_candidate: false
  udp_port: 18012
  tcp_port: 18013
room:
  auto_create: false
keys:
  {key}: {secret}
webhook:
  api_key: {key}
  urls:
    - http://{AUTH}:8080/sfu_webhook
""")
    # Docker Desktop's isolated loopback cannot complete ICE with a native Mac
    # client. The native SFU binds signalling and UDP media only to lo0; the
    # authorization container reaches its signalling API through host-gateway.
    private_file(NATIVE_SFU_CONFIG, f"""port: 18011
bind_addresses:
  - 127.0.0.1
rtc:
  use_external_ip: false
  node_ip: 127.0.0.1
  enable_loopback_candidate: true
  interfaces:
    includes:
      - lo0
  ips:
    includes:
      - 127.0.0.0/8
  udp_port: 18012
  tcp_port: 0
room:
  auto_create: false
keys:
  {key}: {secret}
webhook:
  api_key: {key}
  urls:
    - http://127.0.0.1:18010/sfu_webhook
""")
    private_file(AUTH_ENV, f"""LIVEKIT_URL={SFU_URL}
LIVEKIT_KEY={key}
LIVEKIT_SECRET={secret}
LIVEKIT_FULL_ACCESS_HOMESERVERS=morpheus-a.test
LIVEKIT_CS_API_URL_OVERRIDES=morpheus-a.test=http://{SYNAPSE}:8008
LIVEKIT_INSECURE_SKIP_VERIFY_TLS=YES_I_KNOW_WHAT_I_AM_DOING
""")
    private_file(SFU_LOOPBACK_SCRIPT, SFU_LOOPBACK_SOURCE)
    private_file(FEDERATION_PROXY, '''"""Private HTTPS bridge for local OpenID verification only."""
import http.server
import ssl
import urllib.error
import urllib.request


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if not self.path.startswith('/_matrix/federation/v1/openid/userinfo?'):
            self.send_error(404)
            return
        try:
            with urllib.request.urlopen(
                'http://morpheus-synapse-a:8008' + self.path, timeout=8
            ) as response:
                body = response.read()
                self.send_response(response.status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        except urllib.error.HTTPError as error:
            self.send_response(error.code)
            self.end_headers()
        except urllib.error.URLError:
            self.send_error(502)

    def log_message(self, format, *args):
        # The request URL contains a short-lived OpenID token.
        pass


server = http.server.ThreadingHTTPServer(('0.0.0.0', 8448), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain('/etc/rtc/federation.crt', '/etc/rtc/federation.key')
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
''')
    if not FEDERATION_CERT.exists() or not FEDERATION_KEY.exists():
        command = subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-keyout", str(FEDERATION_KEY), "-out", str(FEDERATION_CERT),
             "-days", "365", "-subj", "/CN=morpheus-a.test"],
            capture_output=True, text=True,
        )
        if command.returncode:
            raise RuntimeError("Could not generate local federation TLS certificate")
        os.chmod(FEDERATION_KEY, 0o600)
        os.chmod(FEDERATION_CERT, 0o600)


def configure_synapse() -> bool:
    if not running(SYNAPSE):
        raise RuntimeError("Start Synapse A with scripts/local_matrix_env.py up first")
    script = r'''
from pathlib import Path
path = Path('/data/homeserver.yaml')
config = path.read_text()
marker = '# morpheus local rtc test configuration'
if marker in config:
    assert 'livekit_service_url: http://127.0.0.1:18010' in config
    if '  msc4354_enabled: true' not in config:
        config = config.replace(
            '  msc4222_enabled: true\n',
            '  msc4222_enabled: true\n  msc4354_enabled: true\n',
            1,
        )
        path.write_text(config)
        print('updated')
    else:
        print('unchanged')
else:
    config += ''' + repr("\n\n# morpheus local rtc test configuration\nexperimental_features:\n  msc3266_enabled: true\n  msc4143_enabled: true\n  msc4222_enabled: true\n  msc4354_enabled: true\nmax_event_delay_duration: 24h\nrc_delayed_event_mgmt:\n  per_second: 1\n  burst_count: 20\nmatrix_rtc:\n  transports:\n    - type: livekit\n      livekit_service_url: http://127.0.0.1:18010\n") + r'''
    path.write_text(config)
    print('updated')
'''
    result = docker("exec", "--user", "root", SYNAPSE, "python3", "-c", script)
    return result.stdout.strip() == "updated"


def wait_http(url: str, *, expected: int = 200, duration: int = 40) -> None:
    deadline = time.monotonic() + duration
    while (remaining := deadline - time.monotonic()) > 0:
        try:
            with urllib.request.urlopen(url, timeout=min(3, remaining)) as response:
                if response.status == expected:
                    return
        except (urllib.error.URLError, ConnectionError, TimeoutError, http.client.HTTPException):
            # A published Docker port can reset before its service accepts
            # HTTP. urllib can surface that directly, outside URLError.
            pass
        time.sleep(min(1, max(0, deadline - time.monotonic())))
    raise RuntimeError(f"Service did not become ready: {url}")


def verify_discovery() -> None:
    matrix_env.stage("rtc_versions")
    with urllib.request.urlopen("http://127.0.0.1:18008/_matrix/client/versions", timeout=8) as response:
        flags = json.load(response)["unstable_features"]
    if not flags.get("org.matrix.simplified_msc3575") or not flags.get("org.matrix.msc4354"):
        raise RuntimeError("Synapse A lacks sliding sync or MSC4354 sticky events")
    accounts = json.loads((FIXTURE_HOME / "accounts.json").read_text())["accounts"]
    alice = next(account for account in accounts if account["label"] == "a_primary")
    login = urllib.request.Request(
        alice["homeserver_url"] + "/_matrix/client/v3/login",
        data=json.dumps({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": alice["username"]},
            "password": alice["password"],
        }).encode(),
        headers={"Content-Type": "application/json"},
    )
    matrix_env.stage("rtc_login")
    with urllib.request.urlopen(login, timeout=8) as response:
        token = json.load(response)["access_token"]
    request = urllib.request.Request(
        alice["homeserver_url"] + "/_matrix/client/unstable/org.matrix.msc4143/rtc/transports",
        headers={"Authorization": "Bearer " + token},
    )
    matrix_env.stage("rtc_transports")
    with urllib.request.urlopen(request, timeout=8) as response:
        transports = json.load(response)["rtc_transports"]
    if not any(item.get("type") == "livekit" and item.get("livekit_service_url") == DISCOVERY_URL for item in transports):
        raise RuntimeError("Synapse A did not advertise the expected LiveKit transport")


def probe() -> None:
    """Exercise authenticated discovery and MSC4195 token exchange, never printing tokens."""
    verify_discovery()
    fixture = json.loads((FIXTURE_HOME / "accounts.json").read_text())
    alice = next(account for account in fixture["accounts"] if account["label"] == "a_primary")
    login_request = urllib.request.Request(
        alice["homeserver_url"] + "/_matrix/client/v3/login",
        data=json.dumps({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": alice["username"]},
            "password": alice["password"],
        }).encode(),
        headers={"Content-Type": "application/json"},
    )
    matrix_env.stage("rtc_probe_login")
    with urllib.request.urlopen(login_request, timeout=8) as response:
        login = json.load(response)
    openid_request = urllib.request.Request(
        alice["homeserver_url"] + "/_matrix/client/v3/user/"
        + urllib.parse.quote(alice["user_id"], safe="") + "/openid/request_token",
        data=b"{}",
        headers={
            "Content-Type": "application/json",
            "Authorization": "Bearer " + login["access_token"],
        },
    )
    matrix_env.stage("rtc_openid")
    with urllib.request.urlopen(openid_request, timeout=8) as response:
        openid = json.load(response)
    token_request = urllib.request.Request(
        DISCOVERY_URL + "/get_token",
        data=json.dumps({
            "room_id": fixture["rooms"]["encrypted_room"],
            "slot_id": "m.call#ROOM",
            "openid_token": openid,
            "member": {
                "id": str(uuid.uuid4()),
                "claimed_user_id": alice["user_id"],
                "claimed_device_id": login["device_id"],
            },
        }).encode(),
        headers={"Content-Type": "application/json"},
    )
    matrix_env.stage("rtc_token_exchange")
    with urllib.request.urlopen(token_request, timeout=35) as response:
        token = json.load(response)
    if not token.get("jwt") or token.get("url") != SFU_URL:
        raise RuntimeError("RTC service returned no JWT or the wrong local SFU URL")
    matrix_env.stage("rtc_sfu_ready")
    wait_http("http://127.0.0.1:18011/")
    print("Authenticated RTC discovery and LiveKit JWT exchange passed; SFU is reachable.")


def native_sfu_pid() -> int | None:
    if not NATIVE_SFU_PID.exists():
        return None
    try:
        pid = int(NATIVE_SFU_PID.read_text().strip())
    except ValueError:
        return None
    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "args="], capture_output=True, text=True
    )
    if result.returncode or str(NATIVE_SFU_CONFIG) not in result.stdout:
        return None
    return pid


def stop_native_sfu() -> None:
    pid = native_sfu_pid()
    if pid is not None:
        os.kill(pid, signal.SIGTERM)
        for _ in range(50):
            if native_sfu_pid() is None:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("Native LiveKit did not stop after SIGTERM")
    NATIVE_SFU_PID.unlink(missing_ok=True)


def start_native_sfu() -> None:
    if native_sfu_pid() is not None:
        return
    try:
        with socket.create_connection(("127.0.0.1", 18011), timeout=0.25):
            raise RuntimeError("LiveKit port 18011 is occupied by an unmanaged process")
    except OSError:
        pass
    binary = shutil.which("livekit-server")
    if binary is None:
        raise RuntimeError("Install pinned LiveKit 1.13.9 first (`brew install livekit` on macOS)")
    version = subprocess.run([binary, "--version"], capture_output=True, text=True)
    if version.returncode or "version 1.13.9" not in version.stdout:
        raise RuntimeError("Native LiveKit must be version 1.13.9 for this fixture")
    external_path(NATIVE_SFU_LOG)
    fd = os.open(NATIVE_SFU_LOG, os.O_CREAT | os.O_APPEND | os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0), 0o600)
    with os.fdopen(fd, "a") as log:
        process = subprocess.Popen(
            [binary, "--config", str(NATIVE_SFU_CONFIG)],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    private_file(NATIVE_SFU_PID, f"{process.pid}\n")
    try:
        wait_http("http://127.0.0.1:18011/", duration=15)
        if process.poll() is not None:
            raise RuntimeError("Native LiveKit exited during startup; inspect its private log")
    except RuntimeError:
        stop_native_sfu()
        raise


def start_federation_bridge() -> None:
    present = exists("container", FEDERATION)
    if present:
        mounts = json.loads(docker("inspect", "--format", "{{json .Mounts}}", FEDERATION).stdout)
        source = next((mount.get("Source") for mount in mounts if mount.get("Destination") == "/etc/rtc"), None)
        if source is None or Path(source).resolve() != STATE.resolve():
            # A fresh fixture home has a new certificate and proxy script. Do
            # not restart a stale container bound to a deleted/older test home.
            # This container owns no Synapse data volume.
            docker("rm", "-f", FEDERATION)
            present = False
    if not present:
        docker(
            "run", "-d", "--name", FEDERATION, "--network", NETWORK,
            "--network-alias", "morpheus-a.test", "--entrypoint", "python3",
            "--mount", f"type=bind,src={STATE},dst=/etc/rtc,readonly",
            matrix_env.IMAGE,
            "-u", "/etc/rtc/federation_proxy.py",
        )
    elif not running(FEDERATION):
        docker("start", FEDERATION)


def start_authorization(*, native_sfu: bool) -> None:
    # The bridge owns the namespace and the existing loopback-only auth port.
    # The JWT service joins that namespace: its 127.0.0.1:18011 goes through
    # the bridge, while desktop clients' 127.0.0.1:18011 goes straight to SFU.
    # In Docker mode the bridge resolves the SFU's network alias. Native mode
    # (macOS Docker Desktop only) maps the same name to the host gateway.
    # Recreate both containers on mode changes; remove the namespace user first.
    matrix_env.stage("rtc_auth_bridge")
    if exists("container", AUTH):
        docker("rm", "-f", AUTH)
    if exists("container", SFU_LOOPBACK):
        docker("rm", "-f", SFU_LOOPBACK)
    args = [
        "run", "-d", "--name", SFU_LOOPBACK, "--network", NETWORK,
        "--network-alias", AUTH,
        "--publish", "127.0.0.1:18010:8080/tcp",
        "--entrypoint", "python3",
        "--mount", f"type=bind,src={SFU_LOOPBACK_SCRIPT},dst=/sfu_loopback.py,readonly",
    ]
    if native_sfu:
        args += ["--add-host", "morpheus-rtc.localhost:host-gateway"]
    # Reuse the already-required Synapse Python image; do not rebuild or alter
    # the pinned authorization service. No keys/config are mounted in bridge.
    args += [matrix_env.IMAGE, "-u", "/sfu_loopback.py"]
    docker(*args)
    try:
        matrix_env.stage("rtc_auth_service")
        docker(
            "run", "-d", "--name", AUTH,
            "--network", f"container:{SFU_LOOPBACK}",
            "--env-file", str(AUTH_ENV), AUTH_IMAGE,
        )
    except RuntimeError:
        docker("rm", "-f", AUTH, check=False)
        docker("rm", "-f", SFU_LOOPBACK, check=False)
        raise


def room_creation_id(events, sender: str) -> str:
    """Capture the exact immutable creation ID of this freshly created room."""
    if not isinstance(events, list) or len(events) > 64:
        raise RuntimeError("Fresh RTC room creation receipt unavailable")
    creates = [event for event in events if isinstance(event, dict) and event.get("type") == "m.room.create"]
    if len(creates) != 1:
        raise RuntimeError("Fresh RTC room creation receipt unavailable")
    event = creates[0]
    identity = event.get("event_id")
    if (event.get("sender") != sender or event.get("state_key") != ""
            or not isinstance(identity, str) or not 2 <= len(identity) <= 512
            or not identity.startswith("$") or any(ord(char) < 33 or ord(char) > 126 for char in identity)):
        raise RuntimeError("Fresh RTC room creation receipt rejected")
    return identity


def rtc_fixture(*, group: bool = False) -> None:
    """Create a fresh E2EE account pair, separate from message/recovery QA."""
    server = matrix_env.SERVERS[0]
    if not running(SYNAPSE):
        raise RuntimeError("Run scripts/local_matrix_env.py up first")
    nonce = secrets.token_hex(8)
    accounts = []
    users = [("a_primary", f"rtcalice{nonce}"), ("a_second_device", f"rtcbob{nonce}")]
    if group:
        users.append(("a_third_device", f"rtccarol{nonce}"))
    for label, username in users:
        accounts.append({
            "label": label,
            "username": username,
            "user_id": f"@{username}:{server['server_name']}",
            "homeserver_url": matrix_env.endpoint(server),
            "password": secrets.token_urlsafe(32),
        })
    data = {"accounts": accounts, "rooms": {}}
    path = STATE / f"accounts-{nonce}.json"
    # Keep generated passwords even if account registration is interrupted.
    private_file(path, json.dumps(data, indent=2) + "\n")
    alice, bob = accounts[:2]
    peers = accounts[1:]
    for account in accounts:
        matrix_env.register(account)
    room_id = matrix_env.create_room(alice, {
        "name": f"Morpheus native RTC {nonce}",
        "preset": "private_chat",
        "invite": [peer["user_id"] for peer in peers],
        "initial_state": [{
            "type": "m.room.encryption", "state_key": "",
            "content": {"algorithm": "m.megolm.v1.aes-sha2"},
        }],
    })
    data["rooms"]["encrypted_room"] = room_id
    private_file(path, json.dumps(data, indent=2) + "\n")
    matrix_env.api(
        matrix_env.endpoint(server),
        "/_matrix/client/v3/rooms/" + urllib.parse.quote(room_id, safe="") + "/join",
        token=matrix_env.login(bob)["access_token"], body={},
    )
    # Element Call's compatibility mode writes msc3401 membership as room
    # state. Give this disposable test peer enough power to publish it.
    alice_token = matrix_env.login(alice)["access_token"]
    power_path = (
        "/_matrix/client/v3/rooms/" + urllib.parse.quote(room_id, safe="")
        + "/state/m.room.power_levels/"
    )
    power = matrix_env.api(matrix_env.endpoint(server), power_path, token=alice_token)
    power.setdefault("users", {})[bob["user_id"]] = 100
    request = urllib.request.Request(
        matrix_env.endpoint(server) + power_path,
        data=json.dumps(power).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {alice_token}"},
        method="PUT",
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        json.load(response)
    # Keep a cleartext room in the fixture to verify that current-format
    # MatrixRTC rejects it before creating a call with an unmappable peer
    # identity. Encrypted media acceptance uses the room above.
    unencrypted_room_id = matrix_env.create_room(alice, {
        "name": f"Morpheus RTC unencrypted guard {nonce}",
        "preset": "private_chat",
        "invite": [peer["user_id"] for peer in peers],
    })
    data["rooms"]["unencrypted_room"] = unencrypted_room_id
    private_file(path, json.dumps(data, indent=2) + "\n")
    matrix_env.api(
        matrix_env.endpoint(server),
        "/_matrix/client/v3/rooms/" + urllib.parse.quote(unencrypted_room_id, safe="") + "/join",
        token=matrix_env.login(bob)["access_token"], body={},
    )
    if group:
        for room in (room_id, unencrypted_room_id):
            matrix_env.api(matrix_env.endpoint(server),
                           "/_matrix/client/v3/rooms/" + urllib.parse.quote(room, safe="") + "/join",
                           token=matrix_env.login(peers[1])["access_token"], body={})
    for account in accounts:
        joined = matrix_env.api(
            matrix_env.endpoint(server), "/_matrix/client/v3/joined_rooms",
            token=matrix_env.login(account)["access_token"],
        )["joined_rooms"]
        if room_id not in joined or unencrypted_room_id not in joined:
            raise RuntimeError(f"RTC fixture room absent for {account['label']}")
    if recovery_identity() is not None:
        events = matrix_env.api(matrix_env.endpoint(server),
            "/_matrix/client/v3/rooms/" + urllib.parse.quote(room_id, safe="") + "/state",
            token=alice_token)
        data["outage_creation_event_id"] = room_creation_id(events, alice["user_id"])
        private_file(path, json.dumps(data, indent=2) + "\n")
    print(path)


def up() -> None:
    matrix_env.stage("rtc_disk_check")
    if shutil.disk_usage(FIXTURE_HOME.parent).free < 8 * 1024**3:
        raise RuntimeError("Less than 8 GiB free; refusing to start MatrixRTC images")
    matrix_env.stage("rtc_network")
    if not exists("network", NETWORK):
        raise RuntimeError("Run scripts/local_matrix_env.py up first")
    matrix_env.stage("rtc_stop_native")
    labels = recovery_identity()
    if labels is None:
        stop_native_sfu()
    elif NATIVE_SFU_PID.exists():
        raise RuntimeError("Owned SFU cannot take over a native process")
    matrix_env.stage("rtc_configs")
    write_configs()
    matrix_env.stage("rtc_federation")
    start_federation_bridge()
    matrix_env.stage("rtc_sfu")
    if labels is not None and exists("container", SFU):
        raise RuntimeError("Owned SFU creation requires an absent container")
    if not exists("container", SFU):
        label_args = [part for key, value in (labels or {}).items() for part in ("--label", f"{key}={value}")]
        created = docker(
            "run", "-d", "--name", SFU, "--network", NETWORK, *label_args,
            "--network-alias", "morpheus-rtc.localhost",
            "--publish", "127.0.0.1:18011:18011/tcp",
            "--publish", "127.0.0.1:18012:18012/udp",
            "--publish", "127.0.0.1:18013:18013/tcp",
            "--mount", f"type=bind,src={SFU_CONFIG},dst=/etc/livekit.yaml,readonly",
            SFU_IMAGE, "--config", "/etc/livekit.yaml",
        )
        if labels is not None:
            container = created.stdout.strip()
            try:
                remember_sfu(container, labels)
            except BaseException:
                # The full ID was returned by this fresh creation, not recovered
                # from a name. Refuse unknown identity instead of deleting by name.
                inspect_owned_sfu(container, labels)
                owned_docker("container", "rm", "--force", container)
                raise
    elif not running(SFU):
        docker("start", SFU)
    start_authorization(native_sfu=False)
    matrix_env.stage("rtc_auth_ready")
    wait_http(DISCOVERY_URL + "/healthz")
    matrix_env.stage("rtc_synapse_configure")
    if configure_synapse():
        matrix_env.stage("rtc_synapse_restart")
        docker("restart", SYNAPSE)
    matrix_env.stage("rtc_synapse_ready")
    wait_http("http://127.0.0.1:18008/_matrix/client/versions")
    verify_discovery()
    print("Local MatrixRTC auth/SFU and Synapse A discovery are ready on loopback.")
    print("  Auth: " + DISCOVERY_URL)
    print("  LiveKit: " + SFU_URL)
    print("  Private configuration: " + str(STATE))


def up_native() -> None:
    if sys.platform != "darwin":
        raise RuntimeError("The loopback-only native SFU fixture is currently macOS-only")
    matrix_env.stage("rtc_network")
    if not exists("network", NETWORK):
        raise RuntimeError("Run scripts/local_matrix_env.py up first")
    matrix_env.stage("rtc_configs")
    write_configs()
    matrix_env.stage("rtc_federation")
    start_federation_bridge()
    matrix_env.stage("rtc_sfu")
    if running(SFU):
        docker("stop", SFU)
    start_native_sfu()
    start_authorization(native_sfu=True)
    matrix_env.stage("rtc_auth_ready")
    wait_http(DISCOVERY_URL + "/healthz")
    matrix_env.stage("rtc_synapse_configure")
    if configure_synapse():
        matrix_env.stage("rtc_synapse_restart")
        docker("restart", SYNAPSE)
    matrix_env.stage("rtc_synapse_ready")
    wait_http("http://127.0.0.1:18008/_matrix/client/versions")
    verify_discovery()
    print("Native loopback LiveKit and local MatrixRTC authorization are ready.")
    print("  Auth: " + DISCOVERY_URL)
    print("  LiveKit: " + SFU_URL)
    print("  Private configuration: " + str(STATE))


def status() -> None:
    for name in (SYNAPSE, FEDERATION, SFU, SFU_LOOPBACK, AUTH):
        print(f"{name}: {'running' if running(name) else 'stopped/absent'}")
    print("RTC config: " + (str(STATE) if STATE.exists() else "absent"))
    print("native LiveKit: " + ("running" if native_sfu_pid() is not None else "stopped"))


def down() -> None:
    labels = recovery_identity()
    if labels is None:
        stop_native_sfu()
        for name in (AUTH, SFU_LOOPBACK, SFU, FEDERATION):
            if exists("container", name):
                docker("rm", "-f", name)
    else:
        failures = []
        if NATIVE_SFU_PID.exists():
            failures.append("native_owner")
        for name in (AUTH, SFU_LOOPBACK, SFU, FEDERATION):
            try:
                if name == SFU:
                    if exists("container", SFU):
                        record, _ = recovery_sfu()
                        owned_docker("container", "rm", "--force", record["container"])
                        SFU_OWNER.unlink()
                elif exists("container", name):
                    docker("rm", "-f", name)
            except BaseException:
                # Keep trying the other fresh fixture resources, then reject a
                # clean receipt. Never delete a replacement SFU by its name.
                failures.append("container_owner")
        if failures:
            raise RuntimeError("Owned RTC fixture teardown incomplete")
    print("Local RTC containers stopped; private config retained. Synapse A remains running.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("up", "up-native", "rtc-fixture", "rtc-group-fixture", "sfu-outage", "sfu-resume", "probe", "status", "down"))
    args = parser.parse_args()
    {"up": up, "up-native": up_native, "rtc-fixture": rtc_fixture,
     "rtc-group-fixture": lambda: rtc_fixture(group=True), "sfu-outage": sfu_outage,
     "sfu-resume": sfu_resume, "probe": probe, "status": status, "down": down}[args.command]()


def run_cli() -> int:
    try:
        main()
    except Exception as error:
        # HTTP/Docker errors can carry tokens, private paths or server bodies.
        # Use the existing allowlisted channel; never format the exception.
        matrix_env.report_failure(error)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(run_cli())
