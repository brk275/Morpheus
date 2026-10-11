"""Offline contract/security tests; never start Docker or contact a homeserver."""

import email.message
import contextlib
import http.server
import importlib.util
import io
import json
import os
from pathlib import Path
import ssl
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
        self.issuer = "https://127.0.0.1:12001/"
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

    def test_mas_empty_fragment_becomes_the_actual_http_callback_target(self):
        # MAS v1.26.0 query-mode redirects end with '#'. Python urljoin may
        # preserve it; browsers strip fragments before sending HTTP requests.
        # Model the actual HTTPS issuer -> HTTP native-listener transition.
        opener = FakeOpener([response(303, location=self.callback + "#")])
        self.assertEqual(auth.browser(self.request, opener=opener), {"callback_url": self.callback})
        self.assertEqual(len(opener.requests), 1)
        self.assertTrue(opener.requests[0].full_url.startswith("https://"))
        self.assertTrue(self.callback.startswith("http://"))
        for suffix in ("#private-fragment", "##"):
            with self.subTest(suffix=suffix), self.assertRaises(auth.BrowserCallbackError) as raised:
                auth.browser(self.request, opener=FakeOpener([response(303, location=self.callback + suffix)]))
            self.assertEqual(raised.exception.failure, auth.Failure.CALLBACK_FRAGMENT)

    def test_callback_query_checks_preserve_blank_duplicate_and_field_bounds(self):
        for query, expected in (
            ("state=fixture-state&state=&code=fixture", auth.Failure.CALLBACK_STATE),
            ("state=fixture-state&code=fixture&code=", auth.Failure.CALLBACK_CODE),
            ("state=fixture-state&code=", auth.Failure.CALLBACK_CODE),
            ("state=fixture-state&code=fixture&missing_equals", auth.Failure.CALLBACK_QUERY),
            ("state=fixture-state&code=fixture&" + "&".join("a=value" for _ in range(32)),
             auth.Failure.CALLBACK_QUERY),
        ):
            with self.subTest(expected=expected), self.assertRaises(auth.BrowserCallbackError) as raised:
                auth.browser(self.request, opener=FakeOpener([response(303, location=self.redirect + "?" + query)]))
            self.assertEqual(raised.exception.failure, expected)

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
        for value in ("https://localhost:1234/", "http://localhost:1234/", "http://127.0.0.1/",
                      "http://127.0.0.1:0/", "http://127.0.0.1:65536/", "http://127.0.0.1:1234@elsewhere/",
                      "http://user@127.0.0.1:1234/", "http://127.0.0.1:1234\\evil/",
                      "http://127.0.0.1:1234/\n", "file:///tmp/file"):
            with self.subTest(value=value), self.assertRaises(auth.FixtureError):
                auth.loopback_url(value)
        self.assertEqual(auth.loopback_url("https://127.0.0.1:1234/").scheme, "https")

    def test_issuer_requires_https_but_native_callback_remains_http(self):
        cases = [dict(self.request, authorization_url=self.authorization.replace("https:", "http:"),
                      issuer_url=self.issuer.replace("https:", "http:")),
                 dict(self.request, redirect_url=self.redirect.replace("http:", "https:"))]
        for request in cases:
            opener = FakeOpener([])
            with self.subTest(request=request), self.assertRaises(auth.FixtureError):
                auth.browser(request, opener=opener)
            self.assertEqual(opener.requests, [])

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

    @unittest.skipUnless(sys.platform == "linux" and os.environ.get("GITHUB_ACTIONS") == "true",
                         "real HTTPS cookie flow runs only on the disposable Linux runner")
    def test_real_https_cookie_jar_login_consent_and_intercepted_callback(self):
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

        with tempfile.TemporaryDirectory() as directory:
            tls = Path(directory)
            # This isolated helper test trusts only its own leaf certificate;
            # unlike the actual acceptance fixture it never changes system trust.
            subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                            "-subj", "/CN=127.0.0.1", "-addext", "subjectAltName=IP:127.0.0.1",
                            "-keyout", str(tls / "key.pem"), "-out", str(tls / "cert.pem")],
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           timeout=30, check=True, umask=0o077)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(tls / "cert.pem", tls / "key.pem")
            server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
            server.socket = context.wrap_socket(server.socket, server_side=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            self.addCleanup(server.server_close)
            self.addCleanup(thread.join, 3)
            self.addCleanup(server.shutdown)
            issuer = f"https://127.0.0.1:{server.server_port}/"
            redirect = "http://127.0.0.1:12002/callback"
            request = dict(self.request, issuer_url=issuer, redirect_url=redirect,
                           authorization_url=issuer + "authorize?" + urllib.parse.urlencode({
                               "state": "fixture-state", "redirect_uri": redirect,
                           }))
            # Exercise the real urllib opener, redirects and cookie handling.
            # The server returns the exact permitted HTTP native callback, but
            # the driver must intercept it rather than opening that endpoint.
            original_reply = Handler.reply
            def reply(handler, **kwargs):
                if kwargs.get("location", "").startswith("/callback?"):
                    kwargs["location"] = redirect + "?" + kwargs["location"].split("?", 1)[1]
                original_reply(handler, **kwargs)
            opener = auth.urllib.request.build_opener(
                auth.urllib.request.ProxyHandler({}), auth.NoRedirect(),
                auth.urllib.request.HTTPSHandler(context=ssl.create_default_context(cafile=str(tls / "cert.pem"))),
                auth.urllib.request.HTTPCookieProcessor(auth.http.cookiejar.CookieJar()))
            with mock.patch.object(Handler, "reply", reply):
                result = auth.browser(request, opener=opener)
            self.assertEqual(result, {"callback_url": redirect + "?code=fixture-code&state=fixture-state"})
            self.assertIn(("login-valid", True), observations)
            self.assertIn(("consent-valid", True), observations)
            self.assertNotIn(("GET", "/callback"), observations)
            self.assertEqual([item for item in observations if item[0] in ("GET", "POST")],
                             [("GET", "/authorize"), ("GET", "/login"), ("POST", "/login"),
                              ("GET", "/consent"), ("POST", "/consent")])



class FixtureTests(unittest.TestCase):
    def test_configuration_is_private_loopback_only_and_uses_generated_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            account = auth.fixture_configuration(root, "a" * 32, {"postgres": 12000, "mas": 12001, "synapse": 12002,
                                                                  "mas_internal": 12003})
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
            self.assertEqual(mas["http"]["issuer"], "https://127.0.0.1:12001/")
            self.assertEqual(mas["http"]["public_base"], account["issuer_url"])
            self.assertEqual(mas["http"]["listeners"][0]["tls"], {
                "certificate_file": "/fixture/tls/server.crt", "key_file": "/fixture/tls/server.key"})
            self.assertEqual(mas["http"]["listeners"][1], {
                "name": "synapse-internal", "binds": [{"host": "127.0.0.1", "port": 12003}],
                "resources": [{"name": "discovery"}, {"name": "oauth"}]})
            synapse = json.loads((root / "synapse-overlay.json").read_text())
            self.assertEqual(synapse["listeners"][0]["bind_addresses"], ["127.0.0.1"])
            self.assertFalse(synapse["password_config"]["enabled"])
            self.assertFalse(synapse["enable_registration"])
            self.assertTrue(synapse["matrix_authentication_service"]["enabled"])
            self.assertEqual(synapse["matrix_authentication_service"]["endpoint"], "http://127.0.0.1:12003/")

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
                with self.assertRaisesRegex(auth.FixtureError, "cleanup failed"):
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


class CertificateTests(unittest.TestCase):
    def test_missing_or_non_executable_tool_fails_before_fixture_creation(self):
        for present, executable in ((False, True), (True, False)):
            with self.subTest(present=present, executable=executable), tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "fixture"
                with mock.patch.object(auth.sys, "platform", "linux"), \
                        mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"}, clear=True), \
                        mock.patch.object(auth.Path, "is_file", return_value=present), \
                        mock.patch.object(auth.os, "access", return_value=executable), \
                        mock.patch.object(auth, "private_command") as run:
                    with self.assertRaises(auth.FixtureError):
                        auth.up(root)
                    run.assert_not_called()
                self.assertFalse(root.exists())

    def test_non_hosted_and_ambient_trust_overrides_fail_before_commands(self):
        for env in ({}, {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "self-hosted"},
                    *({"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", name: "private-path"}
                      for name in ("SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"))):
            with self.subTest(env=env), mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(auth.sys, "platform", "linux"), mock.patch.object(auth, "private_command") as run:
                with self.assertRaises(auth.FixtureError):
                    auth.install_certificate(Path("/unused"), {"owner": "a" * 32})
                run.assert_not_called()

    def test_install_records_exact_public_ca_before_privileged_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            trusted_root = Path(directory) / "system-trust"
            trusted_root.mkdir()
            state = {"schema": 1, "owner": "a" * 32, "containers": []}
            commands = []

            def run(_, args):
                commands.append(args)
                if args[0] == "/usr/bin/openssl":
                    for name in ("ca.key", "ca.crt", "server.key", "server.csr", "server.crt"):
                        auth.write_private(root / "tls" / name, "disposable-" + name)
                if "/usr/bin/install" in args:
                    recorded = json.loads((root / auth.STATE_FILE).read_text())
                    self.assertEqual(recorded["certificate_sha256"], auth.certificate_digest(root / "tls" / "ca.crt"))
                    self.assertEqual(args[-2:], [str(root / "tls" / "ca.crt"), str(trusted_root / ("morpheus-oauth-" + "a" * 32 + ".crt"))])
                    raise KeyboardInterrupt("private-interruption")

            with mock.patch.object(auth, "CA_DIRECTORY", trusted_root), \
                    mock.patch.object(auth, "certificate_preflight"), \
                    mock.patch.object(auth, "private_command", side_effect=run):
                with self.assertRaises(KeyboardInterrupt):
                    auth.install_certificate(root, state)
            self.assertEqual(len(commands), 4)
            self.assertEqual((root / auth.STATE_FILE).stat().st_mode & 0o777, 0o600)
            self.assertTrue(all(path.stat().st_mode & 0o777 == 0o600 for path in (root / "tls").iterdir()))

    def test_cleanup_requires_digest_and_retries_trust_refresh_after_partial_remove(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            trusted_root = Path(directory) / "system-trust"
            trusted_root.mkdir()
            with mock.patch.object(auth, "CA_DIRECTORY", trusted_root):
                trusted = auth.trusted_certificate("a" * 32)
                trusted.write_text("owned-public-certificate")
                state = {"schema": 1, "owner": "a" * 32, "containers": [],
                         "certificate_sha256": auth.certificate_digest(trusted)}
                auth.save_state(root, state)

                def run(_, args):
                    if "/usr/bin/rm" in args:
                        trusted.unlink()
                    else:
                        raise auth.FixtureError("refresh failed")

                with mock.patch.object(auth, "private_command", side_effect=run):
                    with self.assertRaises(auth.FixtureError):
                        auth.remove_certificate(root, state)
                self.assertIn("certificate_sha256", json.loads((root / auth.STATE_FILE).read_text()))
                self.assertFalse(trusted.exists())
                with mock.patch.object(auth, "private_command") as run:
                    auth.remove_certificate(root, state)
                run.assert_called_once_with(root, ["/usr/bin/sudo", "--non-interactive", "/usr/bin/timeout",
                                                  "--signal=TERM", "--kill-after=2s", "20s", auth.CA_UPDATE])
                self.assertNotIn("certificate_sha256", json.loads((root / auth.STATE_FILE).read_text()))

    def test_cleanup_refuses_changed_or_symlinked_system_entry(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            with mock.patch.object(auth, "CA_DIRECTORY", Path(directory)):
                trusted = auth.trusted_certificate("a" * 32)
                state = {"owner": "a" * 32, "certificate_sha256": "0" * 64}
                trusted.write_text("unrelated-public-certificate")
                for symlink in (False, True):
                    if symlink:
                        trusted.unlink()
                        trusted.symlink_to(root / "absent")
                    with self.subTest(symlink=symlink), mock.patch.object(auth, "private_command") as run:
                        with self.assertRaises((auth.FixtureError, OSError)):
                            auth.remove_certificate(root, state)
                        run.assert_not_called()

    def test_docker_interrupt_still_attempts_ca_cleanup_and_keeps_evidence(self):
        for error in (KeyboardInterrupt(), SystemExit(1), auth.FixtureError("private-docker-error")):
            with self.subTest(error=type(error).__name__), tempfile.TemporaryDirectory() as directory:
                root = auth.private_directory(Path(directory) / "fixture")
                state = {"schema": 1, "owner": "a" * 32, "containers": []}
                auth.save_state(root, state)
                with mock.patch.object(auth.Docker, "run", side_effect=error), \
                        mock.patch.object(auth, "remove_certificate") as cleanup:
                    with self.assertRaises(type(error)):
                        auth.down(root)
                    cleanup.assert_called_once_with(root, state)
                self.assertTrue((root / auth.STATE_FILE).is_file())

    def test_failed_ca_cleanup_keeps_retry_record_after_containers_are_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            state = {"schema": 1, "owner": "a" * 32, "containers": [], "certificate_sha256": "b" * 64}
            auth.save_state(root, state)
            with mock.patch.object(auth.Docker, "run", return_value=subprocess.CompletedProcess([], 0, b"")), \
                    mock.patch.object(auth, "remove_certificate", side_effect=auth.FixtureError("private-error")):
                with self.assertRaises(auth.FixtureError):
                    auth.down(root)
            self.assertEqual(json.loads((root / auth.STATE_FILE).read_text()), state)

    def test_docker_cleanup_budget_reserves_time_for_certificate_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = auth.private_directory(Path(directory) / "fixture")
            state = {"schema": 1, "owner": "a" * 32, "containers": []}
            auth.save_state(root, state)
            # No Docker command is started after its cleanup budget expires;
            # the certificate cleanup is still attempted and evidence retained.
            with mock.patch.object(auth.time, "monotonic", side_effect=[100, 146]), \
                    mock.patch.object(auth.Docker, "run") as docker, \
                    mock.patch.object(auth, "remove_certificate") as cleanup:
                with self.assertRaises(auth.FixtureError):
                    auth.down(root)
                docker.assert_not_called()
                cleanup.assert_called_once_with(root, state)
            self.assertTrue(root.exists())

    def test_privileged_watchdog_is_inside_sudo_with_group_signals(self):
        with mock.patch.object(auth, "private_command") as run:
            auth.privileged_command(Path("/private/fixture"), [auth.CA_UPDATE])
        run.assert_called_once_with(Path("/private/fixture"), [
            "/usr/bin/sudo", "--non-interactive", "/usr/bin/timeout", "--signal=TERM",
            "--kill-after=2s", "20s", auth.CA_UPDATE])
        self.assertNotIn("--foreground", run.call_args.args[1])

    def test_interruption_waits_for_retained_command_before_returning(self):
        with tempfile.TemporaryDirectory() as directory:
            process = mock.Mock()
            process.wait.side_effect = [KeyboardInterrupt("private-value"), 0]
            with mock.patch.object(auth.subprocess, "Popen", return_value=process) as spawn, \
                    mock.patch.object(auth.time, "monotonic", side_effect=[100, 100, 105]):
                with self.assertRaises(KeyboardInterrupt):
                    auth.private_command(Path(directory), ["/usr/bin/sudo", "private-test-arguments"])
            self.assertTrue(spawn.call_args.kwargs["start_new_session"])
            self.assertEqual(process.wait.call_args_list, [mock.call(timeout=30), mock.call(timeout=25)])
            process.kill.assert_not_called()

    def test_python_timeout_never_uses_unbounded_reap_or_formats_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            process = mock.Mock()
            process.wait.side_effect = [subprocess.TimeoutExpired(["private-argument"], 30), 0]
            with mock.patch.object(auth.subprocess, "Popen", return_value=process), \
                    mock.patch.object(auth.time, "monotonic", side_effect=[100, 100]):
                with self.assertRaises(auth.FixtureError) as error:
                    auth.private_command(Path(directory), ["private-argument"])
            process.kill.assert_called_once_with()
            self.assertEqual(process.wait.call_args_list, [mock.call(timeout=30), mock.call(timeout=3)])
            self.assertNotIn("private-argument", str(error.exception))


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
                        mock.patch.object(auth, "certificate_preflight"), \
                        mock.patch.object(auth, "PortReservations") as ports, \
                        mock.patch.object(auth, "install_certificate"), \
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
        request = {"issuer_url": "https://127.0.0.1:12001/", "redirect_url": redirect,
                   "authorization_url": "https://127.0.0.1:12001/authorize?" + urllib.parse.urlencode({
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
            self.assert_failure(result, stage, "browser_callback_state" if stage == "browser_callback" else "browser_protocol")

    def test_callback_provider_errors_only_emit_fixed_categories(self):
        redirect = "http://127.0.0.1:12002/callback"
        request = {"issuer_url": "https://127.0.0.1:12001/", "redirect_url": redirect,
                   "authorization_url": "https://127.0.0.1:12001/authorize?" + urllib.parse.urlencode({
                       "redirect_uri": redirect, "state": "fixture-state",
                   }), "username": "fixture-user", "password": "private-password"}
        for provider_error, expected in (
            ("access_denied", "browser_oauth_access_denied"),
            ("login_required", "browser_oauth_login_required"),
            ("consent_required", "browser_oauth_consent_required"),
            ("interaction_required", "browser_oauth_interaction_required"),
            ("invalid_request", "browser_oauth_invalid_request"),
            ("server_error", "browser_oauth_server_error"),
            ("private-provider-secret", "browser_oauth_other"),
            ("", "browser_oauth_other"),
        ):
            location = redirect + "?" + urllib.parse.urlencode({
                "state": "fixture-state", "error": provider_error,
                "error_description": "private-password https://elsewhere/?code=private-code",
            }) + "#"
            with self.subTest(expected=expected), mock.patch.object(
                    auth, "http_opener", return_value=FakeOpener([response(303, location=location)])):
                result = self.invoke(["browser"], stdin=json.dumps(request).encode())
            self.assert_failure(result, "browser_callback", expected)
            for private in ("private-provider-secret", "private-password", "private-code", "error_description"):
                self.assertNotIn(private, result[1] + result[2])

    def test_callback_failure_rejects_non_enum_codes(self):
        with self.assertRaises(TypeError):
            auth.BrowserCallbackError("private-error")

    def test_stage_rejects_non_enum_values(self):
        with self.assertRaises(TypeError):
            auth.set_stage("private-password")


if __name__ == "__main__":
    unittest.main()
