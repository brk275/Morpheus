"""Exercise the local RTC route without Docker, real devices, or accounts."""

import socket
import socketserver
import threading
import types
import unittest
import json
import copy
import hashlib
import os
from pathlib import Path
import tempfile
import subprocess
import sys
from types import SimpleNamespace
from contextlib import contextmanager
from unittest.mock import call, patch

import local_matrix_rtc as rtc


@contextmanager
def serving(server):
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class RtcLoopbackTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "Exact Linux owned pipe collector")
    def test_owned_client_bounds_each_real_pipe_and_reaps_before_reporting_overflow(self):
        real_popen = subprocess.Popen
        for descriptor in (1, 2):
            children = []
            def launch(_args, **kwargs):
                child = real_popen([sys.executable, "-I", "-c",
                    f"import os,time; os.write({descriptor}, b'x'*131073); time.sleep(30)"], **kwargs)
                children.append(child)
                return child
            observed = None
            reaped_before_fallback = None
            cleanup = True
            try:
                with patch.object(rtc.subprocess, "Popen", side_effect=launch):
                    try:
                        rtc.owned_docker("container", "inspect", "a" * 64)
                    except RuntimeError as error:
                        observed = str(error)
                        # Capture the collector's receipt before defensive test
                        # cleanup can repair a missing kill/wait.
                        if len(children) == 1:
                            reaped_before_fallback = children[0].returncode
            finally:
                for child in children:
                    try:
                        if child.poll() is None:
                            child.kill()
                        child.wait(timeout=3)
                    except (OSError, subprocess.SubprocessError):
                        cleanup = False
            self.assertTrue(cleanup)
            self.assertEqual(observed, "Owned SFU operation output exceeds limit")
            self.assertEqual(len(children), 1)
            self.assertIsNotNone(reaped_before_fallback)
            self.assertLess(reaped_before_fallback, 0)
            self.assertIsNotNone(children[0].returncode)
            self.assertLess(children[0].returncode, 0)

    @unittest.skipUnless(sys.platform == "linux", "Exact Linux owned pipe collector")
    def test_owned_client_captures_bounded_protocol_and_stderr_without_public_forwarding(self):
        real_popen = subprocess.Popen
        children = []
        def launch(_args, **kwargs):
            child = real_popen([sys.executable, "-I", "-c",
                "import os; os.write(1,b'[]'); os.write(2,b'synthetic-private-stderr')"], **kwargs)
            children.append(child)
            return child
        outcome = None
        cleanup = True
        try:
            with patch.object(rtc.subprocess, "Popen", side_effect=launch):
                outcome = rtc.owned_docker("container", "inspect", "a" * 64)
        finally:
            for child in children:
                try:
                    if child.poll() is None:
                        child.kill()
                    child.wait(timeout=3)
                except (OSError, subprocess.SubprocessError):
                    cleanup = False
        self.assertTrue(cleanup)
        self.assertEqual(outcome.stdout, "[]")
        self.assertEqual(outcome.stderr, "synthetic-private-stderr")
        self.assertEqual(children[0].returncode, 0)

    def test_fresh_creation_receipt_requires_exact_one_authenticated_room_state_entry(self):
        original = {"type": "m.room.create", "state_key": "", "event_id": "$created:example.org",
                    "sender": "@creator:example.org", "content": {}}
        self.assertEqual(rtc.room_creation_id([original], "@creator:example.org"), "$created:example.org")
        for events in ([], [original, original], [original | {"sender": "@foreign:example.org"}],
                       [original | {"state_key": "other"}], [original | {"event_id": None}],
                       [original | {"event_id": "not-an-event"}], [original | {"event_id": "$contains whitespace"}]):
            with self.assertRaises(RuntimeError):
                rtc.room_creation_id(events, "@creator:example.org")

    def owned_item(self):
        return {
            "Id": "a" * 64, "Name": "/" + rtc.SFU,
            "Config": {"Image": rtc.SFU_IMAGE, "Cmd": ["--config", "/etc/livekit.yaml"],
                       "Labels": {rtc.SFU_OWNER_LABEL: "matrixrtc", rtc.SFU_RUN_LABEL: "b" * 32,
                                  rtc.SFU_SOURCE_LABEL: "c" * 40}},
            "Image": "sha256:" + "d" * 64,
            "HostConfig": {"NetworkMode": rtc.NETWORK, "PortBindings": {
                f"{port}/{protocol}": [{"HostIp": "127.0.0.1", "HostPort": str(port)}]
                for port, protocol in ((18011, "tcp"), (18012, "udp"), (18013, "tcp"))}},
            "Mounts": [{"Type": "bind", "Source": str(rtc.SFU_CONFIG),
                        "Destination": "/etc/livekit.yaml", "RW": False}],
            "State": {"Running": True, "Pid": 123, "StartedAt": "before"},
        }

    def test_full_container_identity_and_original_image_mount_labels_are_required(self):
        item = self.owned_item()
        labels = item["Config"]["Labels"]
        with patch.object(rtc, "owned_docker", return_value=SimpleNamespace(stdout=json.dumps([item]))) as docker:
            self.assertEqual(rtc.inspect_owned_sfu(item["Id"], labels, image=item["Image"]), item)
        docker.assert_called_once_with("container", "inspect", item["Id"])
        for bad_id in (rtc.SFU, "a" * 12, "A" * 64, "a" * 63 + ";"):
            with patch.object(rtc, "owned_docker") as docker, self.assertRaises(RuntimeError):
                rtc.inspect_owned_sfu(bad_id, labels)
            docker.assert_not_called()
        for path, replacement in (
                (("Id",), "e" * 64), (("Image",), "sha256:" + "e" * 64),
                (("Name",), "/replacement"), (("Config", "Image"), "livekit:latest"),
                (("Config", "Labels", rtc.SFU_RUN_LABEL), "e" * 32),
                (("Config", "Cmd"), ["--config", "/other"]),
                (("HostConfig", "NetworkMode"), "host"),
                (("HostConfig", "PortBindings"), {}), (("Mounts",), []),
                (("Mounts", 0, "RW"), True), (("Mounts", 0, "Source"), "/foreign")):
            bad = copy.deepcopy(item)
            parent = bad
            for part in path[:-1]:
                parent = parent[part]
            parent[path[-1]] = replacement
            with patch.object(rtc, "owned_docker", return_value=SimpleNamespace(stdout=json.dumps([bad]))), \
                    self.assertRaises(RuntimeError):
                rtc.inspect_owned_sfu(item["Id"], labels, image=item["Image"])

    def test_recovery_actions_signal_only_retained_full_identity_and_check_new_lifetime(self):
        item = self.owned_item()
        stopped = copy.deepcopy(item)
        stopped["State"].update(Running=False, Pid=0)
        restarted = copy.deepcopy(item)
        restarted["State"]["StartedAt"] = "after"
        record = {"container": item["Id"]}
        with patch.object(rtc, "recovery_sfu", side_effect=[(record, item), (record, stopped)]), \
                patch.object(rtc, "owned_docker") as docker:
            rtc.sfu_outage()
        docker.assert_called_once_with("container", "kill", "--signal", "KILL", item["Id"])
        with patch.object(rtc, "recovery_sfu", side_effect=[(record, stopped), (record, restarted)]), \
                patch.object(rtc, "owned_docker") as docker, patch.object(rtc, "wait_http") as ready:
            rtc.sfu_resume()
        docker.assert_called_once_with("container", "start", item["Id"])
        ready.assert_called_once_with("http://127.0.0.1:18011/", duration=20)
        with patch.object(rtc, "recovery_sfu", side_effect=RuntimeError("identity refused")), \
                patch.object(rtc, "owned_docker") as docker, self.assertRaises(RuntimeError):
            rtc.sfu_outage()
        docker.assert_not_called()

    @unittest.skipIf(os.name == "nt", "POSIX private fixture capability")
    def test_private_config_is_opened_nofollow_single_link_bounded_and_owned(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            root.chmod(0o700)
            state = root / "rtc"
            state.mkdir(mode=0o700)
            config = state / "livekit.yaml"
            config.write_bytes(b"owned-config")
            config.chmod(0o600)
            with patch.multiple(rtc, FIXTURE_HOME=root, STATE=state, SFU_CONFIG=config, SFU_OWNER=state / "sfu-owner.json"):
                self.assertEqual(rtc.private_bytes(config, 32)[0], b"owned-config")
                with self.assertRaises(RuntimeError):
                    rtc.private_bytes(config, 2)
                config.chmod(0o644)
                with self.assertRaises(RuntimeError):
                    rtc.private_bytes(config, 32)
                config.chmod(0o600)
                alias = state / "alias"
                os.link(config, alias)
                with self.assertRaises(RuntimeError):
                    rtc.private_bytes(config, 32)
                alias.unlink()
                config.unlink()
                config.symlink_to(alias)
                with self.assertRaises(OSError):
                    rtc.private_bytes(config, 32)

    def test_changed_root_config_or_run_never_reaches_container_mutation(self):
        labels = self.owned_item()["Config"]["Labels"]
        meta = SimpleNamespace(st_dev=1, st_ino=2)
        root = SimpleNamespace(st_dev=1, st_ino=3, st_uid=os.getuid(), st_mode=0o40700)
        record = {"protocol": 1, "container": "a" * 64, "image": "sha256:" + "d" * 64,
                  "labels": labels, "root": [1, 3], "config": [1, 2],
                  "config_sha256": hashlib.sha256(b"owned").hexdigest()}
        for key, bad_value in (("labels", {}), ("root", [1, 4]), ("config", [1, 4]),
                               ("config_sha256", "0" * 64)):
            bad = record | {key: bad_value}
            with patch.object(rtc, "recovery_identity", return_value=labels), \
                    patch.object(rtc, "private_bytes", side_effect=[(json.dumps(bad).encode(), meta, root), (b"owned", meta, root)]), \
                    patch.object(rtc, "inspect_owned_sfu") as inspect, self.assertRaises(RuntimeError):
                rtc.recovery_sfu()
            inspect.assert_not_called()

    def test_new_fixture_home_replaces_stale_federation_bind_only(self):
        def docker_result(*args, **kwargs):
            return SimpleNamespace(stdout=json.dumps([{
                "Source": "/tmp/morpheus-old-fixture/rtc", "Destination": "/etc/rtc",
            }]))

        with patch.object(rtc, "exists", return_value=True), patch.object(rtc, "docker", side_effect=docker_result) as docker:
            rtc.start_federation_bridge()
        commands = [item.args for item in docker.call_args_list]
        self.assertEqual(commands[1], ("rm", "-f", rtc.FEDERATION))
        self.assertEqual(commands[2][:4], ("run", "-d", "--name", rtc.FEDERATION))
        self.assertIn(f"type=bind,src={rtc.STATE},dst=/etc/rtc,readonly", commands[2])
        self.assertFalse(any("volume" in command for command in commands))

    def test_bridge_preserves_bytes_and_half_close_on_loopback(self):
        module = types.ModuleType("fixture_sfu_loopback")
        exec(compile(rtc.SFU_LOOPBACK_SOURCE, "fixture_sfu_loopback.py", "exec"), module.__dict__)

        class Backend(socketserver.BaseRequestHandler):
            def handle(self):
                self.request.settimeout(3)
                chunks = []
                while chunk := self.request.recv(65536):
                    chunks.append(chunk)
                self.request.sendall(b"response:" + b"".join(chunks))

        payload = bytes(range(256)) * 1024
        with serving(socketserver.TCPServer(("127.0.0.1", 0), Backend)) as backend:
            with serving(module.Forwarder(target=backend.server_address, port=0)) as forwarder:
                self.assertEqual(forwarder.server_address[0], "127.0.0.1")
                with socket.create_connection(forwarder.server_address, timeout=3) as client:
                    client.sendall(payload)
                    client.shutdown(socket.SHUT_WR)
                    chunks = []
                    while chunk := client.recv(65536):
                        chunks.append(chunk)
                self.assertEqual(b"".join(chunks), b"response:" + payload)

    def assert_authorization_commands(self, *, native_sfu):
        with patch.object(rtc, "exists", return_value=True), patch.object(rtc, "docker") as docker:
            rtc.start_authorization(native_sfu=native_sfu)
        commands = [item.args for item in docker.call_args_list]
        self.assertEqual(commands[:2], [("rm", "-f", rtc.AUTH), ("rm", "-f", rtc.SFU_LOOPBACK)])
        bridge, authorization = commands[2:]
        self.assertIn("127.0.0.1:18010:8080/tcp", bridge)
        self.assertIn(rtc.matrix_env.IMAGE, bridge)
        self.assertIn(("--network-alias", rtc.AUTH), list(zip(bridge, bridge[1:])))
        self.assertNotIn(str(rtc.AUTH_ENV), bridge)
        self.assertNotIn(str(rtc.SECRET_FILE), bridge)
        self.assertEqual(authorization, (
            "run", "-d", "--name", rtc.AUTH,
            "--network", f"container:{rtc.SFU_LOOPBACK}",
            "--env-file", str(rtc.AUTH_ENV), rtc.AUTH_IMAGE,
        ))
        self.assertNotIn("--publish", authorization)
        return bridge

    def test_docker_mode_uses_sfu_network_alias(self):
        bridge = self.assert_authorization_commands(native_sfu=False)
        self.assertNotIn("--add-host", bridge)

    def test_native_mode_routes_only_bridge_to_host_gateway(self):
        bridge = self.assert_authorization_commands(native_sfu=True)
        self.assertIn("morpheus-rtc.localhost:host-gateway", bridge)

    def test_failed_auth_start_removes_both_partial_containers(self):
        def fail_auth(*args, **kwargs):
            if args[:4] == ("run", "-d", "--name", rtc.AUTH):
                raise RuntimeError("fixture authorization start failed")

        with patch.object(rtc, "exists", return_value=False), patch.object(rtc, "docker", side_effect=fail_auth) as docker:
            with self.assertRaises(RuntimeError):
                rtc.start_authorization(native_sfu=False)
        self.assertEqual(docker.call_args_list[-2:], [
            call("rm", "-f", rtc.AUTH, check=False),
            call("rm", "-f", rtc.SFU_LOOPBACK, check=False),
        ])

    def test_config_returns_literal_loopback_and_writes_bridge(self):
        with patch.object(rtc, "secrets_data", return_value={"key": "test-key", "secret": "test-secret"}), patch.object(rtc, "FEDERATION_CERT"), patch.object(rtc, "FEDERATION_KEY"), patch.object(rtc, "private_file") as write:
            rtc.write_configs()
        outputs = dict(item.args for item in write.call_args_list)
        self.assertIn("LIVEKIT_URL=ws://127.0.0.1:18011\n", outputs[rtc.AUTH_ENV])
        self.assertEqual(outputs[rtc.SFU_LOOPBACK_SCRIPT], rtc.SFU_LOOPBACK_SOURCE)
        self.assertIn("enable_loopback_candidate: false", outputs[rtc.SFU_CONFIG])
        self.assertIn("enable_loopback_candidate: true", outputs[rtc.NATIVE_SFU_CONFIG])
        self.assertIn("node_ip: 127.0.0.1", outputs[rtc.SFU_CONFIG])


if __name__ == "__main__":
    unittest.main()
