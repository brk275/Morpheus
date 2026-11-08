"""Fresh standard-host, source-only inputs for Mac/Windows package notices.

Only locked public Cargo source acquisition runs; no build, generated dependency
code, setup, installer or source-archive program is executed here. Linux retains
its separate namespace route. Outputs and logs never leave this ephemeral job.
"""
from __future__ import annotations
import argparse
import contextlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time

# Isolated direct-file execution omits the script directory from sys.path.
# Admit only these reviewed file-relative sibling helpers, never cwd/PYTHONPATH
# or a path supplied by an archive, manifest, environment or command argument.
if not __package__ and sys.flags.isolated:
    sys.path.insert(0, str(Path(__file__).resolve().parent))

if __package__:
    from . import native_notices as N
    from . import package_revision as R
    from .package_scope import package_matrix
else:
    import native_notices as N
    import package_revision as R
    from package_scope import package_matrix
S = N.catalogue()
ROOT = Path(__file__).resolve().parents[1]
PINS = frozenset({
    "git+https://github.com/element-hq/matrix-rust-rtc?tag=v0.5.0-rc.2#53880ef1ef66b62df555ce71ccd1d6c85b813596",
    "git+https://github.com/jplatte/const_panic?rev=e0b317a9a7bde2d48a7d15b6a60d70e4a41d3b5f#e0b317a9a7bde2d48a7d15b6a60d70e4a41d3b5f",
    "git+https://github.com/matrix-org/matrix-rust-sdk?rev=c6ad52ecc09eaa36601f55effb0a48a533eb87dd#c6ad52ecc09eaa36601f55effb0a48a533eb87dd",
})
RTC_SOURCE = "git+https://github.com/element-hq/matrix-rust-rtc?tag=v0.5.0-rc.2#53880ef1ef66b62df555ce71ccd1d6c85b813596"
RTC_REPOSITORY = "https://github.com/element-hq/matrix-rust-rtc"
RTC_REVISION = "53880ef1ef66b62df555ce71ccd1d6c85b813596"
RTC_VERSION = "0.5.0-rc.2"
RTC_PROVENANCE = {
    "matrix-rtc-call": "UPSTREAM.json",
    "matrix-rtc-call-sdk": "MORPHEUS_PATCH.md",
    "matrix-rtc-core": "MORPHEUS_PROVENANCE.json",
    "matrix-rtc-livekit": "README.md",
    "matrix-rtc-matrix-sdk": "UPSTREAM_FILES.json",
    "matrix-rtc-transport": "MORPHEUS_PATCH.md",
}
SCANNERS = {
    ("darwin", "arm64"): "ba52fb1bfabbcde42f032afad3d6e0b19dff8ed105229a16e7caa338bbc0e84f",
    ("darwin", "x86_64"): "cee01fea7173f1b779dff188e1c26ecbcb4027d394acc573b23aaf0be260e291",
    ("win32", "AMD64"): "17157e2ee8b76fc8b1d8bee607a250e34b8a8023c8bc81822d4b5ee4d78fcb7c",
}

_NOTICE_FAILURE_PHASE = "guard"
_NOTICE_FAILURE_PHASES = frozenset({
    "guard", "event_file", "event_payload", "scanner_path", "scanner_hash",
    "rustup", "cargo", "rustc", "git", "temp_parent", "private_root",
    "owner_marker", "publish", "source", "cleanup",
})


def _notice_phase(phase):
    global _NOTICE_FAILURE_PHASE
    _NOTICE_FAILURE_PHASE = phase


def _notice_report_failure(error):
    # Closed tokens and bounded OS numbers only: never exception text, paths,
    # SIDs, environment values, event contents, source data or a traceback.
    category = "other"
    for kind, token in ((OSError, "os_error"), (ValueError, "value_error"),
                        (AssertionError, "assertion_error"), (KeyError, "key_error"),
                        (TypeError, "type_error"), (RuntimeError, "runtime_error"),
                        (KeyboardInterrupt, "interrupted"), (SystemExit, "system_exit")):
        if isinstance(error, kind):
            category = token
            break
    numbers = {}
    for name in ("errno", "winerror"):
        value = getattr(error, name, None) if isinstance(error, OSError) else None
        numbers[name] = value if type(value) is int and 0 <= value <= 65535 else None
    phase = _NOTICE_FAILURE_PHASE if _NOTICE_FAILURE_PHASE in _NOTICE_FAILURE_PHASES else "source"
    print("MORPHEUS_NATIVE_NOTICE_FAILURE " + json.dumps({"phase": phase, "exception": category,
          **numbers}, sort_keys=True), file=sys.stderr)


