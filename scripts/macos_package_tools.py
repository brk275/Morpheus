"""Exact create-dmg input for ordinary unsigned QA on fresh hosted Macs."""
from __future__ import annotations

import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
import subprocess

ROOT = Path(__file__).resolve().parents[1]
URL = "https://raw.githubusercontent.com/create-dmg/create-dmg/28867ba3563ddef62f55dcf130677103b4296c42/create-dmg"
SCRIPT_BYTES = 16911
SCRIPT_SHA256 = "d4be62531a0d4d4524d34fe0599b4ebdddd6f664931b4e0832536d1f0fd92b26"
INVALID = "Mac packaging tool identity or fresh-runner boundary rejected"


def require(value):
    if not value:
        raise ValueError(INVALID)


def unique_json(data):
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result)
            result[key] = value
        return result
    return json.loads(data, object_pairs_hook=pairs)


def hosted_guard(environment):
    require(sys.platform == "darwin" and os.getuid() != 0)
    for key, value in {"GITHUB_ACTIONS": "true", "CI": "true",
                       "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "macOS",
                       "GITHUB_REPOSITORY": "brk275/Morpheus", "GITHUB_REF": "refs/heads/main",
                       "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_ACTOR": "brk275",
                       "GITHUB_TRIGGERING_ACTOR": "brk275"}.items():
        require(environment.get(key) == value)
    require(re.fullmatch(r"[0-9a-f]{40}", environment.get("MORPHEUS_PACKAGE_SOURCE_REVISION", "")))
    temporary = Path(environment.get("RUNNER_TEMP", ""))
    directory(temporary)
    event_path = Path(environment.get("GITHUB_EVENT_PATH", ""))
    require(event_path.is_absolute() and event_path.name == "event.json"
            and event_path.is_relative_to(temporary)
            and ".." not in event_path.parts)
    directory(event_path.parent)
    raw, _ = regular(event_path, 512 * 1024)
    event = unique_json(raw)
    require(type(event) is dict)
    repository, sender = event.get("repository"), event.get("sender")
    require(type(repository) is dict and type(sender) is dict
            and repository.get("full_name") == "brk275/Morpheus"
            and repository.get("private") is False)
    owner = repository.get("owner")
    require(type(owner) is dict and type(owner.get("id")) is int and owner["id"] > 0
            and type(sender.get("id")) is int and owner["id"] == sender["id"])
    inputs = event.get("inputs")
    require(type(inputs) is dict and inputs.get("platform") in {"macos", "macos-arm64", "macos-x64", "all"})
    selected = environment["MORPHEUS_PACKAGE_SOURCE_REVISION"]
    revision = inputs.get("source_revision")
    require(revision == selected or revision == "" and selected == environment.get("GITHUB_SHA"))
    flags = {}
    for key in ("native_updates", "appimage_updates", "appimage_capture", "source_bundle"):
        value = inputs.get(key)
        require(type(value) is bool or type(value) is str and value in {"true", "false"})
        flags[key] = value is True or value == "true"
    require(not flags["native_updates"])
    # Reuse the exact existing selector; unrelated Linux choices in an all
    # request must not disable the ordinary Mac worker.
    if __package__:
        from .package_scope import package_matrix
    else:
        from package_scope import package_matrix
    package_matrix(inputs["platform"], inputs.get("linux_formats"),
                   flags["appimage_updates"], flags["appimage_capture"],
                   flags["source_bundle"], flags["native_updates"])
    require(not any(environment.get(key) for key in
                    ("MORPHEUS_MAC_UPDATE_ACCEPTANCE_BUILD", "MORPHEUS_MAC_UPDATE_ACCEPTANCE_PLAN",
                     "CARGO_PACKAGER_SIGN_PRIVATE_KEY", "CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD",
                     "APPLE_SIGNING_IDENTITY", "APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD")))


def directory(path):
    path = Path(path)
    require(path.is_absolute())
    for parent in reversed((path, *path.parents)):
        info = parent.lstat()
        require(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode)
                and info.st_uid in {0, os.getuid()} and not info.st_mode & 0o022)
    info = path.lstat()
    require(info.st_uid == os.getuid())
    return (info.st_dev, info.st_ino)


