import struct
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch, MagicMock, call

from scripts import package_smoke


class PackageSmokeTests(unittest.TestCase):
    def test_windows_startup_reduces_only_exact_reviewed_error_lines(self):
        for line, category in package_smoke.WINDOWS_STARTUP_ERROR_LINES.items():
            with self.subTest(category=category), tempfile.TemporaryFile() as log:
                log.write(b"private account body and path must not be emitted\r\n" + line + b"\r\n")
                with patch.object(package_smoke.sys, "platform", "win32"):
                    facts = package_smoke.startup_failure_diagnostics(log, 1)
                self.assertEqual(facts, {"category": category, "exit_status": 1, "missing_library": None})
                self.assertNotIn("private", repr(facts))
                self.assertNotIn("Error:", repr(facts))

    def test_windows_startup_unknown_ambiguous_or_unbounded_logs_never_claim_a_cause(self):
        known = b"Error: NotWritable"
        for data, status, category in (
            (b"Error: private account and renderer error", 1, "unclassified"),
            (known + b": private path", 1, "unclassified"),
            (known + b"\r\r", 1, "unclassified"),
            (b"prefix " + known, 1, "unclassified"),
            (known + b"\n" + known, 1, "unclassified"),
            (known + b"\nError: RecoveryRequired\n", 1, "unclassified"),
            (known + b"\nError: arbitrary renderer failure\n", 1, "unclassified"),
            (known, 0, "unclassified"),
            (known, 127, "unclassified"),
            (b"x" * (package_smoke.STARTUP_LOG_BOUND + 1), 1, "private_log_exceeded_bound"),
        ):
            with self.subTest(status=status, category=category), tempfile.TemporaryFile() as log:
                log.write(data)
                with patch.object(package_smoke.sys, "platform", "win32"):
                    facts = package_smoke.startup_failure_diagnostics(log, status)
                self.assertEqual(facts["category"], category)
                self.assertNotIn("private account", repr(facts))
                self.assertIsNone(facts["missing_library"])
        unavailable = Mock()
        unavailable.seek.side_effect = OSError("private log handle")
        with patch.object(package_smoke.sys, "platform", "win32"):
            self.assertEqual(package_smoke.startup_failure_diagnostics(unavailable, 1)["category"], "private_log_unavailable")

    def test_windows_startup_failure_stays_failed_emits_fixed_facts_and_stops_owned_process(self):
        process = Mock()
        facts = {"category": "windows_update_not_writable", "exit_status": 1, "missing_library": None}
        with patch.object(package_smoke.sys, "platform", "win32"), \
                patch.object(package_smoke, "require_nonreaping_wait"), \
                patch.object(package_smoke.subprocess, "Popen", return_value=process), \
                patch.object(package_smoke, "process_status", return_value=1), \
                patch.object(package_smoke, "stop_process") as stop, \
                patch.object(package_smoke, "startup_failure_diagnostics", return_value=facts) as reduce, \
                patch.object(package_smoke, "record") as record, patch("builtins.print") as output:
            with self.assertRaises(RuntimeError):
                package_smoke.launch_smoke(Path("synthetic-app.exe"), "win32")
        stop.assert_called_once_with(process)
        reduce.assert_called_once()
        record.assert_not_called()
        emitted = json.loads(output.call_args.args[0])
        self.assertEqual(emitted, {"check": "installed_startup", "status": "failed",
                                  "app_log_public": False, "diagnostic": facts})

    def test_partial_linux_qa_cannot_claim_untested_formats(self):
        self.assertEqual(package_smoke.package_formats("linux-x64"), ["deb", "appimage"])
        self.assertEqual(package_smoke.package_formats("linux-x64", "appimage"), ["appimage"])
        for target, value in [("linux-x64", ""), ("linux-x64", "appimage,appimage"),
                              ("windows-x64", "deb"), ("macos-arm64", "appimage")]:
            with self.subTest(target=target, value=value), self.assertRaises(RuntimeError):
                package_smoke.package_formats(target, value)
        with patch.object(package_smoke, "run"), patch.object(package_smoke, "record"), \
                patch.object(package_smoke, "output", return_value="sha256:" + "a" * 64), \
                patch.object(package_smoke, "run_owned_container") as launch:
            package_smoke.linux_smoke(Path("/qa/dist"), ["appimage"])
        self.assertEqual(launch.call_count, 1)
        self.assertIn("--linux-container appimage", launch.call_args.args[0][-1])

    def test_deb_requires_a_maintainer_contact_before_acceptance(self):
        package_smoke.maintainer_metadata("Morpheus <support@example.org>")
        for value in ["", "Morpheus", "support@example.org", "Morpheus <not-an-address>",
                      "Morpheus <support@example.org>\nInjected: value"]:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                package_smoke.maintainer_metadata(value)

    def test_pe_requires_x64_gui_subsystem(self):
        data = bytearray(512)
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 60, 128)
        data[128:132] = b"PE\0\0"
        struct.pack_into("<H", data, 132, 0x8664)
        struct.pack_into("<H", data, 152, 0x20B)
        struct.pack_into("<H", data, 220, 2)
        with tempfile.TemporaryDirectory() as temp, patch.object(package_smoke, "record"):
            executable = Path(temp) / "app.exe"
            executable.write_bytes(data)
            package_smoke.check_windows_binary(executable)
            struct.pack_into("<H", data, 220, 3)  # Regression: console subsystem.
            executable.write_bytes(data)
            with self.assertRaisesRegex(RuntimeError, "Windows GUI"):
                package_smoke.check_windows_binary(executable)

    def test_pe_rejects_invalid_header_offsets(self):
        data = bytearray(64)
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 60, 0xFFFFFFFF)
        with self.assertRaisesRegex(RuntimeError, "Invalid PE"):
            package_smoke.parse_pe_header(data)

    def test_symbol_versions_compare_numerically(self):
        text = "Name: GLIBC_2.9 Name: GLIBC_2.39 Name: GLIBCXX_3.4.9 Name: GLIBCXX_3.4.30"
        self.assertEqual(package_smoke.required_symbols(text, "GLIBC"), (2, 39))
        self.assertEqual(package_smoke.required_symbols(text, "GLIBCXX"), (3, 4, 30))
        with self.assertRaises(RuntimeError):
            package_smoke.required_symbols("GLIBC_PRIVATE", "GLIBC")

    def test_elf_rejects_a_newer_abi_than_declared(self):
        data = bytearray(20)
        data[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", data, 18, 62)
        with tempfile.TemporaryDirectory() as temp, patch.object(package_smoke, "record"):
            executable = Path(temp) / "app"
            executable.write_bytes(data)
            for versions in ["GLIBC_2.40 GLIBCXX_3.4.30", "GLIBC_2.39 GLIBCXX_3.4.31"]:
                with patch.object(package_smoke, "output", return_value=versions):
                    with self.assertRaisesRegex(RuntimeError, "baseline"):
                        package_smoke.check_elf(executable)

    def test_desktop_metadata_checks_launch_and_icon(self):
        valid = "[Desktop Entry]\nName=Morpheus\nType=Application\nTerminal=false\nExec=morpheus-app %U\nIcon=morpheus-app\n"
        package_smoke.desktop_metadata(valid)
        for invalid in [valid.replace("Terminal=false", "Terminal=true"),
                        valid.replace("Exec=morpheus-app", "Exec=other-app"),
                        valid.replace("Exec=morpheus-app", "Exec=/missing/morpheus-app"),
                        valid.replace("Exec=morpheus-app %U", "Exec=morpheus-app --unexpected-mode"),
                        valid.replace("Icon=morpheus-app", "Icon=missing-icon")]:
            with self.assertRaises(RuntimeError):
                package_smoke.desktop_metadata(invalid)

    def test_container_timeout_removes_only_the_created_container(self):
        identity = "a" * 64
        def invoke(command, **kwargs):
            if command[:2] == ["docker", "run"]:
                Path(command[command.index("--cidfile") + 1]).write_text(identity)
                raise subprocess.TimeoutExpired(command, 900)
            self.assertEqual(command, ["docker", "rm", "--force", identity])
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
        with patch.object(package_smoke.subprocess, "run", side_effect=invoke) as run:
            with self.assertRaises(subprocess.TimeoutExpired):
                package_smoke.run_owned_container(["docker", "run", "--rm", "ubuntu:24.04"])
        self.assertEqual(run.call_count, 2)

    def test_failed_container_creation_does_not_remove_unrelated_containers(self):
        with patch.object(package_smoke.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "docker")) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                package_smoke.run_owned_container(["docker", "run", "--rm", "ubuntu:24.04"])
        self.assertEqual(run.call_count, 1)

    def test_installed_resources_must_match_source(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(package_smoke, "record"):
            root = Path(temp) / "source"
            (root / "themes").mkdir(parents=True)
            (root / "LICENSE").write_bytes(b"Original license\n")
            (root / "themes/aurora.toml").write_bytes(b"name = 'Aurora'\n")
            (root / "licenses").mkdir()
            for name in ("Symphonia-MPL-2.0.txt", "Symphonia-NOTICE.txt"):
                (root / "licenses" / name).write_bytes(b"Required source license\n")
            installed = Path(temp) / "installed"
            installed.mkdir()
            (installed / "LICENSE").write_bytes(b"Original license\r\n")
            (installed / "aurora.toml").write_bytes(b"name = 'Aurora'\r\n")
            for name in ("Symphonia-MPL-2.0.txt", "Symphonia-NOTICE.txt"):
                (installed / name).write_bytes(b"Required source license\r\n")
            with patch.object(package_smoke, "ROOT", root):
                package_smoke.check_resources(installed)
                (installed / "LICENSE").write_bytes(b"Wrong license\n")
                with self.assertRaisesRegex(RuntimeError, "LICENSE"):
                    package_smoke.check_resources(installed)
                (installed / "LICENSE").write_bytes(b"Original license\n")
                (installed / "Symphonia-MPL-2.0.txt").unlink()
                with self.assertRaisesRegex(RuntimeError, "Symphonia-MPL-2.0"):
                    package_smoke.check_resources(installed)
                (installed / "Symphonia-MPL-2.0.txt").write_bytes(b"Required source license\n")
                (installed / "Symphonia-NOTICE.txt").write_bytes(b"Wrong source notice\n")
                with self.assertRaisesRegex(RuntimeError, "Symphonia-NOTICE"):
                    package_smoke.check_resources(installed)

    def test_macos_install_uses_canonical_temp_root_without_resolving_bundle_resources(self):
        alias = Path("/synthetic/alias-temp/owned")
        canonical = Path("/synthetic/canonical-temp/owned")
        mount = canonical / "mounted"
        source = mount / "Morpheus.app"
        installed = canonical / "Applications/Morpheus.app"
        contents = installed / "Contents"
        executable = contents / "MacOS/morpheus-app"
        package = Path("/synthetic/dist/Morpheus.dmg")
        dist = package.parent
        info = {"CFBundleIdentifier": "io.github.brk275.morpheus", "CFBundleExecutable": "morpheus-app",
                "CFBundleShortVersionString": "0.1.0", "NSMicrophoneUsageDescription": "fixture",
                "NSCameraUsageDescription": "fixture", "NSScreenCaptureUsageDescription": "fixture"}
        temporary = MagicMock()
        temporary.__enter__.return_value = str(alias)
        def resolve(path, **kwargs):
            self.assertEqual(path, alias)
            self.assertEqual(kwargs, {"strict": True})
            return canonical
        with patch.object(package_smoke.tempfile, "TemporaryDirectory", return_value=temporary) as allocate, \
                patch.object(Path, "resolve", autospec=True, side_effect=resolve) as resolved, \
                patch.object(Path, "mkdir", autospec=True), \
                patch.object(Path, "open", autospec=True), \
                patch.object(Path, "exists", return_value=False), \
                patch.object(package_smoke, "artifact", return_value=package), \
                patch.object(package_smoke.plistlib, "load", return_value=info), \
                patch.object(package_smoke, "package_version", return_value="0.1.0"), \
                patch.object(package_smoke, "output", return_value="arm64"), \
                patch.object(package_smoke, "check_resources") as resources, \
                patch.object(package_smoke, "macos_installed_launches") as launch, \
                patch.object(package_smoke.shutil, "rmtree") as remove, \
                patch.object(package_smoke, "run") as run, \
                patch.object(package_smoke, "record"):
            package_smoke.macos_smoke(dist, "arm64")
        allocate.assert_called_once_with(prefix="morpheus-package-")
        resolved.assert_called_once_with(alias, strict=True)
        resources.assert_called_once_with(contents / "Resources", dist)
        launch.assert_called_once_with(executable, canonical)
        remove.assert_called_once_with(installed)
        self.assertEqual(run.call_args_list, [
            call(["hdiutil", "verify", str(package)], stdout=subprocess.DEVNULL),
            call(["hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint", str(mount), str(package)],
                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL),
            call(["ditto", str(source), str(installed)]),
            call(["hdiutil", "detach", str(mount)], stdout=subprocess.DEVNULL),
        ])
        temporary.__exit__.assert_called_once()

    def test_windows_install_uses_one_canonical_owned_temp_root_for_all_operations(self):
        alias = Path("/synthetic/SHORT~1/owned")
        canonical = Path("/synthetic/Long User/owned")
        installed = canonical / "Installed Morpheus"
        executable = installed / "morpheus-app.exe"
        uninstaller = installed / "uninstall.exe"
        copied = canonical / "qa-uninstall.exe"
        setup = Path("/synthetic/dist/setup.exe")
        dist = Path("/synthetic/dist")
        temporary = MagicMock()
        temporary.__enter__.return_value = str(alias)
        def resolve(path, **kwargs):
            try:
                suffix = path.relative_to(alias)
            except ValueError:
                return path
            return canonical / suffix
        entries = [{"DisplayName":"Morpheus", "DisplayVersion":"0.1.0",
                    "InstallLocation":str(installed)}]
        with patch.object(package_smoke, "windows_smoke_workspace", return_value=temporary) as allocate, \
                patch.object(Path, "resolve", autospec=True, side_effect=resolve) as resolved, \
                patch.object(Path, "exists", autospec=True, side_effect=lambda path:path == uninstaller), \
                patch.object(package_smoke, "artifact", return_value=setup), \
                patch.object(package_smoke, "uninstall_registry", side_effect=[[], entries, []]), \
                patch.object(package_smoke, "package_version", return_value="0.1.0"), \
                patch.object(package_smoke, "check_windows_binary") as pe, \
                patch.object(package_smoke, "check_resources") as resources, \
                patch.object(package_smoke, "windows_installed_launches") as launch, \
                patch.object(package_smoke, "run") as run, \
                patch.object(package_smoke.shutil, "copy2") as copy, \
                patch.object(package_smoke, "record"):
            package_smoke.windows_smoke(dist)
        allocate.assert_called_once_with()
        self.assertEqual(resolved.call_args_list[0], call(alias, strict=True))
        pe.assert_called_once_with(executable)
        resources.assert_called_once_with(installed, dist)
        launch.assert_called_once_with(executable)
        copy.assert_called_once_with(uninstaller, copied)
        self.assertEqual(run.call_args_list, [call(package_smoke.nsis_command(setup, installed)),
                                              call(package_smoke.nsis_command(copied, installed, uninstall=True))])
        temporary.__exit__.assert_called_once()

    def test_windows_smoke_root_requires_owned_prepared_source_without_step_context(self):
        from scripts import native_notice_source as source
        anchor = Path("/synthetic/private/source")
        environment = {
            "GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
            "GITHUB_REPOSITORY": "brk275/Morpheus", "GITHUB_REF": "refs/heads/main",
            "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_ACTOR": "brk275",
            "GITHUB_TRIGGERING_ACTOR": "brk275", "RUNNER_OS": "Windows",
        }
        state = {"schema": 1, "source_revision": "a" * 40, "prepared": True,
                 "logs_scanned": True, "logs_complete": True,
                 "owned_processes_stopped": True, "distribution_ready": False}
        with patch.object(package_smoke, "os", SimpleNamespace(name="nt", environ=environment)), \
                patch.object(package_smoke.sys, "platform", "win32"), \
                patch.object(package_smoke, "ROOT", anchor / "fetch-source"), \
                patch.object(source, "owned_source_root", return_value=anchor) as owned, \
                patch.object(source, "checked") as checked, \
                patch.object(source.S, "read_owned", return_value=b"state") as read, \
                patch.object(source.S, "unique_json", return_value=state) as decode, \
                patch.object(source.R, "expected_revision", return_value="a" * 40):
            self.assertEqual(package_smoke.windows_smoke_root(), anchor)
            owned.assert_called_once_with(environment)
            checked.assert_called_once_with(anchor / "fetch-source", directory=True, private=True)
            read.assert_called_once_with(anchor / "state.json", 4096, private=True)
            self.assertNotIn("MORPHEUS_NATIVE_PACKAGE_CONTEXT", package_smoke.os.environ)
            for key, value in [("prepared", False), ("logs_scanned", False),
                               ("logs_complete", False), ("owned_processes_stopped", False),
                               ("distribution_ready", True), ("schema", True),
                               ("source_revision", "b" * 40), ("logs_complete", 1),
                               ("extra", True)]:
                decode.return_value = {**state, key: value}
                with self.subTest(state_key=key, value=value), self.assertRaises(RuntimeError):
                    package_smoke.windows_smoke_root()
            decode.return_value = state
            with patch.object(package_smoke, "ROOT", anchor / "unrelated"), self.assertRaises(RuntimeError):
                package_smoke.windows_smoke_root()
            checked.side_effect = ValueError("Rejected source ACL")
            with self.assertRaises(ValueError):
                package_smoke.windows_smoke_root()
            checked.side_effect = None
            for invalid in (None, {}, [], {key: value for key, value in state.items() if key != "prepared"}):
                decode.return_value = invalid
                with self.subTest(invalid_state=invalid), self.assertRaises(RuntimeError):
                    package_smoke.windows_smoke_root()
            decode.return_value = state
            owned.side_effect = ValueError("Rejected reservation")
            with self.assertRaises(ValueError):
                package_smoke.windows_smoke_root()
            owned.reset_mock()
            for key in environment:
                with patch.dict(package_smoke.os.environ, {key: "untrusted"}), self.assertRaises(RuntimeError):
                    package_smoke.windows_smoke_root()
            owned.assert_not_called()

    def test_windows_smoke_rejected_reservation_cannot_allocate_or_install(self):
        with patch.object(package_smoke, "artifact"), \
                patch.object(package_smoke, "uninstall_registry", return_value=[]), \
                patch.object(package_smoke, "windows_smoke_root", side_effect=ValueError("Rejected reservation")), \
                patch.object(package_smoke.tempfile, "TemporaryDirectory") as allocate, \
                patch.object(package_smoke, "run") as run:
            with self.assertRaises(ValueError):
                package_smoke.windows_smoke(Path("/synthetic/dist"))
        allocate.assert_not_called()
        run.assert_not_called()

    def test_windows_smoke_workspace_uses_exclusive_private_creator_and_exact_cleanup(self):
        from scripts import native_notice_source as source
        anchor = Path("/synthetic/private/source")
        work = anchor / ("morpheus-package-" + "a" * 32)
        identities = {anchor: SimpleNamespace(st_dev=1, st_ino=2),
                      work: SimpleNamespace(st_dev=1, st_ino=3)}
        with patch.object(package_smoke, "windows_smoke_root", return_value=anchor) as admit, \
                patch.object(package_smoke.secrets, "token_hex", return_value="a" * 32) as nonce, \
                patch.object(source, "private_directory", return_value=work) as create, \
                patch.object(source, "checked") as checked, \
                patch.object(Path, "lstat", autospec=True, side_effect=lambda path: identities[path]), \
                patch.object(package_smoke.shutil, "rmtree") as remove, \
                patch.object(package_smoke.tempfile, "TemporaryDirectory") as unsafe_temp:
            with package_smoke.windows_smoke_workspace() as actual:
                self.assertEqual(actual, work)
                create.assert_called_once_with(work)
                remove.assert_not_called()
            self.assertEqual(admit.call_count, 3)
            nonce.assert_called_once_with(16)
            self.assertEqual(checked.call_args_list, [call(work, directory=True, private=True)] * 2)
            remove.assert_called_once_with(work)
            unsafe_temp.assert_not_called()

    def test_windows_smoke_workspace_never_adopts_or_deletes_changed_authority(self):
        from scripts import native_notice_source as source
        anchor = Path("/synthetic/private/source")
        work = anchor / ("morpheus-package-" + "a" * 32)
        identities = {anchor: SimpleNamespace(st_dev=1, st_ino=2),
                      work: SimpleNamespace(st_dev=1, st_ino=3)}
        with patch.object(package_smoke, "windows_smoke_root", return_value=anchor), \
                patch.object(package_smoke.secrets, "token_hex", return_value="a" * 32), \
                patch.object(source, "private_directory", return_value=work) as create, \
                patch.object(source, "checked") as checked, \
                patch.object(Path, "lstat", autospec=True, side_effect=lambda path: identities[path]), \
                patch.object(package_smoke.shutil, "rmtree") as remove, \
                patch.object(package_smoke, "record") as record:
            create.side_effect = ValueError("Refused existing child")
            with self.assertRaises(ValueError):
                with package_smoke.windows_smoke_workspace():
                    self.fail("An existing or refused workspace must not be entered")
            remove.assert_not_called()
            create.side_effect = None
            for changed in (anchor, work):
                original = identities[changed]
                with self.subTest(changed=changed), self.assertRaises(RuntimeError):
                    with package_smoke.windows_smoke_workspace():
                        identities[changed] = SimpleNamespace(st_dev=1, st_ino=99)
                identities[changed] = original
                remove.assert_not_called()
            checked.side_effect = [None, ValueError("Refused workspace ACL")]
            with self.assertRaisesRegex(RuntimeError, "Primary installed failure"):
                with package_smoke.windows_smoke_workspace():
                    raise RuntimeError("Primary installed failure")
            remove.assert_not_called()
            record.assert_called_once_with("windows_smoke_workspace_retained", status="failed")
            record.side_effect = BrokenPipeError("Closed output")
            checked.side_effect = [None, ValueError("Refused workspace ACL")]
            with self.assertRaisesRegex(RuntimeError, "Primary installed failure"):
                with package_smoke.windows_smoke_workspace():
                    raise RuntimeError("Primary installed failure")
            remove.assert_not_called()

    def test_windows_smoke_workspace_rechecks_anchor_before_installer_entry(self):
        from scripts import native_notice_source as source
        anchor = Path("/synthetic/private/source")
        work = anchor / ("morpheus-package-" + "a" * 32)
        identities = {anchor: SimpleNamespace(st_dev=1, st_ino=2),
                      work: SimpleNamespace(st_dev=1, st_ino=3)}
        def replaced_anchor(path):
            self.assertEqual(path, work)
            identities[anchor] = SimpleNamespace(st_dev=1, st_ino=99)
            return work
        with patch.object(package_smoke, "windows_smoke_root", return_value=anchor), \
                patch.object(package_smoke.secrets, "token_hex", return_value="a" * 32), \
                patch.object(source, "private_directory", side_effect=replaced_anchor), \
                patch.object(source, "checked"), \
                patch.object(Path, "lstat", autospec=True, side_effect=lambda path: identities[path]), \
                patch.object(package_smoke.shutil, "rmtree") as remove, \
                patch.object(package_smoke, "record"):
            with self.assertRaisesRegex(RuntimeError, "reservation identity changed"):
                with package_smoke.windows_smoke_workspace():
                    self.fail("Installer body must not enter after anchor replacement")
            remove.assert_not_called()

    @unittest.skipUnless(os.name == "nt", "Actual Windows DACL boundary requires a fresh hosted Windows runner")
    def test_windows_private_workspace_real_dacl_read_and_owned_cleanup(self):
        from scripts import native_notice_source as source
        from scripts import native_notices as notices
        for key, value in {
            "GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Windows",
            "GITHUB_REPOSITORY": "brk275/Morpheus", "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_ACTOR": "brk275", "GITHUB_TRIGGERING_ACTOR": "brk275",
        }.items():
            self.assertEqual(os.environ.get(key), value)
        self.assertIn(os.environ.get("IMAGEOS"), {"win25", "win25-vs2026"})
        self.assertGreaterEqual(sys.version_info, (3, 12, 10))
        security = source.S.windows_security()
        # create_root uses exclusive CreateDirectoryW + exact user/SYSTEM DACL;
        # no existing directory or source/profile/vault is read or repaired.
        anchor = security.create_root(Path.home().resolve(strict=True))
        original = anchor.lstat()
        try:
            security.path(anchor, directory=True, private=True)
            with tempfile.TemporaryDirectory(prefix="morpheus-legacy-", dir=anchor) as temp:
                legacy = Path(temp).resolve(strict=True)
                self.assertEqual(legacy.parent, anchor)
                leaf = legacy / "notice.txt"
                leaf.write_bytes(b"Synthetic notice boundary")
                # CPython mkdir(0700) explicitly installs OWNER_RIGHTS (OW),
                # which the unchanged ordinary source/notice policy rejects.
                with self.assertRaises(ValueError):
                    notices.read_regular(leaf, 128)
            self.assertFalse(legacy.exists())
            # Only reservation admission is modeled: all directory creation,
            # Win32 ownership/DACL queries, opened reads and deletion are real.
            with patch.object(package_smoke, "windows_smoke_root", return_value=anchor):
                with package_smoke.windows_smoke_workspace() as work:
                    self.assertEqual(work.parent, anchor)
                    security.path(work, directory=True, private=True)
                    leaf = work / "notice.txt"
                    leaf.write_bytes(b"Synthetic notice boundary")
                    self.assertEqual(notices.read_regular(leaf, 128), b"Synthetic notice boundary")
            self.assertFalse(work.exists())
        finally:
            security.path(anchor, directory=True, private=True)
            current = anchor.lstat()
            self.assertEqual((current.st_dev, current.st_ino), (original.st_dev, original.st_ino))
            # Never recursively delete an unexpected child after a failed case.
            self.assertEqual(list(anchor.iterdir()), [])
            anchor.rmdir()
        self.assertFalse(anchor.exists())

    def test_nsis_last_option_keeps_destination_unquoted(self):
        for uninstall, flag in [(False, "/D="), (True, "_?=")]:
            command = package_smoke.nsis_command(r"C:\QA tools\setup.exe", r"C:\QA work\Morpheus", uninstall)
            self.assertEqual(command, '"C:\\QA tools\\setup.exe" /S ' + flag + r"C:\QA work\Morpheus")
        with self.assertRaises(RuntimeError):
            package_smoke.nsis_command("setup.exe", 'C:\\bad"path')

    def test_ambiguous_artifacts_cannot_produce_evidence(self):
        with tempfile.TemporaryDirectory() as temp:
            dist = Path(temp)
            for name in ["old.deb", "new.deb"]:
                (dist / name).write_bytes(b"test")
            with self.assertRaisesRegex(RuntimeError, "exactly one"):
                package_smoke.artifact(dist, ".deb")

    def test_installation_refuses_non_ci_machine_before_subprocess(self):
        with patch.dict(package_smoke.os.environ, {"GITHUB_ACTIONS": "false"}), \
                patch.object(package_smoke.sys, "argv", ["package_smoke.py", "--target", "macos-arm64"]), \
                patch.object(package_smoke.subprocess, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "ephemeral GitHub Actions"):
                package_smoke.main()
            run.assert_not_called()

    @unittest.skipUnless(os.name == "posix", "POSIX process-group lifecycle")
    def test_stop_rejects_reaped_leader_without_signalling_reused_group(self):
        process = Mock(pid=1234, returncode=0)
        with patch.object(package_smoke.os, "killpg") as kill:
            with self.assertRaisesRegex(RuntimeError, "reaped before group cleanup"):
                package_smoke.stop_process(process)
        kill.assert_not_called()

    def test_darwin_empty_zombie_group_is_distinct_from_signal_denial(self):
        process = Mock(pid=1234, returncode=None)
        for state in ["Z", "Z+", "S", "R"]:
            with self.subTest(state=state), patch.object(package_smoke.sys, "platform", "darwin"), \
                    patch.object(package_smoke.os, "killpg", side_effect=PermissionError, create=True), \
                    patch.object(package_smoke, "process_status", return_value=0), \
                    patch.object(package_smoke.subprocess, "run", return_value=subprocess.CompletedProcess(
                        [], 0, stdout=f"1234 {state}\n5678 S\n")):
                if state.startswith("Z"):
                    package_smoke.signal_process_group(process, signal.SIGTERM)
                else:
                    with self.assertRaises(PermissionError):
                        package_smoke.signal_process_group(process, signal.SIGTERM)

    @unittest.skipUnless(os.name == "posix", "POSIX process-group lifecycle")
    def test_exited_process_stays_unreaped_until_cleanup(self):
        package_smoke.require_nonreaping_wait()
        process = subprocess.Popen([sys.executable, "-c", "raise SystemExit(7)"], start_new_session=True)
        try:
            deadline = time.monotonic() + 3
            while package_smoke.process_status(process) is None:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.01)
            self.assertIsNone(process.returncode)
            self.assertEqual(package_smoke.process_status(process), 7)
        finally:
            package_smoke.stop_process(process)
        self.assertEqual(process.returncode, 7)

    @unittest.skipUnless(os.name == "posix", "POSIX process-group lifecycle")
    def test_stop_kills_term_resistant_child_before_reaping_leader(self):
        package_smoke.require_nonreaping_wait()
        child_program = (
            "import os, signal, sys, time\n"
            "from pathlib import Path\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "ready = Path(sys.argv[1]); temporary = ready.with_suffix('.tmp')\n"
            "temporary.write_text(str(os.getpid())); temporary.replace(ready)\n"
            "time.sleep(20)\n"
        )
        parent_program = (
            "import subprocess, sys, time\n"
            "subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]])\n"
            "time.sleep(30)\n"
        )
        with tempfile.TemporaryDirectory() as temp:
            ready = Path(temp) / "child-ready"
            process = subprocess.Popen([sys.executable, "-c", parent_program, child_program, str(ready)],
                                       start_new_session=True)
            try:
                deadline = time.monotonic() + 3
                while not ready.exists():
                    self.assertIsNone(package_smoke.process_status(process))
                    self.assertLess(time.monotonic(), deadline, "child did not initialize")
                    time.sleep(0.01)
                child_pid = int(ready.read_text())
                observations = []
                killpg = os.killpg

                def observe_kill(group, sig):
                    if sig == signal.SIGKILL:
                        status = package_smoke.process_status(process) if process.returncode is None else None
                        observations.append((group, process.returncode, status))
                    killpg(group, sig)

                with patch.object(package_smoke.os, "killpg", side_effect=observe_kill):
                    package_smoke.stop_process(process)
                self.assertEqual(observations, [(process.pid, None, -signal.SIGTERM)])
                self.assertEqual(process.returncode, -signal.SIGTERM)
                deadline = time.monotonic() + 3
                while True:
                    # An orphan can await init reaping on Linux; a zombie is
                    # stopped. Use ps because macOS has no /proc interface.
                    state = subprocess.run(["ps", "-p", str(child_pid), "-o", "stat="],
                                           capture_output=True, text=True, timeout=3).stdout.strip()
                    if not state or state.startswith("Z"):
                        break
                    self.assertLess(time.monotonic(), deadline, "owned descendant survived cleanup")
                    time.sleep(0.01)
            finally:
                if process.returncode is None:
                    package_smoke.stop_process(process)

    @unittest.skipUnless(os.name == "posix", "POSIX process-group lifecycle")
    def test_stop_reaps_a_real_running_process_group(self):
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        try:
            package_smoke.stop_process(process)
            self.assertIsNotNone(process.poll())
            with self.assertRaises(ProcessLookupError):
                os.killpg(process.pid, 0)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