def _notice_failure_site(error):
    # Code metadata only: never inspect locals, arguments, exception text,
    # chained errors or source lines. Only exact owned module files map to a
    # fixed public enum; an unknown innermost frame suppresses the site.
    try:
        owned = {
            os.path.normcase(str(ROOT / "scripts/native_notice_source.py")): "notice_source",
            os.path.normcase(str(ROOT / "scripts/native_notice_windows.py")): "windows_security",
            os.path.normcase(str(ROOT / "scripts/native_notices.py")): "native_notices",
            os.path.normcase(str(ROOT / "packaging/linux/corresponding_source.py")): "source_catalogue",
            os.path.normcase(str(ROOT / "scripts/run_windows_capture_acceptance.py")): "windows_processes",
        }
        trace, site, visited = error.__traceback__, None, 0
        while trace is not None and visited < 32:
            visited += 1
            code = trace.tb_frame.f_code
            filename = code.co_filename
            module_file = trace.tb_frame.f_globals.get("__file__")
            if (type(filename) is str and len(filename) <= 4096
                    and type(module_file) is str and len(module_file) <= 4096
                    and os.path.normcase(filename) == os.path.normcase(module_file)
                    and os.path.normcase(filename) in owned):
                # Retain the rejecting caller's predicate, not this generic
                # helper's `raise`. Function names never enter the record.
                if code.co_name != "require":
                    line = trace.tb_lineno
                    site = ({"module": owned[os.path.normcase(filename)], "line": line}
                            if type(line) is int and 0 < line <= 99999 else None)
            else:
                site = None
            trace = trace.tb_next
        return site if trace is None else None
    except BaseException:
        # Diagnostics cannot replace the original closed failure with a new
        # traceback if code metadata is unavailable or malformed.
        return None

def _cargo_stderr_classes(data):
    require(type(data) is bytes and len(data) <= 8 * 1024**2)
    lower = data.lower()
    return sorted(name for name, markers in _CARGO_STDERR_MARKERS.items()
                  if any(marker in lower for marker in markers)) or ["other"]

def _cargo_fetch_failure_diagnostic(commands, scanned):
    if (scanned is not True or getattr(commands, "logs_complete", None) is not True
            or getattr(commands, "cleanup_failed", None) is not False
            or getattr(commands, "retained", None) != []):
        return None
    failure = getattr(commands, "cargo_fetch_failure", None)
    if type(failure) is not tuple or len(failure) != 3:
        return None
    path, status, size = failure
    if (type(status) is not int or status == 0 or not -255 <= status <= 0xffffffff
            or type(size) is not int or not 0 <= size <= 8 * 1024**2):
        return None
    path = Path(path)
    require(path.parent == commands.logs and commands.logs == commands.root / "logs"
            and re.fullmatch(r"[0-9]{5}\.err", path.name) is not None)
    # Existing strict private-path and stable opened-file identity checks; no
    # partial stderr, unsafe alias or changed byte count can become a hint.
    checked(path, private=True)
    data = N.read_regular(path, 8 * 1024**2, allow_empty=True)
    require(len(data) == size)
    return {"schema": 1, "check": "native_notice_cargo_fetch_failure", "status": status,
            "stderr_classes": _cargo_stderr_classes(data)}

def _notice_report_cargo_fetch_failure(commands, scanned):
    try:
        facts = _cargo_fetch_failure_diagnostic(commands, scanned)
        if facts is not None:
            print("MORPHEUS_NATIVE_NOTICE_CARGO_FETCH " + json.dumps(facts, sort_keys=True), file=sys.stderr)
    except BaseException:
        return  # Diagnostic refusal never replaces the original command error.

def private_directory(path, *, parents=False, exist_ok=False):
    path = Path(path)
    if os.name == "nt":
        return S.windows_security().private_directory(path, parents=parents, exist_ok=exist_ok)
    path.mkdir(mode=0o700, parents=parents, exist_ok=exist_ok)
    return path

def workspace_repository(commands, root, revision, git, environment):
    """Authenticate Windows source through an owned bare repository, not checkout ACLs.

    The selected public commit is still authenticated by git_export/build's
    complete raw commit/tree/blob checks. No checkout configuration, template,
    hooks, alternate object store, credential helper or local transport is used.
    """
    if os.name != "nt":
        return ROOT
    require(type(revision) is str and re.fullmatch(r"[0-9a-f]{40}", revision) is not None
            and revision != "0" * 40 and revision == R.expected_revision(environment))
    require(Path(git) == system_git())
    root = checked(root, directory=True, private=True)
    cwd = checked(root/"fetch", directory=True, private=True)
    repository = private_directory(root/"workspace.git")
    base = [*S.safe_git(str(git), repository),
            "-c", "protocol.allow=never", "-c", "protocol.https.allow=always",
            "-c", "http.sslVerify=true", "-c", "http.followRedirects=false",
            "-c", "http.extraHeader=", "-c", "credential.helper=",
            "-c", "credential.interactive=never", "-c", "core.askPass="]
    clean = S.clean_git_environment(environment)
    commands.run("workspace-init", [*base, "init", "--bare", "--quiet", "--template="],
                 cwd=cwd, env=clean, timeout=30, output_limit=65536)
    checked(repository, directory=True, private=True)
    commands.run("workspace-fetch", [*base, "fetch", "--quiet", "--depth=1", "--no-tags",
                 "--no-write-fetch-head", "--no-recurse-submodules", "--no-auto-maintenance",
                 "https://github.com/brk275/Morpheus.git", revision], cwd=cwd,
                 env=clean | {"GIT_ALLOW_PROTOCOL": "https"}, timeout=1200,
                 output_limit=65536, stderr_limit=65536)
    checked(repository, directory=True, private=True)
    # Reuse the existing acquisition inventory limits: no links/reparse points,
    # at most300000 entries/4GiB and each file within the source bundle ceiling.
    check_cache(root)
    return repository

