#!/usr/bin/env python3
"""Serve the immutable official Element Call release for local Linux acceptance.

Only stock application assets are served. Deployment JSON and nginx binding are
configured outside the checkout. This helper never accepts account credentials.
``up`` prints only its private metadata path; diagnostics are fixed enums on
stderr. Import ``up``/``down`` for use by the acceptance harness.
"""

from __future__ import annotations

import argparse
from enum import Enum
import hashlib
import http.client
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


VERSION = "v0.28.2"
REVISION = "b4dd44bd78c1296fb8c28519bfc41cb488d636fd"
SDK_REVISION = "3f05dc4d104a3dfc753c5e3549a18c74fab4b076"
IMAGE_REPOSITORY = "ghcr.io/element-hq/element-call"
# Published multi-platform manifest, verified by registry HTTP and OCI labels.
IMAGE_DIGEST = "sha256:c61fc9e4e04f75690c693d4556d01c09a2b9246f3fc8d35222d3aac36ba29906"
IMAGE = f"{IMAGE_REPOSITORY}:{VERSION}@{IMAGE_DIGEST}"
OWNER_LABEL = "io.github.brk275.morpheus.element-peer-fixture"
STATE_FILE = "element-peer-owner.json"
FIXTURE_FILE = "element-peer.json"
MAX_BODY = 1024 * 1024

PATCHED_SDK_KIND = "patched_sdk_element_real_crypto_diagnostic"
SDK_PRODUCTION_FILES = {
    "src/matrixrtc/MembershipManager.ts": "ea3655bd51c665bc6dba35564bedbe477cc919e5798ca80c6f9d7929f0a01427",
    "src/matrixrtc/MembershipManagerActionScheduler.ts": "442c36ebd042ad85aeb076d3980512926da8abe4265affcb4ec8d84f65baf2e1",
}
SDK_PATCH_SHA256 = "ff0adceb780806c43f11a5831b69372c6342242c043a21986e0e70d34ee1c6e5"
MAX_ASSET_FILES = 16_384
MAX_ASSET_FILE_BYTES = 32 * 1024 * 1024
MAX_ASSET_TOTAL_BYTES = 512 * 1024 * 1024


