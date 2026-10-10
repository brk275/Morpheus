#!/usr/bin/env python3
"""Run disposable local Matrix homeservers for Morpheus acceptance testing.

Requires Docker Desktop/Engine. All endpoints bind to loopback. Credentials and
server data remain local; nothing in this script registers on a public server.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


IMAGE = "ghcr.io/element-hq/synapse:v1.162.0"
NETWORK = "morpheus-matrix-test"
HOME = Path(os.environ.get("MORPHEUS_MATRIX_TEST_HOME", Path(tempfile.gettempdir()) / "morpheus-matrix-test"))
CREDS = HOME / "accounts.json"
SERVERS = (
    {"name": "a", "container": "morpheus-synapse-a", "volume": "morpheus-synapse-a-data", "server_name": "morpheus-a.test", "port": 18008},
    {"name": "b", "container": "morpheus-synapse-b", "volume": "morpheus-synapse-b-data", "server_name": "morpheus-b.test", "port": 18009},
)
TOKENS: dict[str, dict] = {}


def docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(["docker", *args], text=True, capture_output=True)
    if check and result.returncode:
        # Registration can put a password in argv. Never include argv in errors.
        raise RuntimeError(f"Docker command failed: {result.stderr.strip()[-1000:]}")
    return result


def exists(kind: str, name: str) -> bool:
    return docker(kind, "inspect", name, check=False).returncode == 0


def endpoint(server: dict[str, object]) -> str:
    return f"http://127.0.0.1:{server['port']}"


def api(url: str, path: str, *, token: str | None = None, body: dict | None = None) -> dict:
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(
        url + path,
        data=json.dumps(body).encode() if body is not None else None,
        headers=headers,
        method="POST" if body is not None else "GET",
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)


def wait_ready(server: dict[str, object]) -> None:
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        try:
            result = api(endpoint(server), "/_matrix/client/versions")
            if result.get("versions"):
                return
        except (urllib.error.URLError, TimeoutError, ValueError, http.client.HTTPException):
            time.sleep(2)
    raise RuntimeError(f"Synapse {server['name']} did not become ready at {endpoint(server)}")


def configure(server: dict[str, object]) -> bool:
    script = r'''
from pathlib import Path
import re
import secrets

path = Path('/data/homeserver.yaml')
config = path.read_text()
original = config
if not re.search(r'^registration_shared_secret:', config, re.MULTILINE):
    config += "\nregistration_shared_secret: '" + secrets.token_hex(32) + "'\n"
if not re.search(r'^public_baseurl:', config, re.MULTILINE):
    config += "public_baseurl: '" + __import__('sys').argv[1] + "/'\n"
if not re.search(r'^rc_login:', config, re.MULTILINE):
    config += """rc_login:
  address: {per_second: 10, burst_count: 100}
  account: {per_second: 10, burst_count: 100}
  failed_attempts: {per_second: 10, burst_count: 100}