def _remove_readonly_owned_leaf(root, root_identity, function, path, error):
    """Retry only an access-denied readonly unlink inside the authorized tree.

    The caller has already proved stopped children, complete logs and a fresh
    default scan. The original owner marker may be gone during rmtree; retain
    its root inode authority instead of reopening a partially removed marker.
    """
    if (os.name != "nt" or (function is not os.unlink and function is not os.remove)
            or not isinstance(error, PermissionError) or error.errno != 13
            or getattr(error, "winerror", None) != 5):
        raise error
    root, path = Path(root), Path(path)
    require(root.is_absolute() and re.fullmatch(r"morpheus-native-source-[a-z0-9_]{8,32}", root.name)
            and path.is_absolute() and path != root and path.is_relative_to(root))
    def validate_path():
        checked(root, directory=True, private=True)
        current = root.lstat()
        require((current.st_dev, current.st_ino) == root_identity)
        parent = path.parent
        while parent != root:
            checked(parent, directory=True, private=True)
            parent = parent.parent
        checked(path, private=True)
        return path.lstat()
    def identity(info):
        born = info.st_birthtime_ns
        require(type(born) is int and born > 0)
        return (info.st_dev, info.st_ino, stat.S_IFMT(info.st_mode), info.st_nlink,
                info.st_size, info.st_mtime_ns, born)
    before = validate_path()
    attributes = getattr(before, "st_file_attributes", 0)
    if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1
            or not attributes & 0x1 or attributes & (0x400 | 0x10)
            or not 0 <= before.st_size <= S.MAX_BUNDLE):
        raise error
    expected = identity(before)
    descriptor = S.windows_security().open_read(path, private=True)
    try:
        # The existing handle denies write/delete sharing while the only
        # intended mutation clears the Windows readonly attribute. Attribute
        # access is not blocked by the file's sharing mode; ACLs are unchanged.
        require(identity(os.fstat(descriptor)) == expected)
        os.chmod(path, stat.S_IWRITE)
        after = validate_path()
        after_attributes = getattr(after, "st_file_attributes", 0)
        require(identity(after) == expected and identity(os.fstat(descriptor)) == expected
                and not after_attributes & 0x1
                and after_attributes & ~(0x1 | 0x80) == attributes & ~(0x1 | 0x80))
    finally:
        os.close(descriptor)
    # Close the deny-delete handle before retry, then recheck the private root,
    # path/ACL and exact file identity once more. No directory or foreign leaf
    # is chmod'ed, and a second deletion error propagates without another try.
    final = validate_path()
    require(identity(final) == expected
            and not getattr(final, "st_file_attributes", 0) & 0x1
            and getattr(final, "st_file_attributes", 0) & ~(0x1 | 0x80)
                == attributes & ~(0x1 | 0x80))
    function(path)

_CARGO_STDERR_MARKERS = {
    "manifest": (b"failed to parse manifest", b"failed to load manifest", b"no targets specified in the manifest"),
    "lockfile": (b"needs to be updated but --locked was passed", b"failed to parse lock file", b"lock file needs to be updated"),
    "checkout": (b"failed to checkout", b"failed to clone", b"failed to fetch into:", b"filename too long", b"path too long", b"invalid path"),
    "network": (b"failed to download", b"network failure", b"could not resolve host", b"connection timed out", b"failed to connect", b"connection reset"),
    "tls": (b"ssl certificate problem", b"certificate verify failed", b"certificate verification", b"schannel", b"peer certificate", b"tls handshake"),
    "auth": (b"authentication required", b"authentication failed", b"failed to authenticate", b"unauthorized"),
}


def system_git():
    require(os.name == "nt" and sys.platform == "win32")
    return S.windows_security().system_git()

def require(value):
    if not value:
        raise ValueError("Native notice source preparation failed")


def checked(path, *, directory=False, private=False):
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path)
    if os.name == "nt":
        return S.windows_security().path(path, directory=directory, private=private)
    for item in (path, *path.parents):
        info = item.lstat()
        require(not stat.S_ISLNK(info.st_mode) and info.st_uid in {0, os.getuid()}
                and not info.st_mode & 0o022)
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode) and info.st_nlink == 1)
    require(not private or info.st_uid == os.getuid() and not info.st_mode & 0o077)
    return path


def _notice_checked(phase, path, **options):
    _notice_phase(phase)
    return checked(path, **options)


def write_new(path, data):
    with Path(path).open("xb") as stream:
        if os.name != "nt": os.fchmod(stream.fileno(), 0o600)
        stream.write(data); stream.flush(); os.fsync(stream.fileno())


