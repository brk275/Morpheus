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
SCANNERS = {
    ("darwin", "arm64"): "ba52fb1bfabbcde42f032afad3d6e0b19dff8ed105229a16e7caa338bbc0e84f",
    ("darwin", "x86_64"): "cee01fea7173f1b779dff188e1c26ecbcb4027d394acc573b23aaf0be260e291",
    ("win32", "AMD64"): "17157e2ee8b76fc8b1d8bee607a250e34b8a8023c8bc81822d4b5ee4d78fcb7c",
}


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


def write_new(path, data):
    with Path(path).open("xb") as stream:
        if os.name != "nt": os.fchmod(stream.fileno(), 0o600)
        stream.write(data); stream.flush(); os.fsync(stream.fileno())


def guard(environment, system=sys.platform):
    require(system in {"darwin", "win32"} and sys.version_info >= (3,12))
    for key, value in {"GITHUB_ACTIONS":"true", "RUNNER_ENVIRONMENT":"github-hosted",
            "GITHUB_REPOSITORY":"brk275/Morpheus", "GITHUB_REF":"refs/heads/main",
            "GITHUB_EVENT_NAME":"workflow_dispatch", "GITHUB_ACTOR":"brk275", "GITHUB_TRIGGERING_ACTOR":"brk275",
            "RUNNER_OS":"macOS" if system == "darwin" else "Windows"}.items():
        require(environment.get(key) == value)
    require(environment.get("ImageOS") in ({"macos15", "macos26"} if system == "darwin" else {"win25", "win25-vs2026"}))
    event = S.unique_json(N.read_regular(Path(environment["GITHUB_EVENT_PATH"]), 1024 * 1024))
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


def source_inputs(files):
    # Same closed lock/origin policy as the unchanged Linux source-only route.
    packages = S.lock_packages(files)
    require({p["source"] for p in packages if p.get("source", "").startswith("git+")} == PINS)
    documents = {"Cargo.toml"} | {record["manifest"] for record in S.package_catalog(files).values()}
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
    host = {("darwin", "arm64"):"aarch64-apple-darwin", ("darwin", "x86_64"):"x86_64-apple-darwin",
            ("win32", "AMD64"):"x86_64-pc-windows-msvc"}.get((sys.platform, platform.machine()))
    require(host is not None)
    original_home = environment.get("USERPROFILE") if os.name == "nt" else environment.get("HOME")
    rustup_value = environment.get("RUSTUP_HOME")
    require(bool(rustup_value) or bool(original_home))
    home = Path(rustup_value) if rustup_value else Path(original_home)/".rustup"
    rustup = checked(home, directory=True)
    suffix = ".exe" if os.name == "nt" else ""
    directory = rustup / ("toolchains/1.99.0-" + host) / "bin"
    cargo, rustc = (checked(directory / (name + suffix)) for name in ("cargo", "rustc"))
    if os.name == "nt":
        program = Path(environment["ProgramFiles"])
        git = checked(program / "Git/cmd/git.exe")
    else:
        git = checked(Path("/usr/bin/git"))
    return cargo, rustc, git, rustup