"""
if config != original:
    path.write_text(config)
print('updated' if config != original else 'unchanged')
'''
    result = docker(
        "run", "--rm", "--user", "root", "--entrypoint", "python3",
        "--mount", f"type=volume,src={server['volume']},dst=/data",
        IMAGE, "-c", script, endpoint(server),
    )
    return result.stdout.strip() == "updated"


def start_servers() -> None:
    if shutil.disk_usage(HOME.parent).free < 2 * 1024**3:
        raise RuntimeError("At least 2 GiB free space is required before starting local Synapse")
    if not exists("network", NETWORK):
        docker("network", "create", NETWORK)
    for server in SERVERS:
        if not exists("volume", str(server["volume"])):
            docker("volume", "create", str(server["volume"]))
            docker(
                "run", "--rm",
                "--mount", f"type=volume,src={server['volume']},dst=/data",
                "-e", f"SYNAPSE_SERVER_NAME={server['server_name']}",
                "-e", "SYNAPSE_REPORT_STATS=no",
                IMAGE, "generate",
            )
        changed = configure(server)
        if exists("container", str(server["container"])):
            state = docker("inspect", "--format", "{{.State.Running}}", str(server["container"])).stdout.strip()
            if state != "true":
                docker("start", str(server["container"]))
            elif changed:
                docker("restart", str(server["container"]))
        else:
            docker(
                "run", "-d", "--name", str(server["container"]),
                "--network", NETWORK,
                "--publish", f"127.0.0.1:{server['port']}:8008",
                "--mount", f"type=volume,src={server['volume']},dst=/data",
                IMAGE,
            )
        wait_ready(server)


def credential_data() -> dict:
    if CREDS.exists():
        return json.loads(CREDS.read_text())
    HOME.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(HOME, 0o700)
    accounts = []
    for label, username, server in (
        ("a_primary", "alice", SERVERS[0]),
        ("a_second_device", "bob", SERVERS[0]),
        ("b_primary", "carol", SERVERS[1]),
    ):
        accounts.append({
            "label": label,
            "username": username,
            "user_id": f"@{username}:{server['server_name']}",
            "homeserver_url": endpoint(server),
            "password": secrets.token_urlsafe(32),
        })
    data = {"image": IMAGE, "accounts": accounts, "rooms": {}}
    save_credentials(data, exclusive=True)
    return data


def save_credentials(data: dict, *, exclusive: bool = False) -> None:
    HOME.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(HOME, 0o700)
    temporary = CREDS.with_suffix(".json.tmp")
    fd = os.open(temporary, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
    with os.fdopen(fd, "w") as file:
        json.dump(data, file, indent=2)
        file.write("\n")
    os.chmod(temporary, 0o600)
    if exclusive and CREDS.exists():
        temporary.unlink()
        raise RuntimeError("Credential file appeared concurrently; retry")
    temporary.replace(CREDS)


def login(account: dict) -> dict:
    if account["user_id"] in TOKENS:
        return TOKENS[account["user_id"]]
    result = api(account["homeserver_url"], "/_matrix/client/v3/login", body={
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": account["username"]},
        "password": account["password"],
    })
    TOKENS[account["user_id"]] = result
    return result


def register(account: dict) -> None:
    try:
        login(account)
        return
    except urllib.error.HTTPError as error:
        if error.code not in (400, 403):
            raise
    server = next(item for item in SERVERS if endpoint(item) == account["homeserver_url"])
    docker(
        "exec", str(server["container"]), "register_new_matrix_user",
        "--no-admin", "-u", account["username"], "-p", account["password"],
        "-c", "/data/homeserver.yaml", "http://localhost:8008",
    )
    if login(account)["user_id"] != account["user_id"]:
        raise RuntimeError(f"Unexpected user ID for {account['label']}")


def create_room(account: dict, body: dict) -> str:
    token = login(account)["access_token"]
    return api(account["homeserver_url"], "/_matrix/client/v3/createRoom", token=token, body=body)["room_id"]


def seed_rooms(data: dict) -> None:
    accounts = {account["label"]: account for account in data["accounts"]}
    rooms = data["rooms"]
    for key, label in (("a_room", "Morpheus A only"), ("b_room", "Morpheus B only")):
        if key not in rooms:
            account = accounts["a_primary" if key == "a_room" else "b_primary"]
            rooms[key] = create_room(account, {"name": label, "preset": "private_chat"})
            save_credentials(data)
    if "encrypted_room" not in rooms:
        alice, bob = accounts["a_primary"], accounts["a_second_device"]
        room_id = create_room(alice, {
            "name": "Morpheus encrypted test",
            "preset": "private_chat",
            "invite": [bob["user_id"]],
            "initial_state": [{"type": "m.room.encryption", "state_key": "", "content": {"algorithm": "m.megolm.v1.aes-sha2"}}],
        })
        rooms["encrypted_room"] = room_id
        save_credentials(data)
    bob = accounts["a_second_device"]
    bob_token = login(bob)["access_token"]
    joined = api(bob["homeserver_url"], "/_matrix/client/v3/joined_rooms", token=bob_token)["joined_rooms"]
    if rooms["encrypted_room"] not in joined:
        api(bob["homeserver_url"], "/_matrix/client/v3/rooms/" + urllib.parse.quote(rooms["encrypted_room"], safe="") + "/join", token=bob_token, body={})


def verify(data: dict) -> None:
    tokens = {}
    for account in data["accounts"]:
        result = login(account)
        if result["user_id"] != account["user_id"]:
            raise RuntimeError(f"Unexpected user ID for {account['label']}")
        tokens[account["label"]] = result["access_token"]
    rooms = data["rooms"]
    for label, room_key in (("a_primary", "a_room"), ("a_primary", "encrypted_room"), ("a_second_device", "encrypted_room"), ("b_primary", "b_room")):
        account = next(item for item in data["accounts"] if item["label"] == label)
        joined = api(account["homeserver_url"], "/_matrix/client/v3/joined_rooms", token=tokens[label])["joined_rooms"]
        if rooms[room_key] not in joined:
            raise RuntimeError(f"Expected room absent for {label}")
    encryption = api(
        next(item for item in data["accounts"] if item["label"] == "a_primary")["homeserver_url"],
        "/_matrix/client/v3/rooms/" + urllib.parse.quote(rooms["encrypted_room"], safe="") + "/state/m.room.encryption",
        token=tokens["a_primary"],
    )
    if encryption.get("algorithm") != "m.megolm.v1.aes-sha2":
        raise RuntimeError("Encrypted test room lacks Megolm state")
    alice = next(item for item in data["accounts"] if item["label"] == "a_primary")
    carol = next(item for item in data["accounts"] if item["label"] == "b_primary")
    try:
        api(carol["homeserver_url"], "/_matrix/client/v3/joined_rooms", token=tokens["a_primary"])
    except urllib.error.HTTPError as error:
        if error.code != 401:
            raise RuntimeError(f"Cross-server token rejection returned {error.code}") from error
    else:
        raise RuntimeError("Server B accepted server A token")
    try:
        api(alice["homeserver_url"], "/_matrix/client/v3/joined_rooms", token=tokens["b_primary"])
    except urllib.error.HTTPError as error:
        if error.code != 401:
            raise RuntimeError(f"Cross-server token rejection returned {error.code}") from error
    else:
        raise RuntimeError("Server A accepted server B token")


def save_recovery_fixture(path: Path, data: dict, *, create: bool = False) -> None:
    """Keep a recovery run's secrets separate from the app QA fixture."""
    HOME.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(HOME, 0o700)
    if create:
        fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        temporary = path
    else:
        fd, name = tempfile.mkstemp(prefix=".recovery-", dir=HOME)
        temporary = Path(name)
    try:
        with os.fdopen(fd, "w") as file:
            json.dump(data, file, indent=2)
            file.write("\n")
        os.chmod(temporary, 0o600)
        if not create:
            temporary.replace(path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def recovery_fixture() -> None:
    """Create a fresh encrypted-room pair without changing accounts.json."""
    server = SERVERS[0]
    if not exists("container", str(server["container"])):
        raise RuntimeError("Start local Synapse A with `up` before creating a recovery fixture")
    api(endpoint(server), "/_matrix/client/versions")

    nonce = secrets.token_hex(8)
    accounts = []
    for label, username in (
        ("a_primary", f"recalice{nonce}"),
        ("a_second_device", f"recbob{nonce}"),
    ):
        accounts.append({
            "label": label,
            "username": username,
            "user_id": f"@{username}:{server['server_name']}",
            "homeserver_url": endpoint(server),
            "password": secrets.token_urlsafe(32),
        })
    data = {"image": IMAGE, "accounts": accounts, "rooms": {}}
    path = HOME / f"recovery-{nonce}.json"
    # Persist the generated passwords before mutating Synapse, so an interrupted
    # registration never leaves a test account whose credential was discarded.
    save_recovery_fixture(path, data, create=True)

    alice, bob = accounts
    register(alice)
    register(bob)
    room_id = create_room(alice, {
        "name": f"Morpheus recovery {nonce}",
        "preset": "private_chat",
        "invite": [bob["user_id"]],
        "initial_state": [{
            "type": "m.room.encryption",
            "state_key": "",
            "content": {"algorithm": "m.megolm.v1.aes-sha2"},
        }],
    })
    data["rooms"]["encrypted_room"] = room_id
    save_recovery_fixture(path, data)
    bob_token = login(bob)["access_token"]
    api(
        endpoint(server),
        "/_matrix/client/v3/rooms/" + urllib.parse.quote(room_id, safe="") + "/join",
        token=bob_token,
        body={},
    )
    for account in accounts:
        token = login(account)["access_token"]
        if login(account)["user_id"] != account["user_id"]:
            raise RuntimeError(f"Unexpected user ID for {account['label']}")
        joined = api(endpoint(server), "/_matrix/client/v3/joined_rooms", token=token)
        if room_id not in joined["joined_rooms"]:
            raise RuntimeError(f"Recovery room absent for {account['label']}")
    encryption = api(
        endpoint(server),
        "/_matrix/client/v3/rooms/" + urllib.parse.quote(room_id, safe="") + "/state/m.room.encryption",
        token=login(alice)["access_token"],
    )
    if encryption.get("algorithm") != "m.megolm.v1.aes-sha2":
        raise RuntimeError("Recovery fixture room lacks Megolm state")
    print(path)


def up() -> None:
    start_servers()
    data = credential_data()
    for account in data["accounts"]:
        register(account)
    seed_rooms(data)
    verify(data)
    print("Two loopback Synapse homeservers are ready and account/room checks passed.")
    for server in SERVERS:
        print(f"  {server['server_name']}: {endpoint(server)}")
    print(f"Private credentials: {CREDS}")


def status() -> None:
    for server in SERVERS:
        state = docker("inspect", "--format", "{{.State.Status}}", str(server["container"]), check=False)
        print(f"{server['server_name']}: {state.stdout.strip() if state.returncode == 0 else 'absent'} ({endpoint(server)})")
    print(f"Credentials: {CREDS if CREDS.exists() else 'absent'}")


def down() -> None:
    for server in SERVERS:
        if exists("container", str(server["container"])):
            docker("rm", "-f", str(server["container"]))
    if exists("network", NETWORK):
        docker("network", "rm", NETWORK)
    print("Local Synapse containers stopped; test volumes and credentials retained.")


def destroy() -> None:
    down()
    for server in SERVERS:
        if exists("volume", str(server["volume"])):
            docker("volume", "rm", str(server["volume"]))
    if CREDS.exists():
        CREDS.unlink()
    for fixture in HOME.glob("recovery-*.json"):
        fixture.unlink()
    print("Local Matrix test data and credentials removed; shared image retained.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("up", "recovery-fixture", "status", "down", "destroy"))
    args = parser.parse_args()
    {"up": up, "recovery-fixture": recovery_fixture, "status": status, "down": down, "destroy": destroy}[args.command]()


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError, urllib.error.URLError, OSError, KeyError) as error:
        print(f"Local Matrix environment failed: {error}", file=sys.stderr)
        raise SystemExit(1)