def guard(environment, system=sys.platform):
    _notice_phase("guard")
    require(system in {"darwin", "win32"} and sys.version_info >= (3,12))
    for key, value in {"GITHUB_ACTIONS":"true", "RUNNER_ENVIRONMENT":"github-hosted",
            "GITHUB_REPOSITORY":"brk275/Morpheus", "GITHUB_REF":"refs/heads/main",
            "GITHUB_EVENT_NAME":"workflow_dispatch", "GITHUB_ACTOR":"brk275", "GITHUB_TRIGGERING_ACTOR":"brk275",
            "RUNNER_OS":"macOS" if system == "darwin" else "Windows"}.items():
        require(environment.get(key) == value)
    # main deliberately snapshots os.environ. CPython uppercases every key on
    # Windows; the plain dict no longer has os.environ's case-insensitive lookup.
    image_key = "ImageOS" if system == "darwin" else "IMAGEOS"
    require(environment.get(image_key) in ({"macos15", "macos26"} if system == "darwin" else {"win25", "win25-vs2026"}))
    _notice_phase("event_payload")
    # This control metadata comes from the immutable main workflow's GitHub
    # context, not a source/archive file or the shared hosted TEMP event file.
    # Only declared nonsecret selectors and numeric owner/sender IDs are passed.
    raw = environment.get("MORPHEUS_NATIVE_PACKAGE_CONTEXT")
    require(type(raw) is str and 0 < len(raw) <= 1024 * 1024 and raw.isascii())
    context = S.unique_json(raw.encode("ascii"))
    require(type(context) is dict and set(context) == {"owner_id", "sender_id", "private", "inputs"}
            and type(context["sender_id"]) is int and context["sender_id"] > 0
            and type(context["private"]) is bool and type(context["inputs"]) is dict)
    selected_inputs = context["inputs"]
    require(set(selected_inputs) == {"source_revision", "platform", "linux_formats", "appimage_updates",
                "appimage_capture", "source_bundle", "native_updates"}
            and all(type(selected_inputs[name]) is str for name in ("source_revision", "platform", "linux_formats"))
            and all(type(selected_inputs[name]) is bool for name in
                    ("appimage_updates", "appimage_capture", "source_bundle", "native_updates"))
            and selected_inputs["source_revision"] == R.expected_revision(environment))
    # Retain the original origin and matrix policy against the same typed facts.
    event = {"repository": {"owner": {"id": context["owner_id"]}, "private": context["private"]},
             "sender": {"id": context["sender_id"]}, "inputs": selected_inputs}
    owner = event.get("repository", {}).get("owner", {}).get("id")
    inputs = event.get("inputs", {})
    require(type(owner) is int and owner > 0 and event.get("sender", {}).get("id") == owner
            and event.get("repository", {}).get("private") is False)
    def boolean(value):
        if type(value) is bool: return value
        require(type(value) is str and value in {"true", "false"})
        return value == "true"
    selected = package_matrix(inputs.get("platform"), inputs.get("linux_formats"),
        boolean(inputs.get("appimage_updates")), boolean(inputs.get("appimage_capture")),
        boolean(inputs.get("source_bundle")), boolean(inputs.get("native_updates")))
    label = {("darwin","arm64"):"macos-arm64", ("darwin","x86_64"):"macos-x64",
             ("win32","AMD64"):"windows-x64"}.get((system,platform.machine()))
    require(label is not None and sum(item["label"] == label for item in selected["include"]) == 1
            and not boolean(inputs.get("source_bundle")))


def lock_git_sources(packages):
    sources = frozenset(p["source"] for p in packages if p.get("source", "").startswith("git+"))
    # Two closed cohorts, never an arbitrary subset of the known origins.
    require(sources in (PINS, PINS - {RTC_SOURCE}))
    return sources


def vendored_rtc_sources(files, packages, catalog):
    patches = S.toml_file(files, "Cargo.toml").get("patch", {}).get(RTC_REPOSITORY)
    require(type(patches) is dict and set(patches) == set(RTC_PROVENANCE))
    locked = [p for p in packages if p["name"].startswith("matrix-rtc-")]
    require(len(locked) == len(RTC_PROVENANCE)
            and {p["name"] for p in locked} == set(RTC_PROVENANCE)
            and all(p["version"] == RTC_VERSION and p.get("source") is None for p in locked))
    for name, provenance in RTC_PROVENANCE.items():
        path = "vendor/" + name
        require(patches[name] == {"path": path})
        local = catalog.get((name, RTC_VERSION))
        require(type(local) is dict and local["manifest"] == path + "/Cargo.toml"
                and {path + "/LICENSE", path + "/LICENSE-COMMERCIAL"}
                    <= set(local["license_text_candidates"]))
        body = files.get(path + "/" + provenance)
        require(type(body) is bytes and 0 < len(body) <= 64 * 1024)
        if provenance.endswith(".json"):
            record = S.unique_json(body)
            repository = "upstream_repository" if name == "matrix-rtc-core" else "repository"
            require(type(record) is dict and record.get(repository) == RTC_REPOSITORY
                    and record.get("revision") == RTC_REVISION)
            if name == "matrix-rtc-core":
                require(record.get("package") == name and record.get("version") == RTC_VERSION)
            elif name == "matrix-rtc-call":
                require(record.get("crate_path") == "crates/" + name and record.get("version") == RTC_VERSION)
            else:
                require(record.get("package_path") == "crates/" + name)
        else:
            require(RTC_REVISION.encode() in body and name.encode() in body and RTC_VERSION.encode() in body)


def source_inputs(files):
    packages = S.lock_packages(files)
    sources = lock_git_sources(packages)
    catalog = S.package_catalog(files)
    if RTC_SOURCE not in sources:
        vendored_rtc_sources(files, packages, catalog)
    # The manifest allowlist retains all three exact origins: locally patched
    # RTC dependencies still declare the original tag in their manifests.
    documents = {"Cargo.toml"} | {record["manifest"] for record in catalog.values()}
    expected = set()
    for source in PINS:
        url, query = source[4:].split("?", 1); kind, value = query.split("#", 1)[0].split("=", 1)
        expected.add((url, kind, value))
    def inspect(value):
        if type(value) is dict:
            if "git" in value:
                kinds = [key for key in ("rev", "tag", "branch") if key in value]
                require(len(kinds) == 1 and (value["git"], kinds[0], value[kinds[0]]) in expected)
            require("registry" not in value or value["registry"] == "crates-io")
            require("registry-index" not in value)
            for child in value.values(): inspect(child)
        elif type(value) is list:
            for child in value: inspect(child)
    for name in documents:
        document = S.toml_file(files, name)
        require(not document.get("replace") and set(document.get("patch", {})) <= {"crates-io", *(x[0] for x in expected)})
        inspect(document)
    return packages


