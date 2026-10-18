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
import secrets
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
NONCE_ENV = "MORPHEUS_PACKAGE_SMOKE_NONCE"


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


def linux_process_identity(pid, proc_root=Path("/proc")):
    # comm may itself contain parentheses/spaces. Fields after its final ')' are
    # state, ppid, pgrp, session, ... starttime (field 22).
    fields = (proc_root / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()
    return fields[0], int(fields[2]), int(fields[3]), int(fields[19])


def owned_linux_pid(pid, group, nonce, proc_root=Path("/proc")):
    """Accept the launcher or AppImage descendants, never an unrelated title."""
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        before = linux_process_identity(pid, proc_root)
        with (proc_root / str(pid) / "environ").open("rb") as source:
            environment = source.read(1024 * 1024 + 1)
        after = linux_process_identity(pid, proc_root)
        return (before[1:] == after[1:] and before[0] != "Z" and after[0] != "Z"
                and before[1:3] == (group, group)
                and len(environment) <= 1024 * 1024
                and (NONCE_ENV + "=" + nonce).encode() in environment.split(b"\0"))
    except (OSError, ValueError, IndexError):
        return False


def x11_output(argv):
    # X11 titles/properties and tool stderr are private diagnostic data.
    result = subprocess.run(argv, capture_output=True, text=True, timeout=3)
    require(result.returncode == 0 and len(result.stdout) <= 262144,
            "Could not inspect the package smoke X11 window")
    return result.stdout


def owned_x11_windows(group, nonce):
    tree = x11_output(["xwininfo", "-root", "-tree"])
    windows = set(re.findall(r"^\s+(0x[0-9a-fA-F]+)\s", tree, re.MULTILINE))
    require(len(windows) <= 128, "Unexpected number of smoke X11 windows")
    found = []
    for window in sorted(windows):
        metadata = x11_output(["xprop", "-id", window, "_NET_WM_PID"])
        match = re.fullmatch(r"_NET_WM_PID\(CARDINAL\) = ([1-9][0-9]*)\s*", metadata)
        if not match:
            continue
        pid = int(match[1])
        if owned_linux_pid(pid, group, nonce):
            attributes = x11_output(["xwininfo", "-id", window, "-stats"])
            if re.search(r"^\s*Map State: IsViewable\s*$", attributes, re.MULTILINE) \
                    and owned_linux_pid(pid, group, nonce):
                found.append((window, pid))
    return found


# Emit only reviewed literal names. An arbitrary SONAME could contain private
# text, so a recognized loader failure with any other name withholds that name.
STARTUP_LIBRARY_NAMES = frozenset((
    "libasound.so.2", "libatomic.so.1", "libc.so.6", "libcap.so.2",
    "libcrypto.so.3", "libdbus-1.so.3", "libEGL.so.1", "libexpat.so.1", "libfontconfig.so.1",
    "libfreetype.so.6", "libfuse.so.2", "libgcc_s.so.1", "libGL.so.1",
    "libglib-2.0.so.0", "libgobject-2.0.so.0", "libICE.so.6", "libm.so.6",
    "libpipewire-0.3.so.0", "libSM.so.6", "libsqlite3.so.0", "libssl.so.3", "libstdc++.so.6",
    "libudev.so.1", "libwayland-client.so.0", "libX11.so.6", "libX11-xcb.so.1",
    "libxcb.so.1", "libxcb-render.so.0", "libxcb-shape.so.0", "libxcb-shm.so.0",
    "libxcb-xfixes.so.0", "libxcb-xkb.so.1", "libXcursor.so.1", "libXext.so.6",
    "libXfixes.so.3", "libXi.so.6", "libxkbcommon.so.0", "libxkbcommon-x11.so.0",
    "libXrandr.so.2", "libXrender.so.1", "libz.so.1",
))
STARTUP_LOG_BOUND = 65536


def startup_failure_diagnostics(log, status):
    """Reduce private Linux stderr to fixed facts; never return foreign text.

    Exit 127 alone does not identify a loader defect. Match complete standard
    ELF loader diagnostic lines only, and keep unknown or ambiguous input
    unclassified. No command, file path, symbol, version or arbitrary library
    name can reach public output through this reducer.
    """
    status = status if type(status) is int and -128 <= status <= 255 else None
    facts = {"category": "unclassified", "exit_status": status, "missing_library": None}
    if status != 127:
        return facts
    try:
        log.seek(0)
        raw = log.read(STARTUP_LOG_BOUND + 1)
    except (OSError, ValueError):
        return {**facts, "category": "private_log_unavailable"}
    if type(raw) is not bytes or len(raw) > STARTUP_LOG_BOUND:
        return {**facts, "category": "private_log_exceeded_bound"}
    missing = re.findall(
        rb"(?m)^[^\r\n]{1,4096}: error while loading shared libraries: "
        rb"([A-Za-z0-9_.+-]{1,128}): cannot open shared object file: No such file or directory\r?$", raw)
    symbols = re.findall(
        rb"(?m)^[^\r\n]{1,4096}: symbol lookup error: [^\r\n]{1,4096}: "
        rb"undefined symbol: [A-Za-z0-9_@.]{1,256}\r?$", raw)
    versions = re.findall(
        rb"(?m)^[^\r\n]{1,4096}: [^\r\n]{1,4096}: version [`'][^'\r\n]{1,256}' "
        rb"not found \(required by [^\r\n]{1,4096}\)\r?$", raw)
    matches = len(missing) + len(symbols) + len(versions)
    if matches > 1:
        return {**facts, "category": "ambiguous_loader_diagnostics"}
    if missing:
        name = missing[0].decode("ascii")
        return {**facts, "category": "missing_shared_library",
                "missing_library": name if name in STARTUP_LIBRARY_NAMES else None}
    if symbols:
        return {**facts, "category": "undefined_loader_symbol"}
    if versions:
        return {**facts, "category": "missing_library_version"}
    return facts


def launch_smoke(executable, window_check=None, seconds=12, stop=None):
    """Launch the installed executable for `seconds`, require it to keep running, then stop it.

    `stop` receives the Popen object in the finally block and defaults to the
    shared stop_process. The macOS smoke passes its driver's stop_group so the
    plain launch's whole group is proven empty before its leader is reaped;
    Linux and Windows keep the default.
    """
    if os.name != "nt":
        require_nonreaping_wait()
    env = os.environ.copy()
    # The QA profile override is debug-only. Release packages use the fresh
    # ephemeral runner/container profile, without signing in to any account.
    env.pop("MORPHEUS_QA_PROFILE_DIR", None)
    if sys.platform.startswith("linux"):
        env.update(SLINT_BACKEND="winit-software", WINIT_UNIX_BACKEND="x11", APPIMAGE_EXTRACT_AND_RUN="1")
    nonce = secrets.token_hex(16)
    env[NONCE_ENV] = nonce
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen([str(executable)], env=env, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=os.name != "nt")
        saw_window = False
        status = None
        try:
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                status = process_status(process)
                require(status is None, f"Installed app exited during startup (status {status})")
                if window_check == "win32":
                    saw_window = saw_window or bool(visible_windows(process.pid))
                elif window_check == "x11" and not saw_window:
                    saw_window = bool(owned_x11_windows(process.pid, nonce))
                time.sleep(0.25)
            require(window_check is None or saw_window, "Installed app did not create the expected native window")
        except Exception:
            # Never publish app logs: even startup errors can contain private
            # paths, URLs or account details. Only fixed reduced facts are public.
            failure = {"check": "installed_startup", "status": "failed", "app_log_public": False}
            if sys.platform.startswith("linux"):
                failure["diagnostic"] = startup_failure_diagnostics(log, status)
            print(json.dumps(failure, sort_keys=True), flush=True)
            raise
        finally:
            (stop_process if stop is None else stop)(process)
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
            macos_installed_launches(executable, work)
            shutil.rmtree(installed)
            require(not installed.exists(), "Temporary installed app was not removed")
            record("macos_app_removal")
        finally:
            run(["hdiutil", "detach", str(mount)], stdout=subprocess.DEVNULL)


def macos_module():
    if __package__:
        from . import macos_package_ui
    else:
        import macos_package_ui
    return macos_package_ui


def macos_bridge_build(work):
    """Compile the Swift accessibility bridge with the runner's system toolchain, before any launch."""
    source = Path(__file__).with_name("macos_package_ui.swift")
    binary = Path(work) / "morpheus-ax-bridge"
    # Compiler output can quote source paths; keep it private like every other stream.
    with tempfile.TemporaryFile() as log:
        result = subprocess.run(["xcrun", "swiftc", "-O", "-module-name", "MorpheusAxBridge", "-o", str(binary),
                                 str(source)], stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                                timeout=600)
    require(result.returncode == 0 and binary.is_file(), "Accessibility bridge did not compile")
    record("macos_accessibility_bridge_compiled")
    return binary


def macos_keyboard_launch(executable, nonce, bridge):
    """Second launch of the same installed bundle executable for the bounded AX/keyboard driver.

    Returns (passed, stopped): whether the whole check passed, and whether the
    driver verified that the app's group and the bridge stopped.
    """
    script = Path(__file__).with_name("macos_package_ui.py")
    with tempfile.TemporaryFile() as log:
        result = subprocess.run([sys.executable, str(script), str(executable), nonce, str(bridge)],
                                stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, timeout=360)
        log.seek(0)
        report = keyboard_report(log.read(65537), check="installed_macos_keyboard_ax",
                                 validate=validate_macos_keyboard_diagnostic)
    print(json.dumps(report, sort_keys=True), flush=True)
    return (result.returncode == 0 and report["status"] == "passed", report["diagnostic"]["cleanup"] == "passed")


def macos_installed_launches(executable, work):
    """One owned profile around the plain launch and the keyboard relaunch.

    The bridge is compiled and its read-only permission state proven before
    any profile or application work. The release executable creates its
    account store inside the Application Support profile on first start, so
    the smoke establishes that profile before the plain launch and the driver
    only proves the markers. The profile is removed only after both launches
    verifiably stopped; otherwise it is retained on the disposable runner and
    the smoke fails.
    """
    module = macos_module()
    bridge = macos_bridge_build(work)
    module.probe_permissions(bridge)
    record("macos_accessibility_permission_present")
    profile = module.PackageProfile.create()
    stopped = False
    try:
        # The plain launch stops through the driver's group proof: its leader is
        # reaped only once no live group member remains, otherwise this raises
        # and the profile is retained before any second launch. The keyboard
        # driver then relaunches the identical installed bundle executable.
        launch_smoke(executable, stop=module.stop_group)
        passed, stopped = macos_keyboard_launch(executable, profile.nonce, bridge)
        require(passed, "Installed macOS keyboard acceptance failed")
    finally:
        if stopped:
            require(profile.remove(), "Synthetic macOS profile was not removed")
            record("macos_profile_removed")
        else:
            print(json.dumps({"check": "macos_profile_retained", "status": "failed"}, sort_keys=True), flush=True)


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


def windows_profile():
    """The one synthetic release profile shared by both installed launches."""
    if __package__:
        from . import windows_package_ui
    else:
        import windows_package_ui
    return windows_package_ui.PackageProfile.create()


def windows_keyboard_launch(executable, nonce):
    """Second launch of the same installed executable for the bounded UIA/keyboard driver.

    Returns (passed, stopped): whether the whole check passed, and whether the
    driver verified that its job, holding the app and the bridge, is empty.
    """
    script = Path(__file__).with_name("windows_package_ui.py")
    # The driver's own stream stays private: PowerShell and UI Automation can
    # print diagnostics with element names or paths outside its JSON report.
    with tempfile.TemporaryFile() as log:
        result = subprocess.run([sys.executable, str(script), str(executable), nonce], stdin=subprocess.DEVNULL,
                                stdout=log, stderr=subprocess.STDOUT, timeout=360)
        log.seek(0)
        report = keyboard_report(log.read(65537), check="installed_windows_keyboard_uia",
                                 validate=validate_windows_keyboard_diagnostic)
    print(json.dumps(report, sort_keys=True), flush=True)
    return (result.returncode == 0 and report["status"] == "passed", report["diagnostic"]["cleanup"] == "passed")


def windows_installed_launches(executable):
    """One owned profile around the plain launch and the keyboard relaunch.

    The release executable creates its account store inside the known-folder
    profile on first start, so the smoke establishes that profile before any
    launch and the driver only proves the markers. The profile is removed only
    after both launches verifiably stopped; otherwise it is retained on the
    disposable runner and the smoke fails.
    """
    profile = windows_profile()
    stopped = False
    try:
        launch_smoke(executable, "win32")
        # launch_smoke's stop_process raised unless the process stopped. The
        # keyboard driver relaunches the identical installed executable.
        passed, stopped = windows_keyboard_launch(executable, profile.nonce)
        require(passed, "Installed Windows keyboard acceptance failed")
    finally:
        if stopped:
            require(profile.remove(), "Synthetic Windows profile was not removed")
            record("windows_profile_removed")
        else:
            print(json.dumps({"check": "windows_profile_retained", "status": "failed"}, sort_keys=True), flush=True)


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
            windows_installed_launches(executable)
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


def linux_display_launch(executable, keyboard=False):
    script = Path(__file__).with_name("linux_package_ui.py") if keyboard else Path(__file__)
    arguments = [str(executable)] if keyboard else ["--launch-x11", str(executable)]
    # The driver has a 180-second alarm plus bounded cleanup; leave headroom
    # before this outer deadline. The owning Docker container is removed even
    # if this launcher is terminated before its children exit.
    command = ["runuser", "-u", "packageqa", "--", "dbus-run-session", "--", "xvfb-run", "-a", "-s", "-screen 0 1280x800x24",
               "python3", str(script), *arguments]
    if not keyboard:
        subprocess.run(command, check=True, timeout=180)
        return
    # Native accessibility/X11 libraries can print diagnostics outside Python's
    # exception handler. Keep the whole driver's stream private as well.
    with tempfile.TemporaryFile() as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=240)
        log.seek(0)
        report = keyboard_report(log.read(65537))
    print(json.dumps(report, sort_keys=True), flush=True)
    require(result.returncode == 0 and report["status"] == "passed", "Installed keyboard acceptance failed")