def environment_for_acquisition(environment, root, cargo, rustc, git, rustup):
    # An allowlist rather than a credential-name blocklist. No inherited Cargo,
    # SSH, proxy, signing, account, loader, wrapper or credential-provider state.
    result = {key: value for key, value in environment.items() if key in {
        "SystemRoot", "WINDIR", "RUNNER_TEMP", "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS", "ImageOS",
        "GITHUB_REPOSITORY", "GITHUB_REF", "GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_SHA",
        "GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR", "MORPHEUS_PACKAGE_SOURCE_REVISION"}}
    result = S.clean_git_environment(result)
    paths = [str(cargo.parent), str(git.parent)]
    if os.name == "nt":
        system = checked(Path(environment["SystemRoot"]) / "System32", directory=True)
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
        self.retained = []; self.bytes = 0; self.logs_complete = True; self.deadline = time.monotonic() + 1800

    def run(self, stage, command, *, cwd, env, timeout, output_limit, stderr_limit=65536, log_root=None,
            expected_status=(0,), binary_output=False, stdin_bytes=None):
        require(self.sequence < 16000 and 0 < timeout <= 1200 and 0 < output_limit <= 128*1024**2
                and 0 < stderr_limit <= 8*1024**2)
        checked(Path(command[0])); checked(cwd, directory=True)
        timeout = min(timeout, self.deadline - time.monotonic()); require(timeout > 0)
        self.sequence += 1
        output, errors = ((self.logs if log_root is None else log_root)/(str(self.sequence).zfill(5) + suffix)
                          for suffix in (".out", ".err"))
        process = job = handle = None
        status = None
        with contextlib.ExitStack() as stack:
            destinations = [stack.enter_context(path.open("xb")) for path in (output,errors)]
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
        require(status in expected_status)
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
        scan_logs.mkdir(mode=0o700, exist_ok=True)
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
    for source in sorted(PINS):
        revision=source.rsplit("#",1)[1]; matches=[]
        for mirror in mirrors:
            raw=runner.run("cache-commit",[*S.safe_git(str(git),mirror),"cat-file","commit",revision],
                cwd=mirror,env=S.clean_git_environment(environment),timeout=30,output_limit=1024*1024,
                stderr_limit=65536,expected_status=(0,1,128),binary_output=True)
            if S.object_id("commit",raw)==revision: matches.append(str(mirror))
        require(len(matches)==1);result[source]=matches[0]
    return {"schema":1,"registry_cache":str(cache),"git_mirrors":result}


def publish_environment(root, environment):
    path = checked(Path(environment["GITHUB_ENV"]))
    require(not any(c in str(root) for c in "\r\n\0") and path.stat().st_size < 1024*1024)
    with path.open("ab") as stream:
        stream.write(("MORPHEUS_NATIVE_SOURCE_ROOT="+str(root)+"\n").encode())
        stream.write(("MORPHEUS_NATIVE_SOURCE_BUNDLE="+str(root/"output"/S.BUNDLE_NAME)+"\n").encode())
        stream.write(("MORPHEUS_NATIVE_SOURCE_RECEIPT="+str(root/"receipt.json")+"\n").encode())
        stream.flush();os.fsync(stream.fileno())



def fetch_export(root, files, identity):
    """Copy authenticated source data only; Cargo never reads ambient checkout files."""
    target = root/"fetch-source";target.mkdir(mode=0o700)
    require(set(files) == set(identity["files"]))
    names={}
    for name,data in sorted(files.items()):
        N.portable_source_name(name,names)
        entry=identity["files"][name]
        # Current supported workspace is regular-only. Do not invent Windows
        # symlink creation or follow a repository-provided link during fetch.
        require(entry["mode"] in {"100644","100755"} and S.digest(data)==entry["sha256"])
        path=target/name;path.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
        write_new(path,data)
        if os.name!="nt":path.chmod(0o700 if entry["mode"]=="100755" else 0o600)
    return target

