#!/usr/bin/env python3
"""Install, inspect, briefly launch and remove unsigned packages on CI runners.

This deliberately refuses developer machines. It never signs in, requests media
permissions, uploads installers, or treats a startup smoke as release acceptance.
"""

import argparse
import configparser
import ctypes
import hashlib
import json
import os
import platform
import plistlib
import re
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PRODUCT = "Morpheus"
BINARY = "morpheus-app"
DEB_PACKAGE = "morpheus"
IDENTIFIER = "io.github.brk275.morpheus"
GLIBC_MAX = (2, 39)
GLIBCXX_MAX = (3, 4, 30)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(argv, **kwargs):
    return subprocess.run(argv, check=True, timeout=180, **kwargs)


def output(argv, **kwargs):
    return run(argv, capture_output=True, text=True, **kwargs).stdout.strip()


def record(check, **details):
    print(json.dumps({"check": check, "status": "passed", **details}, sort_keys=True), flush=True)


def package_version():
    with (ROOT / "Packager.toml").open("rb") as source:
        return tomllib.load(source)["version"]


def artifact(dist, suffix):
    matches = sorted(dist.glob(f"*{suffix}"))
    require(len(matches) == 1, f"Expected exactly one {suffix} package; found {len(matches)}")
    path = matches[0]
    with path.open("rb") as package:
        digest = hashlib.file_digest(package, "sha256").hexdigest()
    record("package_identity", file=path.name, sha256=digest, bytes=path.stat().st_size)
    return path


def check_resources(root):
    for name, source in [("LICENSE", ROOT / "LICENSE"), ("aurora.toml", ROOT / "themes/aurora.toml")]:
        expected = source.read_bytes().replace(b"\r\n", b"\n")
        candidates = [p for p in root.rglob(name) if p.is_file()]
        require(any(p.read_bytes().replace(b"\r\n", b"\n") == expected for p in candidates),
                f"Installed {name} is absent or differs from the source")
    record("installed_license_and_theme")


def parse_pe_header(data):
    require(len(data) >= 64 and data[:2] == b"MZ", "Missing DOS executable header")
    offset = struct.unpack_from("<I", data, 60)[0]
    require(offset + 94 <= len(data) and data[offset:offset + 4] == b"PE\0\0", "Invalid PE header")
    machine = struct.unpack_from("<H", data, offset + 4)[0]
    magic = struct.unpack_from("<H", data, offset + 24)[0]
    subsystem = struct.unpack_from("<H", data, offset + 24 + 68)[0]
    return machine, magic, subsystem


def check_windows_binary(path):
    require(parse_pe_header(path.read_bytes()) == (0x8664, 0x20B, 2),
            "Installed app must be an x64 PE32+ Windows GUI executable")
    record("installed_pe_x64_gui_subsystem")


def required_symbols(text, namespace):
    matches = re.findall(r"\b" + re.escape(namespace) + r"_([0-9]+(?:\.[0-9]+)+)\b", text)
    require(bool(matches), f"No {namespace} symbol requirements found")
    return max(tuple(int(part) for part in value.split(".")) for value in matches)


def check_elf(path):
    with path.open("rb") as executable:
        header = executable.read(20)
    require(len(header) == 20 and header[:6] == b"\x7fELF\x02\x01"
            and struct.unpack_from("<H", header, 18)[0] == 62, "Expected an x64 ELF executable")
    versions = output(["readelf", "--version-info", str(path)])
    glibc, glibcxx = required_symbols(versions, "GLIBC"), required_symbols(versions, "GLIBCXX")
    require(glibc <= GLIBC_MAX, f"ELF exceeds the declared GLIBC baseline: {glibc}")
    require(glibcxx <= GLIBCXX_MAX, f"ELF exceeds the declared libstdc++ baseline: {glibcxx}")
    record("elf_symbol_baseline", glibc=".".join(map(str, glibc)), glibcxx=".".join(map(str, glibcxx)))


def desktop_metadata(text):
    desktop = configparser.ConfigParser(interpolation=None, strict=True)
    desktop.optionxform = str
    desktop.read_string(text)
    entry = desktop["Desktop Entry"]
    require(entry.get("Name") == PRODUCT and entry.get("Type") == "Application", "Invalid desktop identity")
    require(entry.get("Terminal") == "false", "Desktop launcher must not open a terminal")
    executable = shlex.split(entry.get("Exec", ""))
    require(bool(executable) and executable[0] in (BINARY, f"/usr/bin/{BINARY}")
            and executable[1:] in ([], ["%U"]), "Desktop entry points at the wrong executable or arguments")
    require(entry.get("Icon") == BINARY, "Desktop entry has the wrong icon")
    return entry