KEYBOARD_FAILURES = frozenset({
    "none", "selector_ambiguous", "tree_bound", "app_exited", "wait_timeout",
    "password_role", "deadline", "attribute_error", "type_error",
    "not_implemented", "lookup_error", "os_error", "unclassified",
    "value_error", "overflow_error", "runtime_error",
})
KEYBOARD_STEPS = frozenset({
    "setup", "atspi_setup", "homeserver", "username", "password", "password_role",
    "preview", "account_menu", "appearance", "normal_close", "cleanup", "complete",
    "appearance_reach_scale", "appearance_initial_scale", "appearance_saved_scale",
    "appearance_invalid_notice", "appearance_invalid_retained", "appearance_restored",
    "menu_trigger_focus", "menu_open_return", "menu_return_focus", "menu_return_visible",
    "menu_escape", "menu_escape_hidden", "menu_escape_focus", "menu_open_space",
    "menu_space_focus", "menu_tab_manage", "menu_manage_activate", "menu_manage_visible",
    "menu_done_focus", "menu_add_tab", "menu_done_backtab", "menu_manage_escape", "menu_final_focus",
})
KEYBOARD_FOCUS_KINDS = frozenset({
    "unavailable", "none", "account_trigger", "account_choice", "manage_choice",
    "add_choice", "done", "add_account", "other", "ambiguous",
})
KEYBOARD_OPERATIONS = frozenset({
    "none", "desktop", "application_pid", "ownership", "node_cache", "node_children",
    "node_name", "node_role", "process_status", "cleanup",
    "desktop_child_count", "desktop_child_index", "node_child_count", "node_child_index",
})
KEYBOARD_OBSERVATIONS = frozenset({
    "desktop_app_seen", "owned_app_seen", "owned_node_seen", "homeserver_seen",
    "username_seen", "password_seen", "focus_anchor_seen", "focus_requested",
})


