#!/usr/bin/env python3
"""Owner/manual Linux WebRTC DATA staging and separately admitted native build.

All invoked recipe bytes are fixed below. No source/tool downloaded by staging
is executed in the stage phase. A later build requires the actual preserved
handoff, its immutable cache identity/digest, and independent build gate files.
No installer publication, release acceptance or paid storage is granted here.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import copy
import gzip
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import re
import shutil
import ssl
import stat
import sys
import tarfile
import time
import types
import urllib.parse

REPOSITORY = "brk275/Morpheus"
MAX_CACHE_CAPACITY = 10_000_000_000
MAX_HANDOFF = 4_000_000_000
CACHE_RESERVE = 1_000_000_000
MAX_EXPANDED = 32 * 1024 * 1024 * 1024
MAX_MEMBERS = 1_000_000
MAX_JSON = 128 * 1024 * 1024
INPUT_PLAN = "2489cdd737218109dd56378f9b7049aeda68df3798dc670c1899cb1e90a9d32c"
PATCH_MANIFEST = "e97be792680220ec3d21f39a0a950599a3266fd3041e2a8d5027df36d946b714"
SECURITY_PLAN = "a9f2220dfb0e527690ace518911e3e92c12124ab8c37e6cb391a47c89f19c080"
PYTHON_PLAN = "b3cd3a5066338da7fa450fe48580d987dbb88f90d9497dc2106802ab989c0aea"
INSTRUMENTED_PLAN = "927a95029b9ddc8f4b4cce20f07975505b343260d913b9c3488bc6f6182ad0f9"
HELPERS = {
    "git_source_stager.py": "5debe4d363c339de898b965dee15660867b9a0bc4a96380f434bf19deab20c7b",
    "source_importer.py": "20d92bfd183f60be2011da16cc7a6916158dfbc7e76fcedc126cfe53210cc5e5",
    "linux_stager.py": "f3a908bc20170ee01f2643169ce60397627981d86de11ffaedf03dd8ea9e1c8c",
    "python_stager.py": "4de9a016a988535b7576f2e0d7561afa290529dae7b1eb267bb8382956d49d02",
    "instrumented_stager.py": "8903aef2de2ea5a7778ad4ba21047909ad7595d3e1359f151829e489910fc84f",
    "linux_materials.py": "acca8d027e3e2f1ce3a38181345b522a2906e5eeb1a1fc39cd3c1d2ff3bf0b51",
}
BUILD_RECIPE = "934991862c91f25a50a161c22e54858d23dc9d02daf75be88ddd7d32763908e1"
HEX = re.compile(r"[0-9a-f]{64}\Z")


class Refused(Exception):
    pass


def need(value):
    if not value:
        raise Refused()


def read(path, limit=MAX_JSON):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and before.st_size <= limit)
        data = bytearray()
        while len(data) <= before.st_size:
            part = os.read(fd, min(1024 * 1024, before.st_size + 1 - len(data)))
            if not part:
                break
            data.extend(part)
        after = os.fstat(fd)
        identity = lambda st: (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns)
        need(len(data) == before.st_size and identity(before) == identity(after)
             == identity(path.lstat()))
        return bytes(data)
    finally:
        os.close(fd)


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def host_fact(path):
    # System tools are root-owned and may have distribution hardlink aliases.
    # They are not private staged files. No executable is run by this reader.
    need(path.is_absolute() and path == path.resolve(strict=True))
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        need(stat.S_ISREG(before.st_mode) and before.st_uid == 0
             and not before.st_mode & 0o022 and before.st_size <= 128 * 1024 * 1024)
        h, count = hashlib.sha256(), 0
        while True:
            block = os.read(fd, 1024 * 1024)
            if not block: break
            count += len(block); need(count <= before.st_size); h.update(block)
        identity = lambda st: (st.st_dev, st.st_ino, st.st_size, st.st_mode, st.st_uid,
                               st.st_mtime_ns, st.st_ctime_ns)
        need(count == before.st_size and identity(before) == identity(os.fstat(fd))
             == identity(path.lstat()))
        return {"bytes": count, "sha256": h.hexdigest()}
    finally:
        os.close(fd)


def load(path, expected):
    need(type(expected) is str and HEX.fullmatch(expected))
    raw = read(path)
    need(sha(raw) == expected)
    def pairs(rows):
        out = {}
        for key, value in rows:
            need(key not in out)
            out[key] = value
        return out
    return json.loads(raw, object_pairs_hook=pairs)


def module(name, expected=None):
    path = Path(__file__).resolve().parent / "native_webrtc" / name
    raw = read(path, 256 * 1024)
    need(sha(raw) == (expected or HELPERS[name]))
    # Execute only the source-reviewed, exact in-repository recipe bytes on
    # the admitted fresh runner. Never import a module selected by a manifest.
    value = types.ModuleType("morpheus_recipe_" + name.replace(".", "_"))
    value.__file__ = str(path)
    sys.modules[value.__name__] = value
    exec(compile(raw, str(path), "exec"), value.__dict__)
    return value


# These are code-owned phases from the exact pinned helpers, never manifest or argv values.
_TOOL_PHASES = frozenset({"admission", "receipt"} | {
    prefix + name for prefix in ("download_", "stage_")
    for name in ("gn", "ninja", "clang", "rust", "sysroot-amd64", "sysroot-i386", "llvmobjdump")
})
_HELPER_PHASES = {
    "linux_stager.py": _TOOL_PHASES,
    "python_stager.py": _TOOL_PHASES | {"stage_python_data"},
    "instrumented_stager.py": _TOOL_PHASES | {
        prefix + name for prefix in ("download_", "stage_")
        for name in ("msan-chained-origins-noble-lib", "msan-no-origins-noble-lib")
    },
}


def helper_failure_context(value, error):
    helper, phase, lines = "unknown", "unknown", []
    if type(value) is types.ModuleType:
        namespace = value.__dict__
        for name in HELPERS:
            module_name = "morpheus_recipe_" + name.replace(".", "_")
            path = str(Path(__file__).resolve().parent / "native_webrtc" / name)
            if (namespace.get("__name__") == module_name and sys.modules.get(module_name) is value
                    and namespace.get("__file__") == path):
                helper = name
                selected = namespace.get("PHASE")
                if type(selected) is str and selected in _HELPER_PHASES.get(name, ()):
                    phase = selected
                frame = error.__traceback__
                # Only fixed helper frames: no traceback formatting, filenames, function names or locals.
                for _ in range(64):
                    if frame is None:
                        break
                    if (frame.tb_frame.f_globals is namespace and frame.tb_frame.f_code.co_filename == path
                            and type(frame.tb_lineno) is int and 0 < frame.tb_lineno <= 99999):
                        lines = (lines + [frame.tb_lineno])[-3:]
                    frame = frame.tb_next
                if frame is not None:
                    lines = []
                break
    return {"helper": helper, "phase": phase, "lines": lines}


def cli(value, argv):
    original = sys.argv
    sys.argv = [value.__file__, *argv]
    try:
        result = value.main()
        need(result in (None, 0))
    except BaseException as error:
        # Imported main() bypasses the helper's __main__ closed error handler.
        # Emit bounded code context only, then preserve the original failure and traceback.
        try:
            print("MORPHEUS_NATIVE_HELPER_FAILED " + json.dumps(
                helper_failure_context(value, error), sort_keys=True, separators=(",", ":")), file=sys.stderr)
        except BaseException:
            pass  # Diagnostic output must never replace the original helper failure.
        raise
    finally:
        sys.argv = original


def api(path):
    need((path == "/repos/" + REPOSITORY or path.startswith("/repos/" + REPOSITORY + "/"))
         and "\r" not in path and "\n" not in path)
    token = os.environ.get("NATIVE_READ_TOKEN", "")
    need(bool(token))
    connection = http.client.HTTPSConnection("api.github.com", 443, timeout=30,
                                           context=ssl.create_default_context())
    try:
        connection.request("GET", path, headers={"Accept": "application/vnd.github+json",
            "Authorization": "Bearer " + token, "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "Morpheus-native-source-producer"})
        response = connection.getresponse()
        need(response.status == 200 and response.getheader("Location") is None
             and response.getheader("Content-Encoding") in (None, "identity"))
        raw = response.read(1024 * 1024 + 1)
        need(len(raw) <= 1024 * 1024)
        return json.loads(raw)
    finally:
        connection.close()


def runner():
    need(os.name == "posix" and os.uname().sysname == "Linux" and os.uname().machine == "x86_64"
         and os.environ.get("GITHUB_ACTIONS") == "true"
         and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
         and os.environ.get("RUNNER_OS") == "Linux" and os.environ.get("RUNNER_ARCH") == "X64"
         and os.environ.get("GITHUB_REPOSITORY") == REPOSITORY
         and os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
         and os.environ.get("GITHUB_REF") == "refs/heads/main"
         and os.environ.get("GITHUB_ACTOR") == "brk275"
         and os.environ.get("GITHUB_TRIGGERING_ACTOR") == "brk275")
    # Do not inherit a writable/privileged job or a private repository context.
    repository = api("/repos/" + REPOSITORY)
    need(repository["full_name"] == REPOSITORY)
    need(repository["private"] is False and repository["default_branch"] == "main")
    temp = Path(os.environ["RUNNER_TEMP"])
    need(temp.is_absolute() and temp == temp.resolve(strict=True))
    return temp


def flat_cache_response(raw):
    # This response contract is flat. Decode only JSON strings and literal
    # booleans, never ask a general decoder to allocate nested input objects.
    need(type(raw) is bytes and 0 < len(raw) <= 65536)
    text = raw.decode("utf-8")
    decoder, position, result = json.JSONDecoder(), 0, {}
    def spaces():
        nonlocal position
        while position < len(text) and text[position] in " \t\r\n": position += 1
    def string():
        nonlocal position
        need(position < len(text) and text[position] == '"')
        value, end = decoder.raw_decode(text, position)
        need(type(value) is str and len(value) <= 16384)
        position = end
        return value
    spaces(); need(position < len(text) and text[position] == "{"); position += 1
    while True:
        spaces(); key = string(); spaces()
        need(key not in result and key in {"ok", "matched_key", "signed_download_url"}
             and position < len(text) and text[position] == ":")
        position += 1; spaces()
        if text.startswith("true", position): value = True; position += 4
        elif text.startswith("false", position): value = False; position += 5
        else: value = string()
        result[key] = value; spaces()
        need(position < len(text) and text[position] in ",}")
        token = text[position]; position += 1
        if token == "}": break
    spaces(); need(position == len(text) and set(result) ==
                   {"ok", "matched_key", "signed_download_url"}
                   and result["ok"] is True and type(result["matched_key"]) is str
                   and type(result["signed_download_url"]) is str)
    return result


def signed_blob_url(value):
    need(type(value) is str and 0 < len(value) <= 16384
         and all(33 <= ord(c) < 127 for c in value) and "\\" not in value)
    signed = urllib.parse.urlsplit(value)
    need(signed.scheme == "https" and signed.hostname is not None
         and re.fullmatch(r"[a-z0-9]{3,24}\.blob\.core\.windows\.net", signed.hostname)
         and signed.netloc in (signed.hostname, signed.hostname + ":443")
         and signed.port in (None, 443) and not signed.username and not signed.password
         and not signed.fragment and signed.path.startswith("/") and signed.query)
    return signed


def cache_download(args, temp, material, plan, *, mac_target=None):
    """Exact cache lookup; never invoke the action's prefix restore or tar."""
    record = cache_identity(args, material)
    origin = urllib.parse.urlsplit(os.environ["ACTIONS_RESULTS_URL"])
    need(origin.scheme == "https" and origin.hostname == "results-receiver.actions.githubusercontent.com"
         and origin.port in (None, 443) and not origin.username and not origin.password
         and origin.netloc in (origin.hostname, origin.hostname + ":443")
         and origin.path in ("", "/") and not origin.query and not origin.fragment)
    connection = http.client.HTTPSConnection(origin.hostname, 443, timeout=30,
                                           context=ssl.create_default_context())
    try:
        connection.request("POST", "/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL",
            body=json.dumps({"key": args.cache_key, "version": record["version"],
                             "restore_keys": []}, separators=(",", ":")).encode(),
            headers={"Authorization": "Bearer " + os.environ["ACTIONS_RUNTIME_TOKEN"],
                     "Content-Type": "application/json", "Accept": "application/json"})
        response = connection.getresponse()
        need(response.status == 200 and response.getheader("Location") is None
             and response.getheader("Content-Type") == "application/json"
             and response.getheader("Content-Encoding") in (None, "identity"))
        raw = response.read(65537); need(len(raw) <= 65536)
        value = flat_cache_response(raw)
        need(value["matched_key"] == args.cache_key)
        signed = signed_blob_url(value["signed_download_url"])
    finally:
        connection.close()
    directory = temp / "native-webrtc-cache-intake"
    need(not os.path.lexists(directory)); directory.mkdir(mode=0o700)
    archive = directory / "cache.bin"
    connection = http.client.HTTPSConnection(signed.hostname, 443, timeout=30,
                                           context=ssl.create_default_context())
    try:
        # The runtime token is sent only to the exact results service above.
        # The storage read is credential-free except for its private signed URL.
        connection.request("GET", urllib.parse.urlunsplit(("", "", signed.path, signed.query, "")),
                           headers={"Accept-Encoding": "identity"})
        response = connection.getresponse()
        need(response.status == 200 and response.getheader("Location") is None
             and response.getheader("Content-Encoding") in (None, "identity")
             and response.getheader("Content-Length") == str(record["size_in_bytes"])
             and response.getheader("Transfer-Encoding") is None)
        remaining = record["size_in_bytes"]
        deadline = time.monotonic() + 300
        with open(archive, "xb") as out:
            os.chmod(archive, 0o600)
            while remaining:
                need(time.monotonic() < deadline)
                block = response.read(min(1024 * 1024, remaining)); need(bool(block))
                out.write(block); remaining -= len(block)
            need(response.read(1) == b""); out.flush(); os.fsync(out.fileno())
    finally:
        connection.close()
    outer = directory / "outer.tar"
    with open(archive, "rb") as stream:
        magic = stream.read(4)
    if magic == b"\x28\xb5\x2f\xfd":
        tool = plan["host_zstd"]
        need(set(tool) == {"path", "bytes", "sha256"})
        executable = Path(tool["path"])
        if mac_target is None:
            need(executable == Path("/usr/bin/zstd").resolve(strict=True))
        else:
            need(mac_target in ("mac-arm64", "mac-x64") and os.uname().sysname == "Darwin"
                 and os.uname().machine == ("arm64" if mac_target == "mac-arm64" else "x86_64")
                 and (executable.is_relative_to(Path("/usr/local"))
                      or executable.is_relative_to(Path("/opt/homebrew"))))
            need(executable == executable.resolve(strict=True))
        need(host_fact(executable) == {key: tool[key] for key in ("bytes", "sha256")})
        home = directory / "home"; home.mkdir(mode=0o700)
        # Reuse the existing original owned-process/pipe/session implementation.
        # This is an independently bound hosted DATA decompressor, not a native
        # compiler, source hook or downloaded executable.
        source = module("git_source_stager.py")
        source.run_git_owned([str(executable), "-d", "-c", "--", str(archive)], directory,
            {"HOME": str(home), "PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C"},
            1800, outer, directory / "decompress.stderr", MAX_HANDOFF + 16 * 1024 * 1024, None)
    else:
        need(magic[:2] == b"\x1f\x8b")
        expand_gzip(archive, outer, MAX_HANDOFF + 16 * 1024 * 1024)
    destination = temp / "native-webrtc-handoff"
    need(not os.path.lexists(destination)); destination.mkdir(mode=0o700)
    expected_name = os.path.relpath(str(temp / "native-webrtc-handoff/handoff.tar.gz"),
                                   os.environ["GITHUB_WORKSPACE"]).replace(os.sep, "/")
    extracted = False
    with bounded_tar(outer) as source:
        for member in source:
            need(not extracted and member.name == expected_name and member.isfile()
                 and not member.issparse() and 0 < member.size == args.handoff_bytes <= MAX_HANDOFF
                 and not source.pax_headers and set(member.pax_headers) <= {"path", "mtime", "atime", "ctime"})
            for key, value in member.pax_headers.items():
                need(value == expected_name if key == "path" else
                     type(value) is str and re.fullmatch(r"-?[0-9]{1,20}(?:\.[0-9]{1,12})?", value))
            stream = source.extractfile(member); need(stream is not None)
            with outer_payload(source):
                with open(destination / "handoff.tar.gz", "xb") as out:
                    os.chmod(destination / "handoff.tar.gz", 0o600)
                    remaining = member.size
                    while remaining:
                        block = stream.read(min(1024 * 1024, remaining)); need(bool(block))
                        out.write(block); remaining -= len(block)
                    need(stream.read(1) == b""); out.flush(); os.fsync(out.fileno())
            extracted = True
        # tar's logical end cannot hide a second archive or arbitrary trailing
        # bytes. GNU save padding is accepted only within the same parser cap.
        while True:
            tail = source.fileobj.read(512)
            if not tail: break
            need(not any(tail))
    need(extracted and material.file_fact(destination / "handoff.tar.gz") ==
         {"sha256": args.handoff_sha256, "bytes": args.handoff_bytes}
         and cache_identity(args, material) == record)
    return record