def patched_sdk_assets(receipt: Path, expected_sha256: str) -> tuple[Path, dict]:
    """Validate an independently bound build receipt before mounting any assets.

    This is an explicit diagnostic entrypoint. A receipt cannot turn source-built
    assets into evidence about the application's unchanged official image.
    Every asset is checked, including source maps; links and extra files fail.
    """
    def identity(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_nlink,
                info.st_size, info.st_mtime_ns, info.st_ctime_ns)

    def regular_bytes(path, limit):
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_nlink != 1 or info.st_size > limit
                or info.st_mode & 0o022):
            raise FixtureError("invalid diagnostic asset")
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd, "rb") as stream:
            opened = os.fstat(stream.fileno())
            if identity(opened) != identity(info):
                raise FixtureError("diagnostic asset changed")
            body = stream.read(limit + 1)
            if len(body) != info.st_size or identity(os.fstat(stream.fileno())) != identity(opened):
                raise FixtureError("diagnostic asset changed")
        return body

    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError
            result[key] = value
        return result

    def mount_path(value):
        # Docker --mount parses comma-delimited fields. Neither the receipt nor
        # its asset directory may introduce a mount option or control byte.
        if not isinstance(value, str) or any(char in value for char in (",", "\r", "\n", "\x00")):
            raise ValueError
        return Path(value)

    try:
        if not re.fullmatch(r"[a-f0-9]{64}", expected_sha256):
            raise ValueError
        receipt = mount_path(str(receipt))
        unresolved_receipt = receipt.expanduser().absolute()
        receipt = external_path(receipt)
        if receipt != unresolved_receipt:
            raise ValueError
        body = regular_bytes(receipt, 4 * MAX_BODY)
        if hashlib.sha256(body).hexdigest() != expected_sha256:
            raise ValueError
        data = json.loads(body, object_pairs_hook=unique_object)
        if (set(data) != {"schema", "kind", "element_revision", "sdk_revision",
                          "sdk_patch_sha256", "sdk_production_files", "stock_interoperability",
                          "official_application_assets", "assets", "inventory", "build_inputs_sha256"}
                or type(data["schema"]) is not int or data["schema"] != 1 or data["kind"] != PATCHED_SDK_KIND
                or data["element_revision"] != REVISION or data["sdk_revision"] != SDK_REVISION
                or data["sdk_patch_sha256"] != SDK_PATCH_SHA256
                or data["sdk_production_files"] != SDK_PRODUCTION_FILES
                or data["stock_interoperability"] is not False
                or data["official_application_assets"] is not False
                or not re.fullmatch(r"[a-f0-9]{64}", data["build_inputs_sha256"])):
            raise ValueError
        unresolved_assets = mount_path(data["assets"])
        assets = external_path(unresolved_assets)
        if not unresolved_assets.is_absolute() or unresolved_assets != assets:
            raise ValueError
        if (assets == receipt.parent or not assets.is_dir()
                or assets.stat().st_uid != os.getuid() or assets.stat().st_mode & 0o077):
            raise ValueError
        inventory = data["inventory"]
        if not isinstance(inventory, dict) or not 1 <= len(inventory) <= MAX_ASSET_FILES:
            raise ValueError
        total = 0
        found = set()
        # Inventory is bounded before traversal. Reject unexpected directories
        # too, so a deeply nested or link-filled tree cannot escape these bounds.
        stack = [(assets, 0)]
        entries = 0
        while stack:
            directory, depth = stack.pop()
            if depth > 16:
                raise ValueError
            for item in directory.iterdir():
                entries += 1
                if entries > 2 * MAX_ASSET_FILES:
                    raise ValueError
                info = item.lstat()
                if (info.st_uid != os.getuid() or info.st_mode & 0o022
                        or stat.S_ISLNK(info.st_mode)):
                    raise ValueError
                relative = item.relative_to(assets).as_posix()
                if not re.fullmatch(r"[A-Za-z0-9_@+.-]+(?:/[A-Za-z0-9_@+.-]+)*", relative):
                    raise ValueError
                if stat.S_ISDIR(info.st_mode):
                    stack.append((item, depth + 1))
                    continue
                if relative == "config.json" or relative not in inventory:
                    raise ValueError
                fact = inventory[relative]
                if (not isinstance(fact, dict) or set(fact) != {"bytes", "sha256"}
                        or type(fact["bytes"]) is not int
                        or not 0 <= fact["bytes"] <= MAX_ASSET_FILE_BYTES
                        or not re.fullmatch(r"[a-f0-9]{64}", fact["sha256"])):
                    raise ValueError
                content = regular_bytes(item, MAX_ASSET_FILE_BYTES)
                total += len(content)
                if (total > MAX_ASSET_TOTAL_BYTES or len(content) != fact["bytes"]
                        or hashlib.sha256(content).hexdigest() != fact["sha256"]):
                    raise ValueError
                found.add(relative)
        if found != set(inventory) or "index.html" not in found:
            raise ValueError
        return assets, data
    except (OSError, ValueError, TypeError, KeyError, RecursionError):
        raise FixtureError("invalid patched SDK build receipt") from None


class Stage(str, Enum):
    ROOT = "element_fixture_root"
    CONFIGURATION = "element_configuration"
    PULL = "element_image_pull"
    IMAGE = "element_image_verify"
    CREATE = "element_container_create"
    START = "element_container_start"
    READY = "element_ready"
    CLEANUP = "element_cleanup"


_stage = Stage.ROOT


class FixtureError(RuntimeError):
    """Only static descriptions; subprocess diagnostics stay in private files."""


def set_stage(stage: Stage):
    global _stage
    if not isinstance(stage, Stage):
        raise TypeError("stage must be a fixed enum")
    _stage = stage
    print("MORPHEUS_ELEMENT_STAGE " + stage.value, file=sys.stderr, flush=True)


def loopback_url(value: str) -> str:
    """Accept only unambiguous credential-free local HTTP service origins."""
    if not isinstance(value, str) or re.search(r"[\s\\\x00-\x1f\x7f]", value):
        raise FixtureError("invalid loopback service origin")
    try:
        parsed = urllib.parse.urlsplit(value)
        port = parsed.port
    except ValueError:
        raise FixtureError("invalid loopback service origin") from None
    if (parsed.scheme != "http" or parsed.hostname != "127.0.0.1"
            or parsed.username is not None or parsed.password is not None
            or port is None or not 1 <= port <= 65535
            or parsed.path not in ("", "/") or parsed.query or parsed.fragment
            or parsed.netloc != f"127.0.0.1:{port}"):
        raise FixtureError("invalid loopback service origin")
    return f"http://127.0.0.1:{port}"


