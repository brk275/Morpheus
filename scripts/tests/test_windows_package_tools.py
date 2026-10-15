import contextlib
import hashlib
import io
import json
import os
import re
import stat
import tempfile
import unittest
import urllib.request
import zipfile
from pathlib import Path
from unittest.mock import Mock, patch

from scripts import windows_package_tools as tools


# Independent copy of the actual cached-path requirements in pinned Packager
# 0.11.8. All contents below are inert synthetic bytes, never native tools.
REQUIRED = ["makensis.exe", "Bin/makensis.exe", "Stubs/lzma-x86-unicode",
            "Stubs/lzma_solid-x86-unicode", "Plugins/x86-unicode/ApplicationID.dll",
            "Plugins/x86-unicode/nsis_tauri_utils.dll", "Include/MUI2.nsh",
            "Include/FileFunc.nsh", "Include/x64.nsh", "Include/nsDialogs.nsh",
            "Include/WinMessages.nsh"]


def zipped(entries):
    target = io.BytesIO()
    with zipfile.ZipFile(target, "w") as archive:
        for name, value in entries:
            archive.writestr(name, value)
    return target.getvalue()


def fixture():
    contents = {name: ("synthetic " + name).encode() for name in REQUIRED}
    content = zipped([("nsis-3.09/" + name, data) for name, data in contents.items()
                      if not name.startswith("Plugins/")] + [("nsis-3.09/Docs/license.txt", b"license fixture")])
    plugin = zipped([("ReleaseUnicode/ApplicationID.dll", contents[REQUIRED[4]]),
                     ("Debug/ApplicationID.dll", b"unselected fixture")])
    blobs = {"nsis": content, "application_id": plugin, "tauri_utils": contents[REQUIRED[5]]}
    pins = {"schema": 1, "packager_version": "0.11.8",
            "assets": {name: {"url": "https://github.com/tools/" + name, "bytes": len(data),
                               "sha256": hashlib.sha256(data).hexdigest()} for name, data in blobs.items()},
            "required_files": {name: hashlib.sha256(data).hexdigest() for name, data in contents.items()}}
    def fetch(pin):
        return blobs[pin["url"].rsplit("/", 1)[1]]
    return pins, fetch, contents


