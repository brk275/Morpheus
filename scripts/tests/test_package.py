import tempfile
import gzip
import hashlib
import io
import json
import struct
import tarfile
import os
import re
import sys
from unittest import mock
import unittest
from pathlib import Path

from scripts.package import package_config, preflight_deb_authors, linux_appimage, macos_update_formats
from scripts import prepare_macos_update as mac_update
from scripts import prepare_windows_update, native_notices


def fixture_notices(root, platform, version):
    """Inert package-ledger input; full source authentication is tested separately."""
    text = b"Synthetic Slint notice fixture\n"
    checksum = hashlib.sha256(text).hexdigest()
    record = {"name": "slint", "version": "1.18.1", "source": "committed-workspace",
              "source_identity": {"tree": "b" * 40}, "declared_license": "synthetic-test-only",
              "license_texts": [{"source_path": "LICENSE", "sha256": checksum}],
              "missing_license_text": False, "license_obligations_verified": False,
              "native_linked_inputs_verified": False}
    value = {"schema": 1, "source_revision": "a" * 40, "source_tree": "b" * 40,
             "cargo_lock_sha256": hashlib.sha256((root / "Cargo.lock").read_bytes()).hexdigest(),
             "built_cargo_lock_sha256": hashlib.sha256((root / "Cargo.lock").read_bytes()).hexdigest(),
             "package_version": version, "platform": platform,
             "source_bundle_sha256": "c" * 64, "source_manifest_sha256": "d" * 64,
             "text_sha256": checksum, "text_bytes": len(text),
             "texts": [{"sha256": checksum, "offset": 0, "bytes": len(text)}],
             "locked_packages": [record], "local_patch_provenance": [
                 {"source_path": "vendor/cpal/OWNED_COREAUDIO.md", "sha256": checksum}],
             "native_source_evidence": {"coverage": "rust-only", "bound_to_this_platform_binary": False},
             "distribution_ready": False, "license_compliance_verified": False,
             "unresolved_release_requirements": list(native_notices.OPEN)}
    return {native_notices.TEXT: text, native_notices.INVENTORY: json.dumps(value, sort_keys=True).encode()}


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
        (root / "Cargo.lock").write_bytes(b"version = 4\n")
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
            configured, payload = prepare_windows_update.prepare(config, root, output, notice_contents=fixture_notices(root, "win32", "0.1.0"))
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
                    prepare_windows_update.prepare(changed, root, output, notice_contents=fixture_notices(root, "win32", "0.1.0"))
                self.assertEqual(list(output.iterdir()), [])
            binary = root / "target/release/morpheus-app.exe"
            data = bytearray(binary.read_bytes())
            struct.pack_into("<I", data, 428, 0xC0000040)
            binary.write_bytes(data)
            with self.assertRaises(ValueError):
                prepare_windows_update.prepare(config, root, output, notice_contents=fixture_notices(root, "win32", "0.1.0"))
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
                    prepare_windows_update.prepare(config, root, output, notice_contents=fixture_notices(root, "win32", "0.1.0"))
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
                        mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
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
        (source / "Cargo.lock").write_bytes(b"version = 4\n")
        for name, content in fixture_notices(source, "darwin", "0.2.0").items():
            (app / "Contents/Resources" / name).write_bytes(content)
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
                        mac_update.prepare_update(app, output, version="0.2.0", arch=arch, root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
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
                receipt = mac_update.prepare_update(app, output, version="0.2.0", arch=arch, root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
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
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
            self.assertEqual(output.read_bytes(), b"retained earlier signing input")
            output.unlink()
            (app / "Contents/Resources/LICENSE").write_bytes(b"changed required notice")
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
            self.assertFalse(output.exists())
            (app / "Contents/Resources/LICENSE").write_bytes((source / "LICENSE").read_bytes())
            plist = app / "Contents/Info.plist"
            original_plist = plist.read_bytes()
            body = original_plist.split(b"<dict>", 1)[1].rsplit(b"</dict>", 1)[0]
            plist.write_bytes(b'<plist version="1.0"><dict><key>nested</key><dict>' + body + b'</dict></dict></plist>')
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
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
                mac_update.prepare_update(app, output, version="0.2.0", arch="aarch64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
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
                mac_update.prepare_update(app, output, version="0.2.0", arch="x86_64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
            self.assertEqual(sentinel.read_bytes(), b"keep outside")
            escape.unlink()
            long = app / "Contents/Resources" / ("a" * 200)
            long.mkdir()
            with self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="x86_64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
            long.rmdir()
            before = mac_update._snapshot(app)
            with mock.patch.object(mac_update, "MAX_EXPANDED_BYTES", 2048), self.assertRaises(ValueError):
                mac_update.prepare_update(app, output, version="0.2.0", arch="x86_64", root=source, notice_contents=fixture_notices(source, "darwin", "0.2.0"))
            self.assertEqual(mac_update._snapshot(app), before)
            self.assertFalse(output.exists())
            self.assertFalse(list(root.glob(".morpheus-mac-update-*.tmp")))
        target = io.BytesIO()
        writer = mac_update._BoundedWriter(target, 4)
        writer.write(b"123")
        with self.assertRaises(ValueError):
            writer.write(b"45")
        self.assertEqual(target.getvalue(), b"123")





class NativeNoticeEvidenceTests(unittest.TestCase):
    @staticmethod
    def evidence(drop_license=False, registry=False):
        source = native_notices.catalogue()
        files = {"Cargo.toml": b'[workspace]\nmembers = ["app", "vendor/cpal", "vendor/slint"]\n',
                 "Cargo.lock": b'version = 4\n[[package]]\nname = "fixture"\nversion = "0.1.0"\n[[package]]\nname = "cpal"\nversion = "0.18.2"\n[[package]]\nname = "slint"\nversion = "1.18.1"\n',
                 "LICENSE": b"Fixture root source license\n",
                 "app/Cargo.toml": b'[package]\nname = "fixture"\nversion = "0.1.0"\nlicense = "MIT"\n',
                 "app/LICENSE": b"Fixture application license\n",
                 "vendor/cpal/Cargo.toml": b'[package]\nname = "cpal"\nversion = "0.18.2"\nlicense = "Apache-2.0"\n',
                 "vendor/cpal/LICENSE": b"Fixture CPAL license\n",
                 "vendor/cpal/OWNED_COREAUDIO.md": b"Fixture exact local patch provenance\n",
                 "vendor/slint/Cargo.toml": b'[package]\nname = "slint"\nversion = "1.18.1"\nlicense = "GPL-3.0-only"\n',
                 "vendor/slint/LICENSE": b"Fixture Slint license\n",
                 "vendor/slint/LICENSES/extra.txt": b"Fixture extra source license\n"}
        registry_files = {"Cargo.toml": b'[package]\nname = "fixture-registry"\nversion = "1.0.0"\nlicense = "MIT"\n',
                          "LICENSE": b"Fixture exact registry license\n"}
        def packed(values):
            output = io.BytesIO()
            with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as zipped:
                with tarfile.open(fileobj=zipped, mode="w|", format=tarfile.PAX_FORMAT) as archive:
                    for name, data in sorted(values.items()): source.tar_add(archive, name, data)
            return output.getvalue()
        registry_archive = packed({"fixture-registry-1.0.0/" + key: value for key, value in registry_files.items()})
        if registry:
            files["Cargo.lock"] += ('[[package]]\nname = "fixture-registry"\nversion = "1.0.0"\nsource = "'
                + source.REGISTRY + '"\nchecksum = "' + source.digest(registry_archive) + '"\n').encode()
        if drop_license:
            files.pop("LICENSE"); files.pop("app/LICENSE")
        entries = {name: {"mode": "100644", "oid": source.object_id("blob", data), "bytes": len(data)}
                   for name, data in files.items()}
        tree = source.tree_identity(entries)
        commit = ("tree " + tree + "\nauthor Fixture <fixture@example.org> 0 +0000\n\nfixture\n").encode()
        revision = source.object_id("commit", commit)
        workspace = packed(files)
        identity = {"git_identity_schema": 2, "commit": revision, "commit_object_hex": commit.hex(),
                    "tree": tree, "archive_sha256": source.digest(workspace), "git_attributes_applied": False,
                    "files": {name: {**entries[name], "sha256": source.digest(data)} for name, data in files.items()}}
        catalog = source.package_catalog(files)
        records = [{"name": name, "version": version, "source": "committed-workspace",
                    "workspace_archive": "workspace.tar.gz", **metadata}
                   for (name, version), metadata in catalog.items()]
        if registry:
            metadata = source.package_catalog(registry_files, published=True)[("fixture-registry", "1.0.0")]
            records.append({"name":"fixture-registry", "version":"1.0.0", "source":source.REGISTRY,
                "source_archive":"registry/fixture-registry-1.0.0.crate", "archive_sha256":source.digest(registry_archive),
                "archive_bytes":len(registry_archive), **metadata})
        manifest = {"schema": 1, "source": identity, "cargo_lock_sha256": source.digest(files["Cargo.lock"]),
                    "cargo_packages": records, "git_sources": {},
                    "native_sources": {"coverage": "rust-only", "native_sources_complete": False,
                                       "actual_native_build_inputs_verified": False},
                    "distribution_ready": False, "license_compliance_verified": False,
                    "unresolved_release_requirements": list(source.UNRESOLVED)}
        def build(changed=None, omit_registry=False, corrupt_registry=False):
            selected = manifest if changed is None else changed
            raw = json.dumps(selected, sort_keys=True).encode()
            outer = {"workspace.tar.gz": workspace, "MANIFEST.json": raw, "BUILDING.txt": b"Fixture evidence\n"}
            if registry and not omit_registry:
                outer["registry/fixture-registry-1.0.0.crate"] = registry_archive + (b"changed" if corrupt_registry else b"")
            bundle = packed(outer)
            receipt = {"schema": 1, "source_revision": revision, "source_tree": tree,
                       "bundle_sha256": source.digest(bundle), "manifest_sha256": source.digest(raw),
                       "registry_packages": int(registry), "git_sources": 0, "locked_packages": len(records),
                       "distribution_ready": False, "unresolved_release_requirements": list(source.UNRESOLVED)}
            return bundle, json.dumps(receipt).encode()
        return source, files, manifest, revision, build

    def test_source_bound_plain_notices_cover_all_packages_patch_and_slint_without_legal_acceptance(self):
        source, files, manifest, revision, build = self.evidence()
        bundle, receipt = build()
        contents = native_notices.prepare_bytes(bundle, receipt, revision, platform="darwin", version="0.1.0")
        parsed = native_notices.verify_bytes(contents, revision=revision, platform="darwin", version="0.1.0")
        self.assertEqual(len(parsed["locked_packages"]), 3)
        self.assertIn(b"This application uses Slint", contents[native_notices.TEXT])
        self.assertIn(b"Fixture extra source license", contents[native_notices.TEXT])
        self.assertIn(b"Fixture exact local patch provenance", contents[native_notices.TEXT])
        self.assertFalse(parsed["distribution_ready"])
        self.assertFalse(parsed["license_compliance_verified"])
        self.assertFalse(parsed["native_source_evidence"]["bound_to_this_platform_binary"])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            native_notices.write_files(root, contents)
            self.assertEqual(native_notices.read_files(root), contents)
            with self.assertRaises(ValueError): native_notices.write_files(root, contents)

    def test_every_locked_registry_archive_is_required_and_checksum_bound(self):
        source, files, manifest, revision, build = self.evidence(registry=True)
        value = native_notices.prepare_bytes(*build(), revision, platform="win32", version="0.1.0")
        self.assertIn(b"Fixture exact registry license", value[native_notices.TEXT])
        for options in ({"omit_registry": True}, {"corrupt_registry": True}):
            with self.subTest(options=options), self.assertRaises((ValueError, RuntimeError)):
                native_notices.prepare_bytes(*build(**options), revision, platform="win32", version="0.1.0")

    def test_private_windows_source_export_refuses_drives_ads_devices_and_prefix_aliases(self):
        seen={}
        self.assertEqual(native_notices.portable_source_name("vendor/cpal/LICENSE",seen,windows=True),
                         "vendor/cpal/LICENSE")
        for name in ("C:/outside", "file:stream", "dir/NUL.txt", "dir/COM1", "dir/end.", "dir/end ", "dir/a?b"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                native_notices.portable_source_name(name,{},windows=True)
        with self.assertRaises(ValueError):native_notices.portable_source_name("Vendor/other",seen,windows=True)
        with self.assertRaises(ValueError):native_notices.portable_source_name("/".join(["p"]*33),{},windows=True)

    def test_changed_bundle_raw_commit_tree_or_legacy_identity_is_rejected(self):
        source, files, manifest, revision, build = self.evidence()
        for mutation in ("legacy", "raw_commit", "tree"):
            changed = json.loads(json.dumps(manifest))
            if mutation == "legacy": changed["source"].pop("commit_object_hex")
            elif mutation == "raw_commit": changed["source"]["commit_object_hex"] += "00"
            else: changed["source"]["tree"] = "a" * 40
            bundle, receipt = build(changed)
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                native_notices.prepare_bytes(bundle, receipt, revision, platform="darwin", version="0.1.0")
        bundle, receipt = build()
        with self.assertRaises(ValueError):
            native_notices.prepare_bytes(bundle + b"trailing", receipt, revision, platform="win32", version="0.1.0")

    def test_notice_hash_scope_and_false_distribution_gate_are_mandatory(self):
        source, files, manifest, revision, build = self.evidence()
        contents = native_notices.prepare_bytes(*build(), revision, platform="win32", version="0.1.0")
        damaged = dict(contents); damaged[native_notices.TEXT] += b"changed"
        with self.assertRaises(ValueError): native_notices.verify_bytes(damaged)
        value = json.loads(contents[native_notices.INVENTORY]); value["distribution_ready"] = True
        damaged = dict(contents); damaged[native_notices.INVENTORY] = json.dumps(value).encode()
        with self.assertRaises(ValueError): native_notices.verify_bytes(damaged)
        with self.assertRaises(ValueError): native_notices.verify_bytes(contents, platform="darwin")

    def test_missing_license_text_is_recorded_and_never_becomes_distribution_acceptance(self):
        source, files, manifest, revision, build = self.evidence(drop_license=True)
        contents = native_notices.prepare_bytes(*build(), revision, platform="darwin", version="0.1.0")
        value = native_notices.verify_bytes(contents)
        fixture = next(item for item in value["locked_packages"] if item["name"] == "fixture")
        self.assertTrue(fixture["missing_license_text"])
        self.assertFalse(fixture["license_obligations_verified"])
        self.assertFalse(value["distribution_ready"])

    def test_changed_working_source_is_rejected_before_notice_file_publication(self):
        source, files, manifest, revision, build = self.evidence()
        bundle, receipt = build()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name, data in files.items():
                path = root / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(data)
            (root / "app/LICENSE").write_bytes(b"substituted source")
            with self.assertRaises(ValueError):
                native_notices.prepare_bytes(bundle, receipt, revision, platform="win32", version="0.1.0", working=root)
            self.assertFalse((root / native_notices.TEXT).exists())
            self.assertFalse((root / native_notices.INVENTORY).exists())


class NativeSourceAcquisitionTests(unittest.TestCase):
    def test_isolated_file_entry_loads_only_reviewed_siblings_outside_the_checkout(self):
        import subprocess
        scripts=Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory() as temporary:
            result=subprocess.run([sys.executable,"-I","-B",str(scripts/"native_notice_source.py"),"--help"],
                cwd=temporary,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=10,check=False)
        self.assertEqual(result.returncode,0,result.stderr.decode("utf-8","replace"))
        self.assertIn(b"--prepare",result.stdout)
        self.assertIn(b"--finish",result.stdout)
        self.assertLessEqual(len(result.stdout)+len(result.stderr),16384)

    def test_fresh_environment_strips_credentials_proxies_and_configs(self):
        from scripts import native_notice_source as acquisition
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inherited = {"HOME":"/personal", "CARGO_HOME":"/personal-cargo", "RUSTFLAGS":"--cfg injected",
                "HTTPS_PROXY":"private", "GIT_SSH_COMMAND":"untrusted", "GIT_CONFIG_COUNT":"1",
                "CARGO_REGISTRIES_CRATES_IO_TOKEN":"private", "CARGO_PACKAGER_SIGN_PRIVATE_KEY":"private",
                "GITHUB_TOKEN":"private", "DYLD_INSERT_LIBRARIES":"untrusted"}
            with mock.patch.object(acquisition.os, "name", "posix"):
                selected = acquisition.environment_for_acquisition(inherited,root,Path("/pinned/bin/cargo"),
                    Path("/pinned/bin/rustc"),Path("/usr/bin/git"),Path("/pinned/rustup"))
            for name in inherited:
                if name not in {"HOME","CARGO_HOME"}: self.assertNotIn(name, selected)
            self.assertEqual(selected["HOME"],str(root/"home"))
            self.assertEqual(selected["CARGO_HOME"],str(root/"cargo"))
            self.assertEqual(selected["GIT_CONFIG_GLOBAL"],os.devnull)
            self.assertEqual(selected["CARGO_NET_GIT_FETCH_WITH_CLI"],"false")
            self.assertNotIn("/personal",selected["PATH"])

    def test_pinned_windows_tools_use_explicit_rustup_without_requiring_posix_home(self):
        from scripts import native_notice_source as acquisition
        with mock.patch.object(acquisition, "os", type("Windows",(),{"name":"nt"})()), \
                mock.patch.object(acquisition.sys,"platform","win32"), \
                mock.patch.object(acquisition.platform,"machine",return_value="AMD64"), \
                mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)):
            cargo,rustc,git,rustup=acquisition.tools({"RUSTUP_HOME":"/pinned/rustup","ProgramFiles":"/programs"})
        self.assertEqual(cargo.name,"cargo.exe");self.assertEqual(rustc.name,"rustc.exe")
        self.assertEqual(rustup,Path("/pinned/rustup"))
        self.assertIn("1.99.0-x86_64-pc-windows-msvc",str(cargo))

    def test_source_origin_policy_matches_existing_linux_route_without_importing_it_on_windows(self):
        from scripts import native_notice_source as acquisition
        self.assertEqual(len(acquisition.PINS),3)
        files = {"Cargo.toml": b'[workspace]\nmembers = ["app"]\n[patch.crates-io]\n',
                 "app/Cargo.toml":b'[package]\nname = "fixture"\nversion = "0.1.0"\n',
                 "Cargo.lock": b'version = 4\n' + b''.join(('[[package]]\nname = "fixture'+str(i)
                      +'"\nversion = "0.1.0"\nsource = "'+pin+'"\n').encode()
                      for i,pin in enumerate(sorted(acquisition.PINS)))}
        self.assertEqual(len(acquisition.source_inputs(files)),3)
        foreign = dict(files);foreign["app/Cargo.toml"] += b'[dependencies]\nforeign = {git = "https://example.org/repo", rev = "a"}\n'
        with self.assertRaises(ValueError):acquisition.source_inputs(foreign)


class NativeNoticeCommandOwnershipTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform in {"darwin","win32"},"native retained command route")
    def test_separate_raw_streams_and_output_overrun_stop_before_observation(self):
        from scripts import native_notice_source as acquisition
        for overflow,binary in ((False,False),(True,False),(False,True),(True,True)):
            with self.subTest(overflow=overflow,binary=binary), tempfile.TemporaryDirectory() as temporary:
                root=Path(temporary).resolve()
                for name in ("logs","temp"):(root/name).mkdir(mode=0o700)
                environment={key:os.environ[key] for key in ("SystemRoot","WINDIR") if key in os.environ}
                environment.update(HOME=str(root),USERPROFILE=str(root),PATH=str(Path(sys.executable).resolve().parent))
                commands=acquisition.Commands(root,environment)
                program='import os; os.write(1,b"x"*4097); os.write(2,b"stderr")' if overflow else \
                        'import os; os.write(1,b"raw-git-object"); os.write(2,b"independent-stderr")'
                # File/ACL admission is covered separately; this actual retained
                # child case isolates raw pipe bounds, process cleanup and EOF.
                with mock.patch.object(acquisition,"checked",side_effect=lambda path,**kwargs:Path(path)):
                    if overflow:
                        with self.assertRaises(ValueError):
                            commands.run("fixture",[str(Path(sys.executable).resolve()),"-I","-B","-c",program],
                                cwd=root,env=environment,timeout=10,output_limit=4096,binary_output=binary)
                    else:
                        data=commands.run("fixture",[str(Path(sys.executable).resolve()),"-I","-B","-c",program],
                            cwd=root,env=environment,timeout=10,output_limit=4096,binary_output=binary)
                        self.assertEqual(data,b"raw-git-object")
                        self.assertEqual((root/"logs/00001.err").read_bytes(),b"independent-stderr")
                self.assertFalse(commands.cleanup_failed)
                self.assertFalse(commands.retained)
                self.assertEqual(commands.logs_complete,not overflow)
                if binary:
                    self.assertFalse((root/"logs/00001.out").exists())
                    self.assertEqual(list((root/"temp").iterdir()),[])
                else:
                    self.assertLessEqual((root/"logs/00001.out").stat().st_size,4096)
                self.assertIn((root/"logs/00001.err"),list((root/"logs").iterdir()))


class NativeNoticeWindowsOwnershipTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "actual Windows owned file/ACL primitive")
    def test_private_inherited_root_and_opened_handle_reject_hardlinks_and_live_writers(self):
        from scripts import native_notice_windows as windows
        with tempfile.TemporaryDirectory() as temporary:
            security = windows.security()
            root = security.create_root(Path(temporary).resolve())
            target = root/"source.txt";target.write_bytes(b"fixed source data")
            security.path(root,directory=True,private=True)
            descriptor = security.open_read(target,private=True)
            with os.fdopen(descriptor,"rb") as opened:
                self.assertEqual(opened.read(),b"fixed source data")
                # The retained no-write/delete-sharing read rejects a writer,
                # rather than claiming fstat/hash alone prevents that race.
                with self.assertRaises(OSError):
                    target.open("wb")
            linked = root/"second-name.txt";os.link(target,linked)
            with self.assertRaises(ValueError):security.open_read(target,private=True)
            self.assertEqual(target.read_bytes(),b"fixed source data")

    def test_acl_rejects_unknown_ace_and_foreign_writer_but_not_read_only_trustee(self):
        import ctypes
        from ctypes import wintypes
        from scripts import native_notice_windows as windows
        owner, trustee = ctypes.create_string_buffer(16), ctypes.create_string_buffer(16)
        ace = ctypes.create_string_buffer(24)
        class Api:
            mask=0x120089;kind=0
            def GetNamedSecurityInfoW(self,value,kind,flags,owner_out,group,dacl_out,sacl,descriptor):
                owner_out._obj.value=ctypes.addressof(owner);dacl_out._obj.value=1;descriptor._obj.value=1;return 0
            def GetAclInformation(self,dacl,output,size,kind):
                output._obj.count=1;output._obj.used=32;return True
            def GetAce(self,dacl,index,output):
                ctypes.c_ubyte.from_buffer(ace,0).value=self.kind
                ctypes.c_ubyte.from_buffer(ace,2).value=24
                wintypes.DWORD.from_buffer(ace,4).value=self.mask
                output._obj.value=ctypes.addressof(ace);return True
        security=windows.Security.__new__(windows.Security);security.a=Api()
        security.k=type("Kernel",(),{"LocalFree":staticmethod(lambda value:None)})()
        security.sid="current-user";security.default_owner="bound-token-owner"
        security.test_owner="current-user";security.test_writer="foreign"
        security.sid_string=lambda value: security.test_owner if getattr(value,"value",value)==ctypes.addressof(owner) else security.test_writer
        security.acl("fixture")
        security.a.mask=0x2
        with self.assertRaises(ValueError):security.acl("fixture")
        security.a.mask=0x4
        with self.assertRaises(ValueError):security.acl("fixture")
        security.acl("fixture",ancestor=True)
        security.a.kind=9
        with self.assertRaises(ValueError):security.acl("fixture",ancestor=True)
        security.a.kind=0;security.a.mask=0x120089
        security.test_owner="bound-token-owner"
        security.acl("fixture",private=True)
        security.test_writer="bound-token-owner";security.a.mask=0x2
        with self.assertRaises(ValueError):security.acl("fixture",private=True)
        security.test_owner="S-1-5-32-544";security.a.mask=0x120089
        with self.assertRaises(ValueError):security.acl("fixture",private=True)


if __name__ == "__main__":
    unittest.main()
