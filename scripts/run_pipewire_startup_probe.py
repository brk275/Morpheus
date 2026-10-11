#!/usr/bin/env python3
"""Explicit free-runner comparison of system, control and patched capture clients.

Every capture uses a fresh PID namespace and private desktop. Private libraries
are passed to one test ELF through the verified system loader's --library-path;
no installation, loader environment override, or artifact upload occurs. A
patched-client pass never substitutes for the system-library acceptance gate.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pipewire_startup_probe as preparation
import run_wayland_acceptance as wayland
from element_process_namespace import assert_contained, containment_check, namespace_command

REPO = Path(__file__).resolve().parents[1]
VARIANTS = ("system", "control", "patched")
MAX_BINARY = 256 * 1024 * 1024


def runtime_environment(environment):
    # Keep the pinned Cargo/toolchain path, but remove inherited loader and
    # PipeWire overrides before any services, compiler or test process starts.
    return {key: value for key, value in environment.items()
            if not key.startswith(("LD_", "DYLD_", "GLIBC_", "GCONV_", "PIPEWIRE_", "SPA_"))}


def write_json(path: Path, value):
    with path.open("x", encoding="ascii") as output:
        json.dump(value, output, sort_keys=True)
    path.chmod(0o600)


def read_json(path: Path):
    metadata = path.lstat()
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
            or metadata.st_mode & 0o077 or path.resolve(strict=True) != path):
        raise RuntimeError("private diagnostic record rejected")
    return json.loads(preparation.read_regular(path, 64 * 1024, "paired_provenance"))


def private_binding_facts(path: Path, variant: str) -> dict:
    expected = {"schema": 1, "variant": variant, "checked": 33, "private_probe": 33,
                "digest_verified": True, "build_id_verified": True, "mapped_file_verified": True}
    try:
        actual = json.loads(preparation.read_regular(path, 512, "paired_provenance"))
        valid = (variant in ("control", "patched") and isinstance(actual, dict)
                 and set(actual) == set(expected)
                 and all(type(actual[key]) is type(value) and actual[key] == value
                         for key, value in expected.items()))
    except (OSError, ValueError, preparation.ProbeFailure):
        valid = False
    return {"status": "valid", **expected} if valid else {"status": "invalid"}


def public_provenance(record):
    """Retain immutable identities after private artifacts are deleted."""
    def hex_value(value, minimum=64, maximum=64):
        if (type(value) is not str or not minimum <= len(value) <= maximum
                or len(value) % 2 or not re.fullmatch(r"[0-9a-f]+", value)):
            raise RuntimeError("diagnostic provenance digest rejected")
        return value
    return {"source_version": preparation.VERSION, "upstream_commit": preparation.COMMIT,
            "test_elf_sha256": hex_value(record["binary_sha256"]),
            "loader_sha256": hex_value(record["provenance"]["loader_sha256"]),
            "configuration_sha256": hex_value(record["provenance"]["configuration_sha256"]),
            "build_recipe_sha256": hex_value(record["provenance"]["build_recipe_sha256"]),
            **{variant: {"sha256": hex_value(record[variant]["sha256"]),
                         "build_id": hex_value(record[variant]["build_id"], 16, 128)}
               for variant in ("control", "patched")}}


def service_library_facts(services) -> bool:
    """All three PipeWire clients stay mapped to ordinary root-owned libraries.

    Only a boolean escapes; maps contain private addresses and process paths.
    The frontend may not use PipeWire itself, but any mapping it has must also
    be the system library. Leaders remain unreaped throughout the fixture.
    """
    try:
        for role in ("pipewire", "wireplumber", "portal-wlr", "portal"):
            process = services.get(role)
            if process is None or wayland.peek_returncode(process) is not None:
                return False
            with Path(f"/proc/{process.pid}/maps").open("rb") as source:
                maps = source.read(2 * 1024 * 1024 + 1)
            if len(maps) > 2 * 1024 * 1024:
                return False
            libraries = set()
            for line in maps.decode("utf-8", errors="strict").splitlines():
                fields = line.split(maxsplit=5)
                if len(fields) != 6 or "libpipewire-0.3.so" not in fields[5]:
                    continue
                path = Path(fields[5]).resolve(strict=True)
                metadata = path.stat()
                major, minor = fields[3].split(":")
                if (path.parent != Path("/usr/lib/x86_64-linux-gnu")
                        or not re.fullmatch(r"libpipewire-0\.3\.so\.[0-9.]+", path.name)
                        or not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0
                        or metadata.st_mode & 0o022 or int(fields[4]) != metadata.st_ino
                        or (int(major, 16), int(minor, 16)) != (os.major(metadata.st_dev), os.minor(metadata.st_dev))):
                    return False
                libraries.add(path)
            if len(libraries) != (0 if role == "portal" and not libraries else 1):
                return False
        return True
    except (OSError, ValueError, RuntimeError):
        return False


class PrivateClient:
    def __init__(self, record, variant):
        if variant not in ("control", "patched"):
            raise RuntimeError("diagnostic variant rejected")
        self.variant = variant
        self.artifact_path = Path(record[f"{variant}_metadata_path"])
        self.record = record

    def command(self):
        artifact = read_json(self.artifact_path)
        binary, loader = Path(self.record["binary"]), Path(self.record["loader_path"])
        if (artifact != self.record[self.variant] or artifact["variant"] != self.variant
                or hashlib.sha256(preparation.read_regular(binary, MAX_BINARY, "paired_provenance")).hexdigest() != self.record["binary_sha256"]
                or hashlib.sha256(preparation.read_regular(loader, 4 * 1024 * 1024, "loader")).hexdigest() != self.record["provenance"]["loader_sha256"]):
            raise RuntimeError("diagnostic executable identity changed")
        metadata = loader.lstat()
        if (loader != Path("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2")
                or loader.resolve(strict=True) != loader or not stat.S_ISREG(metadata.st_mode)
                or metadata.st_uid != 0 or metadata.st_mode & 0o022):
            raise RuntimeError("diagnostic loader identity rejected")
        library = Path(artifact["library_path"])
        if (library.parent != self.artifact_path.parent or library.name != preparation.LIBRARY
                or library.resolve(strict=True) != library):
            raise RuntimeError("diagnostic artifact path rejected")
        return [str(loader), "--library-path", str(library.parent), str(binary)]


def prepare(root: Path):
    assert_contained()
    counter = 0

    def command(stage, argv, *, cwd, env, timeout, output_limit):
        nonlocal counter
        counter += 1
        path = root / f"command-{counter}.log"
        with wayland.CappedPrivateLog(path, limit=output_limit) as sink:
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                sink.attach(process.stdout)
                status = wayland.wait_owned_process(process, timeout)
            finally:
                wayland.stop_owned_groups([process])
                sink.finish()
            if status or sink.failed or sink.truncated or not sink.drained:
                raise preparation.ProbeFailure(stage, "command", status)
            return path.read_bytes()

    clients = preparation.prepare_clients(root, command)
    output = command("control_build", ["cargo", "test", "--locked", "-p", "morpheus-rtc",
                     "--features", "native-devices", "--test", "wayland_capture", "--no-run",
                     "--message-format=json"], cwd=REPO, env=runtime_environment(dict(os.environ)),
                     timeout=900, output_limit=8 * 1024 * 1024)
    binaries = []
    for line in output.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if (item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "wayland_capture"
                and item.get("profile", {}).get("test") is True and item.get("executable")):
            binaries.append(Path(item["executable"]).resolve(strict=True))
    if len(binaries) != 1 or REPO / "target" not in binaries[0].parents:
        raise RuntimeError("diagnostic test executable was not discovered")
    binary = binaries[0]
    # The separately named provenance regressions must execute too: --ignored
    # capture alone would silently skip them. No native tests run on the host.
    tests = command("control_build", [str(binary), "pipewire_probe::tests::", "--test-threads=1"],
                    cwd=REPO, env=runtime_environment(dict(os.environ)), timeout=60, output_limit=64 * 1024)
    if not re.search(rb"^test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored;", tests, re.MULTILINE):
        raise RuntimeError("diagnostic provenance regressions did not execute")
    record = {"schema": 1, "control": clients.control, "patched": clients.patched,
              "control_metadata_path": str(clients.control_metadata_path),
              "patched_metadata_path": str(clients.patched_metadata_path),
              "loader_path": str(clients.loader_path), "provenance": clients.provenance,
              "binary": str(binary), "binary_sha256": hashlib.sha256(
                  preparation.read_regular(binary, MAX_BINARY, "paired_provenance")).hexdigest()}
    write_json(root / "prepared.json", record)
    print("PipeWire startup probe preparation: " + json.dumps(clients.facts(), sort_keys=True))
    print("PipeWire startup probe artifact identities: " + json.dumps(public_provenance(record), sort_keys=True))


def capture(root: Path, prepared: Path, variant: str):
    record = read_json(prepared)
    client = None if variant == "system" else PrivateClient(record, variant)
    return wayland.session(root, private_client=client, record_probe=True)


def collect_outcome(root: Path, status: int):
    value = read_json(root / "probe-outcome.json")
    keys = {"capture_attempted", "capture_completed", "bindings_verified", "capture_passed",
            "cleanup_passed", "services_system", "trace_complete"}
    if (set(value) != keys | {"schema"} or type(value["schema"]) is not int or value["schema"] != 1
            or any(type(value[key]) is not bool for key in keys)
            or not value["capture_attempted"] or not value["cleanup_passed"]
            or not value["capture_completed"] or not value["bindings_verified"]
            or not value["services_system"] or status not in (0, 1)
            or value["capture_passed"] != (status == 0)):
        raise RuntimeError("diagnostic session was not safely completed")
    return value


def run():
    preparation.check_eligibility(dict(os.environ))
    clean = runtime_environment(dict(os.environ))
    for key in set(os.environ) - set(clean):
        os.environ.pop(key, None)
    with tempfile.TemporaryDirectory(prefix="morpheus-pipewire-probe-") as directory:
        root = Path(directory).resolve()
        root.chmod(0o700)
        containment_check(root / "containment-probe")
        build = root / "preparation"
        build.mkdir(mode=0o700)
        script = str(Path(__file__).resolve())
        invocation = namespace_command([sys.executable, script, "--prepare", str(build)], root / "build-namespace", clean)
        if wayland.run_session(invocation, clean, timeout=1860):
            raise RuntimeError("diagnostic preparation failed")
        prepared = build / "prepared.json"
        results = {}
        for variant in VARIANTS:
            session_root = root / variant
            session_root.mkdir(mode=0o700)
            wayland.configs(session_root)
            env = wayland.private_environment(session_root)
            invocation = namespace_command([sys.executable, script, "--contained", str(session_root),
                "--prepared", str(prepared), "--variant", variant], root / f"namespace-{variant}", env)
            print("PipeWire startup comparison: " + variant, flush=True)
            status = wayland.run_session(invocation, env)
            results[variant] = collect_outcome(session_root, status)
        print("PipeWire startup comparison results: " + json.dumps(results, sort_keys=True))
        # A diagnostic can be fully collected while its system gate is still
        # failing. Preserve that failure in workflow status, without claiming
        # that A/B alone proves a particular lost wake or root cause.
        return 0 if all(item["capture_passed"] and item["trace_complete"] for item in results.values()) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepare", type=Path)
    parser.add_argument("--contained", type=Path)
    parser.add_argument("--inside", type=Path)
    parser.add_argument("--prepared", type=Path)
    parser.add_argument("--variant", choices=VARIANTS)
    args = parser.parse_args()
    if args.prepare:
        prepare(args.prepare)
        return 0
    if args.contained:
        assert_contained()
        os.execv("/usr/bin/dbus-run-session", ["/usr/bin/dbus-run-session", "--", sys.executable,
            str(Path(__file__).resolve()), "--inside", str(args.contained), "--prepared", str(args.prepared),
            "--variant", args.variant])
    if args.inside:
        return capture(args.inside, args.prepared, args.variant)
    return run()


if __name__ == "__main__":
    def interrupted(_signal, _frame):
        raise RuntimeError("diagnostic interrupted")
    signal.signal(signal.SIGTERM, interrupted)
    try:
        raise SystemExit(main())
    except preparation.ProbeFailure as error:
        print("PipeWire startup probe failure: " + json.dumps(error.facts(), sort_keys=True), file=sys.stderr)
        raise SystemExit(1)
    except Exception:
        print("PipeWire startup probe failed; no raw command or service output published.", file=sys.stderr)
        raise SystemExit(1)