def prepare(environment, scanner):
    guard(environment)
    scanner=checked(scanner); require(S.digest(N.read_regular(scanner,32*1024**2))==SCANNERS[(sys.platform,platform.machine())])
    cargo,rustc,git,rustup=tools(environment)
    parent=checked(Path(environment["RUNNER_TEMP"]),directory=True)
    root = S.windows_security().create_root(parent) if os.name=="nt" else Path(tempfile.mkdtemp(prefix="morpheus-native-source-",dir=parent))
    if os.name!="nt":root.chmod(0o700)
    info=root.lstat();revision=R.expected_revision(environment)
    write_new(root/"owner.json",json.dumps({"schema":1,"device":info.st_dev,"inode":info.st_ino,"source_revision":revision}).encode()+b"\n")
    # Publish before acquisition so the always-step can scan and remove a failed
    # owned tree. No bundle/receipt consumer runs if this step fails.
    publish_environment(root,environment)
    for name in ("home","cargo","fetch","target","temp","logs","output"):(root/name).mkdir(mode=0o700)
    controlled=environment_for_acquisition(environment,root,cargo,rustc,git,rustup)
    commands=Commands(root,controlled);success=False;scanned=False
    try:
        for tool,prefix in ((cargo,b"cargo 1.99.0 "),(rustc,b"rustc 1.99.0 ")):
            version=commands.run("version",[str(tool),"--version"],cwd=root/"fetch",env=controlled,timeout=30,output_limit=256)
            require(version.startswith(prefix) and version.endswith(b"\n") and version.count(b"\n")==1)
        _,source,files=S.git_export(commands,ROOT,revision,str(git),controlled,S.Budget())
        packages=source_inputs(files)
        fetched_source=fetch_export(root,files,source)
        for directory in (root/"fetch",root,*root.parents):
            require(not any((directory/".cargo"/name).exists() for name in ("config","config.toml")))
        commands.run("fetch",[str(cargo),"--config","net.git-fetch-with-cli=false","--config","net.retry=0",
            "--config","registry.global-credential-providers=[]","fetch","--locked","--manifest-path",str(fetched_source/"Cargo.toml")],
            cwd=root/"fetch",env=controlled,timeout=1200,output_limit=8*1024**2,stderr_limit=8*1024**2)
        check_cache(root/"cargo")
        plan=input_plan(root/"cargo",packages,commands,controlled,git)
        write_new(root/"plan.json",json.dumps(plan,sort_keys=True).encode()+b"\n")
        receipt=S.build(commands,root/"output",ROOT,revision,str(git),controlled,root/"plan.json")
        require(receipt["source_tree"]==source["tree"])
        write_new(root/"receipt.json",json.dumps(receipt,sort_keys=True).encode()+b"\n")
        success=True
    finally:
        try:commands.scan(scanner);scanned=True
        finally:
            write_new(root/"state.json",json.dumps({"schema":1,"source_revision":revision,
                "prepared":success,"logs_scanned":scanned,"logs_complete":commands.logs_complete,
                "owned_processes_stopped":not commands.cleanup_failed,
                "distribution_ready":False},sort_keys=True).encode()+b"\n")
    require(success and scanned and commands.logs_complete and not commands.cleanup_failed)
    print(json.dumps({"check":"native_notice_sources","prepared":True,"distribution_ready":False},sort_keys=True))


def finish(environment, scanner):
    guard(environment)
    parent=checked(Path(environment["RUNNER_TEMP"]),directory=True)
    root=checked(Path(environment["MORPHEUS_NATIVE_SOURCE_ROOT"]),directory=True,private=True)
    require(root.parent==parent and re.fullmatch(r"morpheus-native-source-[a-z0-9_]{8,32}",root.name))
    marker=S.unique_json(S.read_owned(root/"owner.json",4096,private=True)); info=root.lstat()
    require(marker=={"schema":1,"device":info.st_dev,"inode":info.st_ino,"source_revision":R.expected_revision(environment)})
    state=S.unique_json(S.read_owned(root/"state.json",4096,private=True))
    require(set(state)=={"schema","source_revision","prepared","logs_scanned","logs_complete","owned_processes_stopped","distribution_ready"}
        and state["schema"]==1 and state["source_revision"]==R.expected_revision(environment)
        and state["owned_processes_stopped"] is True and state["logs_complete"] is True
        and state["distribution_ready"] is False)
    scanner=checked(scanner);require(S.digest(N.read_regular(scanner,32*1024**2))==SCANNERS[(sys.platform,platform.machine())])
    cargo,rustc,git,rustup=tools(environment)
    commands=Commands(root,environment_for_acquisition(environment,root,cargo,rustc,git,rustup))
    # Never overwrite prior command logs while rescanning a retained failed tree.
    commands.sequence=max((int(p.stem) for folder in (commands.logs,root/"scanner-logs")
                           for p in folder.glob("*.out")),default=0)
    commands.scan(scanner)
    require(not commands.cleanup_failed)
    # Stop receipt + exact private root identity authorizes only this tree.
    shutil.rmtree(root)
    print('{"check":"native_notice_sources_removed","passed":true,"distribution_ready":false}')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    selection=parser.add_mutually_exclusive_group(required=True)
    selection.add_argument("--prepare",action="store_true");selection.add_argument("--finish",action="store_true")
    parser.add_argument("--gitleaks",type=Path,required=True);args=parser.parse_args()
    try:
        (prepare if args.prepare else finish)(dict(os.environ),args.gitleaks)
    except BaseException:
        print("Native notice source preparation failed; private evidence retained.",file=sys.stderr)
        raise SystemExit(1) from None


if __name__=="__main__":main()
