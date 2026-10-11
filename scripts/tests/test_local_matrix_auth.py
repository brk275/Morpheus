"""Offline contract/security tests; never start Docker or contact a homeserver."""

import email.message
import contextlib
import http.server
import importlib.util
import io
import json
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock
import urllib.parse


SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("local_matrix_auth", SCRIPTS / "local_matrix_auth.py")
auth = importlib.util.module_from_spec(spec)
spec.loader.exec_module(auth)


def response(status=200, body="", location=None, content_type="text/html"):
    result = io.BytesIO(body.encode())
    result.status = status
    result.headers = email.message.Message()
    result.headers["Content-Type"] = content_type
    if location:
        result.headers["Location"] = location
    return result


class FakeOpener:
    def __init__(self, responses):
        self.responses = iter(responses)
        self.requests = []

    def open(self, request, timeout):
        self.requests.append(request)
        return next(self.responses)


class BrowserTests(unittest.TestCase):
    def setUp(self):
        self.issuer = "http://127.0.0.1:12001/"
        self.redirect = "http://127.0.0.1:12002/callback"
        self.authorization = self.issuer + "authorize?" + urllib.parse.urlencode({
            "redirect_uri": self.redirect, "state": "fixture-state", "response_type": "code",
        })
        self.request = {"authorization_url": self.authorization, "redirect_url": self.redirect,
                        "issuer_url": self.issuer, "username": "fixture-user", "password": "fixture-password"}
        self.login = '<form method="POST"><input type="hidden" name="csrf" value="a&amp;b">' \
                     '<input name="username"><input name="password" type="password"></form>'
        self.consent = '<form method="POST"><input name="csrf" value="consent-csrf"></form>' \
                       '<form method="POST" action="/logout"><input name="csrf" value="logout-csrf"></form>'
        self.callback = self.redirect + "?code=fixture-code&state=fixture-state"

    def test_login_consent_and_callback_with_no_callback_fetch(self):
        opener = FakeOpener([
            response(303, location="/login?next=fixture"), response(body=self.login),
            response(303, location="/oauth2/consent/fixture"), response(body=self.consent),
            response(303, location=self.callback),
        ])
        self.assertEqual(auth.browser(self.request, opener=opener), {"callback_url": self.callback})
        self.assertEqual(len(opener.requests), 5)
        posts = [request for request in opener.requests if request.data]
        self.assertEqual(urllib.parse.parse_qs(posts[0].data.decode()), {
            "csrf": ["a&b"], "username": ["fixture-user"], "password": ["fixture-password"],
        })
        self.assertEqual(urllib.parse.parse_qs(posts[1].data.decode()), {"csrf": ["consent-csrf"]})
        self.assertTrue(all(request.get_header("Origin") == self.issuer.rstrip("/") for request in posts))
        self.assertTrue(all(request.full_url != self.callback for request in opener.requests))

    def test_does_not_forward_post_password_to_off_origin_redirect(self):
        opener = FakeOpener([response(body=self.login), response(307, location="http://127.0.0.1:12003/steal")])
        with self.assertRaisesRegex(auth.FixtureError, "off-origin redirect"):
            auth.browser(self.request, opener=opener)
        self.assertEqual(len(opener.requests), 2)

    def test_rejects_wrong_callback_state_and_path(self):
        for location in [self.redirect + "?state=other&code=fixture-code",
                         self.redirect + "-wrong?state=fixture-state&code=fixture-code",
                         self.redirect + "?state=fixture-state&error=access_denied",
                         self.redirect + "?state=fixture-state&code=one&code=two"]:
            with self.subTest(location=location), self.assertRaises(auth.FixtureError):
                auth.browser(self.request, opener=FakeOpener([response(303, location=location)]))

    def test_rejects_cross_origin_authorization_before_request(self):
        request = dict(self.request, issuer_url="http://127.0.0.1:12003/")
        opener = FakeOpener([])
        with self.assertRaises(auth.FixtureError):
            auth.browser(request, opener=opener)
        self.assertEqual(opener.requests, [])

    def test_rejects_redirect_uri_substitution(self):
        request = dict(self.request, redirect_url="http://127.0.0.1:12003/callback")
        with self.assertRaises(auth.FixtureError):
            auth.browser(request, opener=FakeOpener([]))

    def test_only_explicit_unambiguous_loopback_urls(self):
        for value in ("https://127.0.0.1:1234/", "http://localhost:1234/", "http://127.0.0.1/",
                      "http://127.0.0.1:0/", "http://127.0.0.1:65536/", "http://127.0.0.1:1234@elsewhere/",
                      "http://user@127.0.0.1:1234/", "http://127.0.0.1:1234\\evil/",
                      "http://127.0.0.1:1234/\n", "file:///tmp/file"):
            with self.subTest(value=value), self.assertRaises(auth.FixtureError):
                auth.loopback_url(value)

    def test_form_requires_exact_csrf_login_contract(self):
        pages = [self.login.replace('method="POST"', 'method="GET"'),
                 self.login.replace('<form method="POST">', '<form method="POST" action="http://elsewhere/">'),
                 self.login.replace('name="csrf"', 'name="not-csrf"'),
                 self.login.replace('</form>', '<input name="password"></form>'),
                 self.login + self.login]
        for page in pages:
            with self.subTest(page=page), self.assertRaises(auth.FixtureError):
                auth.form_body(page, self.authorization, "user", "password", logged_in=False)

    def test_real_opener_disables_proxies_and_automatic_redirects(self):
        opener = auth.http_opener(cookies=True)
        redirects = [handler for handler in opener.handlers if isinstance(handler, auth.NoRedirect)]
        self.assertEqual(len(redirects), 1)
        self.assertIsNone(redirects[0].redirect_request(None, None, 302, "", {}, "http://elsewhere/"))
        self.assertTrue(any(isinstance(handler, auth.urllib.request.HTTPCookieProcessor) for handler in opener.handlers))
        self.assertFalse(any(hasattr(handler, "proxies") and handler.proxies for handler in opener.handlers))

    def test_real_http_cookie_jar_login_consent_and_intercepted_callback(self):
        observations = []
        login, consent = self.login, self.consent

        class Handler(http.server.BaseHTTPRequestHandler):
            def reply(self, *, body="", location=None, cookie=None):
                self.send_response(303 if location else 200)
                self.send_header("Content-Type", "text/html")
                if location:
                    self.send_header("Location", location)
                if cookie:
                    self.send_header("Set-Cookie", cookie)
                data = body.encode()
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                path = urllib.parse.urlsplit(self.path).path
                observations.append(("GET", path))
                if path == "/authorize":
                    self.reply(location="/login", cookie="csrf-cookie=fixture-login; Path=/; HttpOnly; SameSite=Lax")
                elif path == "/login":
                    self.reply(body=login)
                elif path == "/consent":
                    self.reply(body=consent)
                else:
                    self.reply(body="unexpected navigation")

            def do_POST(self):
                body = urllib.parse.parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode())
                observations.append(("POST", self.path))
                if self.path == "/login":
                    valid = (self.headers.get("Cookie") == "csrf-cookie=fixture-login"
                             and body == {"csrf": ["a&b"], "username": ["fixture-user"],
                                          "password": ["fixture-password"]})
                    observations.append(("login-valid", valid))
                    if valid:
                        self.reply(location="/consent", cookie="session=fixture-session; Path=/; HttpOnly; SameSite=Lax")
                        return
                elif self.path == "/consent":
                    valid = ("session=fixture-session" in self.headers.get("Cookie", "")
                             and body == {"csrf": ["consent-csrf"]})
                    observations.append(("consent-valid", valid))
                    if valid:
                        self.reply(location="/callback?code=fixture-code&state=fixture-state")
                        return
                self.reply(body="invalid fixture submission")

            def log_message(self, format, *args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            issuer = f"http://127.0.0.1:{server.server_port}/"
            redirect = issuer + "callback"
            request = dict(self.request, issuer_url=issuer, redirect_url=redirect,
                           authorization_url=issuer + "authorize?" + urllib.parse.urlencode({
                               "state": "fixture-state", "redirect_uri": redirect,
                           }))
            # Exercise the real urllib opener, redirects and cookie handling.
            result = auth.browser(request)
            self.assertEqual(result, {"callback_url": redirect + "?code=fixture-code&state=fixture-state"})
            self.assertIn(("login-valid", True), observations)
            self.assertIn(("consent-valid", True), observations)
            self.assertNotIn(("GET", "/callback"), observations)
            self.assertEqual([item for item in observations if item[0] in ("GET", "POST")],
                             [("GET", "/authorize"), ("GET", "/login"), ("POST", "/login"),
                              ("GET", "/consent"), ("POST", "/consent")])
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=3)