def regular(path, maximum, expected_mode=None, *, allow_empty=False):
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_uid == os.getuid()
            and before.st_nlink == 1 and not before.st_mode & 0o022
            and (0 if allow_empty else 1) <= before.st_size <= maximum)
    if expected_mode is not None:
        require(stat.S_IMODE(before.st_mode) == expected_mode)
    def stamp(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_nlink,
                info.st_size, info.st_mtime_ns, info.st_ctime_ns)
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        opened = os.fstat(stream.fileno())
        require(stamp(before) == stamp(opened))
        body = stream.read(maximum + 1)
        require(stamp(opened) == stamp(os.fstat(stream.fileno())))
    require(stamp(before) == stamp(path.lstat()) and len(body) == before.st_size)
    return body, (before.st_dev, before.st_ino)


def pins():
    directory(ROOT / "scripts")
    raw, _ = regular(ROOT / "scripts/macos_package_tools.json", 4096)
    value = unique_json(raw)
    require(type(value) is dict and set(value) == {"schema", "packager_version", "script", "support"}
            and type(value["schema"]) is int and value["schema"] == 1
            and value["packager_version"] == "0.11.8"
            and value["script"] == {"url": URL, "bytes": SCRIPT_BYTES, "sha256": SCRIPT_SHA256})
    support = value["support"]
    require(type(support) is dict and set(support) == {"template.applescript", "eula-resources-template.xml"})
    for record in support.values():
        require(type(record) is dict and set(record) == {"sha256", "bytes"}
                and type(record["bytes"]) is int and 0 < record["bytes"] <= 4096
                and re.fullmatch(r"[0-9a-f]{64}", record["sha256"]))
    return value


def download():
    import resource
    # A fixed system tool, not a PATH lookup or candidate-supplied executable.
    for path in (Path("/"), Path("/usr"), Path("/usr/bin"), Path("/usr/bin/curl")):
        info = path.lstat()
        require(not stat.S_ISLNK(info.st_mode) and info.st_uid == 0
                and not info.st_mode & 0o022)
        require(stat.S_ISREG(info.st_mode) if path.name == "curl" else stat.S_ISDIR(info.st_mode))
    require(info.st_mode & 0o111)
    command = ["/usr/bin/curl", "--disable", "--fail", "--silent", "--show-error",
               "--proxy", "", "--noproxy", "*", "--proto", "=https",
               "--proto-redir", "=https", "--max-redirs", "0", "--connect-timeout", "10",
               "--max-time", "30", "--max-filesize", str(SCRIPT_BYTES), URL]
    def file_limit():
        resource.setrlimit(resource.RLIMIT_FSIZE, (SCRIPT_BYTES + 1, SCRIPT_BYTES + 1))
    # The output is an unlinked private data file. RLIMIT_FSIZE bounds even
    # chunked/lying responses; parent timeout kills and reaps curl at 35 seconds.
    # No inherited proxy, auth, loader, certificate or home configuration exists.
    with tempfile.TemporaryFile() as stream:
        subprocess.run(command, stdin=subprocess.DEVNULL, stdout=stream,
                       stderr=subprocess.DEVNULL, check=True, timeout=35,
                       env={"PATH": "/usr/bin:/bin", "LANG": "C"}, preexec_fn=file_limit)
        stream.seek(0)
        body = stream.read(SCRIPT_BYTES + 1)
    require(len(body) == SCRIPT_BYTES and hashlib.sha256(body).hexdigest() == SCRIPT_SHA256)
    return body


def fresh_mount():
    # Never enter create-dmg's legacy branch that detaches a preexisting volume.
    require(not os.path.lexists("/Volumes/Morpheus"))