def tools(environment):
    _notice_phase("rustup")
    host = {("darwin", "arm64"):"aarch64-apple-darwin", ("darwin", "x86_64"):"x86_64-apple-darwin",
            ("win32", "AMD64"):"x86_64-pc-windows-msvc"}.get((sys.platform, platform.machine()))
    require(host is not None)
    original_home = environment.get("USERPROFILE") if os.name == "nt" else environment.get("HOME")
    rustup_value = environment.get("RUSTUP_HOME")
    require(bool(rustup_value) or bool(original_home))
    home = Path(rustup_value) if rustup_value else Path(original_home)/".rustup"
    rustup = _notice_checked("rustup", home, directory=True)
    suffix = ".exe" if os.name == "nt" else ""
    directory = rustup / ("toolchains/1.99.0-" + host) / "bin"
    cargo, rustc = (_notice_checked(name, directory / (name + suffix)) for name in ("cargo", "rustc"))
    _notice_phase("git")
    git = system_git() if os.name == "nt" else _notice_checked("git", Path("/usr/bin/git"))
    return cargo, rustc, git, rustup


def environment_for_acquisition(environment, root, cargo, rustc, git, rustup):
    # An allowlist rather than a credential-name blocklist. No inherited Cargo,
    # SSH, proxy, signing, account, loader, wrapper or credential-provider state.
    system_key = "SYSTEMROOT" if os.name == "nt" else "SystemRoot"
    image_key = "IMAGEOS" if os.name == "nt" else "ImageOS"
    result = {key: value for key, value in environment.items() if key in {
        system_key, "WINDIR", "RUNNER_TEMP", "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS", image_key,
        "GITHUB_REPOSITORY", "GITHUB_REF", "GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_SHA",
        "GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR", "MORPHEUS_PACKAGE_SOURCE_REVISION"}}
    result = S.clean_git_environment(result)
    paths = [str(cargo.parent), str(git.parent)]
    if os.name == "nt":
        system = checked(Path(environment[system_key]) / "System32", directory=True)
        paths.append(str(system))
    else:
        paths += ["/usr/bin", "/bin"]
    result.update(HOME=str(root/"home"), USERPROFILE=str(root/"home"), CARGO_HOME=str(root/"cargo"),
        APPDATA=str(root/"home"), LOCALAPPDATA=str(root/"home"), TEMP=str(root/"temp"), TMP=str(root/"temp"),
        TMPDIR=str(root/"temp"), RUSTUP_HOME=str(rustup), RUSTUP_TOOLCHAIN="1.99.0", RUSTC=str(rustc),
        CARGO_TARGET_DIR=str(root/"target"), PATH=os.pathsep.join(paths),
        CARGO_NET_GIT_FETCH_WITH_CLI="false", CARGO_NET_RETRY="0", CARGO_HTTP_TIMEOUT="60",
        CARGO_REGISTRIES_CRATES_IO_PROTOCOL="sparse", CARGO_REGISTRIES_CRATES_IO_INDEX="sparse+https://index.crates.io/",
        GIT_ALLOW_PROTOCOL="https", RUST_BACKTRACE="0", RUST_LOG="warn", LC_ALL="C")
    return result