# Installed Windows UI Automation/keyboard driver: fixed tokens only.
WINDOWS_KEYBOARD_FAILURES = frozenset({
    "none", "guard", "install_state", "profile_invalid", "window_count", "app_exited",
    "selector_ambiguous", "tree_bound", "wait_timeout", "password_semantics",
    "focus_unowned", "foreground_unowned", "input_rejected", "scale_unreachable",
    "bridge_invalid", "bridge_timeout", "bridge_exited", "bridge_overflow",
    "config_mismatch", "exit_status", "process_remaining", "deadline",
    "job_setup", "job_assignment", "process_start", "process_wait", "cleanup", "internal",
    "attribute_error", "type_error", "lookup_error", "value_error", "os_error",
    "runtime_error", "unclassified",
})
WINDOWS_KEYBOARD_STEPS = frozenset({
    "setup", "profile", "launch", "window", "bridge", "homeserver", "username", "password",
    "password_semantics", "preview", "account_menu", "appearance", "config_persisted",
    "normal_close", "cleanup", "complete",
})
WINDOWS_KEYBOARD_OPERATIONS = frozenset({
    "none", "registry", "known_folder", "profile_open", "job", "process_create", "process_id",
    "window_enumeration", "bridge_start", "snapshot", "node_owner", "node_name", "node_role",
    "focus", "focused_element", "key_input", "process_status", "config_read", "close_request",
    "process_wait", "job_query", "cleanup",
})
WINDOWS_KEYBOARD_OBSERVATIONS = frozenset({
    "owned_window_seen", "uia_root_seen", "homeserver_seen", "username_seen", "password_seen",
})
WINDOWS_PASSWORD_SEMANTICS = frozenset({"unchecked", "protected", "unprotected", "not_edit"})
WINDOWS_BRIDGE_ERRORS = frozenset({
    "root_unavailable", "unowned_root", "invalid_id", "unowned", "element_unavailable",
    "invalid_key", "window_gone", "foreground_unowned", "input_rejected", "invalid_command",
    "bridge_error",
})