def check_desktop(root):
    desktop = root / f"usr/share/applications/{BINARY}.desktop"
    desktop_metadata(desktop.read_text())
    run(["desktop-file-validate", str(desktop)])
    require(bool(list((root / "usr/share/icons").glob(f"**/{BINARY}.png"))), "Installed desktop icon is absent")
    record("desktop_entry_and_icon")


def maintainer_metadata(value):
    require(re.fullmatch(r"[^<>\r\n]+ <[^<>\s@]+@[^<>\s@]+>", value) is not None,
            "DEB needs a project Maintainer name and working public email address")


def visible_windows(pid):
    """Return only windows owned by this smoke's process, without reading titles."""
    from ctypes import wintypes
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user32.IsWindowVisible.argtypes = [wintypes.HWND]
    user32.EnumWindows.argtypes = [callback_type, wintypes.LPARAM]
    found = []

    @callback_type
    def collect(window, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(window, ctypes.byref(owner))
        if owner.value == pid and user32.IsWindowVisible(window):
            found.append(window)
        return True

    require(bool(user32.EnumWindows(collect, 0)), "Could not enumerate the app's Win32 windows")
    return found


def require_nonreaping_wait():
    require(all(hasattr(os, name) for name in ("waitid", "P_PID", "WEXITED", "WNOHANG", "WNOWAIT")),
            "POSIX package smoke requires nonreaping waitid support")


def process_status(process):
    if os.name == "nt":
        return process.poll()
    # Linux and macOS support WNOWAIT. Keep even an exited group leader as
    # our child until group cleanup, so its PID cannot name a recycled group.
    require(process.returncode is None, "Package smoke process was reaped before group cleanup")
    result = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    if result is None:
        return None
    return result.si_status if result.si_code == os.CLD_EXITED else -result.si_status


def signal_process_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass
    except PermissionError:
        # XNU's killpg skips zombie members and returns EPERM if none remain
        # signalable. Distinguish that case from a real denied live group;
        # keeping the leader unreaped prevents this group ID being recycled.
        if sys.platform != "darwin" or process_status(process) is None:
            raise
        snapshot = subprocess.run(["ps", "-axo", "pgid=,stat="], check=True,
                                  capture_output=True, text=True, timeout=3).stdout
        for line in snapshot.splitlines():
            group, state = line.split()
            if int(group) == process.pid and not state.startswith("Z"):
                raise


def stop_process(process):
    if os.name == "nt":
        if process.poll() is None:
            from ctypes import wintypes
            user32 = ctypes.WinDLL("user32", use_last_error=True)
            user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
            try:
                for window in visible_windows(process.pid):
                    user32.PostMessageW(window, 0x0010, 0, 0)  # WM_CLOSE
            except RuntimeError:
                # Window enumeration failure must not bypass process cleanup.
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
    else:
        process_status(process)  # Reject an already-reaped, unowned PID.
        try:
            signal_process_group(process, signal.SIGTERM)
            deadline = time.monotonic() + 5
            while process_status(process) is None and time.monotonic() < deadline:
                time.sleep(0.05)
        finally:
            # A wrapper may exit on TERM while descendants ignore it. Kill
            # the whole group before releasing the anchored leader PID.
            try:
                signal_process_group(process, signal.SIGKILL)
            finally:
                process.wait(timeout=5)
    require(process.returncode is not None, "Smoke app did not stop")


def launch_smoke(executable, window_check=None, seconds=12):
    if os.name != "nt":
        require_nonreaping_wait()
    env = os.environ.copy()
    # The QA profile override is debug-only. Release packages use the fresh
    # ephemeral runner/container profile, without signing in to any account.
    env.pop("MORPHEUS_QA_PROFILE_DIR", None)
    if sys.platform.startswith("linux"):
        env.update(SLINT_BACKEND="winit-software", WINIT_UNIX_BACKEND="x11", APPIMAGE_EXTRACT_AND_RUN="1")
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen([str(executable)], env=env, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=os.name != "nt")
        saw_window = False
        try:
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                status = process_status(process)
                require(status is None, f"Installed app exited during startup (status {status})")
                if window_check == "win32":
                    saw_window = saw_window or bool(visible_windows(process.pid))
                elif window_check == "x11" and not saw_window:
                    tree = output(["xwininfo", "-root", "-tree"])
                    saw_window = saw_window or bool(re.search(r'"Morpheus"', tree))
                time.sleep(0.25)
            require(window_check is None or saw_window, "Installed app did not create the expected native window")
        except Exception:
            log.seek(0)
            print(log.read().decode("utf-8", errors="replace")[-6000:], file=sys.stderr)
            raise
        finally:
            stop_process(process)
    record("installed_startup_and_stop", seconds=seconds, window_check=window_check or "process_liveness_only")


def macos_smoke(dist, expected_arch):
    package = artifact(dist, ".dmg")
    run(["hdiutil", "verify", str(package)], stdout=subprocess.DEVNULL)
    with tempfile.TemporaryDirectory(prefix="morpheus-package-") as temp:
        work = Path(temp)
        mount = work / "mounted"
        mount.mkdir()
        run(["hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint", str(mount), str(package)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
        try:
            source = mount / f"{PRODUCT}.app"
            installed = work / "Applications" / source.name
            installed.parent.mkdir()
            run(["ditto", str(source), str(installed)])
            contents = installed / "Contents"
            with (contents / "Info.plist").open("rb") as file:
                info = plistlib.load(file)
            require(info.get("CFBundleIdentifier") == IDENTIFIER, "Incorrect installed bundle identifier")
            require(info.get("CFBundleExecutable") == BINARY, "Incorrect bundle executable")
            require(info.get("CFBundleShortVersionString") == package_version(), "Incorrect installed bundle version")
            for key in ["NSMicrophoneUsageDescription", "NSCameraUsageDescription", "NSScreenCaptureUsageDescription"]:
                require(bool(info.get(key, "").strip()), f"Missing permission purpose string: {key}")
            executable = contents / "MacOS" / BINARY
            require(output(["lipo", "-archs", str(executable)]).split() == [expected_arch], "Wrong installed Mach-O architecture")
            check_resources(contents / "Resources")
            record("dmg_installed_bundle_metadata", architecture=expected_arch, identifier=IDENTIFIER)
            launch_smoke(executable)
            shutil.rmtree(installed)
            require(not installed.exists(), "Temporary installed app was not removed")
            record("macos_app_removal")
        finally:
            run(["hdiutil", "detach", str(mount)], stdout=subprocess.DEVNULL)


def nsis_command(executable, destination, uninstall=False):
    # NSIS requires its final /D= or _?= value UNQUOTED, including paths with
    # spaces. This is a native CreateProcess command, never passed to a shell.
    require(not any(char in str(destination) for char in '\r\n"'), "Invalid NSIS destination")
    return subprocess.list2cmdline([str(executable), "/S"]) + (" _?=" if uninstall else " /D=") + str(destination)


def uninstall_registry():
    import winreg
    entries = []
    for hive in [winreg.HKEY_CURRENT_USER, winreg.HKEY_LOCAL_MACHINE]:
        try:
            with winreg.OpenKey(hive, r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Morpheus",
                                0, winreg.KEY_READ | winreg.KEY_WOW64_64KEY) as key:
                entries.append({name: winreg.QueryValueEx(key, name)[0] for name in ["DisplayName", "DisplayVersion", "InstallLocation"]})
        except FileNotFoundError:
            pass
    return entries


def windows_smoke(dist):
    package = artifact(dist, ".exe")
    require(not uninstall_registry(), "Refusing to replace an existing Morpheus installation")
    with tempfile.TemporaryDirectory(prefix="morpheus-package-") as temp:
        work = Path(temp)
        installed = work / "Installed Morpheus"  # Exercise NSIS paths containing spaces.
        run(nsis_command(package, installed))
        uninstaller = installed / "uninstall.exe"
        try:
            executable = installed / f"{BINARY}.exe"
            check_windows_binary(executable)
            check_resources(installed)
            entries = uninstall_registry()
            require(len(entries) == 1 and entries[0]["DisplayName"] == PRODUCT
                    and entries[0]["DisplayVersion"] == package_version()
                    and Path(entries[0]["InstallLocation"].strip('"')).resolve() == installed.resolve(),
                    "Installed Add/Remove Programs registration does not match")
            record("nsis_install_and_registration")
            launch_smoke(executable, "win32")
        finally:
            if uninstaller.exists():
                # Running a copy with _?= makes the process waitable while the
                # installer removes its own original uninstaller and directory.
                copy = work / "qa-uninstall.exe"
                shutil.copy2(uninstaller, copy)
                run(nsis_command(copy, installed, uninstall=True))
        require(not installed.exists(), "NSIS uninstall left the application directory behind")
        require(not uninstall_registry(), "NSIS uninstall left Add/Remove Programs registration behind")
        record("nsis_uninstall_and_registration_removal")


def apt_install(*packages):
    run(["apt-get", "install", "-y", "--no-install-recommends", *packages], env={**os.environ, "DEBIAN_FRONTEND": "noninteractive"})


def linux_display_launch(executable):
    run(["runuser", "-u", "packageqa", "--", "dbus-run-session", "--", "xvfb-run", "-a", "-s", "-screen 0 1280x800x24",
         "python3", str(Path(__file__)), "--launch-x11", str(executable)])


def linux_container_smoke(dist, kind):
    require(Path("/.dockerenv").exists() and os.geteuid() == 0, "Linux installation checks require the disposable container")
    package = artifact(dist, ".deb" if kind == "deb" else ".AppImage")
    apt_install("binutils", "desktop-file-utils")
    run(["useradd", "--create-home", "packageqa"])
    if kind == "deb":
        fields = {field: output(["dpkg-deb", "--field", str(package), field]) for field in ["Package", "Version", "Architecture", "Depends", "Maintainer"]}
        require(fields["Package"] == DEB_PACKAGE and fields["Architecture"] == "amd64"
                and fields["Version"] == package_version(), "Incorrect Debian package identity")
        require(re.search(r"(?:^|,\s*)libxi6(?:\s|,|$)", fields["Depends"]) is not None, "DEB does not declare libxi6")
        require("libc6 (>= 2.39)" in fields["Depends"] and "libstdc++6 (>= 12.1.0)" in fields["Depends"], "DEB is missing its declared ABI floors")
        maintainer_metadata(fields["Maintainer"])
        record("deb_control", package=fields["Package"], version=fields["Version"], architecture=fields["Architecture"],
               maintainer_present=True)
        apt_install(str(package))
        try:
            executable = Path("/usr/bin") / BINARY
            # Check before Xvfb/desktop test tools can introduce missing runtime
            # libraries transitively. libXi is dynamically loaded, not ELF NEEDED.
            linked = output(["ldd", str(executable)])
            require("not found" not in linked, "Installed DEB has unresolved direct shared libraries")
            ctypes.CDLL("libXi.so.6")
            record("deb_clean_runtime_dependencies_before_xvfb")
            check_elf(executable)
            check_resources(Path("/usr/lib") / BINARY)
            require(Path("/usr/share/doc/morpheus/copyright").read_bytes() == (ROOT / "LICENSE").read_bytes(), "Debian copyright resource differs")
            check_desktop(Path("/"))
            apt_install("xvfb", "xauth", "x11-utils", "dbus-x11", "fonts-dejavu-core")
            linux_display_launch(executable)
        finally:
            run(["apt-get", "purge", "-y", DEB_PACKAGE], env={**os.environ, "DEBIAN_FRONTEND": "noninteractive"})
        require(not Path(f"/usr/bin/{BINARY}").exists() and not Path(f"/usr/lib/{BINARY}").exists()
                and not Path(f"/usr/share/applications/{BINARY}.desktop").exists(), "DEB purge left application files behind")
        record("deb_purge")
    else:
        # Minimal X11 desktop runtime, with no Rust/compiler/library development packages
        # or libraries introduced by installing the DEB in this container.
        apt_install("xvfb", "xauth", "x11-utils", "dbus-x11", "fonts-dejavu-core", "libegl1", "libxkbcommon0",
                    "libxkbcommon-x11-0", "libxcursor1", "libxi6", "libxrandr2")
        with tempfile.TemporaryDirectory(prefix="morpheus-appimage-") as temp:
            work = Path(temp)
            work.chmod(0o755)
            installed = work / package.name
            shutil.copy2(package, installed)
            installed.chmod(0o755)
            env = {key: value for key, value in os.environ.items() if key != "APPIMAGE_EXTRACT_AND_RUN"}
            run([str(installed), "--appimage-extract"], cwd=work, env=env, stdout=subprocess.DEVNULL)
            extracted = work / "squashfs-root"
            check_resources(extracted / "usr/lib" / BINARY)
            check_desktop(extracted)
            check_elf(extracted / "usr/bin" / BINARY)
            # Exercise the normal per-user AppImage startup lock/recovery
            # path, not the root-owned read-only fallback.
            shutil.chown(work, user="packageqa", group="packageqa")
            shutil.chown(installed, user="packageqa", group="packageqa")
            linux_display_launch(installed)
            sidecar = work / ("." + installed.name + ".morpheus-update")
            require(sidecar.is_dir() and (sidecar.stat().st_mode & 0o777) == 0o700,
                    "AppImage did not establish its private update recovery directory")
            require({path.name for path in sidecar.iterdir()} == {"lock"},
                    "Ordinary AppImage startup left a pending update transaction")
            record("appimage_private_startup_lock")
            installed.unlink()
            (sidecar / "lock").unlink()
            sidecar.rmdir()
            require(not installed.exists(), "AppImage was not removed")
            record("appimage_portable_removal")


def package_formats(target, value=None):
    supported = {"linux-x64": ["deb", "appimage"], "windows-x64": ["nsis"],
                 "macos-arm64": ["dmg"], "macos-x64": ["dmg"]}
    require(target in supported, "Unknown package target")
    formats = value.split(",") if value is not None else supported[target]
    require(bool(formats) and len(formats) == len(set(formats))
            and all(kind in supported[target] for kind in formats), "Package formats do not match the target")
    return formats


def linux_smoke(dist, formats):
    run(["docker", "pull", "ubuntu:24.04"])
    image = output(["docker", "image", "inspect", "--format", "{{.Id}}", "ubuntu:24.04"])
    require(re.fullmatch(r"sha256:[a-f0-9]{64}", image) is not None, "Unexpected Docker image identity")
    record("clean_ubuntu_runtime_image", image_id=image)
    for kind in formats:
        command = ["docker", "run", "--rm", "--platform", "linux/amd64", "-e", "GITHUB_ACTIONS=true",
                   "-v", f"{ROOT / 'scripts'}:/qa/scripts:ro", "-v", f"{dist}:/qa/dist:ro",
                   "-v", f"{ROOT / 'Packager.toml'}:/qa/Packager.toml:ro",
                   "-v", f"{ROOT / 'LICENSE'}:/qa/LICENSE:ro", "-v", f"{ROOT / 'themes'}:/qa/themes:ro",
                   image, "bash", "-euc", "apt-get update\napt-get install -y --no-install-recommends python3 ca-certificates\n"
                   f"python3 /qa/scripts/package_smoke.py --linux-container {kind}"]
        run_owned_container(command)


def run_owned_container(command):
    # Killing a Docker CLI on timeout does not stop its daemon-owned container.
    # The fresh cidfile identifies only a container created by this invocation.
    with tempfile.TemporaryDirectory(prefix="morpheus-package-container-") as folder:
        cidfile = Path(folder) / "container.id"
        require(command[:2] == ["docker", "run"], "Expected an owned Docker run command")
        command = command[:2] + ["--cidfile", str(cidfile)] + command[2:]
        try:
            subprocess.run(command, check=True, timeout=900)
        finally:
            if cidfile.exists():
                identity = cidfile.read_text().strip()
                require(re.fullmatch(r"[a-f0-9]{64}", identity) is not None, "Invalid owned container identity")
                removal = subprocess.run(["docker", "rm", "--force", identity],
                                         capture_output=True, text=True, timeout=30)
                require(removal.returncode == 0 or "No such container" in removal.stderr,
                        "Could not remove the package smoke container")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, default=ROOT / "dist")
    parser.add_argument("--target", choices=["linux-x64", "windows-x64", "macos-arm64", "macos-x64"])
    parser.add_argument("--formats", help="Comma-separated package formats to inspect; defaults to every target format")
    parser.add_argument("--linux-container", choices=["deb", "appimage"], help=argparse.SUPPRESS)
    parser.add_argument("--launch-x11", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    require(os.environ.get("GITHUB_ACTIONS") == "true", "Installation smoke is restricted to ephemeral GitHub Actions runners")
    if args.launch_x11:
        require(Path("/.dockerenv").exists() and os.geteuid() != 0, "X11 smoke requires the unprivileged disposable container user")
        launch_smoke(args.launch_x11, "x11")
    elif args.linux_container:
        linux_container_smoke(args.dist.resolve(), args.linux_container)
    else:
        expected = {"linux-x64": ("linux", "x86_64"), "windows-x64": ("win32", "AMD64"),
                    "macos-arm64": ("darwin", "arm64"), "macos-x64": ("darwin", "x86_64")}
        require(args.target in expected and (sys.platform, platform.machine()) == expected[args.target], "Runner does not match requested package architecture")
        formats = package_formats(args.target, args.formats)
        if sys.platform == "darwin":
            macos_smoke(args.dist.resolve(), expected[args.target][1])
        elif sys.platform == "win32":
            windows_smoke(args.dist.resolve())
        else:
            linux_smoke(args.dist.resolve(), formats)
        record("unsigned_package_smoke_complete", target=args.target, formats=formats,
               excluded=["account_login", "media_devices", "accessibility", "wayland", "signing", "updates"])


if __name__ == "__main__":
    main()
