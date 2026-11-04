import tempfile
import gzip
import hashlib
import io
import json
import struct
import tarfile
import os
import shutil
import stat
import re
import sys
from unittest import mock
import unittest
from pathlib import Path
from types import SimpleNamespace

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
    def evidence(drop_license=False, registry=False, drop_alsa_provenance=False, formfeed=False, source_overrides=None):
        source = native_notices.catalogue()
        files = {"Cargo.toml": b'[workspace]\nmembers = ["app", "vendor/cpal", "vendor/slint"]\nexclude = ["vendor/alsa"]\n[patch.crates-io]\nalsa = { path = "vendor/alsa" }\n',
                 "Cargo.lock": b'version = 4\n[[package]]\nname = "fixture"\nversion = "0.1.0"\n[[package]]\nname = "cpal"\nversion = "0.18.2"\n[[package]]\nname = "slint"\nversion = "1.18.1"\n[[package]]\nname = "alsa"\nversion = "0.11.0"\n',
                 "LICENSE": b"Fixture root source license\n",
                 "app/Cargo.toml": b'[package]\nname = "fixture"\nversion = "0.1.0"\nlicense = "MIT"\n',
                 "app/LICENSE": b"Fixture application license\n",
                 "vendor/cpal/Cargo.toml": b'[package]\nname = "cpal"\nversion = "0.18.2"\nlicense = "Apache-2.0"\n',
                 "vendor/cpal/LICENSE": b"Fixture CPAL license\n",
                 "vendor/cpal/OWNED_COREAUDIO.md": b"Fixture exact local patch provenance\n",
                 "vendor/cpal/OWNED_ALSA.md": b"Fixture exact ALSA patch provenance\n",
                 "vendor/alsa/Cargo.toml": b'[package]\nname = "alsa"\nversion = "0.11.0"\nlicense = "Apache-2.0/MIT"\n',
                 "vendor/alsa/Cargo.toml.orig": b"Fixture preserved upstream manifest\n",
                 "vendor/alsa/LICENSE-APACHE": b"Fixture ALSA Apache license\n",
                 "vendor/alsa/LICENSE-MIT": b"Fixture ALSA MIT license\n",
                 "vendor/slint/Cargo.toml": b'[package]\nname = "slint"\nversion = "1.18.1"\nlicense = "GPL-3.0-only"\n',
                 "vendor/slint/LICENSE": b"Fixture Slint license\n",
                 "vendor/slint/LICENSES/extra.txt": b"Fixture extra source license\n"}
        if source_overrides is not None:files.update(source_overrides)
        registry_files = {"Cargo.toml": b'[package]\nname = "fixture-registry"\nversion = "1.0.0"\nlicense = "MIT"\n',
                          "LICENSE": b"Fixture exact registry license\n"}
        if formfeed:registry_files["LICENSE"] = b"Fixture registry license page one\n\fPage two\n"
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
        if drop_alsa_provenance:
            files.pop("vendor/cpal/OWNED_ALSA.md")
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

    def test_plain_license_preserves_formfeed_and_refuses_every_other_forbidden_control(self):
        license_bytes=b"License page one\r\n\t\fPage two\n"
        self.assertIs(native_notices.plain(license_bytes),license_bytes)
        for code in (*range(32),127):
            if code in (9,10,12,13):continue
            with self.subTest(code=code), self.assertRaises(ValueError):
                native_notices.plain(b"License prefix"+bytes([code])+b"suffix")
        for data in (b"\xff",b"",b"too long"):
            with self.subTest(data=data), self.assertRaises(ValueError):
                native_notices.plain(data,maximum=4)

    def test_formfeed_license_prepare_verify_roundtrip_preserves_bytes_hashes_and_unresolved_flags(self):
        source,files,manifest,revision,build=self.evidence(registry=True,formfeed=True)
        expected=b"Fixture registry license page one\n\fPage two\n"
        checksum=hashlib.sha256(expected).hexdigest()
        for platform in ("darwin","win32"):
            with self.subTest(platform=platform):
                contents=native_notices.prepare_bytes(*build(),revision,platform=platform,version="0.1.0")
                ledger=native_notices.verify_bytes(contents,revision=revision,platform=platform,version="0.1.0")
                record=next(item for item in ledger["locked_packages"] if item["name"]=="fixture-registry")
                self.assertIn({"source_path":"LICENSE","sha256":checksum},record["license_texts"])
                span=next(item for item in ledger["texts"] if item["sha256"]==checksum)
                self.assertEqual(contents[native_notices.TEXT][span["offset"]:span["offset"]+span["bytes"]],expected)
                self.assertEqual(span["bytes"],len(expected))
                self.assertFalse(ledger["distribution_ready"])
                self.assertFalse(ledger["license_compliance_verified"])
                self.assertEqual(ledger["unresolved_release_requirements"],list(native_notices.OPEN))

    def test_empty_optional_nested_vendor_readme_remains_authenticated_source_but_not_notice_text(self):
        name="vendor/slint/README.md"
        source,files,manifest,revision,build=self.evidence(source_overrides={name:b""})
        self.assertEqual(files[name],b"")
        self.assertEqual(manifest["source"]["files"][name],
            {"mode":"100644","oid":source.object_id("blob",b""),"bytes":0,
             "sha256":hashlib.sha256(b"").hexdigest()})
        contents=native_notices.prepare_bytes(*build(),revision,platform="darwin",version="0.1.0")
        ledger=native_notices.verify_bytes(contents,revision=revision,platform="darwin",version="0.1.0")
        self.assertEqual(ledger["source_tree"],manifest["source"]["tree"])
        self.assertFalse(any(item["source_path"]==name for item in ledger["local_patch_provenance"]))
        self.assertTrue(all(item["bytes"]>0 for item in ledger["texts"]))
        for mandatory in ("vendor/cpal/OWNED_COREAUDIO.md","vendor/cpal/OWNED_ALSA.md"):
            self.assertIn({"source_path":mandatory,"sha256":source.digest(files[mandatory])},
                          ledger["local_patch_provenance"])
        self.assertFalse(ledger["distribution_ready"])
        self.assertFalse(ledger["license_compliance_verified"])
        # A nonempty optional README still carries exact authenticated evidence.
        _,_,_,revision,build=self.evidence(source_overrides={name:b"Upstream documentation\n"})
        ledger=native_notices.verify_bytes(native_notices.prepare_bytes(*build(),revision,platform="win32",version="0.1.0"))
        self.assertIn({"source_path":name,"sha256":source.digest(b"Upstream documentation\n")},ledger["local_patch_provenance"])

    def test_empty_mandatory_provenance_license_and_nonempty_unsafe_readme_still_refuse(self):
        cases=[("vendor/README.md",b""),("vendor/cpal/MORPHEUS_PATCH.md",b""),
               ("vendor/cpal/OWNED_COREAUDIO.md",b""),("vendor/cpal/OWNED_ALSA.md",b""),
               ("vendor/slint/LICENSE",b""),("vendor/slint/README.md",b"Bad\x00text"),
               ("vendor/slint/README.md",b"Bad\x1btext")]
        overrides=[{name:body} for name,body in cases]
        # A README explicitly declared as license text remains mandatory.
        overrides.append({"vendor/slint/README.md":b"",
            "vendor/slint/Cargo.toml":b'[package]\nname = "slint"\nversion = "1.18.1"\nlicense-file = "README.md"\n'})
        for changed in overrides:
            with self.subTest(paths=sorted(changed)):
                _,_,_,revision,build=self.evidence(source_overrides=changed)
                with self.assertRaises(ValueError):
                    native_notices.prepare_bytes(*build(),revision,platform="darwin",version="0.1.0")

    def test_source_bound_plain_notices_cover_all_packages_patch_and_slint_without_legal_acceptance(self):
        source, files, manifest, revision, build = self.evidence()
        bundle, receipt = build()
        contents = native_notices.prepare_bytes(bundle, receipt, revision, platform="darwin", version="0.1.0")
        parsed = native_notices.verify_bytes(contents, revision=revision, platform="darwin", version="0.1.0")
        self.assertEqual(len(parsed["locked_packages"]), 4)
        self.assertIn(b"This application uses Slint", contents[native_notices.TEXT])
        self.assertIn(b"Fixture extra source license", contents[native_notices.TEXT])
        self.assertIn(b"Fixture exact local patch provenance", contents[native_notices.TEXT])
        self.assertIn(b"Fixture exact ALSA patch provenance", contents[native_notices.TEXT])
        alsa = next(item for item in parsed["locked_packages"] if item["name"] == "alsa")
        self.assertEqual(alsa["source"], "committed-workspace")
        self.assertEqual(alsa["declared_license"], "Apache-2.0/MIT")
        self.assertTrue({"vendor/alsa/LICENSE-APACHE", "vendor/alsa/LICENSE-MIT"}.issubset(
            {item["source_path"] for item in alsa["license_texts"]}))
        self.assertIn({"source_path": "vendor/cpal/OWNED_ALSA.md",
                       "sha256": source.digest(files["vendor/cpal/OWNED_ALSA.md"])},
                      parsed["local_patch_provenance"])
        windows = native_notices.prepare_bytes(bundle, receipt, revision, platform="win32", version="0.1.0")
        self.assertIn(b"Fixture ALSA MIT license", windows[native_notices.TEXT])
        _, _, _, missing_revision, missing_build = self.evidence(drop_alsa_provenance=True)
        with self.assertRaises(ValueError):
            native_notices.prepare_bytes(*missing_build(), missing_revision, platform="win32", version="0.1.0")
        self.assertFalse(parsed["distribution_ready"])
        self.assertFalse(parsed["license_compliance_verified"])
        self.assertFalse(parsed["native_source_evidence"]["bound_to_this_platform_binary"])
        with tempfile.TemporaryDirectory() as temporary:
            # Canonicalize only our owned fixture, not evidence inside the
            # production reader: Darwin temporary roots may have aliases.
            root = Path(temporary).resolve(strict=True)
            native_notices.write_files(root, contents)
            self.assertEqual(native_notices.read_files(root), contents)
            if os.name != "nt":
                alias = root / "notice-alias"
                alias.symlink_to(root, target_is_directory=True)
                with self.assertRaises(ValueError): native_notices.read_files(alias)
                self.assertEqual(native_notices.read_files(root), contents)
            with self.assertRaises(ValueError): native_notices.write_files(root, contents)

    def test_notice_reader_keeps_windows_birth_identity_and_each_api_change_time(self):
        # Exercise the production reader with real file bytes/descriptor and
        # genuine IDs/size/mtime. Model CPython 3.12's named CreationTime vs
        # handle ChangeTime, plus filename-inferred execute bits on .exe only.
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary).resolve() / "scanner.exe"
            target.write_bytes(b'{"fixture":"owned metadata"}')
            named_stat, handle_stat = Path.lstat, os.fstat
            fields = ("st_dev", "st_ino", "st_mode", "st_nlink", "st_size", "st_mtime_ns", "st_ctime_ns")
            def attempt(mutation=None):
                counts = {"named": 0, "fd": 0, "opened": 0}
                def view(real, kind):
                    counts[kind] += 1
                    value = SimpleNamespace(**{key: getattr(real, key) for key in fields})
                    value.st_birthtime_ns = 1_000_000
                    value.st_ctime_ns = 1_000_000 if kind == "named" else 2_000_000
                    value.st_mode = (value.st_mode & ~0o111) | (0o111 if kind == "named" else 0)
                    if mutation == "protection_changed" and kind == "fd" and counts[kind] == 1: value.st_mode ^= 0o200
                    if mutation == "fd_mode_changed" and kind == "fd" and counts[kind] == 2: value.st_mode ^= 0o200
                    if mutation == "named_mode_changed" and kind == "named" and counts[kind] == 3: value.st_mode ^= 0o111
                    if mutation == "fd_exec_changed" and kind == "fd" and counts[kind] == 2: value.st_mode ^= 0o111
                    if mutation == "type_changed" and kind == "fd" and counts[kind] == 1:
                        value.st_mode = (value.st_mode & ~0o170000) | 0o040000
                    if mutation == "missing_birth": del value.st_birthtime_ns
                    if mutation == "invalid_birth": value.st_birthtime_ns = True
                    if mutation == "fd_change" and kind == "fd" and counts[kind] == 2: value.st_ctime_ns += 1
                    if mutation == "named_change" and kind == "named" and counts[kind] == 3: value.st_ctime_ns += 1
                    if mutation == "birth_change" and kind == "fd" and counts[kind] == 2: value.st_birthtime_ns += 1
                    if mutation == "path_replaced" and kind == "named" and counts[kind] == 3: value.st_ino += 1
                    if mutation == "opened_replaced" and kind == "fd" and counts[kind] == 1: value.st_ino += 1
                    if mutation == "link_change" and kind == "fd" and counts[kind] == 2: value.st_nlink += 1
                    if mutation == "mtime_change" and kind == "fd" and counts[kind] == 2: value.st_mtime_ns += 1
                    return value
                def lstat(path, *args, **kwargs):
                    real = named_stat(path, *args, **kwargs)
                    return view(real, "named") if path == target else real
                def opened(path):
                    self.assertEqual(path, target)
                    counts["opened"] += 1
                    return os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
                security = SimpleNamespace(open_read=opened)
                windows = SimpleNamespace(name="nt", O_RDONLY=os.O_RDONLY, fdopen=os.fdopen,
                    fstat=lambda fd: view(handle_stat(fd), "fd"))
                with mock.patch.object(native_notices, "os", windows), \
                        mock.patch.object(native_notices, "catalogue", return_value=SimpleNamespace(windows_security=lambda:security)), \
                        mock.patch.object(Path, "lstat", autospec=True, side_effect=lstat):
                    value = native_notices.read_regular(target, 1024)
                return value, counts
            self.assertEqual(attempt(), (target.read_bytes(), {"named":3,"fd":2,"opened":1}))
            for mutation in ("missing_birth", "invalid_birth", "fd_change", "named_change", "birth_change",
                             "path_replaced", "opened_replaced", "link_change", "mtime_change",
                             "protection_changed", "fd_mode_changed", "named_mode_changed", "fd_exec_changed", "type_changed"):
                with self.subTest(mutation=mutation), self.assertRaises((ValueError, AttributeError)):
                    attempt(mutation)

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
    def test_cargo_stderr_classification_is_bounded_fixed_and_does_not_reflect_private_bytes(self):
        from scripts import native_notice_source as acquisition
        samples = {"manifest":b"failed to parse manifest", "lockfile":b"needs to be updated but --locked was passed",
                   "checkout":b"filename too long", "network":b"could not resolve host", "tls":b"certificate verify failed",
                   "auth":b"authentication failed"}
        for token, marker in samples.items():
            with self.subTest(token=token):
                body = marker.upper() + b" https://synthetic-private.invalid/path?key=private-body \xff"
                self.assertEqual(acquisition._cargo_stderr_classes(body), [token])
        self.assertEqual(acquisition._cargo_stderr_classes(b"unrecognized synthetic-private-body\x00\xff"), ["other"])
        self.assertEqual(acquisition._cargo_stderr_classes(b""), ["other"])
        self.assertEqual(acquisition._cargo_stderr_classes(b"path too long: synthetic-private-path"), ["checkout"])
        self.assertEqual(acquisition._cargo_stderr_classes(b"authentication failed "*100), ["auth"])
        with self.assertRaises(ValueError):
            acquisition._cargo_stderr_classes(b"x"*(8*1024**2+1))

    def test_cargo_fetch_diagnostic_requires_full_scan_cleanup_owned_complete_stderr_and_closed_status(self):
        from scripts import native_notice_source as acquisition
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            logs = root/"logs"
            path = logs/"00001.err"
            body = b"error: failed to parse manifest at synthetic-private-path; certificate verify failed"
            commands = SimpleNamespace(root=root, logs=logs, logs_complete=True, cleanup_failed=False,
                                       retained=[], cargo_fetch_failure=(path,101,len(body)))
            output = io.StringIO()
            with mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)) as checked, \
                    mock.patch.object(acquisition.N,"read_regular",return_value=body) as reader, \
                    mock.patch.object(acquisition.sys,"stderr",output):
                acquisition._notice_report_cargo_fetch_failure(commands, False)
                reader.assert_not_called();checked.assert_not_called()
                for name,value in (("logs_complete",False),("cleanup_failed",True),("retained",[object()])):
                    with mock.patch.object(commands,name,value):
                        acquisition._notice_report_cargo_fetch_failure(commands,True)
                    reader.assert_not_called();checked.assert_not_called()
                self.assertEqual(output.getvalue(), "")
                acquisition._notice_report_cargo_fetch_failure(commands,True)
                checked.assert_called_once_with(path,private=True)
                reader.assert_called_once_with(path,8*1024**2,allow_empty=True)
            facts = json.loads(output.getvalue().removeprefix("MORPHEUS_NATIVE_NOTICE_CARGO_FETCH "))
            self.assertEqual(facts,{"schema":1,"check":"native_notice_cargo_fetch_failure","status":101,
                                    "stderr_classes":["manifest","tls"]})
            self.assertNotIn("synthetic-private",output.getvalue())
            self.assertNotIn(str(root),output.getvalue())
            for failure in ((root/"foreign.err",101,len(body)),(path,True,len(body)),(path,0,len(body)),
                            (path,2**32,len(body)),(path,101,8*1024**2+1),(path,101,len(body)+1)):
                output = io.StringIO()
                with self.subTest(failure=str(failure[1:])), \
                        mock.patch.object(commands,"cargo_fetch_failure",failure), \
                        mock.patch.object(acquisition,"checked",return_value=path), \
                        mock.patch.object(acquisition.N,"read_regular",return_value=body), \
                        mock.patch.object(acquisition.sys,"stderr",output):
                    acquisition._notice_report_cargo_fetch_failure(commands,True)
                self.assertEqual(output.getvalue(), "")
            with mock.patch.object(acquisition,"checked",side_effect=ValueError("synthetic-private-refusal")), \
                    mock.patch.object(acquisition.N,"read_regular") as reader, \
                    mock.patch.object(acquisition.sys,"stderr",io.StringIO()) as output:
                acquisition._notice_report_cargo_fetch_failure(commands,True)
                reader.assert_not_called()
                self.assertEqual(output.getvalue(), "")

    @unittest.skipIf(os.name == "nt", "mocked POSIX child pipes, no process execution")
    def test_failed_cargo_fetch_retains_status_only_after_owned_stop_and_full_pipe_drain(self):
        from scripts import native_notice_source as acquisition, package_smoke, macos_package_ui
        body = b"error: failed to load manifest synthetic-private-body"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root/"logs").mkdir(mode=0o700);(root/"temp").mkdir(mode=0o700)
            commands = acquisition.Commands(root,{})
            events = []
            def spawned(*args,**kwargs):
                os.write(kwargs["stderr"].fileno(),body)
                return SimpleNamespace(returncode=101)
            def stopped(process):
                self.assertIsNone(commands.cargo_fetch_failure)
                events.append("stopped")
            with mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)), \
                    mock.patch.object(acquisition.subprocess,"Popen",side_effect=spawned), \
                    mock.patch.object(package_smoke,"process_status",return_value=101), \
                    mock.patch.object(macos_package_ui,"stop_group",side_effect=stopped), \
                    self.assertRaises(ValueError):
                commands.run("fetch",["/reviewed/cargo","fetch","--locked"],cwd=root,env={},timeout=1,
                             output_limit=1024,stderr_limit=1024)
            self.assertEqual(events,["stopped"])
            self.assertTrue(commands.logs_complete)
            self.assertFalse(commands.cleanup_failed)
            self.assertEqual(commands.cargo_fetch_failure,(root/"logs/00001.err",101,len(body)))
            self.assertEqual((root/"logs/00001.err").read_bytes(),body)

    def test_notice_failure_site_uses_owned_caller_code_only_without_private_values(self):
        from scripts import native_notice_source as acquisition
        marker = "synthetic-private-diagnostic-value"
        try:
            acquisition.checked(Path("relative-source-site"), private=True)
        except ValueError as error:
            # Exception arguments/chain may contain secrets or paths. Neither
            # is an input to the bounded code-location diagnostic.
            error.args = (marker, {"private_body": marker})
            output = io.StringIO()
            with mock.patch.object(acquisition, "_NOTICE_FAILURE_PHASE", "git_export"), \
                    mock.patch.object(acquisition.sys, "stderr", output):
                acquisition._notice_report_failure(error)
        lines = output.getvalue().splitlines()
        self.assertEqual(len(lines), 2)
        failure = json.loads(lines[0].removeprefix("MORPHEUS_NATIVE_NOTICE_FAILURE "))
        self.assertEqual(failure, {"phase":"git_export", "exception":"value_error", "errno":None, "winerror":None})
        site = json.loads(lines[1].removeprefix("MORPHEUS_NATIVE_NOTICE_SITE "))
        self.assertEqual(set(site), {"module", "line"})
        self.assertEqual(site["module"], "notice_source")
        self.assertIs(type(site["line"]), int)
        self.assertTrue(acquisition.checked.__code__.co_firstlineno <= site["line"]
                        < acquisition.checked.__code__.co_firstlineno + 10)
        self.assertNotEqual(site["line"], acquisition.require.__code__.co_firstlineno + 2)
        self.assertNotIn(marker, output.getvalue())
        self.assertNotIn("relative-source-site", output.getvalue())
        self.assertNotIn("native_notice_source.py", output.getvalue())
        self.assertNotIn("checked", output.getvalue())

    def test_notice_failure_site_omits_unknown_inner_code_and_excess_trace_depth(self):
        from scripts import native_notice_source as acquisition
        marker = "synthetic-private-diagnostic-value"
        foreign = "/synthetic-private-home/foreign-diagnostic.py"
        namespace = {"__file__":foreign}
        exec(compile("def foreign():\n    raise ValueError('synthetic-private-diagnostic-value')\n",
                     foreign, "exec"), namespace)
        def deep(depth):
            if depth:
                return deep(depth - 1)
            return acquisition.checked(Path("relative-source-site"))
        class UnavailableMetadata(ValueError):
            def __getattribute__(self, name):
                if name == "__traceback__":
                    raise RuntimeError("synthetic-private-diagnostic-value")
                return super().__getattribute__(name)
        errors = []
        for invoke in (namespace["foreign"], lambda:deep(40)):
            try:
                invoke()
            except ValueError as error:
                errors.append(error)
        errors.append(UnavailableMetadata(marker))
        for error in errors:
            output = io.StringIO()
            with self.subTest(kind=type(error).__name__), \
                    mock.patch.object(acquisition, "_NOTICE_FAILURE_PHASE", "source"), \
                    mock.patch.object(acquisition.sys, "stderr", output):
                acquisition._notice_report_failure(error)
            lines = output.getvalue().splitlines()
            self.assertEqual(len(lines), 1)
            failure = json.loads(lines[0].removeprefix("MORPHEUS_NATIVE_NOTICE_FAILURE "))
            self.assertEqual(set(failure), {"phase", "exception", "errno", "winerror"})
            self.assertEqual(failure["phase"], "source")
            self.assertNotIn("MORPHEUS_NATIVE_NOTICE_SITE", output.getvalue())
            self.assertNotIn(marker, output.getvalue())
            self.assertNotIn(foreign, output.getvalue())

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
                mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)), \
                mock.patch.object(acquisition,"system_git",return_value=Path("/programs/Git/cmd/git.exe")) as selected:
            cargo,rustc,git,rustup=acquisition.tools({"RUSTUP_HOME":"/pinned/rustup","PROGRAMFILES":"/programs"})
        selected.assert_called_once_with()
        self.assertEqual(cargo.name,"cargo.exe");self.assertEqual(rustc.name,"rustc.exe")
        self.assertEqual(rustup,Path("/pinned/rustup"))
        self.assertIn("1.99.0-x86_64-pc-windows-msvc",str(cargo))
        self.assertEqual(git,Path("/programs/Git/cmd/git.exe"))

    @unittest.skipIf(os.name == "nt", "Windows path/ACL admission is modeled without native calls")
    def test_windows_system_git_hardlinks_keep_exact_path_and_all_acl_guards(self):
        from scripts import native_notice_windows as windows
        tool = Path("/programs/Git/cmd/git.exe")
        unrelated = [Path("/programs/Git/bin/git.exe"),Path("/owned/scanner.exe"),
                     Path("/owned/source.json"),Path("/owned/private.json")]
        files = {tool,*unrelated}
        security = object.__new__(windows.Security)
        security.k = SimpleNamespace(GetDriveTypeW=mock.Mock(return_value=3))
        security.acl = mock.Mock()
        def metadata(path):
            return SimpleNamespace(st_mode=0o100777 if path in files else 0o40777,
                                   st_nlink=2 if path in files else 1,st_file_attributes=0)
        with mock.patch.object(windows,"os",SimpleNamespace(name="nt",environ={"PROGRAMFILES":"/programs"})), \
                mock.patch.object(Path,"resolve",autospec=True,side_effect=lambda path,strict:path) as resolved, \
                mock.patch.object(Path,"lstat",autospec=True,side_effect=metadata) as inspected:
            self.assertEqual(security.system_git(),tool)
            self.assertEqual(security.acl.call_args_list,
                [mock.call(str(path),private=False,ancestor=path!=tool) for path in (tool,*tool.parents)])
            for path in [tool,*unrelated]:
                for private in (False,True):
                    with self.subTest(path=str(path),private=private),self.assertRaises(ValueError):
                        security.path(path,private=private)
            with self.assertRaises(TypeError):security.system_git(unrelated[0])
            for changed_path,field,value in ((tool,"st_nlink",0),(tool,"st_mode",0o40777),
                    (tool,"st_mode",0o120777),(tool,"st_file_attributes",0x400),
                    (tool.parent,"st_mode",0o120777),(tool.parent,"st_file_attributes",0x400),
                    (Path("/programs"),"st_file_attributes",0x400)):
                def changed(path):
                    info=metadata(path)
                    if path==changed_path:setattr(info,field,value)
                    return info
                inspected.side_effect=changed
                with self.subTest(path=str(changed_path),field=field,value=value),self.assertRaises(ValueError):
                    security.system_git()
            inspected.side_effect=metadata
            for component in (tool,*tool.parents):
                def changed_acl(path,**kwargs):
                    if path==str(component):raise ValueError("ownership or write ACL changed")
                security.acl.side_effect=changed_acl
                with self.subTest(component=str(component)),self.assertRaises(ValueError):security.system_git()
            security.acl.side_effect=None
            security.k.GetDriveTypeW.return_value=4
            with self.assertRaises(ValueError):security.system_git()
            security.k.GetDriveTypeW.return_value=3
            with mock.patch.object(Path,"resolve",return_value=unrelated[0]),self.assertRaises(ValueError):
                security.system_git()
            for program in ("relative","//server/share"):
                resolved.reset_mock();inspected.reset_mock();security.acl.reset_mock()
                with self.subTest(program=program), \
                        mock.patch.object(windows,"os",SimpleNamespace(name="nt",environ={"PROGRAMFILES":program})), \
                        self.assertRaises(ValueError):
                    security.system_git()
                resolved.assert_not_called();inspected.assert_not_called();security.acl.assert_not_called()
            resolved.side_effect=lambda path,strict:Path("/redirect/Git/cmd/git.exe")
            inspected.reset_mock();security.acl.reset_mock()
            with mock.patch.object(windows,"os",SimpleNamespace(name="nt",environ={"PROGRAMFILES":"/programs/../redirect"})), \
                    self.assertRaises(ValueError):
                security.system_git()
            inspected.assert_not_called();security.acl.assert_not_called()

    @unittest.skipIf(os.name == "nt", "Windows launch admission is modeled without spawning native code")
    def test_windows_system_git_rechecks_acl_at_tools_and_before_every_spawn(self):
        from scripts import native_notice_source as acquisition, native_notice_windows as windows
        from scripts import run_windows_capture_acceptance as capture
        tool=Path("/programs/Git/cmd/git.exe")
        host=SimpleNamespace(name="nt",environ={"PROGRAMFILES":"/programs"})
        security=object.__new__(windows.Security)
        security.k=SimpleNamespace(GetDriveTypeW=lambda anchor:3)
        security.acl=mock.Mock()
        def metadata(path):
            regular=path.suffix==".exe"
            return SimpleNamespace(st_mode=0o100777 if regular else 0o40777,
                st_nlink=2 if regular and not str(path).startswith("/pinned/") else 1,st_file_attributes=0)
        with mock.patch.object(acquisition,"os",host),mock.patch.object(windows,"os",host), \
                mock.patch.object(acquisition.sys,"platform","win32"), \
                mock.patch.object(acquisition.platform,"machine",return_value="AMD64"), \
                mock.patch.object(acquisition.S,"windows_security",return_value=security), \
                mock.patch.object(Path,"resolve",autospec=True,side_effect=lambda path,strict:path), \
                mock.patch.object(Path,"lstat",autospec=True,side_effect=metadata), \
                mock.patch.object(security,"system_git",wraps=security.system_git) as selected, \
                mock.patch.object(acquisition.subprocess,"Popen") as posix_spawn, \
                mock.patch.object(capture,"WindowsJob") as windows_spawn:
            _,_,git,_=acquisition.tools({"RUSTUP_HOME":"/pinned/rustup"})
            self.assertEqual(git,tool);selected.assert_called_once_with()
            commands=acquisition.Commands(Path("/owned"),{})
            security.acl.side_effect=ValueError("ownership or write ACL changed")
            with self.assertRaises(ValueError):
                commands.run("git",[git,"--version"],cwd=Path("/owned"),env={},timeout=1,output_limit=1024)
            self.assertEqual(selected.call_args_list,[mock.call(),mock.call()])
            self.assertEqual(commands.sequence,0)
            security.acl.side_effect=None
            for alias in ("/programs/Git/bin/git.exe","/owned/scanner.exe"):
                with self.subTest(alias=alias),self.assertRaises(ValueError):
                    commands.run("tool",[alias,"--version"],cwd=Path("/owned"),env={},timeout=1,output_limit=1024)
            self.assertEqual(selected.call_count,2)
            self.assertEqual(commands.sequence,0)
            posix_spawn.assert_not_called();windows_spawn.assert_not_called()

    def test_apple_system_git_shim_accepts_only_protected_root_owned_hardlinks(self):
        from scripts import native_notice_source as acquisition
        tool = Path("/usr/bin/git")
        def metadata(path):
            return SimpleNamespace(st_mode=0o100755 if path == tool else 0o40755,
                                   st_uid=0, st_nlink=78 if path == tool else 2)
        with mock.patch.object(acquisition.sys,"platform","darwin"), \
                mock.patch.object(Path,"resolve",autospec=True,side_effect=lambda path,strict: path), \
                mock.patch.object(Path,"lstat",autospec=True,side_effect=metadata) as inspected:
            self.assertEqual(acquisition.system_git(),tool)
            for target,field,value in ((tool,"st_uid",501),(tool,"st_mode",0o100775),
                    (tool,"st_mode",0o120777),(tool,"st_mode",0o100644),
                    (tool,"st_mode",0o40755),(tool,"st_nlink",0),
                    (tool.parent,"st_uid",501),(tool.parent,"st_mode",0o40775),
                    (Path("/usr"),"st_mode",0o120755),(Path("/"),"st_mode",0o40777)):
                def changed(path):
                    info = metadata(path)
                    if path == target: setattr(info,field,value)
                    return info
                inspected.side_effect = changed
                with self.subTest(path=str(target),field=field,value=value), self.assertRaises(ValueError):
                    acquisition.system_git()
            inspected.side_effect = metadata
            # The data/scanner/source admission remains single-link even for
            # the very same protected regular-file metadata.
            with mock.patch.object(acquisition,"os",SimpleNamespace(name="posix",getuid=lambda:501)), \
                    self.assertRaises(ValueError):
                acquisition.checked(tool)
        with mock.patch.object(acquisition.sys,"platform","darwin"), \
                mock.patch.object(Path,"resolve",return_value=Path("/foreign/git")), \
                mock.patch.object(Path,"lstat") as inspected, self.assertRaises(ValueError):
            acquisition.system_git()
        inspected.assert_not_called()
        with mock.patch.object(acquisition.sys,"platform","win32"), \
                mock.patch.object(Path,"resolve") as resolved, self.assertRaises(ValueError):
            acquisition.system_git()
        resolved.assert_not_called()

    @unittest.skipIf(os.name == "nt", "Apple Git launch fixture uses POSIX paths and pipes")
    def test_command_launch_admits_only_exact_darwin_system_git(self):
        from scripts import native_notice_source as acquisition
        class LaunchReached(RuntimeError): pass
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root/"logs").mkdir(mode=0o700); (root/"temp").mkdir(mode=0o700)
            commands = acquisition.Commands(root,{})
            with mock.patch.object(acquisition.sys,"platform","darwin"), \
                    mock.patch.object(acquisition,"system_git",return_value=Path("/usr/bin/git")) as apple, \
                    mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)) as strict, \
                    mock.patch.object(acquisition.subprocess,"Popen",side_effect=LaunchReached) as spawn:
                with self.assertRaises(LaunchReached):
                    commands.run("git",[Path("/usr/bin/git"),"--version"],cwd=root,env={},timeout=1,output_limit=1024)
            apple.assert_called_once_with()
            strict.assert_called_once_with(root,directory=True)
            self.assertEqual(spawn.call_args.args[0],["/usr/bin/git","--version"])
            # No other spelling, platform or executable may inherit the Apple
            # hardlink exception; strict refusal must happen before any child.
            for system, executable in (("darwin","/usr/bin/cargo"),("darwin","/usr/local/bin/git"),
                    ("darwin","git"),("darwin","/usr/bin/./git"),("darwin","/usr/bin/git-link"),
                    ("linux","/usr/bin/git"),("win32","/programs/Git/cmd/git.exe")):
                with self.subTest(system=system,executable=executable), \
                        mock.patch.object(acquisition.sys,"platform",system), \
                        mock.patch.object(acquisition,"system_git") as apple, \
                        mock.patch.object(acquisition,"checked",side_effect=ValueError("strict refusal")) as strict, \
                        mock.patch.object(acquisition.subprocess,"Popen") as spawn:
                    with self.assertRaises(ValueError):
                        commands.run("tool",[executable,"--version"],cwd=root,env={},timeout=1,output_limit=1024)
                    strict.assert_called_once_with(Path(executable))
                    apple.assert_not_called(); spawn.assert_not_called()

    @unittest.skipIf(os.name == "nt", "Apple Git launch fixture uses POSIX paths and pipes")
    def test_command_launch_revalidates_system_git_before_each_child_spawn(self):
        from scripts import native_notice_source as acquisition
        class LaunchReached(RuntimeError): pass
        tool = Path("/usr/bin/git")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root/"logs").mkdir(mode=0o700); (root/"temp").mkdir(mode=0o700)
            commands = acquisition.Commands(root,{})
            rejected = False
            def metadata(path):
                return SimpleNamespace(st_mode=0o100755 if path == tool else 0o40755,
                                       st_uid=501 if rejected and path == tool else 0,
                                       st_nlink=78 if path == tool else 2)
            with mock.patch.object(acquisition.sys,"platform","darwin"), \
                    mock.patch.object(Path,"resolve",autospec=True,side_effect=lambda path,strict:path), \
                    mock.patch.object(Path,"lstat",autospec=True,side_effect=metadata), \
                    mock.patch.object(acquisition,"system_git",wraps=acquisition.system_git) as apple, \
                    mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)) as strict, \
                    mock.patch.object(acquisition.subprocess,"Popen",side_effect=LaunchReached) as spawn:
                with self.assertRaises(LaunchReached):
                    commands.run("git",[tool,"--version"],cwd=root,env={},timeout=1,output_limit=1024)
                self.assertEqual(spawn.call_count,1)
                rejected = True
                with self.assertRaises(ValueError):
                    commands.run("git",[tool,"--version"],cwd=root,env={},timeout=1,output_limit=1024)
                self.assertEqual(spawn.call_count,1)  # Changed ownership never reaches spawn.
                self.assertEqual(apple.call_args_list,[mock.call(),mock.call()])
                strict.assert_called_once_with(root,directory=True)
                self.assertEqual(commands.sequence,1)  # Refusal also precedes log/file allocation.

    def test_copied_windows_environment_preserves_actual_host_guard_and_system_directory(self):
        from scripts import native_notice_source as acquisition
        # dict(os.environ) on Windows contains uppercase keys, unlike the old
        # mixed-case mock. Use the real guard and allowlist with that exact shape.
        context = {"owner_id":42,"sender_id":42,"private":False,
                   "inputs":{"source_revision":"a"*40,"platform":"windows","linux_formats":"appimage",
                             "appimage_updates":False,"appimage_capture":False,
                             "source_bundle":False,"native_updates":False}}
        inherited = {"GITHUB_ACTIONS":"true","RUNNER_ENVIRONMENT":"github-hosted",
            "GITHUB_REPOSITORY":"brk275/Morpheus","GITHUB_REF":"refs/heads/main",
            "GITHUB_EVENT_NAME":"workflow_dispatch","GITHUB_ACTOR":"brk275",
            "GITHUB_TRIGGERING_ACTOR":"brk275","RUNNER_OS":"Windows","IMAGEOS":"win25",
            "GITHUB_EVENT_PATH":"/unsafe/shared/event.json","SYSTEMROOT":"/windows","WINDIR":"/windows",
            "GITHUB_SHA":"b"*40,"MORPHEUS_PACKAGE_SOURCE_REVISION":"a"*40,
            "MORPHEUS_NATIVE_PACKAGE_CONTEXT":json.dumps(context),
            "RUNNER_TEMP":"/runner/temp","PROGRAMFILES":"/programs","USERPROFILE":"/old/profile",
            "GITHUB_TOKEN":"private","HTTPS_PROXY":"private","RUSTFLAGS":"--cfg injected"}
        with mock.patch.object(acquisition.platform,"machine",return_value="AMD64"), \
                mock.patch.object(acquisition.N,"read_regular",side_effect=AssertionError("unsafe event file opened")) as read_event:
            acquisition.guard(inherited,system="win32")
            for image in ("win22",None):
                rejected = dict(inherited)
                if image is None: rejected.pop("IMAGEOS")
                else: rejected["IMAGEOS"] = image
                with self.assertRaises(ValueError): acquisition.guard(rejected,system="win32")
            malformed = [None, "not-json", "[]", json.dumps(context)+" "*(1024*1024+1), "é",
                         json.dumps(context)[:-1]+',"owner_id":42}',
                         json.dumps(context).replace('"native_updates": false', '"native_updates": false,"native_updates": false')]
            for field,value in (("owner_id",True),("owner_id","42"),("owner_id",0),("owner_id",-1),("sender_id",True),
                                ("sender_id",43),("sender_id",42.0),("private",True),("private","false"),("inputs",[])):
                changed=json.loads(json.dumps(context));changed[field]=value;malformed.append(json.dumps(changed))
            for field,value in (("platform","unknown"),("platform",42),("linux_formats","unknown"),
                                ("source_revision","b"*40),("native_updates","false"),("appimage_updates",0),("source_bundle",True)):
                changed=json.loads(json.dumps(context));changed["inputs"][field]=value;malformed.append(json.dumps(changed))
            for nested in (False,True):
                changed=json.loads(json.dumps(context));(changed["inputs"] if nested else changed)["unknown"]=False
                malformed.append(json.dumps(changed))
                changed=json.loads(json.dumps(context));(changed["inputs"] if nested else changed).pop("platform" if nested else "owner_id")
                malformed.append(json.dumps(changed))
            for index,value in enumerate(malformed):
                rejected=dict(inherited)
                if value is None: rejected.pop("MORPHEUS_NATIVE_PACKAGE_CONTEXT")
                else: rejected["MORPHEUS_NATIVE_PACKAGE_CONTEXT"]=value
                with self.subTest(case=index), self.assertRaises((ValueError, acquisition.S.SourceFailure)):
                    acquisition.guard(rejected,system="win32")
            read_event.assert_not_called()
            # Check the actual prepare/finish caller templates, preserving JSON
            # booleans and IDs rather than interpolating anything into a shell.
            workflow=(Path(__file__).resolve().parents[2]/".github/workflows/package-qa.yml").read_text()
            templates=re.findall(r"MORPHEUS_NATIVE_PACKAGE_CONTEXT: >-\n +([^\n]+)",workflow)
            self.assertEqual(len(templates),2);self.assertEqual(templates[0],templates[1])
            values={"github.event.repository.owner.id":42,"github.event.sender.id":42,
                    "github.event.repository.private":False,"needs.scope.outputs.source_revision":"a"*40,
                    **{"inputs."+key:value for key,value in context["inputs"].items() if key!="source_revision"}}
            for template in templates:
                rendered=re.sub(r"\$\{\{ toJSON\(([^)]+)\) \}\}",lambda match:json.dumps(values[match[1]]),template)
                self.assertNotIn("${{",rendered);self.assertEqual(json.loads(rendered),context)
                acquisition.guard({**inherited,"MORPHEUS_NATIVE_PACKAGE_CONTEXT":rendered},system="win32")
            read_event.assert_not_called()
        with mock.patch.object(acquisition,"os",type("Windows",(),{"name":"nt","pathsep":";"})()), \
                mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)) as checked:
            root = Path("/private/source")
            selected = acquisition.environment_for_acquisition(inherited,root,Path("/pinned/bin/cargo.exe"),
                Path("/pinned/bin/rustc.exe"),Path("/programs/Git/cmd/git.exe"),Path("/pinned/rustup"))
        checked.assert_called_once_with(Path("/windows/System32"),directory=True)
        self.assertEqual(selected["SYSTEMROOT"],"/windows")
        self.assertEqual(selected["IMAGEOS"],"win25")
        self.assertEqual(selected["WINDIR"],"/windows")
        self.assertEqual(selected["PATH"],";".join(str(Path(value)) for value in
            ("/pinned/bin","/programs/Git/cmd","/windows/System32")))
        self.assertEqual(selected["USERPROFILE"],str(root/"home"))
        for name in ("SystemRoot","ImageOS","PROGRAMFILES","GITHUB_TOKEN","HTTPS_PROXY","RUSTFLAGS","MORPHEUS_NATIVE_PACKAGE_CONTEXT"):
            self.assertNotIn(name,selected)

    def test_unix_host_environment_keeps_case_sensitive_image_and_system_names(self):
        from scripts import native_notice_source as acquisition
        context = {"owner_id":42,"sender_id":42,"private":False,
                   "inputs":{"source_revision":"a"*40,"platform":"macos","linux_formats":"appimage",
                             "appimage_updates":False,"appimage_capture":False,
                             "source_bundle":False,"native_updates":False}}
        inherited = {"GITHUB_ACTIONS":"true","RUNNER_ENVIRONMENT":"github-hosted",
            "GITHUB_REPOSITORY":"brk275/Morpheus","GITHUB_REF":"refs/heads/main",
            "GITHUB_EVENT_NAME":"workflow_dispatch","GITHUB_ACTOR":"brk275",
            "GITHUB_TRIGGERING_ACTOR":"brk275","RUNNER_OS":"macOS","ImageOS":"macos15",
            "GITHUB_EVENT_PATH":"/unsafe/shared/event.json","SystemRoot":"/original-system",
            "GITHUB_SHA":"b"*40,"MORPHEUS_PACKAGE_SOURCE_REVISION":"a"*40,
            "MORPHEUS_NATIVE_PACKAGE_CONTEXT":json.dumps(context),
            "SYSTEMROOT":"/foreign-system","IMAGEOS":"win25"}
        with mock.patch.object(acquisition.platform,"machine",return_value="x86_64"), \
                mock.patch.object(acquisition.N,"read_regular",side_effect=AssertionError("unsafe event file opened")) as read_event:
            acquisition.guard(inherited,system="darwin")
            with self.assertRaises(ValueError):
                acquisition.guard({key:value for key,value in inherited.items() if key != "ImageOS"},system="darwin")
            read_event.assert_not_called()
        with mock.patch.object(acquisition,"os",type("Unix",(),{"name":"posix","pathsep":":"})()):
            selected = acquisition.environment_for_acquisition(inherited,Path("/private/source"),Path("/pinned/bin/cargo"),
                Path("/pinned/bin/rustc"),Path("/usr/bin/git"),Path("/pinned/rustup"))
        self.assertEqual(selected["ImageOS"],"macos15")
        self.assertEqual(selected["SystemRoot"],"/original-system")
        self.assertNotIn("IMAGEOS",selected)
        self.assertNotIn("SYSTEMROOT",selected)

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


    def test_source_parent_keeps_windows_owned_home_and_unix_existing_temp_policy(self):
        from scripts import native_notice_source as acquisition
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary).resolve()
            other = home/"provider-shared-temp"
            with mock.patch.object(acquisition, "os", SimpleNamespace(name="nt")), \
                    mock.patch.object(acquisition.Path, "home", return_value=home), \
                    mock.patch.object(acquisition, "checked", side_effect=lambda value,**kwargs:value) as check:
                self.assertEqual(acquisition.source_parent({"RUNNER_TEMP":str(other)}), home)
                check.assert_called_once_with(home, directory=True)
            with mock.patch.object(acquisition, "os", SimpleNamespace(name="posix")), \
                    mock.patch.object(acquisition, "checked", side_effect=lambda value,**kwargs:value) as check:
                self.assertEqual(acquisition.source_parent({"RUNNER_TEMP":str(other)}), other)
                check.assert_called_once_with(other, directory=True)

    def test_reserved_source_root_binds_all_paths_revision_and_exact_directory_identity(self):
        from scripts import native_notice_source as acquisition
        with tempfile.TemporaryDirectory() as temporary:
            parent=Path(temporary).resolve();root=parent/("morpheus-native-source-"+"a"*32);root.mkdir(mode=0o700)
            info=root.lstat();marker={"schema":1,"device":info.st_dev,"inode":info.st_ino,"source_revision":"a"*40}
            environment={"MORPHEUS_PACKAGE_SOURCE_REVISION":"a"*40,"GITHUB_SHA":"b"*40,
                "MORPHEUS_NATIVE_SOURCE_ROOT":str(root),
                "MORPHEUS_NATIVE_SOURCE_BUNDLE":str(root/"output"/acquisition.S.BUNDLE_NAME),
                "MORPHEUS_NATIVE_SOURCE_RECEIPT":str(root/"receipt.json")}
            owner=root/"owner.json"
            with mock.patch.object(acquisition,"source_parent",return_value=parent), \
                    mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)), \
                    mock.patch.object(acquisition.S,"read_owned",side_effect=lambda path,*args,**kwargs:path.read_bytes()):
                owner.write_text(json.dumps(marker))
                self.assertEqual(acquisition.owned_source_root(environment),root)
                for key in ("MORPHEUS_NATIVE_SOURCE_BUNDLE","MORPHEUS_NATIVE_SOURCE_RECEIPT"):
                    with self.subTest(export=key),self.assertRaises(ValueError):
                        acquisition.owned_source_root({**environment,key:str(parent/"unrelated")})
                for key,value in (("schema",True),("device",True),("inode",info.st_ino+1),
                                  ("source_revision","b"*40),("unexpected",False)):
                    owner.write_text(json.dumps({**marker,key:value}))
                    with self.subTest(marker=key),self.assertRaises(ValueError):acquisition.owned_source_root(environment)
                owner.write_text(json.dumps(marker))
                with mock.patch.object(acquisition,"source_parent",return_value=parent/"different"),self.assertRaises(ValueError):
                    acquisition.owned_source_root(environment)
                with self.assertRaises(ValueError):
                    acquisition.owned_source_root({**environment,"MORPHEUS_NATIVE_SOURCE_ROOT":str(parent/"wrong-prefix")})
                self.assertEqual(acquisition.owned_source_root(environment),root)

    def test_cleanup_distinguishes_unused_reservation_from_unreceipted_acquisition(self):
        from scripts import native_notice_source as acquisition
        for extra in (None,"acquisition-started.json","unexpected-child"):
            with self.subTest(extra=extra),tempfile.TemporaryDirectory() as temporary:
                parent=Path(temporary).resolve();root=parent/("morpheus-native-source-"+"a"*32);root.mkdir(mode=0o700)
                info=root.lstat();(root/"owner.json").write_text(json.dumps({"schema":1,"device":info.st_dev,
                    "inode":info.st_ino,"source_revision":"a"*40}))
                environment={"MORPHEUS_PACKAGE_SOURCE_REVISION":"a"*40,"GITHUB_SHA":"b"*40,
                    "MORPHEUS_NATIVE_SOURCE_ROOT":str(root),
                    "MORPHEUS_NATIVE_SOURCE_BUNDLE":str(root/"output"/acquisition.S.BUNDLE_NAME),
                    "MORPHEUS_NATIVE_SOURCE_RECEIPT":str(root/"receipt.json")}
                if extra:(root/extra).write_bytes(b"phase marker, not a stop or scan receipt")
                with mock.patch.object(acquisition,"guard"),mock.patch.object(acquisition,"source_parent",return_value=parent), \
                        mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)), \
                        mock.patch.object(acquisition.S,"read_owned",side_effect=lambda path,*args,**kwargs:path.read_bytes()), \
                        mock.patch.object(acquisition,"tools",side_effect=AssertionError("no acquisition receipt")) as tools, \
                        mock.patch.object(acquisition.shutil,"rmtree") as remove, mock.patch("sys.stdout",new_callable=io.StringIO) as output:
                    if extra:
                        with self.assertRaises(OSError):acquisition.finish(environment,parent/"scanner")
                        self.assertTrue(root.exists());self.assertEqual(output.getvalue(),"")
                    else:
                        acquisition.finish(environment,parent/"scanner")
                        self.assertFalse(root.exists())
                        self.assertEqual(json.loads(output.getvalue()),{"check":"native_notice_source_reservation_removed",
                            "passed":True,"distribution_ready":False})
                    tools.assert_not_called();remove.assert_not_called()

    def test_windows_workspace_repository_uses_fixed_private_https_source_without_checkout_config(self):
        from scripts import native_notice_source as acquisition
        root=Path("/private/owned/source");repository=root/"workspace.git";git=Path("/protected/Git/cmd/git.exe")
        revision="a"*40;environment={"MORPHEUS_PACKAGE_SOURCE_REVISION":revision,"GITHUB_SHA":"b"*40,"HOME":str(root/"home"),
                                   "GIT_CONFIG_COUNT":"1","GIT_CONFIG_KEY_0":"url.foreign.insteadOf",
                                   "GIT_CONFIG_VALUE_0":"https://github.com/","SSH_AUTH_SOCK":"foreign"}
        commands=SimpleNamespace(run=mock.Mock(return_value=b""))
        with mock.patch.object(acquisition,"os",SimpleNamespace(name="nt")), \
                mock.patch.object(acquisition,"system_git",return_value=git), \
                mock.patch.object(acquisition,"checked",side_effect=lambda path,**kwargs:Path(path)) as checked, \
                mock.patch.object(acquisition,"private_directory",return_value=repository) as private, \
                mock.patch.object(acquisition.S,"safe_git",return_value=[str(git),"-C",str(repository)]) as safe, \
                mock.patch.object(acquisition,"check_cache") as inventory:
            self.assertEqual(acquisition.workspace_repository(commands,root,revision,git,environment),repository)
        private.assert_called_once_with(repository);safe.assert_called_once_with(str(git),repository)
        self.assertEqual(checked.call_args_list,[mock.call(root,directory=True,private=True),
            mock.call(root/"fetch",directory=True,private=True),mock.call(repository,directory=True,private=True),
            mock.call(repository,directory=True,private=True)])
        self.assertEqual(len(commands.run.call_args_list),2)
        init,fetch=commands.run.call_args_list
        self.assertEqual(init.args[0],"workspace-init");self.assertEqual(fetch.args[0],"workspace-fetch")
        self.assertEqual(init.args[1][-4:],["init","--bare","--quiet","--template="])
        self.assertEqual(fetch.args[1][-9:],["fetch","--quiet","--depth=1","--no-tags","--no-write-fetch-head",
            "--no-recurse-submodules","--no-auto-maintenance","https://github.com/brk275/Morpheus.git",revision])
        for call in (init,fetch):
            argv=call.args[1]
            for value in ("protocol.allow=never","protocol.https.allow=always","http.sslVerify=true",
                          "http.followRedirects=false","http.extraHeader=","credential.helper=",
                          "credential.interactive=never","core.askPass="):
                self.assertIn(value,argv)
            self.assertEqual(call.kwargs["cwd"],root/"fetch")
            self.assertEqual(call.kwargs["output_limit"],65536)
            for name in ("GIT_CONFIG_COUNT","GIT_CONFIG_KEY_0","GIT_CONFIG_VALUE_0","SSH_AUTH_SOCK"):
                self.assertNotIn(name,call.kwargs["env"])
        self.assertEqual(init.kwargs["env"]["GIT_ALLOW_PROTOCOL"],"")
        self.assertEqual(fetch.kwargs["env"]["GIT_ALLOW_PROTOCOL"],"https")
        self.assertEqual(fetch.kwargs["timeout"],1200);self.assertEqual(fetch.kwargs["stderr_limit"],65536)
        inventory.assert_called_once_with(root)

    def test_windows_workspace_repository_refuses_wrong_revision_existing_root_and_fetch_failure(self):
        from scripts import native_notice_source as acquisition
        root=Path("/private/owned/source");git=Path("/protected/Git/cmd/git.exe");revision="a"*40
        commands=SimpleNamespace(run=mock.Mock(return_value=b""))
        with mock.patch.object(acquisition,"os",SimpleNamespace(name="nt")), \
                mock.patch.object(acquisition,"system_git",return_value=git), \
                mock.patch.object(acquisition,"checked",side_effect=lambda path,**kwargs:Path(path)), \
                mock.patch.object(acquisition,"private_directory",return_value=root/"workspace.git") as private, \
                mock.patch.object(acquisition.S,"safe_git",return_value=[str(git)]), \
                mock.patch.object(acquisition,"check_cache") as inventory:
            for wrong in ("0"*40,"A"*40,"a"*39,"../revision","b"*40):
                with self.subTest(revision=wrong),self.assertRaises(ValueError):
                    acquisition.workspace_repository(commands,root,wrong,git,{"MORPHEUS_PACKAGE_SOURCE_REVISION":revision,"GITHUB_SHA":"b"*40})
            private.assert_not_called();commands.run.assert_not_called()
            private.side_effect=FileExistsError("owned existing fixture")
            with self.assertRaises(FileExistsError):
                acquisition.workspace_repository(commands,root,revision,git,{"MORPHEUS_PACKAGE_SOURCE_REVISION":revision,"GITHUB_SHA":"b"*40})
            commands.run.assert_not_called();private.side_effect=None
            commands.run.side_effect=[b"",ValueError("fixed fetch refusal")]
            with self.assertRaises(ValueError):
                acquisition.workspace_repository(commands,root,revision,git,{"MORPHEUS_PACKAGE_SOURCE_REVISION":revision,"GITHUB_SHA":"b"*40})
            inventory.assert_not_called()
        with mock.patch.object(acquisition,"os",SimpleNamespace(name="posix")), \
                mock.patch.object(acquisition,"private_directory") as private:
            self.assertEqual(acquisition.workspace_repository(commands,root,"invalid",git,{}),acquisition.ROOT)
            private.assert_not_called()

    def test_windows_prepare_reuses_authenticated_private_repository_for_export_and_bundle(self):
        from scripts import native_notice_source as acquisition
        class WindowsOS:
            name="nt"
            def __getattr__(self,name):return getattr(os,name)
        with tempfile.TemporaryDirectory() as temporary:
            parent=Path(temporary).resolve();root=parent/"reserved";root.mkdir();(root/"owner.json").write_bytes(b"reservation")
            revision="a"*40;repository=root/"workspace.git";tree="b"*40
            commands=mock.Mock(cleanup_failed=False,logs_complete=True)
            commands.run.side_effect=[b"cargo 1.99.0 fixture\n",b"rustc 1.99.0 fixture\n",b""]
            with mock.patch.object(acquisition,"os",WindowsOS()),mock.patch.object(acquisition,"guard"), \
                    mock.patch.object(acquisition,"owned_source_root",return_value=root), \
                    mock.patch.object(acquisition,"checked",side_effect=lambda path,**kwargs:Path(path)), \
                    mock.patch.object(acquisition.N,"read_regular",return_value=b"scanner"), \
                    mock.patch.object(acquisition,"SCANNERS",{(sys.platform,acquisition.platform.machine()):hashlib.sha256(b"scanner").hexdigest()}), \
                    mock.patch.object(acquisition,"tools",return_value=(parent/"cargo",parent/"rustc",parent/"git",parent/"rustup")), \
                    mock.patch.object(acquisition,"environment_for_acquisition",return_value={"MORPHEUS_PACKAGE_SOURCE_REVISION":revision,"GITHUB_SHA":"b"*40}), \
                    mock.patch.object(acquisition,"private_directory",side_effect=lambda path,**kwargs:Path(path).mkdir(**kwargs)), \
                    mock.patch.object(acquisition,"Commands",return_value=commands), \
                    mock.patch.object(acquisition,"workspace_repository",return_value=repository) as select, \
                    mock.patch.object(acquisition.S,"git_export",return_value=(b"",{"tree":tree},{})) as export, \
                    mock.patch.object(acquisition,"source_inputs",return_value=[]), \
                    mock.patch.object(acquisition,"fetch_export",return_value=root/"fetch-source"), \
                    mock.patch.object(acquisition,"check_cache") as inventory, \
                    mock.patch.object(acquisition,"input_plan",return_value={}), \
                    mock.patch.object(acquisition.S,"build",return_value={"source_tree":tree}) as build:
                acquisition.prepare({"MORPHEUS_PACKAGE_SOURCE_REVISION":revision,"GITHUB_SHA":"b"*40},parent/"scanner")
            self.assertEqual(select.call_args.args[:3],(commands,root,revision))
            self.assertEqual(export.call_args.args[1],repository)
            self.assertEqual(build.call_args.args[2],repository)
            self.assertNotEqual(repository,acquisition.ROOT)
            inventory.assert_called_once_with(root)
            commands.scan.assert_called_once_with(parent/"scanner")
            self.assertTrue(json.loads((root/"state.json").read_text())["prepared"])

    def test_owned_readonly_cleanup_clears_only_attribute_and_closes_handle_before_single_retry(self):
        from scripts import native_notice_source as acquisition
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve()/("morpheus-native-source-"+"a"*32);root.mkdir()
            target=root/"workspace.git/objects/pack/fixture.pack";target.parent.mkdir(parents=True)
            target.write_bytes(b"owned synthetic Git pack");target.chmod(stat.S_IREAD)
            root_info=root.lstat();born=target.lstat().st_ctime_ns;real_lstat=Path.lstat
            descriptors=[];closed=[];mutations=[];deleted=[]
            def metadata(info):
                return SimpleNamespace(st_dev=info.st_dev,st_ino=info.st_ino,st_mode=info.st_mode,
                    st_nlink=info.st_nlink,st_size=info.st_size,st_mtime_ns=info.st_mtime_ns,
                    st_birthtime_ns=born,st_file_attributes=(0x10 if stat.S_ISDIR(info.st_mode)
                        else 0x1 if not info.st_mode & stat.S_IWRITE else 0))
            def opened(path,*,private):
                self.assertTrue(private);fd=os.open(path,os.O_RDONLY);descriptors.append(fd);return fd
            def retry(path):
                self.assertEqual(closed,descriptors)
                with self.assertRaises(OSError):os.fstat(descriptors[0])
                deleted.append(Path(path));os.unlink(path)
            class WindowsOS:
                name="nt"
                unlink=staticmethod(retry);remove=staticmethod(retry)
                def __getattr__(self,name):return getattr(os,name)
                def fstat(self,fd):return metadata(os.fstat(fd))
                def chmod(self,path,mode):mutations.append((Path(path),mode));os.chmod(path,mode)
                def close(self,fd):closed.append(fd);os.close(fd)
            error=PermissionError(13,"owned readonly fixture");error.winerror=5
            with mock.patch.object(acquisition,"os",WindowsOS()), \
                    mock.patch.object(Path,"lstat",autospec=True,side_effect=lambda path:metadata(real_lstat(path))), \
                    mock.patch.object(acquisition,"checked",side_effect=lambda path,**kwargs:Path(path)) as checked, \
                    mock.patch.object(acquisition.S,"windows_security",return_value=SimpleNamespace(open_read=opened)):
                acquisition._remove_readonly_owned_leaf(root,(root_info.st_dev,root_info.st_ino),retry,target,error)
            self.assertEqual(mutations,[(target,stat.S_IWRITE)]);self.assertEqual(deleted,[target])
            self.assertTrue(all(call.kwargs["private"] is True for call in checked.call_args_list))
            self.assertFalse(target.exists());self.assertTrue(root.exists())

    def test_owned_readonly_cleanup_refuses_unknown_errors_foreign_links_and_changed_handle_identity(self):
        from scripts import native_notice_source as acquisition
        temporary=Path(tempfile.gettempdir()).resolve()
        root=temporary/("morpheus-native-source-"+"a"*32);target=root/"workspace.git/objects/fixture"
        regular=dict(st_dev=1,st_ino=3,st_mode=stat.S_IFREG|0o444,st_nlink=1,st_size=8,
                     st_mtime_ns=5,st_birthtime_ns=4,st_file_attributes=1)
        root_info=SimpleNamespace(st_dev=1,st_ino=2)
        retry=mock.Mock();chmod=mock.Mock();closed=mock.Mock()
        security=SimpleNamespace(open_read=mock.Mock(return_value=42))
        error=PermissionError(13,"owned readonly fixture");error.winerror=5
        windows=SimpleNamespace(name="nt",unlink=retry,remove=retry,chmod=chmod,close=closed,
                                fstat=mock.Mock(return_value=SimpleNamespace(**(regular|{"st_ino":99}))))
        with mock.patch.object(acquisition,"os",windows), \
                mock.patch.object(Path,"lstat",autospec=True,side_effect=lambda path:
                    root_info if path==root else SimpleNamespace(**regular)), \
                mock.patch.object(acquisition,"checked",side_effect=lambda path,**kwargs:Path(path)) as checked, \
                mock.patch.object(acquisition.S,"windows_security",return_value=security):
            for failure in (OSError(5,"unknown"),PermissionError(13,"no Win32 code")):
                with self.assertRaises(OSError):
                    acquisition._remove_readonly_owned_leaf(root,(1,2),retry,target,failure)
            for changed in ({"st_nlink":2},{"st_file_attributes":0x401},{"st_file_attributes":0},
                            {"st_mode":stat.S_IFDIR|0o444},{"st_size":acquisition.S.MAX_BUNDLE+1}):
                original=dict(regular);regular.update(changed)
                with self.assertRaises(PermissionError):
                    acquisition._remove_readonly_owned_leaf(root,(1,2),retry,target,error)
                regular.clear();regular.update(original)
            for path,identity in ((temporary/"foreign-cleanup-file",(1,2)),(target,(1,99))):
                with self.assertRaises(ValueError):
                    acquisition._remove_readonly_owned_leaf(root,identity,retry,path,error)
            security.open_read.assert_not_called();chmod.assert_not_called();retry.assert_not_called()
            # A named-file/handle mismatch refuses before any attribute write,
            # closes the exact retained descriptor, and never retries deletion.
            with self.assertRaises(ValueError):
                acquisition._remove_readonly_owned_leaf(root,(1,2),retry,target,error)
            closed.assert_called_once_with(42);chmod.assert_not_called();retry.assert_not_called()
            checked.side_effect=ValueError("private ACL refusal")
            with self.assertRaises(ValueError):
                acquisition._remove_readonly_owned_leaf(root,(1,2),retry,target,error)
            chmod.assert_not_called();retry.assert_not_called()

    def test_prepare_records_acquisition_start_before_admitting_commands(self):
        from scripts import native_notice_source as acquisition
        with tempfile.TemporaryDirectory() as temporary:
            parent=Path(temporary).resolve();root=parent/("morpheus-native-source-"+"a"*32);root.mkdir(mode=0o700)
            (root/"owner.json").write_bytes(b"reservation marker")
            def commands(*args):
                self.assertEqual(json.loads((root/"acquisition-started.json").read_text()),
                                 {"schema":1,"source_revision":"a"*40})
                self.assertTrue((root/"logs").is_dir())
                raise RuntimeError("stop at command admission without starting any child")
            with mock.patch.object(acquisition,"guard"),mock.patch.object(acquisition,"owned_source_root",return_value=root), \
                    mock.patch.object(acquisition,"checked",side_effect=lambda value,**kwargs:Path(value)), \
                    mock.patch.object(acquisition.N,"read_regular",return_value=b"scanner"), \
                    mock.patch.object(acquisition,"SCANNERS",{(sys.platform,acquisition.platform.machine()):hashlib.sha256(b"scanner").hexdigest()}), \
                    mock.patch.object(acquisition,"tools",return_value=(parent/"cargo",parent/"rustc",parent/"git",parent/"rustup")), \
                    mock.patch.object(acquisition,"environment_for_acquisition",return_value={}), \
                    mock.patch.object(acquisition,"private_directory",side_effect=lambda path,**kwargs:Path(path).mkdir(mode=0o700,**kwargs)), \
                    mock.patch.object(acquisition,"Commands",side_effect=commands):
                with self.assertRaises(RuntimeError):
                    acquisition.prepare({"MORPHEUS_PACKAGE_SOURCE_REVISION":"a"*40,"GITHUB_SHA":"b"*40},parent/"scanner")
            self.assertFalse((root/"state.json").exists())

    def test_windows_owned_home_longpaths_config_is_fixed_private_new_and_precedes_commands(self):
        from scripts import native_notice_source as acquisition
        class StopAdmission(Exception): pass
        class HostOS:
            def __init__(self,name):self.name=name
            def __getattr__(self,name):return getattr(os,name)
        cases=[("nt",False),("nt",True)]
        if os.name != "nt":cases.insert(1,("posix",False))  # fchmod is POSIX-only.
        for host,existing in cases:
            with self.subTest(host=host,existing=existing), tempfile.TemporaryDirectory() as temporary:
                parent=Path(temporary).resolve();root=parent/"reserved";root.mkdir(mode=0o700)
                (root/"owner.json").write_bytes(b"synthetic reservation")
                config=root/"home/.gitconfig";checks=[];admitted=[]
                def private(path,**options):
                    Path(path).mkdir(mode=0o700,**options)
                    if existing and Path(path)==root/"home":config.write_bytes(b"foreign config must remain untouched")
                def checked(path,**options):checks.append((Path(path),options));return Path(path)
                def commands(*args):
                    admitted.append(True)
                    self.assertEqual(config.exists(),host=="nt")
                    if host=="nt":
                        self.assertEqual(config.read_bytes(),b"[core]\n\tlongpaths = true\n")
                        self.assertIn((config,{"private":True}),checks)
                    raise StopAdmission()
                with mock.patch.object(acquisition,"os",HostOS(host)), \
                        mock.patch.object(acquisition,"guard"),mock.patch.object(acquisition,"owned_source_root",return_value=root), \
                        mock.patch.object(acquisition,"checked",side_effect=checked), \
                        mock.patch.object(acquisition.N,"read_regular",return_value=b"scanner"), \
                        mock.patch.object(acquisition,"SCANNERS",{(sys.platform,acquisition.platform.machine()):hashlib.sha256(b"scanner").hexdigest()}), \
                        mock.patch.object(acquisition,"tools",return_value=(parent/"cargo",parent/"rustc",parent/"git",parent/"rustup")), \
                        mock.patch.object(acquisition,"environment_for_acquisition",return_value={}), \
                        mock.patch.object(acquisition,"private_directory",side_effect=private), \
                        mock.patch.object(acquisition,"Commands",side_effect=commands):
                    with self.assertRaises(FileExistsError if existing else StopAdmission):
                        acquisition.prepare({"MORPHEUS_PACKAGE_SOURCE_REVISION":"a"*40,"GITHUB_SHA":"b"*40},parent/"scanner")
                self.assertEqual(admitted,[] if existing else [True])
                if existing:self.assertEqual(config.read_bytes(),b"foreign config must remain untouched")

    def test_windows_prepare_fetch_and_scan_use_private_directory_creation_for_every_child(self):
        from scripts import native_notice_source as acquisition
        class StopAdmission(Exception): pass
        class WindowsOS:
            name = "nt"
            def __getattr__(self, name): return getattr(os,name)
        with tempfile.TemporaryDirectory() as temporary:
            parent=Path(temporary).resolve();root=parent/"reserved";root.mkdir()
            (root/"owner.json").write_bytes(b"synthetic reservation")
            calls=[]
            def private(path,*,parents=False,exist_ok=False):
                path=Path(path);calls.append((path,parents,exist_ok))
                path.mkdir(parents=parents,exist_ok=exist_ok);return path
            security=SimpleNamespace(private_directory=private)
            with mock.patch.object(acquisition,"os",WindowsOS()), \
                    mock.patch.object(acquisition.S,"windows_security",return_value=security), \
                    mock.patch.object(acquisition,"guard"),mock.patch.object(acquisition,"owned_source_root",return_value=root), \
                    mock.patch.object(acquisition,"checked",side_effect=lambda path,**kwargs:Path(path)), \
                    mock.patch.object(acquisition.N,"read_regular",return_value=b"scanner"), \
                    mock.patch.object(acquisition,"SCANNERS",{(sys.platform,acquisition.platform.machine()):hashlib.sha256(b"scanner").hexdigest()}), \
                    mock.patch.object(acquisition,"tools",return_value=(parent/"cargo",parent/"rustc",parent/"git",parent/"rustup")), \
                    mock.patch.object(acquisition,"environment_for_acquisition",return_value={}), \
                    mock.patch.object(acquisition,"Commands",side_effect=StopAdmission):
                with self.assertRaises(StopAdmission):
                    acquisition.prepare({"GITHUB_SHA":"a"*40},parent/"scanner")
            expected=[root/name for name in ("home","cargo","fetch","target","temp","logs","output")]
            self.assertEqual(calls,[(path,False,False) for path in expected])
            calls.clear()
            with mock.patch.object(acquisition,"os",WindowsOS()), \
                    mock.patch.object(acquisition.S,"windows_security",return_value=security):
                commands=acquisition.Commands(root,{})
                with mock.patch.object(commands,"run",return_value=b"") as run:
                    commands.scan(parent/"scanner")
                self.assertEqual(run.call_args.kwargs["log_root"],root/"scanner-logs")
                data=b"synthetic source bytes"
                files={"crates/example/Cargo.toml":data}
                identity={"files":{name:{"mode":"100644","sha256":hashlib.sha256(data).hexdigest()} for name in files}}
                exported=acquisition.fetch_export(root,files,identity)
                self.assertEqual((exported/"crates/example/Cargo.toml").read_bytes(),data)
            self.assertEqual(calls,[(root/"scanner-logs",False,True),(root/"fetch-source",False,False),
                                    (root/"fetch-source/crates/example",True,True)])


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
    @unittest.skipUnless(os.name=="nt","actual Windows protected readonly Git-object cleanup")
    def test_owned_readonly_loose_and_pack_objects_remove_with_protected_root_and_original_acl_reader(self):
        from scripts import native_notice_source as acquisition, native_notice_windows as windows
        security=windows.security();root=security.create_root(security.path(Path.home().resolve(),directory=True))
        security.private_directory(root/"workspace.git/objects/aa",parents=True)
        security.private_directory(root/"workspace.git/objects/pack")
        targets=[root/"workspace.git/objects/aa/fixture",root/"workspace.git/objects/pack/fixture.pack"]
        for target in targets:
            target.write_bytes(b"owned synthetic Git object");os.chmod(target,stat.S_IREAD)
            security.path(target,private=True)
            self.assertTrue(target.lstat().st_file_attributes & 1)
        info=root.lstat();seen=[]
        def readonly(function,path,error):
            seen.append(Path(path))
            acquisition._remove_readonly_owned_leaf(root,(info.st_dev,info.st_ino),function,path,error)
        # On failure retain the protected synthetic fixture; never add a
        # permissive cleanup fallback that could hide a real guard failure.
        shutil.rmtree(root,onexc=readonly)
        self.assertCountEqual(seen,targets);self.assertFalse(root.exists())

    @unittest.skipUnless(os.name == "nt", "actual Windows owned file/ACL primitive")
    def test_private_inherited_root_and_opened_handle_reject_hardlinks_and_live_writers(self):
        from scripts import native_notice_windows as windows
        security = windows.security()
        # The shared hosted TEMP ancestor can permit foreign writers. Exercise
        # the real primitive beneath a parent accepted by its unchanged policy.
        parent = security.path(Path.home().resolve(), directory=True)
        # Create the sole fixture directory with the primitive's protected
        # DACL atomically; an inherited stdlib temporary parent is not private.
        root = security.create_root(parent)
        identity = root.lstat()
        target, linked = root/"source.txt", root/"second-name.txt"
        try:
            target.write_bytes(b"fixed source data")
            security.path(root,directory=True,private=True)
            # The same notice/event reader must accept the real retained
            # Windows handle without equating CreationTime and ChangeTime.
            self.assertEqual(native_notices.read_regular(target,1024),b"fixed source data")
            descriptor = security.open_read(target,private=True)
            with os.fdopen(descriptor,"rb") as opened:
                self.assertEqual(opened.read(),b"fixed source data")
                # The retained no-write/delete-sharing read rejects a writer,
                # rather than claiming fstat/hash alone prevents that race.
                with self.assertRaises(OSError):
                    target.open("wb")
            os.link(target,linked)
            with self.assertRaises(ValueError):security.open_read(target,private=True)
            with self.assertRaises(ValueError):native_notices.read_regular(target,1024)
            self.assertEqual(target.read_bytes(),b"fixed source data")
        finally:
            security.path(root,directory=True,private=True)
            current = root.lstat()
            self.assertEqual((current.st_dev,current.st_ino),(identity.st_dev,identity.st_ino))
            # Only these two synthetic leaf names are ours; never recurse or
            # remove the validated parent. Unexpected content makes rmdir fail.
            for name in ("second-name.txt", "source.txt"):
                (root/name).unlink(missing_ok=True)
            security.path(root,directory=True,private=True)
            current = root.lstat()
            self.assertEqual((current.st_dev,current.st_ino),(identity.st_dev,identity.st_ino))
            root.rmdir()

    def test_private_child_creation_uses_protected_user_system_acl_and_rejects_existing_writers(self):
        import ctypes
        from scripts import native_notice_windows as windows
        class WindowsOS:
            name = "nt"
            def __getattr__(self, name): return getattr(os, name)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            protected, created, descriptors, validations = {root}, [], [], []
            security = windows.Security.__new__(windows.Security)
            security.sid = "S-1-5-21-synthetic-user"
            def validate(path, *, directory=False, private=False):
                path = Path(path); validations.append((path,directory,private))
                if not directory or not private or path not in protected or not path.is_dir():
                    raise ValueError("synthetic private ACL refused")
                return path
            def descriptor(sddl, version, output, size):
                descriptors.append(sddl); output._obj.value = 42; return True
            def create(path, attributes):
                path = Path(path); path.mkdir(); protected.add(path); created.append(path); return True
            security.path = validate
            security.a = SimpleNamespace(ConvertStringSecurityDescriptorToSecurityDescriptorW=descriptor)
            security.k = SimpleNamespace(CreateDirectoryW=create, LocalFree=mock.Mock(return_value=None))
            with mock.patch.object(windows,"os",WindowsOS()):
                leaf = root/"nested"/"source"/"logs"
                self.assertEqual(security.private_directory(leaf,parents=True),leaf)
                self.assertEqual(created,[root/"nested",root/"nested/source",leaf])
                self.assertEqual(descriptors,["O:"+security.sid+"D:P(A;OICI;FA;;;"+security.sid+")(A;OICI;FA;;;SY)"]*3)
                self.assertEqual(security.k.LocalFree.call_count,3)
                self.assertEqual(security.private_directory(leaf,parents=True,exist_ok=True),leaf)
                bad = root/"admin-writable"; bad.mkdir()
                for path,options in ((bad,{"exist_ok":True}),(bad/"child",{}),
                                     (root/"nested/../escape",{"parents":True}),(Path("relative"),{})):
                    with self.subTest(path=str(path)),self.assertRaises(ValueError):
                        security.private_directory(path,**options)
                self.assertEqual(len(created),3)
            self.assertTrue(all(directory and private for _,directory,private in validations))

    @unittest.skipUnless(os.name == "nt", "actual Windows private child ACL and inherited file reads")
    def test_private_nested_children_keep_user_system_reads_and_refuse_stdlib_admin_acl(self):
        from scripts import native_notice_windows as windows
        security = windows.security()
        root = security.create_root(security.path(Path.home().resolve(),directory=True))
        identity = root.stat().st_dev,root.stat().st_ino
        leaf, bad = root/"nested"/"logs", root/"stdlib700"
        bad_identity = None
        try:
            security.private_directory(leaf,parents=True)
            target = leaf/"version.out";target.write_bytes(b"synthetic tool version\n")
            security.path(leaf,directory=True,private=True)
            with os.fdopen(security.open_read(target,private=True),"rb") as stream:
                self.assertEqual(stream.read(),b"synthetic tool version\n")
            self.assertEqual(security.private_directory(leaf,parents=True,exist_ok=True),leaf)
            if sys.version_info >= (3,12,4):
                bad.mkdir(mode=0o700)
                created = bad.lstat()
                self.assertTrue(windows.stat.S_ISDIR(created.st_mode) and not windows.stat.S_ISLNK(created.st_mode)
                                and not getattr(created,"st_file_attributes",0) & 0x400)
                self.assertEqual(bad.resolve(strict=True),bad)
                bad_identity = created.st_dev,created.st_ino,created.st_mode,getattr(created,"st_file_attributes",0)
                with self.assertRaises(ValueError):security.path(bad,directory=True,private=True)
                with self.assertRaises(ValueError):security.private_directory(bad,exist_ok=True)
        finally:
            security.path(root,directory=True,private=True)
            self.assertEqual((root.stat().st_dev,root.stat().st_ino),identity)
            (leaf/"version.out").unlink(missing_ok=True)
            for path in (leaf,leaf.parent):
                if path.exists():security.path(path,directory=True,private=True);path.rmdir()
            if bad_identity is not None:
                # This intentionally invalid ACL cannot enter the normal path
                # validator (even its non-private policy rejects OWNER RIGHTS).
                # Remove only our same empty negative fixture beneath the exact
                # still-private reserved root; never repair or admit its ACL.
                security.path(root,directory=True,private=True)
                self.assertEqual((root.stat().st_dev,root.stat().st_ino),identity)
                current = bad.lstat()
                self.assertEqual(bad.parent,root)
                self.assertEqual(bad.resolve(strict=True),bad)
                self.assertTrue(windows.stat.S_ISDIR(current.st_mode) and not windows.stat.S_ISLNK(current.st_mode)
                                and not getattr(current,"st_file_attributes",0) & 0x400)
                self.assertEqual((current.st_dev,current.st_ino,current.st_mode,getattr(current,"st_file_attributes",0)),bad_identity)
                self.assertEqual(list(bad.iterdir()),[])
                bad.rmdir()
            else:
                self.assertFalse(bad.exists(),"unknown negative fixture must remain untouched")
            security.path(root,directory=True,private=True);root.rmdir()

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
        security.test_owner="current-user";security.test_writer="S-1-5-32-544";security.a.mask=0x2
        with self.assertRaises(ValueError):security.acl("fixture",private=True)


if __name__ == "__main__":
    unittest.main()