# Installed macOS accessibility/keyboard driver: fixed tokens only.
MACOS_KEYBOARD_FAILURES = frozenset({
    "none", "guard", "permission_unavailable", "install_state", "profile_invalid", "ownership_lost",
    "window_count", "app_exited", "selector_ambiguous", "tree_bound", "wait_timeout",
    "password_semantics", "focus_unowned", "foreground_unowned", "input_rejected", "scale_unreachable",
    "bridge_invalid", "bridge_timeout", "bridge_exited", "bridge_overflow",
    "config_mismatch", "exit_status", "process_remaining", "deadline", "cleanup",
    "attribute_error", "type_error", "lookup_error", "value_error", "os_error",
    "runtime_error", "unclassified",
})
MACOS_KEYBOARD_STEPS = frozenset({
    "setup", "profile", "bridge", "permission", "launch", "window", "homeserver", "username", "password",
    "password_semantics", "preview", "account_menu", "appearance", "config_persisted",
    "normal_close", "cleanup", "complete",
})
MACOS_KEYBOARD_OPERATIONS = frozenset({
    "none", "bundle", "profile_open", "bridge_start", "permissions", "process_create", "ownership", "attach",
    "window_enumeration", "snapshot", "node_owner", "node_name", "node_role", "focus", "focused_element",
    "key_input", "process_status", "config_read", "close_request", "process_wait", "group_query", "cleanup",
})
MACOS_KEYBOARD_OBSERVATIONS = frozenset({
    "permission_checked", "owned_window_seen", "ax_root_seen", "homeserver_seen", "username_seen",
    "password_seen",
})
MACOS_PASSWORD_SEMANTICS = frozenset({"unchecked", "protected", "unprotected", "not_text_field"})
MACOS_BRIDGE_ERRORS = frozenset({
    "bridge_error", "invalid_command", "attach_mismatch", "not_attached", "window_list_unavailable",
    "unowned_root", "invalid_id", "unowned", "element_unavailable", "invalid_key", "window_gone",
    "foreground_unowned", "input_rejected", "close_unavailable", "window_count",
})