class FixtureTests(unittest.TestCase):
    def test_configuration_is_private_loopback_only_and_uses_generated_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            account = auth.fixture_configuration(root, "a" * 32, {"postgres": 12000, "mas": 12001, "synapse": 12002})
            self.assertEqual(stat.S_IMODE(root.stat().st_mode), 0o700)
            for path in root.iterdir():
                self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700 if path.is_dir() else 0o600)
            self.assertEqual(account, json.loads((root / auth.FIXTURE_FILE).read_text()))
            self.assertEqual(account["access_token_ttl_seconds"], 60)
            self.assertEqual(account["user_id"], "@morpheus_" + "a" * 32 + ":localhost:12002")
            mas = json.loads((root / "mas-overlay.json").read_text())
            self.assertNotIn("secrets", mas)
            self.assertNotIn("policy", mas)
            self.assertNotIn("templates", mas)
            self.assertNotIn("uri", mas["database"])
            self.assertEqual(mas["experimental"]["access_token_ttl"], 60)
            self.assertEqual(mas["http"]["listeners"][0]["binds"], [{"host": "127.0.0.1", "port": 12001}])
            synapse = json.loads((root / "synapse-overlay.json").read_text())
            self.assertEqual(synapse["listeners"][0]["bind_addresses"], ["127.0.0.1"])
            self.assertFalse(synapse["password_config"]["enabled"])
            self.assertFalse(synapse["enable_registration"])
            self.assertTrue(synapse["matrix_authentication_service"]["enabled"])

    def test_checkout_root_and_symlink_output_are_rejected(self):
        with self.assertRaises(ValueError):
            auth.private_directory(SCRIPTS / "oauth-test-secret")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            destination = root / "destination"
            destination.write_text("keep")
            link = root / "link"
            link.symlink_to(destination)
            with self.assertRaises(OSError):
                with auth.private_output(link):
                    self.fail("must refuse symlink")
            self.assertEqual(destination.read_text(), "keep")

    def test_docker_errors_do_not_expose_argv_or_diagnostics(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = auth.Docker(Path(directory))
            with mock.patch.object(auth.subprocess, "run", side_effect=subprocess.TimeoutExpired(["docker", "private-password"], 1)):
                with self.assertRaises(auth.FixtureError) as error:
                    docker.run("exec", "private-password")
            self.assertNotIn("private-password", str(error.exception))
            with mock.patch.object(auth.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, b"private-output")):
                with self.assertRaises(auth.FixtureError) as error:
                    docker.run("exec", "private-password")
            self.assertNotIn("private", str(error.exception).replace("private diagnostics", ""))
            for log in Path(directory).glob("*.log"):
                self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)

    def test_cleanup_checks_label_and_does_not_touch_unrelated_containers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fixture"
            auth.private_directory(root)
            owner, cid = "a" * 32, "b" * 64
            auth.save_state(root, {"schema": 1, "owner": owner, "containers": []})
            results = [subprocess.CompletedProcess([], 0, (cid + "\n").encode()),
                       subprocess.CompletedProcess([], 0, json.dumps({auth.OWNER_LABEL: "different-owner"}).encode())]
            with mock.patch.object(auth.Docker, "run", side_effect=results) as run:
                with self.assertRaisesRegex(auth.FixtureError, "unowned"):
                    auth.down(root)
            self.assertEqual(run.call_count, 2)
            self.assertEqual(run.call_args_list[0].args[-1], "label=" + auth.OWNER_LABEL + "=" + owner)
            self.assertTrue(root.exists())

    def test_cleanup_reconciles_label_for_interrupted_create_then_removes_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fixture"
            auth.private_directory(root)
            owner, cid = "a" * 32, "b" * 64
            auth.save_state(root, {"schema": 1, "owner": owner, "containers": [{"id": None}]})
            results = [subprocess.CompletedProcess([], 0, (cid + "\n").encode()),
                       subprocess.CompletedProcess([], 0, json.dumps({auth.OWNER_LABEL: owner}).encode()),
                       subprocess.CompletedProcess([], 0, b"")]
            with mock.patch.object(auth.Docker, "run", side_effect=results) as run:
                auth.down(root)
            self.assertEqual(run.call_args.args, ("rm", "--force", "--volumes", cid))
            self.assertFalse(root.exists())

    def test_cleanup_refuses_unowned_root(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "unrelated.txt").write_text("keep")
            with mock.patch.object(auth.Docker, "run") as run:
                with self.assertRaises(auth.FixtureError):
                    auth.down(root)
            run.assert_not_called()
            self.assertEqual((root / "unrelated.txt").read_text(), "keep")

    def test_cleanup_can_keep_private_diagnostics_after_containers_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            owner, cid = "a" * 32, "b" * 64
            auth.save_state(root, {"schema": 1, "owner": owner, "containers": []})
            auth.write_private(root / "private.log", "diagnostics")
            results = [subprocess.CompletedProcess([], 0, (cid + "\n").encode()),
                       subprocess.CompletedProcess([], 0, json.dumps({auth.OWNER_LABEL: owner}).encode()),
                       subprocess.CompletedProcess([], 0, b"")]
            with mock.patch.object(auth.Docker, "run", side_effect=results) as run:
                auth.down(root, remove_data=False)
            self.assertEqual(run.call_args.args, ("rm", "--force", "--volumes", cid))
            self.assertEqual((root / "private.log").read_text(), "diagnostics")
            self.assertEqual(stat.S_IMODE((root / "private.log").stat().st_mode), 0o600)

    def test_linux_only_up_refuses_before_any_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fixture"
            with mock.patch.object(auth.sys, "platform", "darwin"), mock.patch.object(auth.Docker, "run") as run:
                with self.assertRaises(auth.FixtureError):
                    auth.up(root)
            run.assert_not_called()
            self.assertFalse(root.exists())

    def test_all_three_images_are_version_and_digest_pinned(self):
        for image in (auth.MAS_IMAGE, auth.SYNAPSE_IMAGE, auth.POSTGRES_IMAGE):
            self.assertRegex(image, r":[^:@]+@sha256:[0-9a-f]{64}$")
            self.assertNotIn(":latest", image)

    def test_synapse_generator_and_server_use_private_writable_log_directory(self):
        # Synapse logger.py derives homeserver.log from cwd, not data-directory.
        # Exercise the shared launch wrapper without importing/running Synapse.
        with mock.patch("os.umask") as umask, mock.patch("os.chdir") as chdir, \
                mock.patch("runpy.run_module") as run, mock.patch.object(sys, "argv", ["python", "--generate-config"]):
            exec(auth.SYNAPSE_COMMAND, {})
        umask.assert_called_once_with(0o077)
        chdir.assert_called_once_with("/fixture/synapse")
        run.assert_called_once_with("synapse.app.homeserver", run_name="__main__")