class PreparedTools:
    def __init__(self, root, identity, script_identity, expected):
        self.root, self.identity = root, identity
        self.script_identity, self.expected = script_identity, expected

    def verify(self, *, package_succeeded):
        fresh_mount()
        require(directory(self.root) == self.identity)
        require({p.name for p in self.root.iterdir()} <= {"script", "share"})
        require({p.name for p in (self.root / "script").iterdir()} == {"create-dmg"})
        directory(self.root / "script")
        body, identity = regular(self.root / "script/create-dmg", SCRIPT_BYTES, 0o700)
        require(identity == self.script_identity and len(body) == SCRIPT_BYTES
                and hashlib.sha256(body).hexdigest() == SCRIPT_SHA256)
        share = self.root / "share"
        if not os.path.lexists(share):
            require(not package_succeeded)
            return
        for current, children in [(share, {"create-dmg"}),
                                  (share / "create-dmg", {"support"})]:
            directory(current)
            require({p.name for p in current.iterdir()} == children)
        support = share / "create-dmg/support"
        directory(support)
        actual = {p.name for p in support.iterdir()}
        require(actual <= set(self.expected["support"]))
        if package_succeeded:
            require(actual == set(self.expected["support"]))
        for name in actual:
            record = self.expected["support"][name]
            body, _ = regular(support / name, record["bytes"])
            require(len(body) == record["bytes"] and hashlib.sha256(body).hexdigest() == record["sha256"])


def prepare(environment):
    hosted_guard(environment)  # No filesystem inspection before hosted/ordinary admission.
    expected = pins()
    fresh_mount()
    home = Path(environment.get("HOME", ""))
    directory(home)
    cache = home / "Library/Caches"
    directory(cache)
    parent = cache / ".cargo-packager"
    if not os.path.lexists(parent):
        parent.mkdir(mode=0o700)
    directory(parent)
    root = parent / "DMG"
    require(not os.path.lexists(root))
    body = download()  # Authenticate bytes before any executable file exists.
    stage = Path(tempfile.mkdtemp(prefix="morpheus-dmg-", dir=parent))
    stage_identity = directory(stage)
    script_directory_identity = None
    staged_file_identity = None
    try:
        (stage / "script").mkdir(mode=0o700)
        script_directory_identity = directory(stage / "script")
        with (stage / "script/create-dmg").open("xb") as stream:
            info = os.fstat(stream.fileno())
            staged_file_identity = (info.st_dev, info.st_ino)
            os.fchmod(stream.fileno(), 0o600)
            stream.write(body)
            stream.flush()
            os.fsync(stream.fileno())
        require(regular(stage / "script/create-dmg", SCRIPT_BYTES, 0o600)[0] == body)
        os.chmod(stage / "script/create-dmg", 0o700, follow_symlinks=False)
        # mkdir and link are exclusive: never replace even a raced empty tree.
        root.mkdir(mode=0o700)
        (root / "script").mkdir(mode=0o700)
        os.link(stage / "script/create-dmg", root / "script/create-dmg", follow_symlinks=False)
        (stage / "script/create-dmg").unlink()
        state = PreparedTools(root, directory(root),
                              regular(root / "script/create-dmg", SCRIPT_BYTES, 0o700)[1], expected)
        state.verify(package_succeeded=False)
        fresh_mount()
        return state
    finally:
        failed = sys.exc_info()[0] is not None
        try:
            require(directory(stage) == stage_identity)
            require({p.name for p in stage.iterdir()} <= {"script"})
            scripts = stage / "script"
            if os.path.lexists(scripts):
                require(directory(scripts) == script_directory_identity)
                require({p.name for p in scripts.iterdir()} <= {"create-dmg"})
                script = scripts / "create-dmg"
                if os.path.lexists(script):
                    _, identity = regular(script, SCRIPT_BYTES + 1, allow_empty=True)
                    require(identity == staged_file_identity)
                    script.unlink()
                scripts.rmdir()
            stage.rmdir()
        except Exception:
            # Unknown/replaced paths are retained on the disposable runner.
            # A cleanup error never conceals the original bounded failure.
            print('{"check":"macos_packager_tool_staging_cleanup","passed":false}', flush=True)
            if not failed:
                raise ValueError(INVALID) from None



@contextlib.contextmanager
def ordinary_tools(environment=None):
    try:
        state = prepare(os.environ if environment is None else environment)
    except Exception:
        raise ValueError(INVALID) from None
    print('{"check":"macos_packager_tool_preseed","passed":true}', flush=True)
    succeeded = False
    try:
        yield
        succeeded = True
    finally:
        # A failure remains a failure; unknown or mutated output is never trusted.
        try:
            state.verify(package_succeeded=succeeded)
        except Exception:
            raise ValueError(INVALID) from None
        print('{"check":"macos_packager_tool_postverify","passed":true}', flush=True)