class Commands:
    def __init__(self, root, environment):
        self.root, self.environment = root, environment
        self.logs = root/"logs"; self.sequence = 0; self.cleanup_failed = False
        self.retained = []; self.bytes = 0; self.logs_complete = True; self.cargo_fetch_failure = None; self.deadline = time.monotonic() + 1800

    def run(self, stage, command, *, cwd, env, timeout, output_limit, stderr_limit=65536, log_root=None,
            expected_status=(0,), binary_output=False, stdin_bytes=None):
        require(self.sequence < 16000 and 0 < timeout <= 1200 and 0 < output_limit <= 128*1024**2
                and 0 < stderr_limit <= 8*1024**2)
        # Revalidate only the exact protected provider Git at EVERY spawn,
        # including all Windows ACLs. Source/scanner/data and all other tools
        # retain checked()'s strict single-link admission.
        if (os.name == "nt" and sys.platform == "win32"
                and str(command[0]) == str(Path(os.environ["PROGRAMFILES"]) / "Git/cmd/git.exe")):
            require(system_git() == Path(command[0]))
        else:
            checked(Path(command[0]))
        checked(cwd, directory=True)
        timeout = min(timeout, self.deadline - time.monotonic()); require(timeout > 0)
        self.sequence += 1
        output, errors = ((self.logs if log_root is None else log_root)/(str(self.sequence).zfill(5) + suffix)
                          for suffix in (".out", ".err"))
        process = job = handle = None
        status = None
        data = b""
        with contextlib.ExitStack() as stack:
            # Raw object stdout is source data, not a diagnostic log. Keep it
            # on one owned temporary descriptor and close/delete it on every
            # outcome. Complete stderr and ordinary stdout remain scan inputs.
            destinations = [
                stack.enter_context(tempfile.TemporaryFile(dir=self.root/"temp"))
                if binary_output else stack.enter_context(output.open("xb")),
                stack.enter_context(errors.open("xb")),
            ]
            readers, writers = [], []
            counts, eof, limits = [0,0], [False,False], [output_limit,stderr_limit]
            for destination in destinations:
                if os.name != "nt": os.fchmod(destination.fileno(),0o600)
                reader,writer=os.pipe()
                try:
                    # CPython >=3.12 supports nonblocking anonymous pipes on
                    # Windows; failure is refusal before any child is created.
                    os.set_blocking(reader,False)
                    readers.append(stack.enter_context(os.fdopen(reader,"rb",buffering=0)));reader=None
                    writers.append(stack.enter_context(os.fdopen(writer,"wb",buffering=0)));writer=None
                finally:
                    if reader is not None:os.close(reader)
                    if writer is not None:os.close(writer)
            input_stream = None
            if stdin_bytes is not None:
                require(type(stdin_bytes) is bytes and len(stdin_bytes) <= 1024*1024)
                input_stream=stack.enter_context(tempfile.TemporaryFile(dir=self.root/"temp"))
                input_stream.write(stdin_bytes);input_stream.seek(0)
            def collect():
                progress=False
                for index,reader in enumerate(readers):
                    if eof[index]:continue
                    try:data=os.read(reader.fileno(),65536)
                    except BlockingIOError:continue
                    if not data:eof[index]=True;continue
                    progress=True
                    remaining=limits[index]-counts[index]
                    destinations[index].write(data[:remaining]);counts[index]+=min(len(data),remaining)
                    if len(data)>remaining:
                        self.logs_complete=False
                        raise ValueError("Native source command output bound exceeded")
                return progress
            try:
                if os.name == "nt":
                    if __package__:from .run_windows_capture_acceptance import WindowsJob
                    else:from run_windows_capture_acceptance import WindowsJob
                    job=WindowsJob()
                    handle=job.spawn(list(map(str,command)),env,writers[0],cwd=cwd,
                                     stderr_stream=writers[1],stdin_stream=input_stream)
                else:
                    process=subprocess.Popen(list(map(str,command)),cwd=cwd,env=env,
                        stdin=subprocess.DEVNULL if input_stream is None else input_stream,
                        stdout=writers[0],stderr=writers[1],start_new_session=True)
                for writer in writers:writer.close()  # Only child copies remain.
                deadline=time.monotonic()+timeout
                while True:
                    progress=collect()
                    if os.name=="nt":
                        outcome=job.kernel.WaitForSingleObject(handle,0);require(outcome in (0,258))
                        if outcome==0:status=job.wait(handle,0);break
                    else:
                        if __package__:from . import package_smoke as smoke
                        else:import package_smoke as smoke
                        status=smoke.process_status(process)  # Exact unreaped child.
                        if status is not None:break
                    require(time.monotonic()<deadline)
                    if not progress:time.sleep(.005)
            finally:
                # Close parent writers even on a failed suspended creation.
                for writer in writers:writer.close()
                try:
                    if job is not None:job.close()
                    if process is not None:
                        if __package__:from .macos_package_ui import stop_group
                        else:from macos_package_ui import stop_group
                        stop_group(process)
                except BaseException:
                    self.cleanup_failed=True;self.retained.append((process,job));raise
                finally:
                    # Retained process cleanup precedes the EOF receipt. Never
                    # scan a capped prefix as a complete raw command log.
                    drain_deadline=time.monotonic()+2
                    try:
                        while not all(eof):
                            progress=collect()
                            if not progress:
                                require(time.monotonic()<drain_deadline);time.sleep(.005)
                    except BaseException:
                        self.logs_complete=False;raise
                    finally:
                        for destination in destinations:destination.flush();os.fsync(destination.fileno())
            if binary_output:
                # The owned child/group/Job and its pipes have already drained.
                # Read by the retained descriptor, never by a replaceable name.
                destinations[0].seek(0)
                data=destinations[0].read(output_limit+1)
                require(len(data)==counts[0] and len(data)<=output_limit)
        if stage == "fetch" and status not in expected_status:
            # This is after the owned Job/group stopped, pipes reached EOF and
            # both exact streams were flushed/fsynced/closed. Publish nothing
            # here; the complete default scan must succeed first.
            self.cargo_fetch_failure = (errors, status, counts[1])
        require(status in expected_status)
        if not binary_output:
            data=S.read_owned(output,output_limit,private=True) if counts[0] else b""
        self.bytes+=sum(counts);require(self.bytes<=4*1024**3)
        return data

    def scan(self, scanner):
        config, ignore = self.root/"scanner.toml", self.root/"scanner.ignore"
        if not config.exists(): write_new(config,b"[extend]\nuseDefault = true\n"); write_new(ignore,b"")
        # Outputs are outside logs, so the scanner cannot recursively consume its
        # own report. Reuse retained command ownership and separate raw streams.
        self.deadline = max(self.deadline, time.monotonic() + 120)
        scan_logs = self.root/"scanner-logs"
        private_directory(scan_logs, exist_ok=True)
        self.run("scan", [str(scanner),"dir","--no-banner","--config",str(config),
            "--gitleaks-ignore-path",str(ignore),"--ignore-gitleaks-allow","--redact=100",
            "--max-target-megabytes","0",str(self.logs)], cwd=self.root, env=self.environment,
            timeout=120, output_limit=65536, stderr_limit=1024*1024, log_root=scan_logs)