def configuration(homeserver_url: str, livekit_service_url: str, server_name: str) -> dict:
    homeserver_url = loopback_url(homeserver_url)
    livekit_service_url = loopback_url(livekit_service_url)
    if (not isinstance(server_name, str)
            or not re.fullmatch(r"[a-z0-9][a-z0-9.-]{0,252}(?::[1-9][0-9]{0,4})?", server_name)):
        raise FixtureError("invalid fixture server name")
    return {
        "default_server_config": {"m.homeserver": {
            "base_url": homeserver_url, "server_name": server_name,
        }},
        "livekit": {"livekit_service_url": livekit_service_url},
        "matrix_rtc_mode": "matrix_2_0",
    }


def nginx_configuration(port: int, *, patched_sdk: bool = False) -> str:
    if type(port) is not int or not 1 <= port <= 65535:
        raise FixtureError("invalid fixture listener")
    if type(patched_sdk) is not bool:
        raise FixtureError("invalid application asset mode")
    app_root = "/patched-sdk-app" if patched_sdk else "/app"
    config_alias = "alias /fixture/config.json; " if patched_sdk else ""
    # Stock mode uses untouched /app. Diagnostic mode has a distinct readonly
    # root; only exact /config.json can expose the private deployment config.
    return f"""pid /tmp/nginx.pid;
master_process off;
error_log /fixture/nginx-error.log warn;
events {{ worker_connections 256; }}
http {{
    include /etc/nginx/mime.types;
    default_type application/octet-stream;
    access_log off;
    client_body_temp_path /tmp/client_temp;
    proxy_temp_path /tmp/proxy_temp;
    fastcgi_temp_path /tmp/fastcgi_temp;
    uwsgi_temp_path /tmp/uwsgi_temp;
    scgi_temp_path /tmp/scgi_temp;
    server {{
        listen 127.0.0.1:{port};
        server_name localhost;
        root {app_root};
        gzip_static on;
        gzip_vary on;
        location = /config.json {{ {config_alias}add_header Cache-Control "no-store"; }}
        location / {{
            try_files $uri $uri/ /index.html;
            add_header Cache-Control "public, max-age=30, stale-while-revalidate=30";
        }}
        location /assets {{ add_header Cache-Control "public, immutable, max-age=31536000"; }}
        location /apple-app-site-association {{ default_type application/json; }}
    }}
}}
"""


class Docker:
    def __init__(self, root: Path, *, deadline: float):
        self.root = root
        self.deadline = deadline

    def run(self, *args: str, capture: bool = True, timeout: int = 30) -> subprocess.CompletedProcess:
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise FixtureError("fixture operation exceeded deadline")
        log = self.root / ("docker-" + secrets.token_hex(8) + ".log")
        try:
            fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "wb") as output:
                result = subprocess.run(
                    ["docker", *args], stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE if capture else output, stderr=output,
                    timeout=min(timeout, remaining), check=False, umask=0o077,
                )
        except (OSError, subprocess.SubprocessError):
            raise FixtureError("fixture Docker operation could not complete") from None
        if result.returncode:
            raise FixtureError("fixture Docker operation failed")
        if capture and len(result.stdout) > MAX_BODY:
            raise FixtureError("fixture Docker response exceeded limit")
        return result


def verify_image(docker: Docker):
    try:
        entries = json.loads(docker.run("image", "inspect", IMAGE).stdout)
        if not isinstance(entries, list) or len(entries) != 1:
            raise ValueError
        entry = entries[0]
        labels = entry["Config"]["Labels"]
        if (not isinstance(labels, dict)
                or f"{IMAGE_REPOSITORY}@{IMAGE_DIGEST}" not in entry["RepoDigests"]
                or labels.get("org.opencontainers.image.revision") != REVISION
                or labels.get("org.opencontainers.image.version") != VERSION
                or labels.get("org.opencontainers.image.source") != "https://github.com/element-hq/element-call"):
            raise ValueError
    except (ValueError, KeyError, TypeError):
        raise FixtureError("official fixture image identity did not match") from None


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def wait_ready(base_url: str, config: dict, *, deadline: float):
    base_url = loopback_url(base_url)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    while time.monotonic() < deadline:
        try:
            with opener.open(base_url + "/config.json", timeout=3) as response:
                body = response.read(MAX_BODY + 1)
                if len(body) > MAX_BODY or json.loads(body) != config:
                    raise FixtureError("served fixture configuration did not match")
            with opener.open(base_url + "/login", timeout=3) as response:
                body = response.read(MAX_BODY + 1)
                if len(body) <= MAX_BODY and b"<html" in body.lower() and b"/assets/" in body:
                    return
        except (urllib.error.URLError, TimeoutError, ValueError, http.client.HTTPException):
            pass
        time.sleep(min(0.5, max(0, deadline - time.monotonic())))
    raise FixtureError("official fixture server did not become ready")


