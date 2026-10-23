import tempfile
import gzip
import hashlib
import io
import json
import struct
import tarfile
import os
import re
from unittest import mock
import unittest
from pathlib import Path

from scripts.package import package_config, preflight_deb_authors, linux_appimage, macos_update_formats
from scripts import prepare_macos_update as mac_update
from scripts import prepare_windows_update


class PackageConfigTests(unittest.TestCase):
    def test_windows_initial_installer_never_executes_legacy_registry_maintenance(self):
        template = (Path(__file__).resolve().parents[2] / "packaging/windows/morpheus-installer.nsi").read_text()
        for expression in [r"(?mi)^\s*(?:ExecWait|ExecShell|EnumRegKey)\b",
                           r"(?mi)^\s*ReadRegStr[^\n]*UninstallString",
                           r"(?mi)^\s*Delete\s+\$(?:R\d|\d)\b"]:
            self.assertNotRegex(template, expression)
        for removed in ["PageReinstall", "RestorePreviousInstallLocation", "FindProcess", "KillProcess", "preinstall_section"]:
            self.assertNotIn(removed, template)
        admission = template.split("Function MorpheusRequireFreshDestination\n", 1)[1].split("FunctionEnd", 1)[0]
        checks = re.findall(r"!insertmacro MorpheusRejectRegisteredInstallation (0x[0-9A-Fa-f]+) (0x[0-9A-Fa-f]+)", admission)
        self.assertEqual(checks, [("0x80000001", "0x101"), ("0x80000001", "0x201"),
                                  ("0x80000002", "0x101"), ("0x80000002", "0x201")])
        self.assertNotIn("${MANUPRODUCTKEY}", admission)
        self.assertNotIn("!define MUI_LANGDLL_REGISTRY_", template)
        self.assertIn("kernel32::GetDriveTypeW(w r0)i.r1", admission)
        self.assertIn("IntCmp $1 3 +2", admission)
        self.assertIn("morpheus_existing_destination:", admission)
        self.assertIn("Abort", admission.split("morpheus_existing_destination:", 1)[1])
        install = template.split("Section Install\n", 1)[1].split("SectionEnd", 1)[0]
        before, after = install.split("!insertmacro MorpheusAcquireLifetimeLock", 1)
        self.assertIn("Call MorpheusRequireFreshDestination", before)
        self.assertLess(after.index("Call MorpheusRequireFreshDestination"), after.index("SetOutPath"))
        self.assertNotIn("SetOutPath", before)
        self.assertLess(after.index("SetOutPath"), after.index('File "${MAINBINARYSRCPATH}"'))
        uninstall = template.split("Section Uninstall\n", 1)[1].split("SectionEnd", 1)[0]
        self.assertLess(uninstall.index("!insertmacro MorpheusAcquireLifetimeLock"), uninstall.index('Delete "$INSTDIR'))

    def test_windows_uninstall_clears_only_the_exact_owned_manufacturer_default(self):
        root = Path(__file__).resolve().parents[2]
        template = (root / "packaging/windows/morpheus-installer.nsi").read_text()
        uninstall = template.split("Section Uninstall\n",1)[1].split("SectionEnd",1)[0]
        block = uninstall.split("  ClearErrors\n  ReadRegStr $R8 SHCTX",1)[1].split("  morpheus_keep_manufacturer_value:",1)[0]
        self.assertIn('"${MANUPRODUCTKEY}" ""',block)
        self.assertLess(block.index("IfErrors morpheus_keep_manufacturer_value"),block.index("DeleteRegValue"))
        self.assertLess(block.index("StrCmpS $R8 $INSTDIR 0 morpheus_keep_manufacturer_value"),block.index("DeleteRegValue"))
        self.assertEqual(re.findall(r'(?m)^  DeleteRegValue (.*)$',block),['SHCTX "${MANUPRODUCTKEY}" ""'])
        self.assertNotIn("DeleteRegKey",block)
        self.assertIn('!insertmacro MorpheusDeleteEmptyUserKey "${MANUPRODUCTKEY}"',block)
        self.assertIn('!insertmacro MorpheusDeleteEmptyUserKey "Software\\${MANUFACTURER}"',block)
        empty = template.split("!macro MorpheusDeleteEmptyUserKey KEY\n",1)[1].split("!macroend",1)[0]
        self.assertIn("RegOpenKeyExW(p 0x80000001",empty)
        self.assertIn("RegQueryInfoKeyW",empty)
        for guard in ("StrCmp $1 0 0 morpheus_empty_close_", "StrCmp $2 0 0 morpheus_empty_close_",
                      "StrCmp $3 0 0 morpheus_empty_close_"):
            self.assertLess(empty.index(guard),empty.index("RegDeleteKeyExW"))
        self.assertIn("RegCloseKey",empty)
        self.assertNotIn("EnumRegValue",empty)
        self.assertNotRegex(empty,r"(?m)^\s*DeleteRegKey")
        contract = json.loads((root / "packaging/windows/update-contract.json").read_bytes())
        self.assertEqual(contract["manufacturer_cleanup"],"exact-install-path-default-value-then-zero-values-and-subkeys-owned-keys")
        self.assertEqual(contract["template_sha256"],hashlib.sha256(template.encode()).hexdigest())
        self.assertEqual(prepare_windows_update.CONTRACT_SHA256,
                         hashlib.sha256(prepare_windows_update.canonical(contract)).hexdigest())

    def windows_fixture(self, root):
        project = Path(__file__).resolve().parents[2]
        for relative in ["packaging/windows/update-contract.json", "packaging/windows/morpheus-installer.nsi"]:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes((project / relative).read_bytes())
        identity = {"protocol": 2, "product": "io.github.brk275.morpheus", "version": "0.1.0",
                    "arch": "x86_64", "persistence": "a" * 64}
        native = bytearray(8192)
        native[:2] = b"MZ"
        struct.pack_into("<I", native, 60, 128)
        native[128:132] = b"PE\0\0"
        struct.pack_into("<HH", native, 132, 0x8664, 1)
        struct.pack_into("<HH", native, 148, 240, 2)
        struct.pack_into("<H", native, 152, 0x20B)
        struct.pack_into("<H", native, 220, 2)
        encoded = prepare_windows_update.canonical(identity)
        native[392:400] = b".mrphupd"
        struct.pack_into("<I", native, 400, len(encoded))
        struct.pack_into("<II", native, 408, 4096, 4096)
        struct.pack_into("<I", native, 428, 0x40000040)
        native[4096:4096 + len(encoded)] = encoded
        for name in prepare_windows_update.FILES:
            path = root / ("target/release/morpheus-app.exe" if name == "morpheus-app.exe" else name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(native if name == "morpheus-app.exe" else b"synthetic resource")
        config = package_config(platform="win32")
        return config, identity

    def test_windows_producer_binds_complete_ledger_without_local_paths_or_installer_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config, identity = self.windows_fixture(root)
            # The normal Cargo artifact may be a hardlink from deps. Only the
            # inert producer accepts that input; native installed files do not.
            os.link(root / "target/release/morpheus-app.exe", root / "target/release/cargo-output-alias.exe")
            output = root / "owned-output"
            output.mkdir()
            configured, payload = prepare_windows_update.prepare(config, root, output)
            marker = json.loads((output / "morpheus-install.json").read_bytes())
            self.assertEqual(marker["identity"], identity)
            self.assertEqual([entry["path"] for entry in marker["files"]], list(prepare_windows_update.FILES))
            self.assertEqual(marker["uninstaller_contract"], prepare_windows_update.CONTRACT_SHA256)
            self.assertNotIn("before-packaging-command", configured)
            self.assertEqual(configured["nsis"]["installMode"], "currentUser")
            self.assertEqual(configured["windows"], {"allowDowngrades": False})
            data = payload.read_bytes()
            self.assertTrue(data.startswith(prepare_windows_update.MAGIC))
            self.assertNotIn(str(root).encode(), data)
            self.assertNotIn("uninstall.exe", [entry["path"] for entry in marker["files"]])
            self.assertLessEqual(len(data), prepare_windows_update.MAX_BYTES)

    def test_windows_producer_rejects_contract_resource_and_pe_drift_before_publication(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config, _ = self.windows_fixture(root)
            output = root / "owned-output"
            output.mkdir()
            for changed in [{**config, "resources": ["../profile"]}, {**config, "nsis": {"preinstallSection": "untrusted"}},
                            {**config, "windows": {"signCommand": "untrusted"}}, {**config, "deepLinkProtocols": ["custom"]}]:
                with self.assertRaises(ValueError):
                    prepare_windows_update.prepare(changed, root, output)
                self.assertEqual(list(output.iterdir()), [])
            binary = root / "target/release/morpheus-app.exe"
            data = bytearray(binary.read_bytes())
            struct.pack_into("<I", data, 428, 0xC0000040)
            binary.write_bytes(data)
            with self.assertRaises(ValueError):
                prepare_windows_update.prepare(config, root, output)
            self.assertEqual(list(output.iterdir()), [])

    def test_windows_producer_rejects_legacy_and_noninteger_helper_protocols_before_publication(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config, identity = self.windows_fixture(root)
            binary = root / "target/release/morpheus-app.exe"
            original = binary.read_bytes()
            self.assertEqual(prepare_windows_update.pe_identity(original), identity)
            output = root / "owned-output"
            output.mkdir()
            for protocol in [0, 1, 3, True, 2.0]:
                encoded = prepare_windows_update.canonical(identity | {"protocol": protocol})
                data = bytearray(original)
                data[4096:] = b"\0" * 4096
                data[4096:4096 + len(encoded)] = encoded
                struct.pack_into("<I", data, 400, len(encoded))
                binary.write_bytes(data)
                with self.subTest(protocol=protocol), self.assertRaises(ValueError):
                    prepare_windows_update.pe_identity(bytes(data))
                with self.subTest(protocol=protocol), self.assertRaises(ValueError):
                    prepare_windows_update.prepare(config, root, output)
                self.assertEqual(list(output.iterdir()), [])
                self.assertEqual(binary.read_bytes(), bytes(data))

    def test_missing_deb_maintainer_fails_before_build_but_other_formats_do_not(self):
        for formats in [None, "deb", "deb,appimage", "all", "default"]:
            with self.subTest(formats=formats), self.assertRaises(ValueError):
                preflight_deb_authors({}, formats, "linux")
        for formats in ["appimage", "pacman"]:
            preflight_deb_authors({}, formats, "linux")
        preflight_deb_authors({}, "nsis", "win32")
        preflight_deb_authors({}, "app", "darwin")

    def test_deb_maintainer_needs_name_and_public_email(self):
        for author in ["Name", "name@example.org", "Name <bad>", "Name <n@example.org>\nInjected: header"]:
            with self.subTest(author=author), self.assertRaises(ValueError):
                preflight_deb_authors({"authors": [author]}, "deb", "linux")
        preflight_deb_authors({"authors": ["Fixture Maintainer <fixture@example.org>"]}, "deb", "linux")

    def test_only_explicit_linux_appimage_routes_to_canonical_installed_gate(self):
        with mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch("scripts.package.os.execv", return_value="canonical-controller") as execute:
            self.assertEqual(linux_appimage("appimage", "/reviewed/scanner"), "canonical-controller")
            args = execute.call_args.args
            self.assertEqual(args[1][:3], [args[0], "-I", "-B"])
            self.assertTrue(args[1][3].endswith("packaging/linux/package_appimage.py"))
            self.assertEqual(args[1][4:6], ["--gitleaks", "/reviewed/scanner"])
            self.assertEqual(args[1][6], "--qa-output")
            self.assertTrue(args[1][7].endswith("/dist"))
        for formats, scanner in ((None, "/scanner"), ("default", "/scanner"), ("all", "/scanner"),
                                 ("deb", "/scanner"), ("appimage,deb", "/scanner"),
                                 ("appimage,appimage", "/scanner"), ("appimage", None)):
            with self.subTest(formats=formats), mock.patch("scripts.package.os.execv") as execute, self.assertRaises(ValueError):
                linux_appimage(formats, scanner)
            execute.assert_not_called()
        with mock.patch.dict(os.environ, {"CARGO_PACKAGER_SIGN_PRIVATE_KEY": "disposable-key"}, clear=True), \
                mock.patch("scripts.package.os.execv") as execute, self.assertRaises(ValueError):
            linux_appimage("appimage", "/scanner")
        execute.assert_not_called()

    def test_linux_includes_platform_dependencies(self):
        config = package_config(platform="linux")
        self.assertIn("libxi6", config["deb"]["depends"])
        self.assertEqual(config["deb"]["files"]["LICENSE"], "usr/share/doc/morpheus/copyright")
        self.assertEqual(config["identifier"], "io.github.brk275.morpheus")

    def test_other_platforms_do_not_load_linux_overlay(self):
        for platform in ["darwin", "win32"]:
            self.assertNotIn("deb", package_config(platform=platform))

    def test_linux_overlay_cannot_override_shared_commands(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "packaging").mkdir()
            (root / "Packager.toml").write_text('name = "morpheus"\n')
            (root / "packaging/linux.toml").write_text('before-packaging-command = "unexpected"\n')
            with self.assertRaises(ValueError):
                package_config(root, "linux")


@unittest.skipUnless(os.name == "posix", "Mac payload files use POSIX ownership and mode contracts")
class MacUpdatePayloadTests(unittest.TestCase):
    def test_link_header_admits_exact_owner_readable_modes_and_rejects_unreadable_or_special_bits(self):
        record = {"path": "Contents/Resources/link", "kind": "link", "target": "LICENSE", "size": 0}
        for mode in [0o777, 0o700, 0o640, 0o400, 0o444]:
            self.assertEqual(mac_update._header(record | {"mode": mode}).mode, mode)
        for mode in [0, 0o200, 0o100, 0o300, 0o077, 0o1777, 0o2777, 0o4777]:
            with self.subTest(mode=mode), self.assertRaises(ValueError):
                mac_update._header(record | {"mode": mode})

    @unittest.skipUnless(os.sys.platform == "darwin", "Darwin no-follow link mode semantics")
    def test_unreadable_link_is_rejected_before_target_read_without_mutating_bundle_or_target(self):
        for mode in [0, 0o200, 0o100, 0o300, 0o077]:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temporary, \
                    mock.patch.dict(os.environ, {}, clear=True):
                root = Path(temporary)
                source, app, output = self.fixture(root, "aarch64")
                target = app / "Contents/Resources/LICENSE"
                target.chmod(0o600)
                data = target.read_bytes()
                link = target.with_name("unreadable-link")
                link.symlink_to("LICENSE")
                # Set only the fresh private fixture link, never its target.
                os.chmod(link, mode, follow_symlinks=False)
                before = link.lstat()
                with mock.patch.object(mac_update.os, "readlink", side_effect=AssertionError("unreadable target read")):
                    with self.assertRaises(ValueError):
                        mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source)
                after = link.lstat()
                self.assertEqual((after.st_dev, after.st_ino, after.st_uid, after.st_nlink, after.st_mode, after.st_ctime_ns),
                                 (before.st_dev, before.st_ino, before.st_uid, before.st_nlink, before.st_mode, before.st_ctime_ns))
                self.assertEqual(after.st_mode & 0o7777, mode)
                self.assertEqual(target.read_bytes(), data)
                self.assertEqual(target.stat().st_mode & 0o7777, 0o600)
                self.assertFalse(output.exists())
                self.assertFalse(list(root.glob(".morpheus-mac-update-*.tmp")))

    @staticmethod
    def fixture(root, arch):
        source = root / "source"
        source.mkdir()
        for relative in mac_update.RESOURCES.values():
            path = source / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(("fixture-resource:" + relative).encode("ascii"))
        purpose = (b'<plist version="1.0"><dict><key>NSCameraUsageDescription</key>'
                   b'<string>Fixture camera purpose.</string><key>NSMicrophoneUsageDescription</key>'
                   b'<string>Fixture microphone purpose.</string><key>NSScreenCaptureUsageDescription</key>'
                   b'<string>Fixture screen purpose.</string></dict></plist>')
        (source / "packaging/macos").mkdir(parents=True)
        (source / "packaging/macos/Info.plist").write_bytes(purpose)
        app = root / "Morpheus.app"
        for relative in ["Contents/MacOS", "Contents/Resources"]:
            (app / relative).mkdir(parents=True, exist_ok=True)
        identity = {"protocol": 2, "product": mac_update.PRODUCT, "version": "0.2.0",
                    "arch": arch, "persistence": "a" * 64}
        encoded = json.dumps(identity, separators=(",", ":")).encode("ascii")
        # Inert Mach-O/identity bytes only, never an executable fixture launch.
        native = bytearray(512 + len(encoded))
        cpu = {"aarch64": 0x0100000c, "x86_64": 0x01000007}[arch]
        for offset, value in [(0, 0xfeedfacf), (4, cpu), (12, 2), (16, 1), (20, 152),
                              (32, 0x19), (36, 152), (92, 5), (96, 1)]:
            struct.pack_into("<I", native, offset, value)
        native[40:46] = b"__TEXT"
        struct.pack_into("<Q", native, 80, len(native))
        native[104:118] = b"__morpheus_upd"
        native[120:126] = b"__TEXT"
        struct.pack_into("<Q", native, 144, len(encoded))
        struct.pack_into("<I", native, 152, 512)
        native[512:] = encoded
        executable = app / mac_update.EXECUTABLE
        executable.write_bytes(native)
        executable.chmod(0o700)
        identity_xml = (f'<key>CFBundleIdentifier</key><string>{mac_update.PRODUCT}</string>'
                        '<key>CFBundleExecutable</key><string>morpheus-app</string>'
                        '<key>CFBundleShortVersionString</key><string>0.2.0</string>'
                        '<key>CFBundlePackageType</key><string>APPL</string>'
                        '<key>CSResourcesFileMapped</key><true/>').encode("ascii")
        (app / "Contents/Info.plist").write_bytes(purpose.replace(b"</dict>", identity_xml + b"</dict>"))
        for name, relative in mac_update.RESOURCES.items():
            (app / "Contents/Resources" / name).write_bytes((source / relative).read_bytes())
        return source, app, root / mac_update.ARCHIVE_NAME

    def test_mac_producer_rejects_legacy_and_noninteger_helper_protocols_before_publication(self):
        for arch in ["aarch64", "x86_64"]:
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as temporary, \
                    mock.patch.dict(os.environ, {}, clear=True):
                root = Path(temporary)
                source, app, output = self.fixture(root, arch)
                binary = app / mac_update.EXECUTABLE
                original = binary.read_bytes()
                identity = mac_update.binary_identity(original, "0.2.0", arch)
                self.assertEqual(identity["protocol"], 2)
                for protocol in [0, 1, 3, True, 2.0]:
                    encoded = json.dumps(identity | {"protocol": protocol}, separators=(",", ":")).encode("ascii")
                    data = bytearray(original[:512] + encoded)
                    struct.pack_into("<Q", data, 80, len(data))
                    struct.pack_into("<Q", data, 144, len(encoded))
                    binary.write_bytes(data)
                    before = mac_update._snapshot(app)
                    with self.subTest(protocol=protocol), self.assertRaises(ValueError):
                        mac_update.binary_identity(bytes(data), "0.2.0", arch)
                    with self.subTest(protocol=protocol), self.assertRaises(ValueError):
                        mac_update.prepare_update(app, output, version="0.2.0", arch=arch, root=source)
                    self.assertFalse(output.exists())
                    self.assertEqual(mac_update._snapshot(app), before)
                    self.assertFalse(list(root.glob(".morpheus-mac-update-*.tmp")))

    def test_mac_selection_retains_exact_one_app_build_and_refuses_other_platforms_or_signing(self):
        config = package_config(platform="darwin")
        with mock.patch.dict(os.environ, {}, clear=True):
            for formats in [None, "default", "all", "dmg", "app,dmg", "dmg,app"]:
                self.assertEqual(macos_update_formats(config, formats), "app,dmg")
            self.assertEqual(macos_update_formats(config, "app"), "app")
            for formats in ["nsis", "deb", "appimage", "dmg,nsis", "dmg,dmg", "", "all,dmg"]:
                with self.subTest(formats=formats), self.assertRaises(ValueError):
                    macos_update_formats(config, formats)
            for field in ["identifier", "product-name", "resources"]:
                altered = dict(config)
                altered.pop(field)
                with self.assertRaises(ValueError):
                    macos_update_formats(altered, "dmg")
        for key in ["CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD"]:
            with mock.patch.dict(os.environ, {key: "fixture-not-a-key"}, clear=True), self.assertRaises(ValueError):
                macos_update_formats(config, "dmg")

    def test_real_producer_keeps_complete_bundle_bytes_modes_links_and_native_identity(self):
        for arch in ["aarch64", "x86_64"]:
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as temp, \
                    mock.patch.dict(os.environ, {}, clear=True):
                root = Path(temp)
                source, app, output = self.fixture(root, arch)
                license = app / "Contents/Resources/LICENSE"
                license.chmod(0o444)
                (app / "Contents/Resources/license-link").symlink_to("LICENSE")
                before = mac_update._snapshot(app)
                receipt = mac_update.prepare_update(app, output, version="0.2.0", arch=arch, root=source)
                self.assertEqual(mac_update._snapshot(app), before)
                self.assertEqual(receipt["bundle_sha256"], before["tree_sha256"])
                self.assertEqual(receipt["sha256"], hashlib.sha256(output.read_bytes()).hexdigest())
                self.assertEqual(receipt["bytes"], output.stat().st_size)
                self.assertFalse(receipt["signed"])
                with tarfile.open(output, "r:gz") as archive:
                    entries = archive.getmembers()
                    self.assertEqual(len(entries), receipt["entries"])
                    self.assertEqual({item.type for item in entries}, {tarfile.REGTYPE, tarfile.DIRTYPE, tarfile.SYMTYPE})
                    for record in before["records"]:
                        member = archive.getmember("Morpheus.app/" + record["path"])
                        self.assertEqual(member.mode, record["mode"])
                        if record["kind"] == "file":
                            self.assertEqual(archive.extractfile(member).read(), (app / record["path"]).read_bytes())
                        elif record["kind"] == "link":
                            self.assertEqual(member.linkname, record["target"])
                # The raw format contains only concrete USTAR entries; no
                # tar library unpack/extraction or candidate launch is used.
                raw = gzip.decompress(output.read_bytes())
                offset = 0
                while any(raw[offset:offset + 512]):
                    header = raw[offset:offset + 512]
                    self.assertEqual(header[257:263], b"ustar\0")
                    self.assertIn(header[156:157], [b"0", b"5", b"2"])
                    size = int(header[124:136].strip(b"\0 ") or b"0", 8)
                    offset += 512 + ((size + 511) // 512) * 512
                self.assertFalse(any(raw[offset:]))
                self.assertFalse(list(root.glob(".morpheus-mac-update-*.tmp")))

    def test_existing_output_resource_mismatch_and_nested_plist_never_replace_anything(self):
        with tempfile.TemporaryDirectory() as temp, mock.patch.dict(os.environ, {}, clear=True):
            root = Path(temp)
            source, app, output = self.fixture(root, "aarch64")
            original = mac_update._snapshot(app)
            output.write_bytes(b"retained earlier signing input")
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source)
            self.assertEqual(output.read_bytes(), b"retained earlier signing input")
            output.unlink()
            (app / "Contents/Resources/LICENSE").write_bytes(b"changed required notice")
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source)
            self.assertFalse(output.exists())
            (app / "Contents/Resources/LICENSE").write_bytes((source / "LICENSE").read_bytes())
            plist = app / "Contents/Info.plist"
            original_plist = plist.read_bytes()
            body = original_plist.split(b"<dict>", 1)[1].rsplit(b"</dict>", 1)[0]
            plist.write_bytes(b'<plist version="1.0"><dict><key>nested</key><dict>' + body + b'</dict></dict></plist>')
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source)
            self.assertFalse(output.exists())
            plist.write_bytes(original_plist)
            self.assertEqual(mac_update._snapshot(app), original)

    def test_separately_validated_plist_is_bound_to_exact_snapshot_bytes(self):
        with tempfile.TemporaryDirectory() as temp, mock.patch.dict(os.environ, {}, clear=True):
            root = Path(temp)
            source, app, output = self.fixture(root, "aarch64")
            plist = app / "Contents/Info.plist"
            valid = plist.read_bytes()
            # A same-size, same-inode invalid version is present in the initial
            # tree. A substituted validation read must not authorize those bytes.
            invalid = valid.replace(b"<string>0.2.0</string>", b"<string>9.2.0</string>")
            self.assertNotEqual(invalid, valid)
            self.assertEqual(len(invalid), len(valid))
            plist.write_bytes(invalid)
            before = mac_update._snapshot(app)
            original_read = mac_update._read

            def substitute_validation_read(path, limit):
                return valid if path == plist else original_read(path, limit)

            with mock.patch.object(mac_update, "_read", side_effect=substitute_validation_read), self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source)
            self.assertEqual(mac_update._snapshot(app), before)
            self.assertFalse(output.exists())
            self.assertFalse(list(root.glob(".morpheus-mac-update-*.tmp")))

    def test_unsafe_links_extensions_and_output_bounds_fail_without_bundle_changes(self):
        with tempfile.TemporaryDirectory() as temp, mock.patch.dict(os.environ, {}, clear=True):
            root = Path(temp)
            source, app, output = self.fixture(root, "x86_64")
            sentinel = root / "sentinel"
            sentinel.write_bytes(b"keep outside")
            escape = app / "Contents/Resources/escape"
            escape.symlink_to(sentinel)
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="x86_64", root=source)
            self.assertEqual(sentinel.read_bytes(), b"keep outside")
            escape.unlink()
            long = app / "Contents/Resources" / ("a" * 200)
            long.mkdir()
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="x86_64", root=source)
            long.rmdir()
            before = mac_update._snapshot(app)
            with mock.patch.object(mac_update, "MAX_EXPANDED_BYTES", 2048), self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="x86_64", root=source)
            self.assertEqual(mac_update._snapshot(app), before)
            self.assertFalse(output.exists())
            self.assertFalse(list(root.glob(".morpheus-mac-update-*.tmp")))
        target = io.BytesIO()
        writer = mac_update._BoundedWriter(target, 4)
        writer.write(b"123")
        with self.assertRaises(ValueError):
            writer.write(b"45")
        self.assertEqual(target.getvalue(), b"123")



if __name__ == "__main__":
    unittest.main()
