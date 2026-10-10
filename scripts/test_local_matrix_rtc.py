"""Exercise the local RTC route without Docker, real devices, or accounts."""

import socket
import socketserver
import threading
import types
import unittest
import json
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


if __name__ == "__main__":
    unittest.main()
