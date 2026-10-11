import struct
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from scripts import package_smoke


class PackageSmokeTests(unittest.TestCase):
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
