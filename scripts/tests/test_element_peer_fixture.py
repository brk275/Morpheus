"""Offline fixture safety/contracts: no Docker, sockets, services or downloads."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("element_peer_fixture", SCRIPTS / "element_peer_fixture.py")
peer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peer)

CID = "a" * 64
OWNER = "b" * 32


def completed(stdout=b"", returncode=0):
    return subprocess.CompletedProcess(["docker"], returncode, stdout=stdout)


def image_inspection():
    return [{
        "RepoDigests": [f"{peer.IMAGE_REPOSITORY}@{peer.IMAGE_DIGEST}"],
        "Config": {"Labels": {
            "org.opencontainers.image.version": peer.VERSION,
            "org.opencontainers.image.revision": peer.REVISION,
            "org.opencontainers.image.source": "https://github.com/element-hq/element-call",
        }},
    }]


class FixtureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="morpheus-element-unit-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve() / "peer"
        self.stderr = contextlib.redirect_stderr(io.StringIO())
        self.stderr.__enter__()
        self.addCleanup(self.stderr.__exit__, None, None, None)

    def make_state(self, container_id=CID):
        peer.private_directory(self.root)
        peer.save_state(self.root, {"schema": 1, "owner": OWNER, "container_id": container_id})

    def container_inspection(self, owner=OWNER):
        return [{"Id": CID, "Name": "/morpheus-element-" + owner,
                 "Config": {"Labels": {peer.OWNER_LABEL: owner}}}]

    def test_rejects_ambiguous_or_remote_service_urls(self):
        self.assertEqual(peer.loopback_url("http://127.0.0.1:18008/"), "http://127.0.0.1:18008")
        for url in ("http://localhost:18008", "https://127.0.0.1:18008", "http://127.0.0.1",
                    "http://127.0.0.1:0", "http://127.0.0.1:65536", "http://127.0.0.1:018008",
                    "http://user:password@127.0.0.1:18008", "http://127.0.0.1:18008/private",
                    "http://127.0.0.1:18008?token=secret", "http://127.0.0.1:18008#secret",
                    "http://127.0.0.1:18008\\@example.org", "http://127.0.0.1:18008\n"):
            with self.subTest(url=url), self.assertRaises(peer.FixtureError):
                peer.loopback_url(url)

    def test_configuration_pins_mode_without_external_telemetry(self):
        config = peer.configuration("http://127.0.0.1:18008", "http://127.0.0.1:18010", "morpheus-a.test")
        self.assertEqual(config["matrix_rtc_mode"], "matrix_2_0")
        self.assertEqual(set(config), {"matrix_rtc_mode", "default_server_config", "livekit"})
        for bad in ("secret@server", "host\nvalue", "https://server", "server/path"):
            with self.assertRaises(peer.FixtureError):
                peer.configuration("http://127.0.0.1:18008", "http://127.0.0.1:18010", bad)

    def test_nginx_only_listens_loopback_and_serves_stock_app(self):
        config = peer.nginx_configuration(18443)
        self.assertIn("listen 127.0.0.1:18443;", config)
        self.assertEqual(config.count("listen "), 1)
        self.assertIn("root /app;", config)
        self.assertIn("error_log /fixture/nginx-error.log warn;", config)
        self.assertIn("access_log off;", config)
        self.assertNotIn("root /fixture", config)
        self.assertNotIn("proxy_pass", config)
        for bad in (0, 65536, True, "80; listen 0.0.0.0:80"):
            with self.assertRaises(peer.FixtureError):
                peer.nginx_configuration(bad)

    def test_verifies_exact_image_digest_revision_version_and_source(self):
        self.assertEqual(peer.IMAGE, f"{peer.IMAGE_REPOSITORY}:{peer.VERSION}@{peer.IMAGE_DIGEST}")
        docker = mock.Mock()
        docker.run.return_value = completed(json.dumps(image_inspection()).encode())
        peer.verify_image(docker)
        for key in ("org.opencontainers.image.revision", "org.opencontainers.image.version",
                    "org.opencontainers.image.source", "RepoDigests"):
            changed = image_inspection()
            if key == "RepoDigests":
                changed[0][key] = []
            else:
                changed[0]["Config"]["Labels"][key] = "untrusted"
            docker.run.return_value = completed(json.dumps(changed).encode())
            with self.subTest(key=key), self.assertRaises(peer.FixtureError):
                peer.verify_image(docker)

    def test_rejects_non_linux_before_docker(self):
        with mock.patch.object(peer.sys, "platform", "darwin"), mock.patch.object(peer, "Docker") as docker:
            with self.assertRaises(peer.FixtureError):
                peer.up(self.root)
        docker.assert_not_called()

    def test_up_stores_identity_before_start_and_private_metadata(self):
        calls = []
        sock = mock.Mock()
        sock.getsockname.return_value = ("127.0.0.1", 18443)

        def docker_run(*args, **kwargs):
            calls.append(args)
            if args[:2] == ("image", "inspect"):
                return completed(json.dumps(image_inspection()).encode())
            if args[0] == "create":
                (self.root / "element-peer.cid").write_text(CID)
                return completed((CID + "\n").encode())
            if args[0] == "start":
                self.assertEqual(json.loads((self.root / peer.STATE_FILE).read_text())["container_id"], CID)
                sock.close.assert_called_once()
            return completed()

        with mock.patch.object(peer.sys, "platform", "linux"), \
             mock.patch.object(peer.socket, "socket", return_value=sock), \
             mock.patch.object(peer.Docker, "run", side_effect=docker_run), \
             mock.patch.object(peer, "wait_ready") as ready:
            metadata = peer.up(self.root)
        self.assertEqual(metadata["base_url"], "http://127.0.0.1:18443")
        self.assertEqual(metadata["image_digest"], peer.IMAGE_DIGEST)
        self.assertTrue(metadata["official_image_identity_verified"])
        self.assertEqual(metadata["protocol"], "matrix_2_0_sticky2025")
        self.assertEqual(json.loads((self.root / peer.FIXTURE_FILE).read_text()), metadata)
        ready.assert_called_once()
        create = next(args for args in calls if args[0] == "create")
        self.assertEqual(create[create.index("--network") + 1], "host")
        self.assertEqual(create[create.index("--entrypoint") + 1], "nginx")
        self.assertIn("--read-only", create)
        self.assertIn("no-new-privileges", create)
        self.assertIn("--cidfile", create)
        self.assertIn(peer.IMAGE, create)
        self.assertNotIn("-p", create)
        self.assertNotIn("--privileged", create)
        self.assertEqual(stat.S_IMODE(self.root.stat().st_mode), 0o700)
        for path in self.root.iterdir():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600, path.name)

    def test_up_failure_preserves_stage_and_cleans_owned_container(self):
        sock = mock.Mock()
        sock.getsockname.return_value = ("127.0.0.1", 18443)
        with mock.patch.object(peer.sys, "platform", "linux"), \
             mock.patch.object(peer.socket, "socket", return_value=sock), \
             mock.patch.object(peer.Docker, "run", side_effect=peer.FixtureError("fixed failure")), \
             mock.patch.object(peer, "down") as down:
            with self.assertRaises(peer.FixtureError):
                peer.up(self.root)
        down.assert_called_once_with(self.root, keep_data=True)
        self.assertEqual(peer._stage, peer.Stage.PULL)
        sock.close.assert_called_once()

    def test_cleanup_recovers_interrupted_create_and_keeps_private_data(self):
        self.make_state(container_id=None)
        with mock.patch.object(peer.Docker, "run", side_effect=[
            completed((CID + "\n").encode()),
            completed(json.dumps(self.container_inspection()).encode()), completed(),
        ]) as run:
            peer.down(self.root, keep_data=True)
        self.assertTrue(self.root.exists())
        self.assertIn("label=" + peer.OWNER_LABEL + "=" + OWNER, run.call_args_list[0].args)
        self.assertEqual(run.call_args_list[-1].args, ("rm", "--force", "--volumes", CID))

    def test_cleanup_refuses_unowned_container_and_invalid_identity(self):
        self.make_state()
        for found, inspect in ((CID, self.container_inspection("c" * 32)), ("short-id", [])):
            with mock.patch.object(peer.Docker, "run", side_effect=[
                completed(found.encode()), completed(json.dumps(inspect).encode()),
            ]) as run, self.assertRaises(peer.FixtureError):
                peer.down(self.root)
            self.assertFalse(any(call.args[0] == "rm" for call in run.call_args_list))
            self.assertTrue(self.root.exists())

    def test_cleanup_removes_data_only_with_owned_state(self):
        self.root.mkdir()
        (self.root / "unrelated.txt").write_text("preserve")
        with mock.patch.object(peer, "Docker") as docker, self.assertRaises(peer.FixtureError):
            peer.down(self.root)
        docker.assert_not_called()
        self.assertTrue((self.root / "unrelated.txt").exists())
        self.make_state()
        with mock.patch.object(peer.Docker, "run", return_value=completed()):
            peer.down(self.root)
        self.assertFalse(self.root.exists())

    def test_paths_reject_checkout_and_root_links(self):
        with self.assertRaises(ValueError):
            peer.checked_root(SCRIPTS / "not-created")
        target = Path(self.temp.name) / "target"
        target.mkdir()
        self.root.symlink_to(target, target_is_directory=True)
        with self.assertRaises(peer.FixtureError):
            peer.checked_root(self.root)

    def test_subprocess_error_and_timeout_never_include_diagnostics(self):
        peer.private_directory(self.root)
        docker = peer.Docker(self.root, deadline=peer.time.monotonic() + 10)
        errors = [subprocess.TimeoutExpired(["docker", "private-password"], 3),
                  subprocess.CalledProcessError(1, ["docker", "private-password"], output=b"private-token")]
        for error in errors:
            with mock.patch.object(peer.subprocess, "run", side_effect=error):
                with self.assertRaises(peer.FixtureError) as raised:
                    docker.run("inspect", "safe")
            self.assertNotIn("private", str(raised.exception))
        for path in self.root.iterdir():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_subprocess_deadline_and_nonzero_are_bounded_and_private(self):
        peer.private_directory(self.root)
        docker = peer.Docker(self.root, deadline=peer.time.monotonic() + 2)
        with mock.patch.object(peer.subprocess, "run", return_value=completed(b"private-token", 1)) as run:
            with self.assertRaisesRegex(peer.FixtureError, "operation failed"):
                docker.run("pull", peer.IMAGE, timeout=300)
        self.assertLessEqual(run.call_args.kwargs["timeout"], 2)
        self.assertEqual(run.call_args.kwargs["umask"], 0o077)
        docker.deadline = 0
        with mock.patch.object(peer.subprocess, "run") as run, self.assertRaises(peer.FixtureError):
            docker.run("pull", peer.IMAGE)
        run.assert_not_called()

    def test_readiness_checks_config_and_app_without_ambient_proxy_or_redirect(self):
        config = peer.configuration("http://127.0.0.1:18008", "http://127.0.0.1:18010", "morpheus-a.test")
        opener = mock.Mock()
        opener.open.side_effect = [io.BytesIO(json.dumps(config).encode()),
                                   io.BytesIO(b'<html><script src="/assets/official.js"></script></html>')]
        with mock.patch.object(peer.urllib.request, "build_opener", return_value=opener) as build:
            peer.wait_ready("http://127.0.0.1:18443", config, deadline=peer.time.monotonic() + 10)
        self.assertEqual(build.call_args.args[0].proxies, {})
        self.assertIsInstance(build.call_args.args[1], peer.NoRedirect)
        self.assertEqual([call.args[0] for call in opener.open.call_args_list],
                         ["http://127.0.0.1:18443/config.json", "http://127.0.0.1:18443/login"])
        opener.open.side_effect = [io.BytesIO(b'{"untrusted":true}')]
        with mock.patch.object(peer.urllib.request, "build_opener", return_value=opener), \
             self.assertRaisesRegex(peer.FixtureError, "did not match"):
            peer.wait_ready("http://127.0.0.1:18443", config, deadline=peer.time.monotonic() + 10)


class CliTests(unittest.TestCase):
    def test_success_stdout_is_only_metadata_path(self):
        output, errors = io.StringIO(), io.StringIO()
        with tempfile.TemporaryDirectory(prefix="morpheus-element-cli-") as temp:
            root = Path(temp).resolve() / "peer"
            with mock.patch.object(sys, "argv", ["helper", "up", "--root", str(root)]), \
                 mock.patch.object(peer, "up") as up, contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                def successful(*args):
                    peer.set_stage(peer.Stage.READY)
                    return {"base_url": "http://127.0.0.1:18443"}
                up.side_effect = successful
                self.assertEqual(peer.run_cli(), 0)
            self.assertEqual(output.getvalue(), str(root / peer.FIXTURE_FILE) + "\n")
        self.assertEqual(errors.getvalue(), "MORPHEUS_ELEMENT_STAGE element_ready\n")

    def test_failure_and_bad_arguments_never_echo_raw_values(self):
        for argv in (["helper", "up", "--root", "/private/fixture"],
                     ["helper", "--private-password=unsafe", "--token=unsafe"]):
            output, errors = io.StringIO(), io.StringIO()
            with mock.patch.object(sys, "argv", argv), \
                 mock.patch.object(peer, "up", side_effect=RuntimeError("private-password token unsafe")), \
                 contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                self.assertEqual(peer.run_cli(), 1)
            self.assertEqual(output.getvalue(), "")
            self.assertEqual(errors.getvalue(), "MORPHEUS_ELEMENT_FAILURE fixture_operation\n")

    def test_only_fixed_stage_enums_accepted_and_flushed(self):
        with mock.patch("builtins.print") as output:
            peer.set_stage(peer.Stage.PULL)
            output.assert_called_once_with("MORPHEUS_ELEMENT_STAGE element_image_pull", file=sys.stderr, flush=True)
        with self.assertRaises(TypeError):
            peer.set_stage("http://secret.invalid/?token=private")


if __name__ == "__main__":
    unittest.main()
