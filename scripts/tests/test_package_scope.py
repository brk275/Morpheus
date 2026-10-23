import ast
import hashlib
import io
import tarfile
import textwrap
import urllib.parse
import urllib.request
import zipfile
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from scripts.package_scope import package_matrix
from scripts import package_scope


class PackageScopeTests(unittest.TestCase):
    def test_linux_only_never_requests_mac_or_windows(self):
        for formats in ("both", "appimage", "deb"):
            matrix = package_matrix("linux", formats)["include"]
            self.assertEqual([job["os"] for job in matrix], ["ubuntu-24.04"])
        self.assertEqual(package_matrix("linux", "appimage", True)["include"][0]["formats"], "appimage")

    def test_explicit_all_selects_exactly_four_standard_targets_once(self):
        for updates, formats in ((False, "both"), (True, "appimage")):
            matrix = package_matrix("all", formats, updates)["include"]
            self.assertCountEqual([job["os"] for job in matrix], [
                "ubuntu-24.04", "windows-2025", "macos-15", "macos-15-intel"
            ])
            self.assertEqual(len({job["label"] for job in matrix}), 4)

    def test_platform_formats_do_not_cross(self):
        self.assertEqual(package_matrix("windows", "both")["include"], [
            {"os": "windows-2025", "label": "windows-x64", "formats": "nsis"}
        ])
        mac = package_matrix("macos", "deb")["include"]
        self.assertEqual([job["os"] for job in mac], ["macos-15", "macos-15-intel"])
        self.assertTrue(all(job["formats"] == "dmg" for job in mac))

    def test_updates_cannot_silently_replace_deb_validation_or_select_absent_linux(self):
        for platform, formats in (("windows", "appimage"), ("macos", "appimage"),
                                  ("linux", "both"), ("all", "deb")):
            with self.subTest(platform=platform, formats=formats), self.assertRaises(ValueError):
                package_matrix(platform, formats, True)

    def test_unknown_selection_fails_closed(self):
        for platform, formats in (("self-hosted", "both"), ("linux", "unknown")):
            with self.assertRaises(ValueError):
                package_matrix(platform, formats)

    def test_installed_capture_selects_only_linux_and_cannot_hide_other_checks(self):
        self.assertEqual(package_matrix("linux", "appimage", False, True)["include"],
                         [{"os": "ubuntu-24.04", "label": "linux-x64", "formats": "appimage"}])
        for platform, formats, updates in (("all", "appimage", False), ("windows", "appimage", False),
                                           ("macos", "appimage", False), ("linux", "deb", False),
                                           ("linux", "appimage", True)):
            with self.subTest(platform=platform, formats=formats, updates=updates), self.assertRaises(ValueError):
                package_matrix(platform, formats, updates, True)

    def test_source_evidence_is_explicit_linux_only_without_installer_modes(self):
        self.assertEqual(package_matrix("linux", "appimage", source_bundle=True)["include"],
                         [{"os": "ubuntu-24.04", "label": "linux-x64", "formats": "appimage"}])
        for platform, formats, updates, capture in (("all", "appimage", False, False),
                ("windows", "appimage", False, False), ("macos", "appimage", False, False),
                ("linux", "both", False, False), ("linux", "deb", False, False),
                ("linux", "appimage", True, False), ("linux", "appimage", False, True)):
            with self.subTest(platform=platform, formats=formats, updates=updates, capture=capture), self.assertRaises(ValueError):
                package_matrix(platform, formats, updates, capture, True)

    def test_source_selection_emits_no_shared_cache_write_and_rejects_untyped_input(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "outputs"
            environment = {"GITHUB_OUTPUT": str(output), "GITHUB_SHA": "a" * 40,
                "PACKAGE_PLATFORM": "linux", "LINUX_FORMATS": "appimage", "APPIMAGE_UPDATES": "false",
                "APPIMAGE_CAPTURE": "false", "SOURCE_BUNDLE": "true"}
            with mock.patch.dict(os.environ, environment, clear=True):
                package_scope.main()
            fields = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual(fields["cache_write"], "false")
            self.assertEqual(fields["source_bundle"], "true")
            self.assertEqual(json.loads(fields["matrix"])["include"][0]["os"], "ubuntu-24.04")
            output.unlink()
            environment["SOURCE_BUNDLE"] = "yes"
            with mock.patch.dict(os.environ, environment, clear=True), self.assertRaises(ValueError):
                package_scope.main()
            self.assertFalse(output.exists())


class NativeUpdateScopeTests(unittest.TestCase):
    def test_only_explicit_mac_or_windows_select_existing_standard_targets(self):
        for platform, labels in (("macos", ["macos-arm64", "macos-x64"]), ("windows", ["windows-x64"])):
            with self.subTest(platform=platform):
                self.assertEqual(package_matrix(platform, "appimage", native_updates=True),
                                 package_matrix(platform, "appimage"))
                self.assertEqual([item["label"] for item in package_matrix(platform, "appimage", native_updates=True)["include"]], labels)
        for platform in ("linux", "all", "unknown"):
            with self.subTest(platform=platform), self.assertRaises(ValueError):
                package_matrix(platform, "appimage", native_updates=True)
        for platform in ("macos", "windows"):
            for formats, updates, capture, source in (("deb", False, False, False), ("both", False, False, False),
                    ("appimage", True, False, False), ("appimage", False, True, False), ("appimage", False, False, True)):
                with self.subTest(platform=platform, formats=formats, updates=updates, capture=capture, source=source), self.assertRaises(ValueError):
                    package_matrix(platform, formats, updates, capture, source, native_updates=True)
        for value in (None, "true", "false", 0, 1):
            with self.subTest(value=value), self.assertRaises(ValueError):
                package_matrix("macos", "appimage", native_updates=value)

    def test_native_input_is_typed_before_any_output_and_separate_source_cannot_write_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "outputs"
            environment = {"GITHUB_OUTPUT": str(output), "GITHUB_SHA": "a" * 40,
                "PACKAGE_SOURCE_REVISION": "b" * 40, "PACKAGE_PLATFORM": "macos", "LINUX_FORMATS": "appimage",
                "APPIMAGE_UPDATES": "false", "APPIMAGE_CAPTURE": "false", "SOURCE_BUNDLE": "false", "NATIVE_UPDATES": "true"}
            with mock.patch.dict(os.environ, environment, clear=True):
                package_scope.main()
            fields = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual((fields["native_updates"], fields["source_bundle"], fields["cache_write"]), ("true", "false", "false"))
            self.assertEqual(fields["source_revision"], "b" * 40)
            self.assertEqual([item["os"] for item in json.loads(fields["matrix"])["include"]], ["macos-15", "macos-15-intel"])
            for changed in ({"NATIVE_UPDATES": "yes"}, {"NATIVE_UPDATES": ""}, {"SOURCE_BUNDLE": "true"},
                            {"PACKAGE_PLATFORM": "all"}, {"APPIMAGE_UPDATES": "true"}, {"PACKAGE_SOURCE_REVISION": "main"}):
                output.unlink(missing_ok=True)
                with mock.patch.dict(os.environ, environment | changed, clear=True), self.assertRaises(ValueError):
                    package_scope.main()
                self.assertFalse(output.exists())


def native_scanner_source():
    source = (Path(__file__).resolve().parents[2] / ".github/workflows/package-qa.yml").read_text()
    start = "          python3 -I -B - <<'NATIVE_UPDATE_SCANNER'\n"
    end = "          NATIVE_UPDATE_SCANNER\n"
    if source.count(start) != 1 or source.count(end) != 1:
        raise AssertionError("Expected one native scanner preparation block")
    return ast.parse(textwrap.dedent(source.split(start, 1)[1].split(end, 1)[0]))


def native_scanner_function(name):
    module = native_scanner_source()
    selected = [node for node in module.body if isinstance(node, (ast.FunctionDef, ast.ClassDef)) and node.name == name]
    if len(selected) != 1 or selected[0].decorator_list:
        raise AssertionError("Expected one undecorated scanner boundary")
    namespace = {"hashlib": hashlib, "io": io, "tarfile": tarfile, "zipfile": zipfile,
                 "urllib": urllib, "MAX_SCANNER_BYTES": 32 * 1024 * 1024}
    # Run only the reviewed isolated definition on future hosted helper tests;
    # no workflow top-level download, environment, file publication or tool runs.
    exec(compile(ast.Module(body=selected, type_ignores=[]), "native-scanner-boundary", "exec"), namespace)
    return namespace[name]


class NativeUpdateWorkflowTests(unittest.TestCase):
    @staticmethod
    def fixture(kind, *, duplicate=False, link=False):
        content = b"synthetic scanner bytes"
        archive = io.BytesIO()
        if kind == "tar":
            with tarfile.open(fileobj=archive, mode="w:gz") as output:
                for _ in range(2 if duplicate else 1):
                    info = tarfile.TarInfo("gitleaks")
                    if link:
                        info.type, info.linkname = tarfile.SYMTYPE, "outside"
                        output.addfile(info)
                    else:
                        info.size = len(content)
                        output.addfile(info, io.BytesIO(content))
        else:
            with zipfile.ZipFile(archive, "w") as output:
                info = zipfile.ZipInfo("gitleaks.exe")
                info.create_system = 3
                info.external_attr = (0o120777 if link else 0o100755) << 16
                output.writestr(info, content)
        return archive.getvalue(), content

    def test_native_scanner_authenticates_before_parse_and_never_publishes_changed_binary(self):
        checked = native_scanner_function("checked_scanner")
        for kind in ("tar", "zip"):
            data, binary = self.fixture(kind)
            digest = hashlib.sha256(data).hexdigest()
            binary_digest = hashlib.sha256(binary).hexdigest()
            with self.subTest(kind=kind):
                self.assertEqual(checked(data, digest, binary_digest, kind), binary)
                with mock.patch.object(tarfile, "open") as tar, mock.patch.object(zipfile, "ZipFile") as zip:
                    with self.assertRaisesRegex(ValueError, "archive identity"):
                        checked(data, "0" * 64, binary_digest, kind)
                    tar.assert_not_called()
                    zip.assert_not_called()
                with self.assertRaisesRegex(ValueError, "executable identity"):
                    checked(data, digest, "0" * 64, kind)
        for options in ({"duplicate": True}, {"link": True}):
            data, binary = self.fixture("tar", **options)
            with self.subTest(options=options), self.assertRaisesRegex(ValueError, "archive member"):
                checked(data, hashlib.sha256(data).hexdigest(), hashlib.sha256(binary).hexdigest(), "tar")
        data, binary = self.fixture("zip", link=True)
        with self.assertRaisesRegex(ValueError, "archive member"):
            checked(data, hashlib.sha256(data).hexdigest(), hashlib.sha256(binary).hexdigest(), "zip")
        data, binary = self.fixture("tar")
        with self.assertRaisesRegex(ValueError, "Unknown scanner"):
            checked(data, hashlib.sha256(data).hexdigest(), hashlib.sha256(binary).hexdigest(), "unknown")

    def test_scanner_redirects_remain_bounded_https_and_fixed_release_hosts(self):
        redirect = native_scanner_function("ScannerRedirect")
        request = urllib.request.Request("https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/fixture")
        for url in ("http://github.com/fixture", "https://evil.example/fixture", "https://github.com.evil.example/fixture",
                    "https://user:secret@github.com/fixture", "https://github.com:8443/fixture", "https://github.com/fixture#fragment"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                redirect().redirect_request(request, None, 302, "Found", {}, url)
        instance = redirect()
        with mock.patch.object(urllib.request.HTTPRedirectHandler, "redirect_request", return_value="allowed"):
            for _ in range(3):
                self.assertEqual(instance.redirect_request(request, None, 302, "Found", {},
                    "https://release-assets.githubusercontent.com/fixture?generated=fixture"), "allowed")
            with self.assertRaises(ValueError):
                instance.redirect_request(request, None, 302, "Found", {}, "https://github.com/fixture")

    def test_permanent_route_has_exact_host_pins_and_no_extra_ordinary_build_or_smoke(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/package-qa.yml").read_text()
        for text in ("NATIVE_UPDATES: ${{ inputs.native_updates }}", "native_updates: ${{ steps.scope.outputs.native_updates }}",
                "github.ref == 'refs/heads/main'", "github.event.repository.private == false",
                "github.actor == 'brk275'", "github.triggering_actor == 'brk275'",
                "native-${{ needs.scope.outputs.source_revision }}-${{ matrix.os }}",
                "needs.scope.outputs.cache_write == 'true'", "persist-credentials: false",
                "Synthetic installed updates ({0}, partial)"):
            self.assertIn(text, workflow)
        for text in ("      - name: Build unsigned QA package\n        if: runner.os != 'Linux' && needs.scope.outputs.native_updates != 'true'",
                "      - name: Check Windows keyboard host before building\n        if: runner.os == 'Windows' && needs.scope.outputs.native_updates != 'true'",
                "      - name: Install, inspect, launch and remove unsigned packages\n        if: needs.scope.outputs.native_updates != 'true' && (runner.os != 'Linux' || !inputs.appimage_updates)"):
            self.assertIn(text, workflow)
        self.assertLess(workflow.index("Check synthetic update runtime before building"),
                        workflow.index("Install pinned Rust toolchain"))
        self.assertIn('sys.version_info >= (3, 12)', workflow)
        self.assertIn('os.chmod in os.supports_follow_symlinks', workflow)
        self.assertEqual(workflow.count("cargo install cargo-packager --version 0.11.8 --locked"), 1)
        self.assertEqual(workflow.count("python3 scripts/windows_package_tools.py"), 1)
        self.assertEqual(workflow.count("python3 scripts/macos_update_acceptance.py"), 1)
        self.assertEqual(workflow.count("python3 scripts/windows_update_acceptance.py"), 1)
        self.assertIn('--target-dir "$MORPHEUS_MAC_UPDATE_TARGET"', workflow)
        self.assertIn('--gitleaks-sha256 "17157e2ee8b76fc8b1d8bee607a250e34b8a8023c8bc81822d4b5ee4d78fcb7c"', workflow)
        self.assertEqual(workflow.count("--execute-on-free-hosted-runner"), 3)  # Existing AppImage plus explicit Mac and Windows.
        for value in ("pull_request_target", "workflow_run:", "upload-artifact", "secrets."):
            self.assertNotIn(value, workflow)
        module = native_scanner_source()
        pin_nodes = [node.value for node in ast.walk(module) if isinstance(node, ast.Assign)
                     and any(isinstance(target, ast.Name) and target.id == "pins" for target in node.targets)]
        self.assertEqual(len(pin_nodes), 1)
        pins = ast.literal_eval(pin_nodes[0])
        self.assertEqual(set(pins), {("darwin", "arm64"), ("darwin", "x86_64"), ("win32", "AMD64")})
        self.assertEqual(pins[("darwin", "arm64")][2], "ba52fb1bfabbcde42f032afad3d6e0b19dff8ed105229a16e7caa338bbc0e84f")
        self.assertEqual(pins[("darwin", "x86_64")][2], "cee01fea7173f1b779dff188e1c26ecbcb4027d394acc573b23aaf0be260e291")
        self.assertEqual(pins[("win32", "AMD64")][2], "17157e2ee8b76fc8b1d8bee607a250e34b8a8023c8bc81822d4b5ee4d78fcb7c")


if __name__ == "__main__":
    unittest.main()
