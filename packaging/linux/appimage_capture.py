"""Private acceptance of the actual ordinary installed Linux AppImage.

Two fresh disposable accounts/profiles: normal loader first, then child-local
LD_DEBUG=bindings/LD_BIND_NOW. The second run proves loader binding, never
substitutes for ordinary scheduling. MatrixRTC receives real portal pixels;
no native diagnostic CLI, frame injection, UI property setter or key override.
"""
from __future__ import annotations

import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import appimage_client as client
from appimage_runtime import RuntimeIdentity
import element_process_namespace as containment
from linux_package_ui import AccessibleUI, IncompleteTraversal
import package_smoke as smoke
from run_wayland_acceptance import CappedPrivateLog, configs, private_environment
from wayland_fixture_backend import prepare_backend

PEER_TEST = "appimage_peer::receives_actual_installed_appimage_screen"
FACTS = ("unavailable_audio_verified", "peer_ready", "exact_sender_joined", "static_frame", "changed_frame",
         "screen_stopped", "reopened_frame", "changed_after_reopen", "source_loss_stopped_track", "app_left", "cleanup", "complete")
CONTAINERS = ("morpheus-synapse-a", "morpheus-synapse-b", "morpheus-rtc-livekit", "morpheus-rtc-auth",
              "morpheus-rtc-federation", "morpheus-rtc-sfu-loopback")
VOLUMES = ("morpheus-synapse-a-data", "morpheus-synapse-b-data")
NETWORK = "morpheus-matrix-test"


class CaptureFailure(RuntimeError):
    def __init__(self):
        super().__init__("Installed AppImage capture acceptance failed")


def require(value):
    if not value:
        raise CaptureFailure()


def executable(name):
    value = shutil.which(name)
    require(value is not None)
    path = Path(value).resolve(strict=True)
    for item in (path, *path.parents):
        info = item.stat()
        require(info.st_uid in {0, os.getuid()} and not info.st_mode & 0o022)
    return str(path)


def fact(root, name):
    require(name in FACTS)
    path = root / (name + ".json")
    if not path.exists():
        return False
    return client.read_regular(path, 64) == b'{"passed":true}\n'


def request(root, name):
    require(name in {"static", "changed", "stopped", "reopened", "changed_again", "source_lost", "left"})
    client.write_new(root / ("request-" + name + ".json"), b'{"proceed":true}\n')


def peer_proof(contents):
    try:
        output = contents.decode("utf-8", "strict")
    except UnicodeError:
        raise CaptureFailure() from None
    proof = re.findall(r"^test " + re.escape(PEER_TEST) + r" \.\.\. (ok|FAILED|ignored)$", output, re.MULTILINE)
    summaries = re.findall(r"^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored;", output, re.MULTILINE)
    require(proof == ["ok"] and summaries == [("ok", "1", "0", "0")])


def fixture_path(output, root):
    # Helper stdout contains exactly one private path. Stage codes go to stderr.
    text = output.decode("utf-8", "strict").strip()
    require("\n" not in text and "\r" not in text)
    path = Path(text)
    require(path.is_absolute() and path.resolve() == path and path.is_relative_to(root))
    client.read_regular(path, 65536)
    require(path.stat().st_uid == os.getuid() and not path.stat().st_mode & 0o077)
    return path


def compile_peer(runner, root, environment, cargo):
    raw = runner.run("capture-peer-build", [cargo, "test", "--locked", "--release", "-p", "morpheus-rtc",
                     "--features", "native-devices", "--test", "wayland_capture", "--no-run", "--message-format=json"],
                     cwd=ROOT, env=environment, timeout=1800, output_limit=8 * 1024 * 1024)
    candidates = []
    for line in raw.splitlines():
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "wayland_capture"
                and item.get("profile", {}).get("test") is True and item.get("executable")):
            candidates.append(Path(item["executable"]))
    require(len(candidates) == 1)
    path = candidates[0].resolve(strict=True)
    require(path.is_relative_to(ROOT / "target") and path.stat().st_uid == os.getuid() and not path.stat().st_mode & 0o022)
    listed = runner.run("capture-peer-list", [str(path), "--list"], cwd=root, env=environment, timeout=15, output_limit=32768)
    require(listed.decode().splitlines().count(PEER_TEST + ": test") == 1)
    return path