class BoundedWriter:
    def __init__(self, stream, maximum):
        self.stream, self.maximum, self.position = stream, maximum, 0

    def write(self, data):
        need(self.position + len(data) <= self.maximum)
        count = self.stream.write(data)
        need(count == len(data)); self.position += count
        return count

    def tell(self):
        return self.position

    def flush(self):
        self.stream.flush()


class BoundedTarFile(io.FileIO):
    # Reject oversized PAX/GNU metadata reads before tarfile can allocate them.
    # File bodies are streamed by callers in bounded blocks; no extractall.
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.parser_bytes, self.payload = 0, False

    def read(self, size=-1):
        need(type(size) is int and 0 <= size <= 1024 * 1024)
        if not self.payload:
            need(self.parser_bytes + size <= 65536)
            self.parser_bytes += size
        return super().read(size)


@contextmanager
def outer_payload(archive):
    stream = archive.fileobj
    need(isinstance(stream, BoundedTarFile) and not stream.payload)
    stream.payload = True
    try:
        yield
    finally:
        stream.payload = False


class ForwardGzip:
    """Forward-only bounded file protocol for tar's metadata parser.

    Unlike r|gz, an oversized PAX/GNU read is refused at this interface before
    tarfile accumulates it. No second expanded copy of the source tree is made.
    """
    def __init__(self, incoming, maximum):
        self.incoming, self.maximum, self.position = incoming, maximum, 0

    def tell(self):
        return self.position

    def read(self, size=-1):
        need(type(size) is int and 0 <= size <= 1024 * 1024)
        data = self.incoming.read(size)
        self.position += len(data)
        need(self.position <= self.maximum)
        return data

    def seek(self, offset, whence=0):
        need(whence == 0 and type(offset) is int and self.position <= offset <= self.maximum)
        while self.position < offset:
            need(bool(self.read(min(1024 * 1024, offset - self.position))))
        return self.position