def checked_root(path: Path) -> Path:
    if path.expanduser().is_symlink():
        raise FixtureError("fixture root must not be a symbolic link")
    return external_path(path)


def save_state(root: Path, state: dict):
    write_private(root / STATE_FILE, json.dumps(state) + "\n")


def up(root: Path, homeserver_url: str = "http://127.0.0.1:18008",
       livekit_service_url: str = "http://127.0.0.1:18010",
       server_name: str = "morpheus-a.test") -> dict:
    """Start only the unchanged stock application assets."""
    return _up(root, homeserver_url, livekit_service_url, server_name)


def up_patched_sdk(root: Path, receipt: Path, expected_sha256: str,
                   homeserver_url: str = "http://127.0.0.1:18008",
                   livekit_service_url: str = "http://127.0.0.1:18010",
                   server_name: str = "morpheus-a.test") -> dict:
    """Explicit diagnostic companion; never supplies stock compatibility proof."""
    assets, data = patched_sdk_assets(receipt, expected_sha256)
    return _up(root, homeserver_url, livekit_service_url, server_name,
               patched=(assets, data, expected_sha256))


def _up(root, homeserver_url, livekit_service_url, server_name, *, patched=None):
    """Start one owned peer with explicitly selected assets and local URL.

    Overall Docker/startup deadline is 420 seconds (image pull at most 300).
    Failed setup attempts remove owned containers and preserve private data.
    """
    set_stage(Stage.ROOT)
    if sys.platform != "linux":
        raise FixtureError("fixture requires Linux Docker host networking")
    config = configuration(homeserver_url, livekit_service_url, server_name)
    root = checked_root(root)
    if root.exists() and any(root.iterdir()):
        raise FixtureError("fixture requires a fresh empty private directory")
    private_directory(root)
    state = {"schema": 1, "owner": secrets.token_hex(16), "container_id": None}
    save_state(root, state)
    deadline = time.monotonic() + 420
    docker = Docker(root, deadline=deadline)
    reservation = None
    try:
        set_stage(Stage.CONFIGURATION)
        reservation = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        reservation.bind(("127.0.0.1", 0))
        reservation.listen(1)
        port = reservation.getsockname()[1]
        base_url = f"http://127.0.0.1:{port}"
        write_private(root / "config.json", json.dumps(config) + "\n", create=True)
        write_private(root / "nginx.conf", nginx_configuration(port, patched_sdk=patched is not None), create=True)
        # nginx's error file remains owner-only regardless of the image umask.
        write_private(root / "nginx-error.log", "", create=True)
        set_stage(Stage.PULL)
        docker.run("pull", IMAGE, capture=False, timeout=300)
        set_stage(Stage.IMAGE)
        verify_image(docker)
        set_stage(Stage.CREATE)
        cidfile = root / "element-peer.cid"
        app_mount = (f"type=bind,src={patched[0]},dst=/patched-sdk-app,readonly" if patched
                     else f"type=bind,src={root / 'config.json'},dst=/app/config.json,readonly")
        result = docker.run(
            "create", "--name", "morpheus-element-" + state["owner"],
            "--cidfile", str(cidfile), "--label", OWNER_LABEL + "=" + state["owner"],
            "--network", "host", "--log-driver", "none", "--read-only",
            "--tmpfs", "/tmp:rw,noexec,nosuid,size=32m", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--user", f"{os.getuid()}:{os.getgid()}",
            "--mount", f"type=bind,src={root},dst=/fixture",
            "--mount", app_mount,
            "--entrypoint", "nginx", IMAGE, "-c", "/fixture/nginx.conf", "-g", "daemon off;",
        )
        cid = result.stdout.decode().strip()
        if not re.fullmatch(r"[0-9a-f]{64}", cid):
            raise FixtureError("fixture received an invalid container identity")
        if cidfile.is_symlink():
            raise FixtureError("invalid fixture identity file")
        if cidfile.exists():
            os.chmod(cidfile, 0o600)
        state["container_id"] = cid
        save_state(root, state)
        set_stage(Stage.START)
        reservation.close()
        reservation = None
        docker.run("start", cid)
        set_stage(Stage.READY)
        wait_ready(base_url, config, deadline=min(deadline, time.monotonic() + 60))
        metadata = {
            "schema": 1, "base_url": base_url, "version": VERSION,
            "revision": REVISION, "image": IMAGE, "image_digest": IMAGE_DIGEST,
            "sdk_revision": SDK_REVISION, "protocol": "matrix_2_0_sticky2025",
            "official_image_identity_verified": True,
        }
        if patched:
            metadata.update({
                "kind": PATCHED_SDK_KIND, "stock_interoperability": False,
                "official_application_assets": False,
                "official_image_identity_verified": False,
                "official_server_image_identity_verified": True,
                "patched_sdk_receipt_sha256": patched[2],
                "sdk_production_files": SDK_PRODUCTION_FILES,
            })
        write_private(root / FIXTURE_FILE, json.dumps(metadata) + "\n", create=True)
        return metadata
    except BaseException:
        failed_stage = _stage
        try:
            down(root, keep_data=True)
        except Exception:
            raise FixtureError("fixture setup failed and cleanup requires retry") from None
        finally:
            set_stage(failed_stage)
        raise
    finally:
        if reservation is not None:
            reservation.close()