class Processes:
    """Unreaped owned group leaders plus bounded private dual-pipe drains."""
    def __init__(self, root):
        self.root, self.owned, self.sinks = root, [], {}
        self.stack = contextlib.ExitStack()

    def start(self, name, argv, environment, *, stdin=None):
        require(re.fullmatch(r"[a-z][a-z0-9-]{0,39}", name) and name not in self.sinks)
        out = self.stack.enter_context(CappedPrivateLog(self.root / (name + ".stdout.log")))
        err = self.stack.enter_context(CappedPrivateLog(self.root / (name + ".stderr.log")))
        input_file = None
        try:
            if stdin is not None:
                require(isinstance(stdin, bytes) and len(stdin) <= 4096)
                input_file = tempfile.TemporaryFile()
                input_file.write(stdin)
                input_file.seek(0)
            process = subprocess.Popen(argv, cwd=ROOT, env=environment, stdin=input_file or subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            # Register ownership before either thread can fail to attach.
            self.owned.append(process)
            self.sinks[name] = (out, err)
            out.attach(process.stdout)
            err.attach(process.stderr)
            return process
        finally:
            if input_file is not None:
                input_file.close()

    def wait_for(self, predicate, *, seconds=30, watch=()):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            require(all(containment.status(process) is None for process in watch))
            require(all(not sink.failed and not sink.truncated for pair in self.sinks.values() for sink in pair))
            value = predicate()
            if value:
                return value
            time.sleep(0.1)
        raise CaptureFailure()

    def finish(self, name, process, seconds=30):
        require(containment.wait_owned(process, seconds) == 0)
        for sink in self.sinks[name]:
            sink.finish()
            require(sink.drained and not sink.failed and not sink.truncated)
        return tuple(client.read_regular(self.root / (name + suffix), 8 * 1024 * 1024)
                     for suffix in (".stdout.log", ".stderr.log"))

    def close(self):
        failed = False
        for process in reversed(self.owned):
            try:
                containment.stop_owned(process)
            except BaseException:
                failed = True
        try:
            self.stack.close()
        except BaseException:
            failed = True
        self.owned.clear()
        require(not failed)


class InstalledUI(AccessibleUI):
    def find_control(self, label, *, editable=False):
        matches = []
        for node in self.nodes():
            if node.name != label:
                continue
            try:
                interface = node.queryEditableText() if editable else node.queryAction()
                if editable or interface.nActions:
                    matches.append((node, interface))
                    require(len(matches) <= 1)
            except NotImplementedError:
                continue
            except (LookupError, self.transient_error):
                self.pass_incomplete = True
                continue
        require(len(matches) <= 1)
        if getattr(self, "pass_incomplete", False):
            self.diagnostics.count("incomplete_passes")
            raise IncompleteTraversal()
        return matches[0] if matches else None

    def enter(self, label, value, *, password=False):
        node, interface = self.wait_for(lambda: self.find_control(label, editable=True))
        if password:
            require(node.getRole() == self.api.ROLE_PASSWORD_TEXT)
        require(interface.setTextContents(value))

    def press(self, label):
        node, action = self.wait_for(lambda: self.find_control(label))
        self.wait_for(lambda: node.getState().contains(self.api.STATE_ENABLED))
        require(action.doAction(0))

    def application_pid(self):
        found = set()
        for application in self.api.Registry.getDesktop(0):
            pid = application.get_process_id()
            if self.owned(pid):
                found.add(pid)
        require(len(found) <= 1)
        return next(iter(found)) if found else None


def enable_private_accessibility(run, environment):
    # This function runs once in a fresh case worker, before pyatspi/Atspi is
    # imported. Its process-global client must use the same owned session bus
    # as the installed app; child-only env overrides cannot configure it.
    require("pyatspi" not in sys.modules and "gi.repository.Atspi" not in sys.modules)
    require(environment.get("DBUS_SESSION_BUS_ADDRESS", "").startswith("unix:"))
    os.environ.clear()
    os.environ.update(environment)
    for name in ("AT_SPI_BUS_ADDRESS", "DBUS_STARTER_ADDRESS", "DBUS_STARTER_BUS_TYPE"):
        os.environ.pop(name, None)
        environment.pop(name, None)
    # AccessKit 0.22.1 observes IsEnabled; obtaining the accessibility bus
    # address alone does not enable its tree. Set only this private session.
    for setting in ("IsEnabled", "ScreenReaderEnabled"):
        run("capture-accessibility", ["gdbus", "call", "--session", "--dest", "org.a11y.Bus",
            "--object-path", "/org/a11y/bus", "--method", "org.freedesktop.DBus.Properties.Set",
            "org.a11y.Status", setting, "<true>"], seconds=5, limit=4096)


def owned_window_on_first_output(tree, pid):
    matches = []
    count = 0
    def visit(node, output=None):
        nonlocal count
        count += 1
        require(count <= 4096 and isinstance(node, dict))
        if node.get("type") == "output":
            output = node.get("name")
        if node.get("pid") == pid:
            matches.append(output)
        for child in node.get("nodes", []) + node.get("floating_nodes", []):
            visit(child, output)
    visit(tree)
    require(matches == ["HEADLESS-1"])


def run_case(runner, root, artifact, appdir, peer_binary, backend, fixture, environment, *, bindings, diagnostics=None):
    control = root / "peer-control"
    mode = "eager_bindings" if bindings else "ordinary"
    def phase(name):
        if diagnostics is not None:
            diagnostics.capture(mode, name, control)
    phase("setup")
    root.mkdir(mode=0o700)
    configs(root)
    env = {**private_environment(root), **environment}
    for name in tuple(env):
        if name.startswith(("LD_", "PIPEWIRE_DEBUG", "MORPHEUS_WAYLAND_")):
            env.pop(name)
    env["MORPHEUS_WAYLAND_CHOOSER_MODE"] = str(root / "chooser-mode")
    env.update(SLINT_BACKEND="winit-software", WINIT_UNIX_BACKEND="wayland", APPIMAGE_EXTRACT_AND_RUN="1")
    for name in ("cache", "state", "data", "tmp"):
        (root / name).mkdir(mode=0o700)
    env.update(XDG_CACHE_HOME=str(root / "cache"), TMPDIR=str(root / "tmp"))
    nonce = secrets.token_hex(16)
    env[smoke.NONCE_ENV] = nonce
    audio_ids = {"microphone": "alsa:morpheus-unavailable-input-" + nonce,
                 "speaker": "alsa:morpheus-unavailable-output-" + nonce}
    config = root / "config/morpheus"
    config.mkdir(mode=0o700)
    client.write_new(config / "config.toml", ("schema_version = 1\n[media]\n"
        + "microphone_device = " + json.dumps(audio_ids["microphone"]) + "\n"
        + "speaker_device = " + json.dumps(audio_ids["speaker"]) + "\n").encode())
    control = root / "peer-control"
    control.mkdir(mode=0o700)
    client.write_new(control / "control.json", json.dumps({"fixture": str(fixture), **audio_ids}).encode())
    data = json.loads(client.read_regular(fixture, 65536))
    accounts = [item for item in data["accounts"] if item["label"] == "a_primary"]
    require(len(accounts) == 1 and re.fullmatch(r"rtcalice[0-9a-f]{16}", accounts[0]["username"]))
    account = accounts[0]
    room_name = "Morpheus native RTC " + account["username"][8:]
    owner = Processes(root)
    identity = None
    result = None
    body_failed = False
    try:
        phase("bus")
        bus = owner.start("bus", [executable("dbus-daemon"), "--session", "--nofork", "--nopidfile", "--print-address=1"], env)
        def bus_address():
            raw = client.read_regular(root / "bus.stdout.log", 8192)
            lines = raw.splitlines()
            return lines[0].decode() if lines and lines[0].startswith(b"unix:") else None
        env["DBUS_SESSION_BUS_ADDRESS"] = owner.wait_for(bus_address, watch=[bus])
        phase("keyring")
        keyring = owner.start("keyring", [executable("gnome-keyring-daemon"), "--foreground", "--unlock", "--components=secrets"], env,
                              stdin=secrets.token_urlsafe(32).encode() + b"\n")
        phase("display")
        sway = owner.start("sway", [executable("sway"), "--config", str(root / "config/sway.conf")], env)
        runtime = root / "runtime"
        env["WAYLAND_DISPLAY"] = owner.wait_for(lambda: next((p.name for p in runtime.glob("wayland-*") if p.is_socket()), None), watch=[bus, sway])
        env["SWAYSOCK"] = str(owner.wait_for(lambda: next(runtime.glob("sway-ipc.*.sock"), None), watch=[bus, sway]))
        def run(stage, argv, seconds=15, limit=65536):
            return runner.run(stage, [executable(argv[0]), *argv[1:]], cwd=ROOT, env=env, timeout=seconds, output_limit=limit)
        run("capture-dbus-env", ["dbus-update-activation-environment", "WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR", "PIPEWIRE_RUNTIME_DIR"])
        enable_private_accessibility(run, env)
        phase("portal")
        pipewire = owner.start("pipewire", [executable("pipewire")], env)
        owner.wait_for(lambda: (runtime / "pipewire-0").is_socket(), watch=[pipewire])
        wireplumber = owner.start("wireplumber", [executable("wireplumber")], env)
        portal_backend = owner.start("portal-wlr", [str(backend), "--loglevel", "ERROR", "--config", str(root / "config/wlr.ini")], env)
        def backend_owned():
            try:
                raw = run("capture-portal-owner", ["gdbus", "call", "--session", "--dest", "org.freedesktop.DBus", "--object-path", "/org/freedesktop/DBus", "--method", "org.freedesktop.DBus.GetConnectionUnixProcessID", "org.freedesktop.impl.portal.desktop.wlr"], seconds=3)
                match = re.fullmatch(rb"\(uint32 ([0-9]+),\)\s*", raw)
                return match is not None and int(match[1]) == portal_backend.pid
            except Exception as error:
                from appimage_process import CommandFailure
                if isinstance(error, CommandFailure) and error.reason == "exit":
                    return False
                raise
        owner.wait_for(backend_owned, watch=[portal_backend, bus])
        portal = owner.start("portal", ["/usr/libexec/xdg-desktop-portal", "--replace"], env)
        run("capture-focus", ["swaymsg", "-q", "focus output HEADLESS-1"])
        peer_env = {**env, "MORPHEUS_APPIMAGE_PEER_CONTROL": str(control)}
        phase("peer")
        peer = owner.start("peer", [str(peer_binary), PEER_TEST, "--ignored", "--exact", "--nocapture", "--test-threads=1"], peer_env)
        owner.wait_for(lambda: fact(control, "peer_ready"), seconds=150, watch=[peer, keyring, bus])
        app_env = dict(env)
        if bindings:
            app_env.update(LD_DEBUG="bindings", LD_BIND_NOW="1")
        phase("launch")
        app = owner.start("app", [str(artifact)], app_env)
        ui = InstalledUI(app, nonce)
        pid = ui.wait_for(ui.application_pid, seconds=30)
        phase("identity")
        identity = RuntimeIdentity(pid, app, nonce, root / "tmp", appdir)
        owned_window_on_first_output(json.loads(run("capture-window", ["swaymsg", "-t", "get_tree", "-r"])), pid)
        phase("login")
        ui.enter("Homeserver URL", account["homeserver_url"])
        ui.enter("Matrix ID or username", account["username"])
        ui.enter("Password", account["password"], password=True)
        ui.press("Sign in to Matrix")
        ui.wait_for(lambda: ui.find_control("Open room " + room_name), seconds=90)
        ui.press("Open room " + room_name)
        ui.press("Open call stage")
        phase("join")
        ui.press("Join native call")
        owner.wait_for(lambda: fact(control, "exact_sender_joined"), seconds=90, watch=[app, peer])
        def share(*, reopened=False):
            ui.press("Choose a display or window to share")
            ui.press("Share display Choose a display in the system dialog…")
            if reopened:
                # A new real pixel change on the reopened source; this does
                # not claim untouched static-source reopening is verified.
                run("capture-yellow", ["swaymsg", "-q", "output HEADLESS-2 bg #f0f020 solid_color"])
            ui.wait("Stop screen sharing")
        phase("static_frame")
        share()
        request(control, "static")
        owner.wait_for(lambda: fact(control, "static_frame"), seconds=50, watch=[app, peer])
        phase("changed_frame")
        run("capture-green", ["swaymsg", "-q", "output HEADLESS-2 bg #20f020 solid_color"])
        request(control, "changed")
        owner.wait_for(lambda: fact(control, "changed_frame"), seconds=50, watch=[app, peer])
        phase("screen_stopped")
        ui.press("Stop screen sharing")
        request(control, "stopped")
        owner.wait_for(lambda: fact(control, "screen_stopped"), watch=[app, peer])
        phase("reopened_frame")
        share(reopened=True)
        request(control, "reopened")
        owner.wait_for(lambda: fact(control, "reopened_frame"), seconds=50, watch=[app, peer])
        phase("changed_after_reopen")
        run("capture-magenta", ["swaymsg", "-q", "output HEADLESS-2 bg #f020f0 solid_color"])
        request(control, "changed_again")
        owner.wait_for(lambda: fact(control, "changed_after_reopen"), seconds=50, watch=[app, peer])
        phase("source_loss")
        run("capture-remove-source", ["swaymsg", "-q", "output HEADLESS-2 disable"])
        request(control, "source_lost")
        owner.wait_for(lambda: fact(control, "source_loss_stopped_track"), watch=[app, peer])
        ui.wait("Choose a display or window to share")
        identity.check()
        phase("app_left")
        ui.press("Leave native call")
        request(control, "left")
        owner.wait_for(lambda: fact(control, "complete"), seconds=75, watch=[app])
        peer_out, _ = owner.finish("peer", peer)
        peer_proof(peer_out)
        require(all(fact(control, name) for name in FACTS))
        identity.check()
        if bindings:
            identity.attest_aliases(client.read_regular(root / "app.stderr.log", 8 * 1024 * 1024))
        phase("app_close")
        run("capture-close-window", ["swaymsg", "-q", "[pid=" + str(pid) + "] kill"])
        _, app_trace = owner.finish("app", app, seconds=30)
        result = {"mode": "eager_bindings" if bindings else "ordinary", "actual_gui_capture": True,
                  "peer": {name: True for name in FACTS}, "untouched_static_reopen_verified": False,
                  "executable_sha256": identity.executable_sha256, "pipewire_sha256": identity.library_sha256}
        if bindings:
            phase("bindings")
            dynamic = run("capture-imports", ["readelf", "--dyn-syms", "--wide", str(appdir / "usr/bin/morpheus-app")], limit=1024 * 1024)
            result["binding_evidence"] = identity.binding_facts(dynamic, app_trace, complete=True)
        phase("complete")
    except BaseException as error:
        body_failed = True
        if diagnostics is not None:
            diagnostics.failed(error, control)
        raise
    finally:
        cleanup_failed = False
        if identity is not None:
            try:
                identity.close()
            except BaseException:
                cleanup_failed = True
        try:
            owner.close()
        except BaseException:
            cleanup_failed = True
        if diagnostics is not None:
            diagnostics.cleanup(not cleanup_failed)
        if cleanup_failed and not body_failed:
            raise CaptureFailure()
    return result


def run(runner, root, artifact, appdir, build_record, environment, diagnostics=None):
    from appimage_capture_worker import run_isolated
    containment.assert_contained()
    require(build_record["appimage_sha256"] == hashlib.sha256(client.read_regular(artifact, 512 * 1024 * 1024)).hexdigest())
    environment = {**environment, "MORPHEUS_MATRIX_TEST_HOME": str(root / "matrix-fixture")}
    docker = executable("docker")
    def inventory(kind, names):
        args = [docker, kind, "ls"] + (["--all"] if kind == "container" else [])
        args += ["--format", "{{.Names}}" if kind == "container" else "{{.Name}}"]
        output = runner.run("capture-inventory", args, cwd=ROOT, env=environment, timeout=30, output_limit=65536)
        require(not set(output.decode().splitlines()).intersection(names))
    # Cleanup ownership is acquired only after every bounded query succeeds.
    inventory("container", CONTAINERS)
    inventory("volume", VOLUMES)
    inventory("network", (NETWORK,))
    def script(name, command):
        return runner.run("capture-" + name.replace("local_matrix_", "") + "-" + command,
                          [str(Path(sys.executable).resolve()), str(ROOT / "scripts" / (name + ".py")), command],
                          cwd=ROOT, env=environment, timeout=120 if command == "down" else 300, output_limit=1024 * 1024)
    # The outer namespace owner can perform the same idempotent cleanup after
    # timeout/SIGKILL. Ownership exists only after all absence queries passed.
    client.write_new(root / "fixture-owned.json", b'{"owned":true}\n')
    outcomes = []
    body_failed = False
    try:
        script("local_matrix_env", "up")
        script("local_matrix_rtc", "up")
        script("local_matrix_rtc", "probe")
        cargo = shutil.which("cargo", path=environment.get("PATH", ""))
        require(cargo is not None)
        peer = compile_peer(runner, root, environment, cargo)
        # Reuse one identical fixture-backend build across both fresh sessions.
        backend = prepare_backend(root, lambda name, argv: runner.run("capture-" + name, [executable(argv[0]), *argv[1:]],
                                  cwd=ROOT, env=environment, timeout=180, output_limit=8 * 1024 * 1024))
        fixture_hashes = set()
        for name, bindings in (("ordinary", False), ("bindings", True)):
            fixture = fixture_path(script("local_matrix_rtc", "rtc-fixture"), root)
            fixture_hash = hashlib.sha256(client.read_regular(fixture, 65536)).hexdigest()
            require(fixture_hash not in fixture_hashes)
            fixture_hashes.add(fixture_hash)
            outcomes.append(run_isolated(root / name, artifact, appdir, peer, backend, fixture, environment, bindings=bindings, diagnostics=diagnostics))
    except BaseException as error:
        body_failed = True
        if diagnostics is not None:
            diagnostics.failed(error)
        raise
    finally:
        failed = False
        for name in ("local_matrix_rtc", "local_matrix_env"):
            try:
                script(name, "down")
            except BaseException:
                failed = True
        try:
            inventory("container", CONTAINERS)
            inventory("network", (NETWORK,))
        except BaseException:
            failed = True
        if diagnostics is not None:
            diagnostics.cleanup(not failed)
        if failed and not body_failed:
            raise CaptureFailure()
    return {"installed_capture_passed": True, "runs": outcomes, "system_gate_replaced": False,
            "fixture_containers_and_network_removed": True, "fixture_volumes_retained": True}


def cleanup(runner, root, environment):
    """Idempotent host-Docker teardown after namespace termination, not wipe."""
    require(client.read_regular(root / "fixture-owned.json", 64) == b'{"owned":true}\n')
    env = {**environment, "MORPHEUS_MATRIX_TEST_HOME": str(root / "matrix-fixture")}
    failed = False
    for name in ("local_matrix_rtc", "local_matrix_env"):
        try:
            runner.run("cleanup-" + name.replace("local_matrix_", ""),
                       [str(Path(sys.executable).resolve()), str(ROOT / "scripts" / (name + ".py")), "down"],
                       cwd=ROOT, env=env, timeout=120, output_limit=1024 * 1024)
        except BaseException:
            failed = True
    for kind, names in (("container", CONTAINERS), ("network", (NETWORK,))):
        try:
            arguments = [executable("docker"), kind, "ls"] + (["--all"] if kind == "container" else [])
            arguments += ["--format", "{{.Names}}" if kind == "container" else "{{.Name}}"]
            result = runner.run("cleanup-inventory", arguments, cwd=ROOT, env=env, timeout=30, output_limit=65536)
            require(not set(result.decode().splitlines()).intersection(names))
        except BaseException:
            failed = True
    require(not failed)