def check_cache(home):
    count = total = 0
    for directory, dirs, files in os.walk(home, followlinks=False):
        for name in dirs + files:
            path = Path(directory)/name; info = path.lstat(); count += 1
            total += info.st_size if not stat.S_ISDIR(info.st_mode) else 0
            require(count <= 300000 and total <= 4*1024**3 and info.st_size <= S.MAX_BUNDLE
                    and not getattr(info,"st_file_attributes",0) & 0x400
                    and (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)
                         or os.name != "nt" and stat.S_ISLNK(info.st_mode) and info.st_size <= 1024))
            if os.name != "nt": require(info.st_uid == os.getuid() and not info.st_mode & 0o022)


def input_plan(home, packages, runner, environment, git):
    caches = list((home/"registry/cache").iterdir())
    require(len(caches)==1 and re.fullmatch(r"index\.crates\.io-[0-9a-f]{16}",caches[0].name))
    cache = checked(caches[0],directory=True)
    expected = {p["name"]+"-"+p["version"]+".crate" for p in packages if p.get("source")==S.REGISTRY}
    require({p.name for p in cache.iterdir()} == expected)
    total = 0
    for package in packages:
        if package.get("source") != S.REGISTRY: continue
        data=S.read_owned(cache/(package["name"]+"-"+package["version"]+".crate"),S.MAX_ARCHIVE)
        total += len(data); require(total <= S.MAX_BUNDLE and S.digest(data)==package["checksum"])
    mirrors = []
    for path in (home/"git/db").iterdir():
        if path.name == ".cargo-lock" and stat.S_ISREG(path.lstat().st_mode): continue
        checked(path,directory=True); require(re.fullmatch(r"[A-Za-z0-9_.-]{1,128}",path.name)); mirrors.append(path)
    require(0 < len(mirrors) <= 16)
    result={}
    for source in sorted(lock_git_sources(packages)):
        revision=source.rsplit("#",1)[1]; matches=[]
        for mirror in mirrors:
            raw=runner.run("cache-commit",[*S.safe_git(str(git),mirror),"cat-file","commit",revision],
                cwd=mirror,env=S.clean_git_environment(environment),timeout=30,output_limit=1024*1024,
                stderr_limit=65536,expected_status=(0,1,128),binary_output=True)
            if S.object_id("commit",raw)==revision: matches.append(str(mirror))
        require(len(matches)==1);result[source]=matches[0]
    return {"schema":1,"registry_cache":str(cache),"git_mirrors":result}


def source_parent(environment):
    # A source tree requires the unchanged owned-source policy. The shared
    # provider TEMP/command directory is not a Windows source-tree parent.
    parent = Path.home().resolve(strict=True) if os.name == "nt" else Path(environment["RUNNER_TEMP"])
    return _notice_checked("temp_parent", parent, directory=True)


def owned_source_root(environment):
    parent = source_parent(environment)
    root = _notice_checked("private_root", Path(environment["MORPHEUS_NATIVE_SOURCE_ROOT"]),
                           directory=True, private=True)
    require(root.parent == parent and re.fullmatch(r"morpheus-native-source-[a-z0-9_]{8,32}", root.name))
    require(environment["MORPHEUS_NATIVE_SOURCE_BUNDLE"] == str(root/"output"/S.BUNDLE_NAME)
            and environment["MORPHEUS_NATIVE_SOURCE_RECEIPT"] == str(root/"receipt.json"))
    _notice_phase("owner_marker")
    marker = S.unique_json(S.read_owned(root/"owner.json", 4096, private=True))
    info = root.lstat()
    require(type(marker) is dict and all(type(marker.get(key)) is int for key in ("schema", "device", "inode"))
            and marker == {"schema":1, "device":info.st_dev, "inode":info.st_ino,
                           "source_revision":R.expected_revision(environment)})
    return root



def fetch_export(root, files, identity):
    """Copy authenticated source data only; Cargo never reads ambient checkout files."""
    target = private_directory(root/"fetch-source")
    require(set(files) == set(identity["files"]))
    names={}
    for name,data in sorted(files.items()):
        N.portable_source_name(name,names)
        entry=identity["files"][name]
        # Current supported workspace is regular-only. Do not invent Windows
        # symlink creation or follow a repository-provided link during fetch.
        require(entry["mode"] in {"100644","100755"} and S.digest(data)==entry["sha256"])
        path=target/name;private_directory(path.parent, parents=True, exist_ok=True)
        write_new(path,data)
        if os.name!="nt":path.chmod(0o700 if entry["mode"]=="100755" else 0o600)
    return target