def down(root: Path, *, keep_data: bool = False):
    """Remove only this nonce's labelled containers, including interrupted create."""
    set_stage(Stage.CLEANUP)
    root = checked_root(root)
    if not root.exists():
        return
    try:
        path = root / STATE_FILE
        if path.is_symlink() or path.stat().st_size > 16384:
            raise ValueError
        state = json.loads(path.read_text())
        owner = state["owner"]
        cid = state["container_id"]
        if (state["schema"] != 1 or not re.fullmatch(r"[0-9a-f]{32}", owner)
                or (cid is not None and not re.fullmatch(r"[0-9a-f]{64}", cid))):
            raise ValueError
    except (OSError, ValueError, KeyError, TypeError):
        raise FixtureError("fixture ownership record is missing or invalid") from None
    docker = Docker(root, deadline=time.monotonic() + 90)
    ids = docker.run("ps", "--all", "--quiet", "--no-trunc", "--filter",
                     "label=" + OWNER_LABEL + "=" + owner).stdout.decode().split()
    for found_id in ids:
        if not re.fullmatch(r"[0-9a-f]{64}", found_id):
            raise FixtureError("fixture cleanup received an invalid container identity")
        try:
            entry = json.loads(docker.run("inspect", found_id).stdout)[0]
            if (entry["Id"] != found_id
                    or not isinstance(entry["Config"]["Labels"], dict)
                    or entry["Config"]["Labels"].get(OWNER_LABEL) != owner
                    or entry["Name"] != "/morpheus-element-" + owner):
                raise ValueError
        except (ValueError, KeyError, TypeError, IndexError):
            raise FixtureError("fixture cleanup refused an unowned container") from None
        docker.run("rm", "--force", "--volumes", found_id)
    if not keep_data:
        shutil.rmtree(root)


class PrivateArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        raise FixtureError("invalid fixture arguments")


def main():
    parser = PrivateArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True, parser_class=PrivateArgumentParser)
    start = subparsers.add_parser("up")
    start.add_argument("--root", type=Path, required=True)
    start.add_argument("--homeserver-url", default="http://127.0.0.1:18008")
    start.add_argument("--livekit-service-url", default="http://127.0.0.1:18010")
    start.add_argument("--server-name", default="morpheus-a.test")
    stop = subparsers.add_parser("down")
    stop.add_argument("--root", type=Path, required=True)
    stop.add_argument("--keep-data", action="store_true")
    args = parser.parse_args()
    if args.command == "up":
        up(args.root, args.homeserver_url, args.livekit_service_url, args.server_name)
        print(checked_root(args.root) / FIXTURE_FILE)
    else:
        down(args.root, keep_data=args.keep_data)


def run_cli() -> int:
    try:
        main()
        return 0
    except (Exception, KeyboardInterrupt):
        print("MORPHEUS_ELEMENT_FAILURE fixture_operation", file=sys.stderr, flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(run_cli())