@contextmanager
def gzip_tar(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as raw, gzip.GzipFile(fileobj=raw, mode="rb") as compressed:
        stream = ForwardGzip(compressed, MAX_EXPANDED + MAX_JSON + 1024 * MAX_MEMBERS)
        with tarfile.open(fileobj=stream, mode="r:") as archive:
            yield archive


@contextmanager
def bounded_tar(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    with BoundedTarFile(fd, "rb", closefd=True) as stream:
        with tarfile.open(fileobj=stream, mode="r:") as archive:
            yield archive


def expand_gzip(source, destination, maximum):
    total = 0
    with open(source, "rb") as raw, gzip.GzipFile(fileobj=raw, mode="rb") as incoming, open(destination, "xb") as out:
        os.chmod(destination, 0o600)
        while True:
            block = incoming.read(1024 * 1024)
            if not block: break
            total += len(block); need(total <= maximum)
            out.write(block)
        out.flush(); os.fsync(out.fileno())


def emit(key, value):
    need(re.fullmatch(r"[a-z_]+", key) and type(value) is str and "\n" not in value
         and "\r" not in value and len(value) <= 256)
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        stream.write(key + "=" + value + "\n")


def scan_tree(material, root):
    files, directories = {}, set()
    def failed(_):
        raise Refused() from None
    for directory, children, names in os.walk(root, followlinks=False, onerror=failed):
        here = Path(directory)
        st = here.lstat()
        need(stat.S_ISDIR(st.st_mode) and st.st_uid == os.geteuid()
             and stat.S_IMODE(st.st_mode) == 0o700)
        directories.add(here.relative_to(root).as_posix())
        need(len(files) + len(directories) <= MAX_MEMBERS)
        for child in children:
            p = here / child
            info = p.lstat()
            need(stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid()
                 and stat.S_IMODE(info.st_mode) == 0o700)
        for child in names:
            path = here / child
            name = path.relative_to(root).as_posix()
            material.relative(name)
            fact = material.file_fact(path)
            mode = stat.S_IMODE(path.lstat().st_mode)
            need(mode in (0o400, 0o444, 0o500, 0o555, 0o600, 0o644, 0o700, 0o755))
            files[name] = {"path": name, **fact, "mode": mode}
            need(len(files) + len(directories) <= MAX_MEMBERS)
    material.topology(root, files, directories)
    return files, directories


def job_fields():
    return {key: os.environ[key] for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB")}


def preflight(temp, material):
    facts = {}
    for role, name in (("git", "/usr/bin/git"), ("python", "/usr/bin/python3"),
                       ("zstd", "/usr/bin/zstd")):
        path = Path(name).resolve(strict=True)
        facts[role] = {"path": str(path), **host_fact(path)}
    print("MORPHEUS_NATIVE_HOST_INPUTS " + json.dumps({"schema": 1,
        "image_os": os.environ.get("ImageOS"), "image_version": os.environ.get("ImageVersion"),
        "source_run_id": os.environ["GITHUB_RUN_ID"], "source_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "source_commit": os.environ["GITHUB_SHA"], "tools": facts,
        "free_bytes": shutil.disk_usage(temp).free,
        "git_version_requires_same_job_checkout_log_binding": True,
        "cache_write": False, "source_or_tools_staged": False, "build_admission_granted": False,
        "native_sources_complete": False, "build_ready": False, "distribution_ready": False},
        sort_keys=True), flush=True)


def stage_data(helper, profile, asset, asset_sha, core, root, control, material, python_archives=None):
    admission = {"schema": {"core": 1, "linux": 2, "python": 3, "instrumented": 4}[profile],
        "purpose": {"core": "stage_verified_tool_data_only",
                    "linux": "stage_verified_linux_build_input_data_only",
                    "python": "stage_verified_linux_python_data_only",
                    "instrumented": "stage_verified_instrumented_library_data_only"}[profile],
        "manifest_sha256": material.CORE_PLAN, "target": "Linuxx64", "root": str(root),
        "public_repository": True, "standard_hosted_runner": True, "repository": REPOSITORY,
        "accept_exact_mac_dangling_alias_omissions": False, **job_fields()}
    if profile != "core":
        admission[profile + "_inputs_sha256"] = asset_sha
    if profile == "python":
        need(python_archives is not None)
        admission["python_archives_root"] = str(python_archives)
    else:
        need(python_archives is None)
    path = control / (profile + "-stage-admission.json")
    fact = material.write_json(path, admission)
    argv = ["--target", "Linuxx64", "--manifest", str(core), "--manifest-sha256", material.CORE_PLAN,
            "--admission", str(path), "--admission-sha256", fact["sha256"], "--root", str(root)]
    if profile != "core":
        argv += ["--" + profile + "-inputs", str(asset), "--" + profile + "-inputs-sha256", asset_sha]
    if profile == "python":
        argv += ["--python-archives", str(python_archives)]
    cli(helper, argv)


def python_archive_inputs(plan, temp, material):
    # The original Python DATA stager deliberately accepts predownloaded raw
    # instances. Supply the exact 33 pinned instances, never mutable labels,
    # using the already reviewed credential-free HTTPS transport. Three old
    # instances use SHA1 CIPD identity; every byte also has an independent SHA256.
    transport = module("linux_stager.py")
    root = temp / ("morpheus-webrtc-python-inputs-" + sha((os.environ["GITHUB_RUN_ID"] + ":" +
            os.environ["GITHUB_RUN_ATTEMPT"]).encode())[:16])
    stage = transport.Stage(root)
    try:
        need(plan["schema"] == "morpheus-native-linux-vpython-data-v1"
             and len(plan["materials"]) == 33)
        for row in plan["materials"]:
            need(row["instance"]["hashAlgo"] in ("SHA1", "SHA256")
                 and HEX.fullmatch(row["sha256"]) and 0 < row["bytes"] <= transport.MAX_ARCHIVE)
            payload = json.dumps({"package": row["package"], "instance": row["instance"]},
                                 separators=(",", ":")).encode()
            connection, response = transport.https(
                "https://chrome-infra-packages.appspot.com/prpc/cipd.Repository/GetInstanceURL",
                method="POST", body=payload, json_response=True)
            try:
                raw = response.read(65537)
                need(len(raw) <= 65536 and raw.startswith(b")]}'"))
                value = json.loads(raw[4:]); need(set(value) == {"signedUrl"})
                signed = urllib.parse.urlsplit(value["signedUrl"])
                need(signed.hostname == "storage.googleapis.com" and signed.path ==
                     "/chrome-infra-packages/store/" + row["instance"]["hashAlgo"] + "/" +
                     row["instance"]["hexDigest"] and signed.query)
            finally:
                connection.close()
            connection, response = transport.https(value["signedUrl"])
            try:
                need(response.getheader("Content-Length") == str(row["bytes"])
                     and response.getheader("Transfer-Encoding") is None)
                deadline = time.monotonic() + 300
                actual, instance, count = hashlib.sha256(), hashlib.new(row["instance"]["hashAlgo"].lower()), 0
                with stage.create(root / material.relative(row["input_path"])) as out:
                    while count < row["bytes"]:
                        need(time.monotonic() < deadline)
                        block = response.read(min(1024 * 1024, row["bytes"] - count)); need(bool(block))
                        count += len(block); actual.update(block); instance.update(block); out.write(block)
                    need(response.read(1) == b"" and actual.hexdigest() == row["sha256"]
                         and instance.hexdigest() == row["instance"]["hexDigest"])
                    out.flush(); os.fsync(out.fileno())
            finally:
                connection.close()
        return stage
    except BaseException:
        stage.finish(False)
        raise


def asset(root, spec, fixed=None):
    need(set(spec) == {"path", "sha256"} and type(spec["path"]) is str
         and "\\" not in spec["path"])
    p = Path(spec["path"])
    need(not p.is_absolute() and all(x not in ("", ".", "..") for x in p.parts))
    path = root / p
    need(path.resolve(strict=True).is_relative_to(root))
    value = load(path, fixed or spec["sha256"])
    need(fixed is None or spec["sha256"] == fixed)
    return path, value


def input_assets(root, rows, destination, material):
    need(type(rows) is list and 0 < len(rows) <= 10000 and not os.path.lexists(destination))
    expected = {material.relative(row["path"]): row for row in rows}
    need(len(expected) == len(rows) and all("\\" not in name for name in expected))
    destination.mkdir(mode=0o700)
    directories = {"."}
    for name in expected:
        directories.update(str(parent) for parent in Path(name).parents)
    for name in sorted(directories - {"."}, key=lambda x: (x.count("/"), x)):
        (destination / name).mkdir(mode=0o700)
    for name, row in sorted(expected.items()):
        need(set(row) == {"path", "bytes", "sha256"})
        here = root
        for component in Path(name).parts[:-1]:
            here = here / component
            st = here.lstat()
            need(stat.S_ISDIR(st.st_mode))
        material.file_fact(root / name, row, copy_to=destination / name)
    return destination


def dependency_decisions(root, spec, ledger_spec):
    _, ledger = asset(root, ledger_spec,
        "2ea59842c90033f32529753950bff4172ef1322107784b7aea3cc00fa2478d58")
    _, decisions = asset(root, spec)
    need(decisions["schema"] == "morpheus-native-linux-dependency-decisions-v1"
         and decisions["source_revision"] == "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
         and decisions["production_targets"] == [":default"]
         and decisions["native_test_target"] == "//:rtc_unittests"
         and decisions["gn_use_siso"] is False and decisions["gn_target_os"] == "linux"
         and decisions["gn_target_cpu"] == "x64")
    original = {row["path"]: row for row in ledger["declarations"]}
    rows = decisions["declarations"]
    need(len(original) == len(ledger["declarations"]) == 13
         and len(rows) == 13 and {r["path"] for r in rows} == set(original))
    for row in rows:
        raw = (json.dumps(original[row["path"]], sort_keys=True, separators=(",", ":")) + "\n").encode()
        need(row["declaration_sha256"] == sha(raw) and row["decision"] in
             ("real-material", "proven-inactive", "unresolved"))
        need(type(row["evidence"]) is list and row["evidence"])
        for evidence in row["evidence"]:
            need(set(evidence) == {"path", "sha256"})
            name = evidence["path"]
            need(type(name) is str and not name.startswith("/") and "\\" not in name
                 and all(part not in ("", ".", "..") for part in name.split("/")))
            need(sha(read(root / name)) == evidence["sha256"])
        # This is the record of reviewed caller/condition evidence. It is not
        # an automated whole-GN-graph claim or a replacement for deps-hooks.
    return decisions


def stage(args, plan, temp, material):
    control = temp / ("morpheus-webrtc-control-" + args.plan_sha256[:16])
    need(not os.path.lexists(control))
    control.mkdir(mode=0o700)
    assets = input_assets(Path(args.plan).resolve().parent, plan["assets"], control / "inputs", material)
    core, _ = asset(assets, plan["core_plan"], material.CORE_PLAN)
    linux, _ = asset(assets, plan["linux_plan"], material.LINUX_PLAN)
    python, python_plan = asset(assets, plan["python_plan"], PYTHON_PLAN)
    instrumented, _ = asset(assets, plan["instrumented_plan"], INSTRUMENTED_PLAN)
    source_path, source_plan = asset(assets, plan["source_plan"], INPUT_PLAN)
    patch_path, patch_manifest = asset(assets, plan["patch_manifest"], PATCH_MANIFEST)
    security, _ = asset(assets, plan["security_plan"], SECURITY_PLAN)
    decisions = dependency_decisions(assets, plan["dependency_decisions"], plan["declaration_ledger"])
    need(shutil.disk_usage(temp).free >= plan["minimum_free_bytes"])
    roots = {kind: temp / ("morpheus-webrtc-tools-" + sha((os.environ["GITHUB_RUN_ID"] + ":" +
            os.environ["GITHUB_RUN_ATTEMPT"] + ":" + kind).encode())[:16])
             for kind in ("core", "linux", "python", "instrumented")}
    for kind, helper_name, path, digest in (
            ("core", "linux_stager.py", core, material.CORE_PLAN),
            ("linux", "linux_stager.py", linux, material.LINUX_PLAN),
            ("python", "python_stager.py", python, PYTHON_PLAN),
            ("instrumented", "instrumented_stager.py", instrumented, INSTRUMENTED_PLAN)):
        if kind == "python":
            inputs = python_archive_inputs(python_plan, temp, material)
            try:
                stage_data(module(helper_name), kind, path, digest, core, roots[kind], control, material,
                           python_archives=inputs.root)
            finally:
                # The real Python DATA output retains every original archive;
                # remove only our separate owned download inputs after copying.
                inputs.finish(False)
        else:
            stage_data(module(helper_name), kind, path, digest, core, roots[kind], control, material)
    # Relocate ONLY the reviewed external-path fields. Every patch byte and
    # before/after record remains exact; a new genuine plan hash is recorded.
    patches = copy.deepcopy(patch_manifest)
    for row in patches["patches"]:
        expected = row["file"].removeprefix("RUNNER_TEMP_INPUT_ROOT_UNBOUND/")
        need(expected != row["file"])
        p = assets / expected
        need(p.resolve(strict=True).is_relative_to(assets)
             and material.file_fact(p) == {key: row[key] for key in ("bytes", "sha256")})
        row["file"] = str(p)
    relocated_patches = control / "patch-manifest.json"
    patch_fact = material.write_json(relocated_patches, patches)
    original_git = source_plan["git"]
    need(all("UNBOUND" in str(v) for v in original_git.values() if v is not None))
    need(Path(plan["host_git"]["path"]) == Path("/usr/bin/git").resolve(strict=True)
         and host_fact(Path(plan["host_git"]["path"])) == {
             key: plan["host_git"][key] for key in ("bytes", "sha256")})
    source_plan["git"] = plan["host_git"]
    source_plan["patch_manifest"] = {"path": str(relocated_patches), "sha256": patch_fact["sha256"]}
    source_plan["security_plan"] = {"path": str(security), "sha256": SECURITY_PLAN}
    relocated_source = control / "source-plan.json"
    source_fact = material.write_json(relocated_source, source_plan)
    original = temp / ("morpheus-webrtc-original-" + args.plan_sha256[:16])
    cli(module("git_source_stager.py"), ["--plan", str(relocated_source), "--plan-sha256",
        source_fact["sha256"], "--output-root", str(original)])
    original_files, original_dirs = scan_tree(material, original)
    original_inventory = control / "original-tree-inventory.json"
    original_fact = material.write_json(original_inventory, {
        "schema": "morpheus-native-provision-tree-inventory-v1",
        "files": list(original_files.values()), "directories": sorted(original_dirs)})
    assembled = temp / ("morpheus-webrtc-assembled-" + args.plan_sha256[:16])
    instrumented_materials = material.file_fact(roots["instrumented"] / "tool-materials.json")
    assembly_plan = {"schema": "morpheus-native-instrumented-assembly-plan-v1",
        "purpose": "assemble_verified_native_instrumented_source_only", "target": "linux-x64",
        "recipe_sha256": HELPERS["source_importer.py"], "public_repository": True,
        "standard_hosted_runner": True, "repository": REPOSITORY,
        "run_id": os.environ["GITHUB_RUN_ID"], "run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "job": os.environ["GITHUB_JOB"], "source": {"root": str(original),
          "inventory": {"path": str(original_inventory), "sha256": original_fact["sha256"]},
          "producer_source_sha256": HELPERS["git_source_stager.py"],
          "producer_binding_sha256": "33d7a49ef10d0c3f6acc2a8466c762e8fbabf6212c1ae0038b81839ded4fcb1b"},
        "instrumented": {"root": str(roots["instrumented"]),
          "plan": {"path": "selected-instrumented-inputs.json", "sha256": INSTRUMENTED_PLAN},
          "materials": {"path": "tool-materials.json", "sha256": instrumented_materials["sha256"]},
          "core_manifest": {"path": "selected-tool-manifest.json", "sha256": material.CORE_PLAN}}}
    assembly_path = control / "instrumented-assembly-plan.json"
    assembly_fact = material.write_json(assembly_path, assembly_plan)
    cli(module("source_importer.py"), ["--assemble-instrumented", "--plan", str(assembly_path),
        "--plan-sha256", assembly_fact["sha256"], "--output-root", str(assembled)])
    base = assembled / "provision"
    files, directories = scan_tree(material, base)
    marker_rows = load(base / "materials/gitlinks.json", material.file_fact(base / "materials/gitlinks.json")["sha256"])
    overlay, files, directories = material.project(base, files, directories,
        [row["exported_path"] for row in marker_rows], [(kind, roots[kind],
         material.file_fact(roots[kind] / "tool-materials.json")["sha256"]) for kind in ("core", "linux")])
    old_overlay = load(base / "materials/instrumented-source-overlay.json",
        material.file_fact(base / "materials/instrumented-source-overlay.json")["sha256"])
    projection = sorted(old_overlay + overlay, key=lambda row: row["path"])
    projection_fact = material.write_json(base / "materials/linux-source-overlay.json", projection)
    inputs_receipt = {"schema": "morpheus-linux-source-projection-v1", "target": "linux-x64",
        "pipeline_plan_sha256": args.plan_sha256, "original_source_plan_sha256": source_fact["sha256"],
        "instrumented_assembly_plan_sha256": assembly_fact["sha256"], "overlay": projection_fact,
        "material_receipts": {kind: material.file_fact(root / "tool-materials.json")
                              for kind, root in roots.items()},
        "dependency_decisions_sha256": plan["dependency_decisions"]["sha256"],
        "unresolved_declarations": [r["path"] for r in decisions["declarations"] if r["decision"] == "unresolved"],
        "source_git_metadata_created": False, "downloaded_native_tools_executed": [],
        "trusted_host_git_used_by_original_producer": True,
        "native_sources_complete": False, "build_ready": False, "distribution_ready": False}
    material.write_json(base / "materials/linux-projection-receipt.json", inputs_receipt)
    files, directories = scan_tree(material, base)
    source_inventory = [{"path": name.removeprefix("checkout/src/"), "type": "file",
        "bytes": row["bytes"], "sha256": row["sha256"],
        "mode": "100755" if row["mode"] == 0o755 else "100644"}
        for name, row in sorted(files.items()) if name.startswith("checkout/src/")]
    material.write_json(assembled / "handoff/linux-source-inventory.json", source_inventory)
    material.write_json(assembled / "handoff/linux-provenance-inventory.json", [
        {"path": name, "type": "file", "bytes": row["bytes"], "sha256": row["sha256"],
         **({"mode": "100755" if row["mode"] == 0o755 else "100644"}
            if row["mode"] in (0o644, 0o755) else {})} for name, row in sorted(files.items())])
    all_roots = {"assembled": assembled, **roots, "control": control}
    inventories = {}
    for name, root in all_roots.items():
        file_rows, dirs = scan_tree(material, root)
        inventories[root.name] = {"role": name, "files": list(file_rows.values()), "directories": sorted(dirs)}
    handoff = {"schema": "morpheus-native-linux-handoff-v1", "pipeline_plan_sha256": args.plan_sha256,
        "source_run_id": os.environ["GITHUB_RUN_ID"], "source_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "source_commit": os.environ["GITHUB_SHA"], "roots": inventories,
        "build_admission_granted": False, "native_sources_complete": False,
        "build_ready": False, "distribution_ready": False}
    cache_dir = temp / "native-webrtc-handoff"
    need(not os.path.lexists(cache_dir))
    cache_dir.mkdir(mode=0o700)
    output = cache_dir / "handoff.tar.gz"
    with open(output, "xb") as stream:
        os.chmod(output, 0o600)
        bounded = BoundedWriter(stream, MAX_HANDOFF)
        with tarfile.open(fileobj=bounded, mode="w:gz", format=tarfile.GNU_FORMAT) as archive:
            raw = (json.dumps(handoff, sort_keys=True, separators=(",", ":")) + "\n").encode()
            need(len(raw) <= MAX_JSON)
            info = tarfile.TarInfo("HANDOFF.json"); info.size = len(raw); info.mode = 0o600
            archive.addfile(info, io.BytesIO(raw))
            for name, root_spec in sorted(inventories.items()):
                root = temp / name
                for directory in root_spec["directories"]:
                    member = name + ("/" + directory if directory != "." else "")
                    info = tarfile.TarInfo(member); info.type = tarfile.DIRTYPE; info.mode = 0o700
                    archive.addfile(info)
                for row in root_spec["files"]:
                    material.file_fact(root / row["path"], row, row["mode"])
                    info = tarfile.TarInfo(name + "/" + row["path"])
                    info.size, info.mode = row["bytes"], row["mode"]
                    fd = os.open(root / row["path"], os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
                    with os.fdopen(fd, "rb") as source:
                        archive.addfile(info, source)
                    need(stream.tell() <= MAX_HANDOFF)
        stream.flush(); os.fsync(stream.fileno())
    fact = material.file_fact(output)
    need(fact["bytes"] <= MAX_HANDOFF)
    for name, spec in inventories.items():
        material.topology(temp / name, {r["path"]: r for r in spec["files"]}, set(spec["directories"]))
    key = "native-webrtc-source-" + args.plan_sha256[:16] + "-" + fact["sha256"]
    material.write_json(cache_dir / "handoff-receipt.json", {"schema": 1, "key": key,
        "archive": fact, "source_commit": os.environ["GITHUB_SHA"], "cache_write_authorized": False,
        "build_admission_granted": False, "native_sources_complete": False})
    print("MORPHEUS_NATIVE_INPUT_RECEIPT " + json.dumps({"target": "linux-x64",
        "handoff": fact, "cache_key": key, "pipeline_plan_sha256": args.plan_sha256,
        "material_receipts": inputs_receipt["material_receipts"],
        "source_overlay": projection_fact,
        "source_inventory": material.file_fact(assembled / "handoff/linux-source-inventory.json"),
        "provenance_inventory": material.file_fact(assembled / "handoff/linux-provenance-inventory.json"),
        "source_receipt": material.file_fact(base / "source-provision-receipt.json"),
        "source_materials": material.file_fact(base / "materials/source-materials.json"),
        "roots": {role: root.name for role, root in all_roots.items()},
        "unresolved_declarations": inputs_receipt["unresolved_declarations"],
        "build_admission_granted": False}, sort_keys=True), flush=True)
    emit("cache_key", key); emit("handoff_sha256", fact["sha256"]); emit("handoff_bytes", str(fact["bytes"]))
    if args.publish_handoff:
        need(plan["cache_handoff_authorized"] is True)
        cache_capacity(key, fact["bytes"])
        emit("cache_save", "true")
    else:
        emit("cache_save", "false")
    print('{"native_linux_producer":"data_staged_and_assembled","build_admission_granted":false}')


def cache_capacity(key, size):
    limit = api("/repos/" + REPOSITORY + "/actions/cache/storage-limit")
    usage = api("/repos/" + REPOSITORY + "/actions/cache/usage")
    need(limit == {"max_cache_size_gb": 10} and type(usage["active_caches_size_in_bytes"]) is int
         and 0 <= usage["active_caches_size_in_bytes"] <= MAX_CACHE_CAPACITY
         and size + CACHE_RESERVE <= MAX_CACHE_CAPACITY - usage["active_caches_size_in_bytes"])
    rows = api("/repos/" + REPOSITORY + "/actions/caches?key=" + urllib.parse.quote(key, safe="")
               + "&ref=refs%2Fheads%2Fmain")
    need(rows["total_count"] == 0 and rows["actions_caches"] == [])


def cache_identity(args, material, after_save=False):
    need(re.fullmatch(r"native-webrtc-source-[0-9a-f]{16}-[0-9a-f]{64}", args.cache_key)
         and args.cache_key.startswith("native-webrtc-source-" + args.plan_sha256[:16] + "-")
         and args.cache_key.endswith("-" + args.handoff_sha256))
    rows = api("/repos/" + REPOSITORY + "/actions/caches?key=" + urllib.parse.quote(args.cache_key, safe="")
               + "&ref=refs%2Fheads%2Fmain")
    need(rows["total_count"] == 1 and len(rows["actions_caches"]) == 1)
    value = rows["actions_caches"][0]
    need(value["key"] == args.cache_key and value["ref"] == "refs/heads/main"
         and type(value["id"]) is int and value["id"] > 0
         and type(value["version"]) is str and HEX.fullmatch(value["version"])
         and type(value["size_in_bytes"]) is int
         and 0 < value["size_in_bytes"] <= MAX_HANDOFF + 128 * 1024 * 1024)
    if not after_save:
        need(str(value["id"]) == args.cache_id)
    return {key: value[key] for key in ("id", "key", "ref", "version", "size_in_bytes")}


def restore(args, temp, material, *, mac_target=None):
    need(mac_target is None or mac_target in ("mac-arm64", "mac-x64"))
    cache = cache_identity(args, material)
    archive = temp / "native-webrtc-handoff/handoff.tar.gz"
    fact = material.file_fact(archive)
    need(fact == {"sha256": args.handoff_sha256, "bytes": args.handoff_bytes}
         and fact["bytes"] <= MAX_HANDOFF)
    expected, dirs, header, seen, total = {}, {}, None, set(), 0
    # The compressed bytes are admitted before parsing. Only exact regular
    # members from its bounded complete manifest can be copied, never links.
    with gzip_tar(archive) as source:
        for number, member in enumerate(source):
            need(number <= MAX_MEMBERS and not member.pax_headers and not member.issparse())
            if number == 0:
                need(member.name == "HANDOFF.json" and member.isfile() and member.size <= MAX_JSON)
                stream = source.extractfile(member); need(stream is not None)
                raw = bytearray()
                while len(raw) < member.size:
                    block = stream.read(min(1024 * 1024, member.size - len(raw))); need(bool(block))
                    raw.extend(block)
                need(stream.read(1) == b"" and len(raw) == member.size)
                header = json.loads(raw)
                need(header["schema"] == ("morpheus-native-linux-handoff-v1" if mac_target is None else "morpheus-native-macos-handoff-v1")
                     and header["pipeline_plan_sha256"] == args.plan_sha256
                     and header["build_admission_granted"] is False
                     and header["native_sources_complete"] is False
                     and header["build_ready"] is False and header["distribution_ready"] is False)
                if mac_target is None:
                    need(len(header["roots"]) == 6 and set(spec["role"] for spec in header["roots"].values()) ==
                         {"assembled", "core", "linux", "python", "instrumented", "control"})
                else:
                    need(header["target"] == mac_target and header["apple_sdk_redistributed"] is False
                         and len(header["roots"]) == 4 and set(spec["role"] for spec in header["roots"].values()) ==
                         {"source", "core", "mac", "control"})
                for name, spec in header["roots"].items():
                    need(re.fullmatch(r"morpheus-webrtc-(?:tools|assembled|control)-[0-9a-f]{16}", name)
                         and not os.path.lexists(temp / name))
                    for directory in spec["directories"]:
                        full = name + ("/" + material.relative(directory) if directory != "." else "")
                        need(full not in dirs); dirs[full] = 0o700
                    for row in spec["files"]:
                        full = name + "/" + material.relative(row["path"])
                        need(full not in expected); expected[full] = row
                need(len(expected) + len(dirs) <= MAX_MEMBERS
                     and sum(row["bytes"] for row in expected.values()) <= MAX_EXPANDED)
                for name in sorted(dirs, key=lambda x: (x.count("/"), x)):
                    need(name not in expected and ((temp / name).parent == temp or
                         (temp / name).parent.is_dir()))
                    (temp / name).mkdir(mode=0o700)
                continue
            need(header is not None and member.name not in seen)
            seen.add(member.name)
            if member.isdir():
                need(member.name in dirs and member.mode == 0o700 and member.size == 0)
                continue
            need(member.isfile() and member.name in expected)
            row = expected[member.name]
            need(member.size == row["bytes"] and member.mode == row["mode"])
            stream = source.extractfile(member); need(stream is not None)
            destination = temp / member.name
            fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            try:
                h, count = hashlib.sha256(), 0
                with os.fdopen(fd, "wb", closefd=False) as out:
                    while True:
                        block = stream.read(1024 * 1024)
                        if not block: break
                        count += len(block); total += len(block)
                        need(count <= row["bytes"] and total <= MAX_EXPANDED)
                        h.update(block); out.write(block)
                    need(count == row["bytes"] and h.hexdigest() == row["sha256"])
                    out.flush(); os.fchmod(fd, row["mode"]); os.fsync(fd)
            finally:
                os.close(fd)
    need(seen == set(expected) | set(dirs))
    for name, spec in header["roots"].items():
        material.topology(temp / name, {r["path"]: r for r in spec["files"]}, set(spec["directories"]))
    return header, cache


def build(args, temp, material, plan):
    admission = load(Path(args.admission), args.admission_sha256)
    need(admission["schema"] == 5 and admission["recipe_sha256"] == BUILD_RECIPE)
    cache_download(args, temp, material, plan)
    header, cache = restore(args, temp, material)
    # Paths are the only relocation: gate bytes, actual material SHA values,
    # source inventories, recipe/input hashes and approval statuses are never
    # synthesized or changed by this producer.
    def relocated(value):
        if isinstance(value, dict): return {k: relocated(v) for k, v in value.items()}
        if isinstance(value, list): return [relocated(v) for v in value]
        if isinstance(value, str) and value.startswith("$RUNNER_TEMP/"):
            name = material.relative(value.removeprefix("$RUNNER_TEMP/"))
            return str(temp / name)
        if isinstance(value, str) and value.startswith("$GITHUB_WORKSPACE/"):
            name = material.relative(value.removeprefix("$GITHUB_WORKSPACE/"))
            return str(Path(os.environ["GITHUB_WORKSPACE"]) / name)
        return value
    actual = relocated(admission)
    need(actual["schema"] == 5 and actual["recipe_sha256"] == BUILD_RECIPE)
    control_name = next(name for name, row in header["roots"].items() if row["role"] == "control")
    decisions = dependency_decisions(temp / control_name / "inputs", plan["dependency_decisions"],
                                    plan["declaration_ledger"])
    need(all(row["decision"] != "unresolved" for row in decisions["declarations"]))
    control = temp / ("morpheus-webrtc-build-control-" + args.plan_sha256[:16])
    need(not os.path.lexists(control)); control.mkdir(mode=0o700)
    manifest = control / "admission.json"
    bound = material.write_json(manifest, actual)
    path = Path(__file__).resolve().parent / "build_native_webrtc_unix.py"
    raw = read(path, 256 * 1024); need(sha(raw) == BUILD_RECIPE)
    value = types.ModuleType("morpheus_admitted_unix_builder"); value.__file__ = str(path)
    sys.modules[value.__name__] = value
    exec(compile(raw, str(path), "exec"), value.__dict__)
    cli(value, ["--target", "linux-x64", "--runner-root", str(temp / "morpheus-webrtc-build"),
        "--admission", str(manifest), "--admission-sha256", bound["sha256"], "--jobs", str(args.jobs)])
    material.write_json(control / "handoff-consumption.json", {"cache": cache,
        "handoff_sha256": args.handoff_sha256, "root_admission_template_sha256": args.admission_sha256,
        "actual_relocated_admission_sha256": bound["sha256"], "publication_admission": False})


class FixedParser(argparse.ArgumentParser):
    def error(self, message):
        raise Refused()


def main():
    parser = FixedParser(allow_abbrev=False)
    parser.add_argument("--phase", choices=("preflight", "stage", "build", "cache-receipt"), required=True)
    parser.add_argument("--plan", required=True); parser.add_argument("--plan-sha256", required=True)
    parser.add_argument("--publish-handoff", action="store_true")
    parser.add_argument("--cache-key", default=""); parser.add_argument("--cache-id", default="")
    parser.add_argument("--handoff-sha256", default="")
    parser.add_argument("--handoff-bytes", type=int, default=0)
    parser.add_argument("--admission"); parser.add_argument("--admission-sha256")
    parser.add_argument("--jobs", type=int, choices=(1, 2, 3, 4), default=2)
    args = parser.parse_args()
    temp = runner(); plan = load(Path(args.plan), args.plan_sha256)
    need(set(plan) == {"schema", "pipeline_sha256", "target", "security_plan_sha256",
         "native_test_count", "security_patch_count", "core_plan", "linux_plan", "python_plan",
         "instrumented_plan", "source_plan", "patch_manifest", "security_plan",
         "dependency_decisions", "declaration_ledger", "assets", "host_git", "host_zstd",
         "minimum_free_bytes", "cache_handoff_authorized"}
         and plan["schema"] == "morpheus-native-linux-pipeline-v1"
         and plan["pipeline_sha256"] == sha(read(Path(__file__).resolve(), 256 * 1024))
         and plan["target"] == "linux-x64" and plan["security_plan_sha256"] == SECURITY_PLAN
         and plan["native_test_count"] == 12 and plan["security_patch_count"] == 8)
    need(type(plan["minimum_free_bytes"]) is int and plan["minimum_free_bytes"] >= 12 * 1024 ** 3
         and type(plan["cache_handoff_authorized"]) is bool)
    material = module("linux_materials.py")
    if args.phase == "preflight":
        need(not args.publish_handoff and not args.cache_key and not args.cache_id
             and not args.handoff_sha256 and args.handoff_bytes == 0
             and args.admission is None and args.admission_sha256 is None)
        preflight(temp, material)
    elif args.phase == "stage":
        need(not args.cache_key and not args.cache_id and not args.handoff_sha256 and args.handoff_bytes == 0
             and args.admission is None and args.admission_sha256 is None)
        stage(args, plan, temp, material)
    elif args.phase == "build":
        need(args.admission is not None and args.admission_sha256 is not None
             and not args.publish_handoff and re.fullmatch(r"[1-9][0-9]*", args.cache_id)
             and HEX.fullmatch(args.handoff_sha256) and 0 < args.handoff_bytes <= MAX_HANDOFF)
        build(args, temp, material, plan)
    else:
        need(not args.publish_handoff and not args.cache_id and args.admission is None
             and args.admission_sha256 is None and HEX.fullmatch(args.handoff_sha256)
             and 0 < args.handoff_bytes <= MAX_HANDOFF)
        value = cache_identity(args, material, after_save=True)
        material.write_json(temp / "native-webrtc-handoff/cache-identity.json", value)
        print(json.dumps({"native_source_cache_id": value["id"], "key": value["key"]}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        print('{"native_linux_producer":"failed","build_or_release_acceptance":false}', file=sys.stderr)
        raise SystemExit(1)