def prepare(environment, scanner):
    guard(environment)
    root = owned_source_root(environment)
    # Only the immutable workflow controller creates/reserves this root. Never
    # reuse a partial acquisition or infer output destinations from borrowed data.
    require({path.name for path in root.iterdir()} == {"owner.json"})
    scanner=_notice_checked("scanner_path",scanner)
    _notice_phase("scanner_hash")
    require(S.digest(N.read_regular(scanner,32*1024**2))==SCANNERS[(sys.platform,platform.machine())])
    cargo,rustc,git,rustup=tools(environment)
    revision=R.expected_revision(environment)
    # The trusted controller already published the exact owned paths for the
    # always-cleanup step. Acquisition never opens a provider command file.
    _notice_phase("source")
    # Irreversible phase marker precedes every Commands/child admission. A
    # killed/partial acquisition can never resemble an untouched reservation.
    write_new(root/"acquisition-started.json", json.dumps({"schema":1, "source_revision":revision}).encode()+b"\n")
    for name in ("home","cargo","fetch","target","temp","logs","output"):private_directory(root/name)
    if os.name == "nt":
        # The pinned Cargo/libgit2 checkout reads this private HOME config.
        # Only working-tree path length support changes; CLI Git still has its
        # isolated null global config, and every external path/ACL guard stays.
        write_new(root/"home"/".gitconfig", b"[core]\n\tlongpaths = true\n")
        checked(root/"home"/".gitconfig", private=True)
    controlled=environment_for_acquisition(environment,root,cargo,rustc,git,rustup)
    commands=Commands(root,controlled);success=False;scanned=False
    try:
        for tool,prefix in ((cargo,b"cargo 1.99.0 "),(rustc,b"rustc 1.99.0 ")):
            version=commands.run("version",[str(tool),"--version"],cwd=root/"fetch",env=controlled,timeout=30,output_limit=256)
            require(version.startswith(prefix) and version.endswith(b"\n") and version.count(b"\n")==1)
        repository=workspace_repository(commands,root,revision,git,controlled)
        _,source,files=S.git_export(commands,repository,revision,str(git),controlled,S.Budget())
        packages=source_inputs(files)
        fetched_source=fetch_export(root,files,source)
        for directory in (root/"fetch",root,*root.parents):
            require(not any((directory/".cargo"/name).exists() for name in ("config","config.toml")))
        commands.run("fetch",[str(cargo),"--config","net.git-fetch-with-cli=false","--config","net.retry=0",
            "--config","registry.global-credential-providers=[]","fetch","--locked","--manifest-path",str(fetched_source/"Cargo.toml")],
            cwd=root/"fetch",env=controlled,timeout=1200,output_limit=8*1024**2,stderr_limit=8*1024**2)
        check_cache(root if os.name == "nt" else root/"cargo")
        plan=input_plan(root/"cargo",packages,commands,controlled,git)
        write_new(root/"plan.json",json.dumps(plan,sort_keys=True).encode()+b"\n")
        receipt=S.build(commands,root/"output",repository,revision,str(git),controlled,root/"plan.json")
        require(receipt["source_tree"]==source["tree"])
        write_new(root/"receipt.json",json.dumps(receipt,sort_keys=True).encode()+b"\n")
        success=True
    finally:
        try:
            commands.scan(scanner);scanned=True
            if os.name == "nt" and not success:
                _notice_report_cargo_fetch_failure(commands, scanned)
        finally:
            write_new(root/"state.json",json.dumps({"schema":1,"source_revision":revision,
                "prepared":success,"logs_scanned":scanned,"logs_complete":commands.logs_complete,
                "owned_processes_stopped":not commands.cleanup_failed,
                "distribution_ready":False},sort_keys=True).encode()+b"\n")
    require(success and scanned and commands.logs_complete and not commands.cleanup_failed)
    print(json.dumps({"check":"native_notice_sources","prepared":True,"distribution_ready":False},sort_keys=True))


def finish(environment, scanner):
    guard(environment)
    _notice_phase("cleanup")
    root = owned_source_root(environment)
    _notice_phase("cleanup")
    if {path.name for path in root.iterdir()} == {"owner.json"}:
        # Preparation failed before creating any acquisition directories, so no
        # Commands child could have started. Remove only this validated empty
        # reservation; do not manufacture log/stop receipts for an acquisition.
        (root/"owner.json").unlink()
        root.rmdir()
        print('{"check":"native_notice_source_reservation_removed","passed":true,"distribution_ready":false}')
        return
    state=S.unique_json(S.read_owned(root/"state.json",4096,private=True))
    require(set(state)=={"schema","source_revision","prepared","logs_scanned","logs_complete","owned_processes_stopped","distribution_ready"}
        and state["schema"]==1 and state["source_revision"]==R.expected_revision(environment)
        and state["owned_processes_stopped"] is True and state["logs_complete"] is True
        and state["distribution_ready"] is False)
    scanner=checked(scanner);require(S.digest(N.read_regular(scanner,32*1024**2))==SCANNERS[(sys.platform,platform.machine())])
    cargo,rustc,git,rustup=tools(environment)
    _notice_phase("cleanup")
    commands=Commands(root,environment_for_acquisition(environment,root,cargo,rustc,git,rustup))
    # Never overwrite prior command logs while rescanning a retained failed tree.
    commands.sequence=max((int(p.stem) for folder in (commands.logs,root/"scanner-logs")
                           for p in folder.glob("*.out")),default=0)
    commands.scan(scanner)
    require(not commands.cleanup_failed)
    # Stop receipt + exact private root identity authorizes only this tree.
    if os.name == "nt":
        require(owned_source_root(environment) == root)
        _notice_phase("cleanup")
        info = root.lstat(); root_identity = (info.st_dev, info.st_ino)
        shutil.rmtree(root, onexc=lambda function, path, error:
                      _remove_readonly_owned_leaf(root, root_identity, function, path, error))
    else:
        shutil.rmtree(root)
    print('{"check":"native_notice_sources_removed","passed":true,"distribution_ready":false}')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    selection=parser.add_mutually_exclusive_group(required=True)
    selection.add_argument("--prepare",action="store_true");selection.add_argument("--finish",action="store_true")
    parser.add_argument("--gitleaks",type=Path,required=True);args=parser.parse_args()
    try:
        (prepare if args.prepare else finish)(dict(os.environ),args.gitleaks)
    except BaseException as error:
        print("Native notice source preparation failed; private evidence retained.",file=sys.stderr)
        _notice_report_failure(error)
        raise SystemExit(1) from None


if __name__=="__main__":main()
