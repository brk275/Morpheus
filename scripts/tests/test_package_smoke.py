import struct
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import package_smoke


class PackageSmokeTests(unittest.TestCase):
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
                        valid.replace("Icon=morpheus-app", "Icon=missing-icon")]:
            with self.assertRaises(RuntimeError):
                package_smoke.desktop_metadata(invalid)

    def test_installed_resources_must_match_source(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(package_smoke, "record"):
            root = Path(temp) / "source"
            (root / "themes").mkdir(parents=True)
            (root / "LICENSE").write_bytes(b"Original license\n")
            (root / "themes/aurora.toml").write_bytes(b"name = 'Aurora'\n")
            installed = Path(temp) / "installed"
            installed.mkdir()
            (installed / "LICENSE").write_bytes(b"Original license\r\n")
            (installed / "aurora.toml").write_bytes(b"name = 'Aurora'\r\n")
            with patch.object(package_smoke, "ROOT", root):
                package_smoke.check_resources(installed)
                (installed / "LICENSE").write_bytes(b"Wrong license\n")
                with self.assertRaisesRegex(RuntimeError, "LICENSE"):
                    package_smoke.check_resources(installed)

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