def validate_macos_keyboard_diagnostic(value, passed):
    counts = ("selector_matches", "incomplete_passes", "activation_recoveries", "bounded_snapshots")
    expected = MACOS_KEYBOARD_OBSERVATIONS | set(counts) | {
        "step", "operation", "failure", "password_semantics", "cleanup", "profile_owned",
        "traversed_node_matched", "bridge_error",
    }
    require(type(value) is dict and value.keys() == expected, "Invalid installed macOS keyboard diagnostic shape")
    require(all(type(value[key]) is str for key in ("step", "operation", "failure", "password_semantics",
                                                     "cleanup", "bridge_error"))
            and value["step"] in MACOS_KEYBOARD_STEPS and value["operation"] in MACOS_KEYBOARD_OPERATIONS
            and value["failure"] in MACOS_KEYBOARD_FAILURES
            and value["password_semantics"] in MACOS_PASSWORD_SEMANTICS
            and value["cleanup"] in {"not_started", "passed", "failed"}
            and value["bridge_error"] in MACOS_BRIDGE_ERRORS | {"none"}
            and all(type(value[key]) is int and 0 <= value[key] <= 2 for key in counts)
            and all(type(value[key]) is bool for key in MACOS_KEYBOARD_OBSERVATIONS)
            and type(value["traversed_node_matched"]) is bool and type(value["profile_owned"]) is bool,
            "Invalid installed macOS keyboard diagnostic value")
    require(not passed or (value["step"] == "complete" and value["failure"] == "none"
            and value["cleanup"] == "passed" and value["profile_owned"]
            and value["password_semantics"] == "protected" and value["bridge_error"] == "none"
            and all(value[key] for key in MACOS_KEYBOARD_OBSERVATIONS)),
            "Incomplete installed macOS keyboard diagnostics")


