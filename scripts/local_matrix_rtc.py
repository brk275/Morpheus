#!/usr/bin/env python3
"""Run a disposable loopback MatrixRTC backend for the local Synapse A server.

The official LiveKit and lk-jwt-service images are used. API keys are generated
into the private temp test directory; no secret belongs in the repository.
Run ``scripts/local_matrix_env.py up`` first. This script only modifies the
disposable Synapse A volume and never changes Synapse B.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import secrets
import shutil
import signal
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


def rtc_fixture() -> None:
    """Create a fresh E2EE account pair, separate from message/recovery QA."""
    server = matrix_env.SERVERS[0]
    if not running(SYNAPSE):
        raise RuntimeError("Run scripts/local_matrix_env.py up first")
    nonce = secrets.token_hex(8)
    accounts = []
    for label, username in (
        ("a_primary", f"rtcalice{nonce}"),
        ("a_second_device", f"rtcbob{nonce}"),
    ):
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
    alice, bob = accounts
    matrix_env.register(alice)
    matrix_env.register(bob)
    room_id = matrix_env.create_room(alice, {
        "name": f"Morpheus native RTC {nonce}",
        "preset": "private_chat",
        "invite": [bob["user_id"]],
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
        "invite": [bob["user_id"]],
    })
    data["rooms"]["unencrypted_room"] = unencrypted_room_id
    private_file(path, json.dumps(data, indent=2) + "\n")
    matrix_env.api(
        matrix_env.endpoint(server),
        "/_matrix/client/v3/rooms/" + urllib.parse.quote(unencrypted_room_id, safe="") + "/join",
        token=matrix_env.login(bob)["access_token"], body={},
    )
    for account in accounts:
        joined = matrix_env.api(
            matrix_env.endpoint(server), "/_matrix/client/v3/joined_rooms",
            token=matrix_env.login(account)["access_token"],
        )["joined_rooms"]
        if room_id not in joined or unencrypted_room_id not in joined:
            raise RuntimeError(f"RTC fixture room absent for {account['label']}")
    print(path)


def up() -> None:
    matrix_env.stage("rtc_disk_check")
    if shutil.disk_usage(FIXTURE_HOME.parent).free < 8 * 1024**3:
        raise RuntimeError("Less than 8 GiB free; refusing to start MatrixRTC images")
    matrix_env.stage("rtc_network")
    if not exists("network", NETWORK):
        raise RuntimeError("Run scripts/local_matrix_env.py up first")
    matrix_env.stage("rtc_stop_native")
    stop_native_sfu()
    matrix_env.stage("rtc_configs")
    write_configs()
    matrix_env.stage("rtc_federation")
    start_federation_bridge()
    matrix_env.stage("rtc_sfu")
    if not exists("container", SFU):
        docker(
            "run", "-d", "--name", SFU, "--network", NETWORK,
            "--network-alias", "morpheus-rtc.localhost",
            "--publish", "127.0.0.1:18011:18011/tcp",
            "--publish", "127.0.0.1:18012:18012/udp",
            "--publish", "127.0.0.1:18013:18013/tcp",
            "--mount", f"type=bind,src={SFU_CONFIG},dst=/etc/livekit.yaml,readonly",
            SFU_IMAGE, "--config", "/etc/livekit.yaml",
        )
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
    stop_native_sfu()
    for name in (AUTH, SFU_LOOPBACK, SFU, FEDERATION):
        if exists("container", name):
            docker("rm", "-f", name)
    print("Local RTC containers stopped; private config retained. Synapse A remains running.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("up", "up-native", "rtc-fixture", "probe", "status", "down"))
    args = parser.parse_args()
    {"up": up, "up-native": up_native, "rtc-fixture": rtc_fixture,
     "probe": probe, "status": status, "down": down}[args.command]()


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