class WindowsPackageToolTests(unittest.TestCase):
    def test_reviewed_manifest_covers_packager_cached_path_and_fixed_inputs(self):
        pins = json.loads(tools.PINS.read_text())
        self.assertEqual(pins["packager_version"], "0.11.8")
        self.assertEqual(set(pins["required_files"]), set(REQUIRED))
        self.assertEqual(set(pins["assets"]), {"nsis", "application_id", "tauri_utils"})
        for pin in pins["assets"].values():
            self.assertTrue(tools.allowed_url(pin["url"]))
            self.assertEqual(len(pin["sha256"]), 64)
            self.assertGreater(pin["bytes"], 0)
            self.assertLessEqual(pin["bytes"], tools.MAX_DOWNLOAD)

    def test_workflow_and_cached_path_contract_match_the_pinned_packager(self):
        root = Path(__file__).resolve().parents[2]
        pins = json.loads(tools.PINS.read_text())
        contract = (root / "packaging/windows/nsis-cached-path-contract.txt").read_bytes()
        self.assertEqual(hashlib.sha256(contract).hexdigest(),
                         pins["packager_source"]["cached_path_contract_sha256"])
        names = re.findall(r'^    "([^"]+)",$', contract.decode(), re.M)
        self.assertEqual(set(names), set(pins["required_files"]))
        self.assertEqual(len(names), len(set(names)))
        workflow = (root / ".github/workflows/package-qa.yml").read_text()
        install = "cargo install cargo-packager --version " + pins["packager_version"] + " --locked"
        preseed = ("      - name: Prepare verified Windows NSIS tools\n"
                   "        if: runner.os == 'Windows'\n"
                   "        run: python3 scripts/windows_package_tools.py\n")
        self.assertEqual(workflow.count(install), 1)
        self.assertEqual(workflow.count(preseed), 1)
        self.assertLess(workflow.index(install), workflow.index(preseed))
        self.assertLess(workflow.index(preseed), workflow.index("      - name: Build unsigned QA package"))

    def test_windows_junction_cache_parent_is_rejected(self):
        folder = Mock()
        folder.lstat.return_value = Mock(st_mode=stat.S_IFDIR | 0o700, st_file_attributes=0x400)
        with self.assertRaisesRegex(tools.PreparationError, "link"):
            tools.require_plain_directory(folder)

    def test_all_hashes_pass_before_any_archive_is_opened(self):
        pins, fetch, _ = fixture()
        def corrupted(pin):
            data = fetch(pin)
            return data[:-1] + bytes([data[-1] ^ 1]) if pin["url"].endswith("application_id") else data
        with patch.object(tools, "archive_files") as extract:
            with self.assertRaisesRegex(tools.PreparationError, "hash"):
                tools.tool_files(pins, corrupted)
        extract.assert_not_called()

    def test_only_reviewed_plugins_and_complete_nsis_layout_are_published(self):
        pins, fetch, contents = fixture()
        with tempfile.TemporaryDirectory() as folder:
            local = Path(folder).resolve()
            count = tools.prepare(local, pins, fetch)
            installed = local / ".cargo-packager/NSIS"
            self.assertEqual(count, len(contents) + 1)
            for name, data in contents.items():
                self.assertEqual((installed / name).read_bytes(), data)
            self.assertEqual((installed / "Docs/license.txt").read_bytes(), b"license fixture")
            self.assertFalse((installed / "Debug").exists())
            self.assertEqual(sorted(p.name for p in installed.parent.iterdir()), ["NSIS"])
            (installed / "Bin/makensis.exe").unlink()
            with self.assertRaisesRegex(tools.PreparationError, "missing"):
                tools.verify_tree(installed, tools.tool_files(pins, fetch))

    def test_preexisting_cache_is_not_read_replaced_deleted_or_downloaded_over(self):
        pins, _, _ = fixture()
        with tempfile.TemporaryDirectory() as folder:
            local = Path(folder).resolve()
            target = local / ".cargo-packager/NSIS"
            target.mkdir(parents=True)
            (target / "keep.txt").write_bytes(b"existing unrelated content")
            fetch = Mock()
            with self.assertRaisesRegex(tools.PreparationError, "existing NSIS"):
                tools.prepare(local, pins, fetch)
            self.assertEqual((target / "keep.txt").read_bytes(), b"existing unrelated content")
            fetch.assert_not_called()

    def test_missing_or_wrong_required_tool_blocks_publication(self):
        pins, fetch, _ = fixture()
        for change in ("hash", "contract"):
            modified = json.loads(json.dumps(pins))
            if change == "hash":
                modified["required_files"]["Bin/makensis.exe"] = "0" * 64
            else:
                del modified["required_files"]["Bin/makensis.exe"]
            with tempfile.TemporaryDirectory() as folder:
                local = Path(folder).resolve()
                with self.assertRaises(tools.PreparationError):
                    tools.prepare(local, modified, fetch)
                self.assertFalse((local / ".cargo-packager/NSIS").exists())

    def test_zip_paths_cannot_escape_or_alias_windows_paths(self):
        bad = ["../outside", "/absolute", "C:/absolute", "safe\\outside", "safe/../outside",
               "safe//file", "safe/./file", "safe/file.", "safe/file ", "safe/NUL.txt",
               "safe/file:stream", "safe//", "safe/\u00b5file"]
        for name in bad:
            with self.subTest(name=name), self.assertRaises(tools.PreparationError):
                tools.archive_files(zipped([(name, b"fixture")]))
        with self.assertRaisesRegex(tools.PreparationError, "Noncanonical"):
            tools.member_path(zipfile.ZipInfo("safe/file\0hidden"))
        for entries in [[("Safe/file", b"one"), ("safe/FILE", b"two")],
                        [("safe", b"one"), ("safe/file", b"two")]]:
            with self.assertRaises(tools.PreparationError):
                tools.archive_files(zipped(entries))
        with self.assertRaisesRegex(tools.PreparationError, "root"):
            tools.archive_files(zipped([("other/makensis.exe", b"fixture")]), prefix="nsis-3.09")

    def test_zip_symlinks_special_files_and_expansion_are_rejected(self):
        for mode in (stat.S_IFLNK, stat.S_IFIFO, stat.S_IFCHR):
            info = zipfile.ZipInfo("safe/file")
            info.create_system = 3
            info.external_attr = (mode | 0o600) << 16
            with self.assertRaisesRegex(tools.PreparationError, "special"):
                tools.archive_files(zipped([(info, b"target")]))
        ordinary = zipped([("a", b"1234"), ("b", b"1234")])
        with patch.object(tools, "MAX_EXPANDED", 7), self.assertRaisesRegex(tools.PreparationError, "expanded"):
            tools.archive_files(ordinary)
        with patch.object(tools, "MAX_ENTRIES", 1), self.assertRaisesRegex(tools.PreparationError, "entry count"):
            tools.archive_files(ordinary)
        with patch.object(tools, "MAX_DOWNLOAD", 3), self.assertRaisesRegex(tools.PreparationError, "member size"):
            tools.archive_files(ordinary)

    def test_redirects_reject_http_unrelated_hosts_and_userinfo(self):
        for value in ["http://github.com/file", "https://example.org/file", "https://user@github.com/file",
                      "https://github.com:444/file", "https://github.com/file#fragment", "https://@github.com/file"]:
            self.assertFalse(tools.allowed_url(value))
            with self.assertRaises(tools.PreparationError):
                tools.SafeRedirect().redirect_request(urllib.request.Request("https://github.com/file"),
                                                     None, 302, "Found", {}, value)
        self.assertTrue(tools.allowed_url("https://release-assets.githubusercontent.com/file?signature=fixture"))

    def test_download_refuses_oversize_and_bad_digest_without_publishing(self):
        pin = {"url": "https://github.com/fixture", "bytes": 3,
               "sha256": hashlib.sha256(b"abc").hexdigest()}
        for content, length in [(b"abcd", None), (b"abc", "4"), (b"bad", "3")]:
            response = io.BytesIO(content)
            response.geturl = lambda: pin["url"]
            response.headers = {} if length is None else {"Content-Length": length}
            opener = Mock()
            opener.open.return_value = response
            with patch.object(tools.urllib.request, "build_opener", return_value=opener):
                with self.assertRaises(tools.PreparationError):
                    tools.download(pin)

    def test_entry_point_refuses_owner_machine_before_path_or_download(self):
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}), \
                patch.object(tools, "local_app_data") as folder, \
                patch.object(tools, "prepare") as prepare, contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit):
                tools.main()
        folder.assert_not_called()
        prepare.assert_not_called()

    def test_failure_output_cannot_reflect_signed_urls_or_exception_text(self):
        env = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Windows",
               "GITHUB_REPOSITORY": "brk275/Morpheus", "GITHUB_REF": "refs/heads/main"}
        output = io.StringIO()
        with patch.dict(os.environ, env), patch.object(tools.sys, "platform", "win32"), \
                patch.object(tools.platform, "machine", return_value="AMD64"), \
                patch.object(tools, "local_app_data", side_effect=RuntimeError("private signed URL fixture")), \
                contextlib.redirect_stdout(output):
            with self.assertRaises(SystemExit):
                tools.main()
        self.assertEqual(json.loads(output.getvalue()), {"check": "verified_windows_nsis_tools", "status": "failed"})


if __name__ == "__main__":
    unittest.main()