def validate_windows_keyboard_diagnostic(value, passed):
    counts = ("selector_matches", "incomplete_passes", "foreground_recoveries", "bounded_snapshots")
    expected = WINDOWS_KEYBOARD_OBSERVATIONS | set(counts) | {
        "step", "operation", "failure", "password_semantics", "cleanup", "profile_owned",
        "traversed_node_matched", "bridge_error",
    }
    require(type(value) is dict and value.keys() == expected, "Invalid installed Windows keyboard diagnostic shape")
    require(all(type(value[key]) is str for key in ("step", "operation", "failure", "password_semantics",
                                                     "cleanup", "bridge_error"))
            and value["step"] in WINDOWS_KEYBOARD_STEPS and value["operation"] in WINDOWS_KEYBOARD_OPERATIONS
            and value["failure"] in WINDOWS_KEYBOARD_FAILURES
            and value["password_semantics"] in WINDOWS_PASSWORD_SEMANTICS
            and value["cleanup"] in {"not_started", "passed", "failed"}
            and value["bridge_error"] in WINDOWS_BRIDGE_ERRORS | {"none"}
            and all(type(value[key]) is int and 0 <= value[key] <= 2 for key in counts)
            and all(type(value[key]) is bool for key in WINDOWS_KEYBOARD_OBSERVATIONS)
            and type(value["traversed_node_matched"]) is bool and type(value["profile_owned"]) is bool,
            "Invalid installed Windows keyboard diagnostic value")
    require(not passed or (value["step"] == "complete" and value["failure"] == "none"
            and value["cleanup"] == "passed" and value["profile_owned"]
            and value["password_semantics"] == "protected" and value["bridge_error"] == "none"
            and all(value[key] for key in WINDOWS_KEYBOARD_OBSERVATIONS)),
            "Incomplete installed Windows keyboard diagnostics")