class CliDiagnosticsTests(unittest.TestCase):
    def invoke(self, arguments, *, stdin=b""):
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(sys, "argv", ["local_matrix_auth.py", *arguments]), \
                mock.patch.object(sys, "stdin", io.TextIOWrapper(io.BytesIO(stdin))), \
                contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = auth.run_cli()
        return status, stdout.getvalue(), stderr.getvalue()

    def assert_failure(self, result, stage, code):
        status, stdout, stderr = result
        self.assertEqual(status, 1)
        self.assertEqual(stdout, "")
        lines = stderr.splitlines()
        allowed_stages = {"MORPHEUS_OAUTH_STAGE " + value.value for value in auth.Stage}
        self.assertTrue(all(line in allowed_stages for line in lines[:-1]))
        self.assertEqual(lines[-2:], ["MORPHEUS_OAUTH_STAGE " + stage,
                                    "MORPHEUS_OAUTH_FAILURE " + code])

    def test_failure_only_emits_fixed_stage_and_code(self):
        def failed_up(root):
            auth.set_stage(auth.Stage.REGISTRATION)
            raise subprocess.CalledProcessError(1, ["docker", "--password", "private-password"],
                                                output="private-token", stderr="private-diagnostic")

        with mock.patch.object(auth, "up", side_effect=failed_up):
            result = self.invoke(["up", "--root", "/private/fixture"])
        self.assert_failure(result, "registration", "fixture_operation")

    def test_browser_failure_has_fixed_category_and_stage(self):
        def failed_browser(request):
            auth.set_stage(auth.Stage.CONSENT)
            raise RuntimeError("http://127.0.0.1/private?code=private-code password=private-password")

        with mock.patch.object(auth, "browser", side_effect=failed_browser):
            result = self.invoke(["browser"], stdin=b'{"password":"private-password"}')
        self.assert_failure(result, "browser_consent", "browser_protocol")

    def test_invalid_json_and_argv_never_echo_input(self):
        result = self.invoke(["browser"], stdin=b'{"password":"private-password", malformed')
        self.assert_failure(result, "browser_authorize", "browser_protocol")
        result = self.invoke(["up", "--private-password"])
        self.assert_failure(result, "fixture_root", "fixture_operation")

    def test_success_up_still_emits_only_fixture_path(self):
        with mock.patch.object(auth, "up", return_value=Path("/private/fixture/oauth-accounts.json")):
            result = self.invoke(["up", "--root", "/private/fixture"])
        self.assertEqual(result, (0, "/private/fixture/oauth-accounts.json\n",
                                 "MORPHEUS_OAUTH_STAGE fixture_root\n"))

    def test_failed_up_preserves_stage_across_successful_and_failed_cleanup(self):
        for cleanup_error in (None, auth.FixtureError("private-cleanup-error"), OSError("private-error")):
            with self.subTest(cleanup_error=type(cleanup_error).__name__), tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "fixture"

                def cleanup(*args, **kwargs):
                    auth.set_stage(auth.Stage.CLEANUP)
                    if cleanup_error is not None:
                        raise cleanup_error

                # No sockets, Docker or services: fail at the first vendor
                # configuration operation, then exercise up's actual cleanup.
                with mock.patch.object(auth.sys, "platform", "linux"), \
                        mock.patch.object(auth, "PortReservations") as ports, \
                        mock.patch.object(auth, "fixture_configuration", return_value={}), \
                        mock.patch.object(auth, "one_shot", side_effect=RuntimeError("private-vendor-output")), \
                        mock.patch.object(auth, "down", side_effect=cleanup):
                    ports.return_value.ports.return_value = {}
                    result = self.invoke(["up", "--root", str(root)])
                self.assert_failure(result, "mas_config", "fixture_operation")
                self.assertIn("MORPHEUS_OAUTH_STAGE cleanup\nMORPHEUS_OAUTH_STAGE mas_config\n", result[2])

    def test_standalone_cleanup_failure_is_distinct(self):
        with mock.patch.object(auth, "down", side_effect=OSError("private-path")):
            result = self.invoke(["down", "--root", "/private/fixture"])
        self.assert_failure(result, "cleanup", "fixture_operation")

    def test_stage_is_flushed_before_potentially_blocking_operation(self):
        with mock.patch("builtins.print") as output:
            auth.set_stage(auth.Stage.POSTGRES_READY)
        output.assert_called_once_with("MORPHEUS_OAUTH_STAGE postgres_ready", file=sys.stderr, flush=True)

    def test_real_browser_state_machine_reports_login_consent_and_callback_failures(self):
        redirect = "http://127.0.0.1:12002/callback"
        request = {"issuer_url": "http://127.0.0.1:12001/", "redirect_url": redirect,
                   "authorization_url": "http://127.0.0.1:12001/authorize?" + urllib.parse.urlencode({
                       "redirect_uri": redirect, "state": "fixture-state",
                   }), "username": "fixture-user", "password": "private-password"}
        login = '<form method="POST"><input name="csrf" value="fixture-csrf">' \
                '<input name="username"><input name="password"></form>'
        cases = [
            ("browser_login", [response(body="private-error-page")]),
            ("browser_consent", [response(body=login), response(303, location="/consent"),
                                 response(body="private-error-page")]),
            ("browser_callback", [response(303, location=redirect + "?code=private-code&state=wrong")]),
        ]
        for stage, replies in cases:
            with self.subTest(stage=stage), mock.patch.object(auth, "http_opener", return_value=FakeOpener(replies)):
                result = self.invoke(["browser"], stdin=json.dumps(request).encode())
            self.assert_failure(result, stage, "browser_protocol")

    def test_stage_rejects_non_enum_values(self):
        with self.assertRaises(TypeError):
            auth.set_stage("private-password")


if __name__ == "__main__":
    unittest.main()