def validate_keyboard_diagnostic(value, passed):
    expected = KEYBOARD_OBSERVATIONS | {"step", "operation", "failure", "selector_matches",
                                        "password_role", "cleanup", "traversed_node_matched",
                                        "child_count_unavailable", "child_unavailable",
                                        "incomplete_passes", "focus_kind", "focused_nodes"}
    require(type(value) is dict and value.keys() == expected, "Invalid installed keyboard diagnostic shape")
    require(all(type(value[key]) is str for key in ("step", "operation", "failure", "password_role", "cleanup"))
            and value["step"] in KEYBOARD_STEPS and value["operation"] in KEYBOARD_OPERATIONS
            and value["failure"] in KEYBOARD_FAILURES
            and value["password_role"] in {"unchecked", "password_text", "entry", "text", "other"}
            and value["cleanup"] in {"not_started", "passed", "failed"}
            and type(value["focus_kind"]) is str and value["focus_kind"] in KEYBOARD_FOCUS_KINDS
            and all(type(value[key]) is int and 0 <= value[key] <= 2
                    for key in ("selector_matches", "child_count_unavailable", "child_unavailable",
                                "incomplete_passes", "focused_nodes"))
            and all(type(value[key]) is bool for key in KEYBOARD_OBSERVATIONS)
            and type(value["traversed_node_matched"]) is bool,
            "Invalid installed keyboard diagnostic value")
    require(not value["focus_requested"] or value["focus_anchor_seen"],
            "Inconsistent installed keyboard focus request diagnostic")
    require((value["focus_kind"] in {"unavailable", "none"} and value["focused_nodes"] == 0)
            or (value["focus_kind"] == "ambiguous" and value["focused_nodes"] == 2)
            or (value["focus_kind"] not in {"unavailable", "none", "ambiguous"}
                and value["focused_nodes"] == 1), "Inconsistent installed keyboard focus diagnostic")
    require(not passed or (value["step"] == "complete" and value["failure"] == "none"
            and value["cleanup"] == "passed" and value["password_role"] == "password_text"
            and value["focus_kind"] not in {"unavailable", "ambiguous"}
            and all(value[key] for key in KEYBOARD_OBSERVATIONS)),
            "Incomplete installed keyboard diagnostics")


def keyboard_report(raw, check="installed_linux_keyboard_atspi", validate=None):
    validate = validate or validate_keyboard_diagnostic
    expected = {"check", "status", "phase", "sign_in_semantics", "account_menu_keyboard",
                "appearance_config", "normal_window_close", "app_log_public", "screen_reader_acceptance", "diagnostic"}
    phases = {"setup", "sign_in_semantics", "account_menu_keyboard", "appearance_config",
              "normal_window_close", "complete"}
    require(len(raw) <= 65536, "Installed keyboard diagnostics exceeded bound")
    matches = []
    for line in raw.splitlines():
        try:
            value = json.loads(line)
        except (ValueError, UnicodeError):
            continue
        if not isinstance(value, dict) or value.get("check") != check:
            continue
        require(value.keys() == expected and value["status"] in {"passed", "failed"}
                and value["phase"] in phases
                and all(type(value[key]) is bool for key in expected - {"check", "status", "phase", "diagnostic"})
                and not value["app_log_public"] and not value["screen_reader_acceptance"],
                "Invalid installed keyboard diagnostic shape")
        validate(value["diagnostic"], value["status"] == "passed")
        require(value["status"] != "passed" or (value["phase"] == "complete"
                and all(value[key] for key in ("sign_in_semantics", "account_menu_keyboard",
                                             "appearance_config", "normal_window_close"))),
                "Incomplete installed keyboard evidence")
        matches.append(value)
    require(len(matches) == 1, "Missing or ambiguous installed keyboard evidence")
    return matches[0]


def linux_appimage_launch(executable):
    linux_display_launch(executable)
    # Accessibility/XTest tooling can add runtime libraries. Prove a launch
    # against the minimal runtime before installing any of that tooling.
    record("appimage_clean_runtime_before_accessibility_tools")
    apt_install("python3-pyatspi", "python3-gi", "at-spi2-core", "libglib2.0-bin", "xdotool")
    linux_display_launch(executable, keyboard=True)


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
        # Minimal X11 desktop/audio runtime, with no Rust/compiler/library development
        # packages or DEB side effects. AppImage excludes ALSA's device-facing
        # library, so Ubuntu 24.04 must supply it before the first launch.
        apt_install("xvfb", "xauth", "x11-utils", "dbus-x11", "fonts-dejavu-core", "libegl1", "libxkbcommon0",
                    "libxkbcommon-x11-0", "libxcursor1", "libxi6", "libxrandr2", "libasound2t64")
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
            linux_appimage_launch(installed)
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
