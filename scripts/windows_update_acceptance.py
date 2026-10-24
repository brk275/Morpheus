"""Explicit private standard-host Windows installed-update mechanism acceptance.

Two version-only native builds use the existing producer, NSIS template, signer
and retained-handle Windows Job route. No release keys, profiles, vault, accounts,
UI or media are used. Shipping activation and broader beta gates stay closed.
"""
from __future__ import annotations
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import time
import tomllib

if __package__:
    from . import package as package, native_notices as notices
    from . import package_revision as revision
    from . import prepare_windows_update as producer
    from . import run_windows_capture_acceptance as capture
else:
    import package
    import native_notices as notices
    import package_revision as revision
    import prepare_windows_update as producer
    import run_windows_capture_acceptance as capture

ROOT = Path(__file__).resolve().parents[1]
CASES = ("invalid-signature", "truncated", "discard", "staged-mutation", "call-exclusion",
         "failed-trial", "interrupted-marker", "upgrade")
KEY = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Morpheus"
MANUFACTURER = r"Software\Morpheus\Morpheus"
LIMIT = 512 * 1024 * 1024
PLAN = "MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_PLAN"
PLAN_HASH = PLAN + "_SHA256"
ENTRY_MAGIC = b"MORPHEUS-WINDOWS-UPDATE-QA-V1\n"

class Failure(RuntimeError):
    def __init__(self):
        super().__init__("Private installed Windows update acceptance failed")

def require(value):
    if not value:
        raise Failure()

def boolean(value, expected):
    return value is expected or type(value) is str and value == str(expected).lower()

def guard(environment, platform=sys.platform):
    require(platform == "win32")
    for name, value in {"GITHUB_ACTIONS":"true", "RUNNER_ENVIRONMENT":"github-hosted", "RUNNER_OS":"Windows",
        "GITHUB_REPOSITORY":"brk275/Morpheus", "GITHUB_REF":"refs/heads/main", "GITHUB_EVENT_NAME":"workflow_dispatch",
        "GITHUB_ACTOR":"brk275", "GITHUB_TRIGGERING_ACTOR":"brk275"}.items():
        require(environment.get(name) == value)
    require(environment.get("IMAGEOS") in {"win25", "win25-vs2026"})
    event = json.loads(read(Path(environment["GITHUB_EVENT_PATH"]), 1024 * 1024))
    owner = event.get("repository", {}).get("owner", {}).get("id")
    require(type(owner) is int and owner > 0 and event.get("sender", {}).get("id") == owner
            and event.get("repository", {}).get("private") is False
            and event.get("inputs", {}).get("platform") in {"windows", "all"}
            and boolean(event.get("inputs", {}).get("source_bundle"), False))

def checked(path, *, directory=False):
    path = Path(path)
    require(path.is_absolute() and path.resolve(strict=True) == path)
    for item in (path, *path.parents):
        info = item.lstat()
        require(not stat.S_ISLNK(info.st_mode) and not getattr(info, "st_file_attributes", 0) & 0x400)
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode) and info.st_nlink == 1)
    return path

def read(path, limit):
    path = checked(path)
    require(0 < path.stat().st_size <= limit)
    with path.open("rb") as source:
        data = source.read(limit + 1)
    require(0 < len(data) <= limit)
    return data

def digest(path, limit=LIMIT):
    path = checked(path)
    before = path.stat()
    require(0 < before.st_size <= limit)
    value, count = hashlib.sha256(), 0
    with path.open("rb") as source:
        for data in iter(lambda: source.read(65536), b""):
            value.update(data); count += len(data)
            require(count <= limit)
    after = path.stat()
    require(count == before.st_size and (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns)
            == (after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns))
    return value.hexdigest()

def write_new(path, data):
    with path.open("xb") as stream:
        stream.write(data); stream.flush(); os.fsync(stream.fileno())
    path.chmod(0o600)

def private_environment(environment, home):
    # CPython Windows os.environ.items() exposes canonical uppercase keys.
    # Keep the same closed toolchain allowlist when copying that real mapping.
    names = {"PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "PROGRAMFILES", "PROGRAMFILES(X86)",
        "PROGRAMW6432", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS", "INCLUDE", "LIB", "LIBPATH",
        "VCINSTALLDIR", "VSINSTALLDIR", "VCTOOLSINSTALLDIR", "WINDOWSSDKDIR", "WINDOWSSDKVERSION",
        "UNIVERSALCRTSDKDIR", "UCRTVERSION", "CARGO_HOME", "RUSTUP_HOME", "RUNNER_TEMP", "IMAGEOS",
        "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS", "GITHUB_REPOSITORY", "GITHUB_REF",
        "GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_SHA", "GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR",
        "MORPHEUS_PACKAGE_SOURCE_REVISION"}
    result = {k:v for k,v in environment.items() if k in names}
    # Preserve actual fresh-host toolchain locations before replacing HOME.
    original = Path(environment["USERPROFILE"])
    result.setdefault("CARGO_HOME", str(original / ".cargo"))
    result.setdefault("RUSTUP_HOME", str(original / ".rustup"))
    for name in ("home", "roaming", "local", "temp"):
        (home / name).mkdir(parents=True, exist_ok=True)
    result.update(HOME=str(home/"home"), USERPROFILE=str(home/"home"), APPDATA=str(home/"roaming"),
        LOCALAPPDATA=str(home/"local"), TEMP=str(home/"temp"), TMP=str(home/"temp"),
        RUST_LOG="warn", RUST_BACKTRACE="0", CARGO_INCREMENTAL="0", CARGO_BUILD_JOBS="2",
        RUSTUP_TOOLCHAIN="1.99.0", GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
    return result

class Commands:
    def __init__(self, root, environment):
        self.root, self.environment = root, environment
        self.logs = root / "logs"; self.logs.mkdir()
        self.sequence = 0
        self.cleanup_failed = False
        self.deadline = time.monotonic() + 6900
    def close_job(self, job):
        try:
            job.close()
        except BaseException:
            self.cleanup_failed = True
            raise
    def start(self, job, args, environment=None, cwd=ROOT):
        checked(cwd, directory=True)
        self.sequence += 1
        log = self.logs / (str(self.sequence).zfill(4) + ".log")
        with log.open("xb") as stream:
            log.chmod(0o600)
            handle = job.spawn(list(map(str,args)), self.environment | (environment or {}), stream, cwd=cwd)
        return handle, log
    def bounded_logs(self):
        total = 0
        logs = list(self.logs.glob("*.log"))
        require(len(logs) <= 128)
        for log in logs:
            size = checked(log).stat().st_size
            require(size <= 8 * 1024 * 1024)
            total += size
        require(total <= 64 * 1024 * 1024)
    def wait(self, job, handle, seconds):
        end = min(self.deadline, time.monotonic() + seconds)
        while True:
            self.bounded_logs()
            state = job.kernel.WaitForSingleObject(handle, 0)
            if state == 0:
                return job.wait(handle, 0)
            require(state == 258 and time.monotonic() < end)
            time.sleep(.025)
    def empty(self, job, seconds=30):
        end = min(self.deadline, time.monotonic()+seconds)
        while job.active():
            self.bounded_logs()
            require(time.monotonic() < end)
            time.sleep(.025)
        self.bounded_logs()
    def run(self, args, *, environment=None, cwd=ROOT, seconds=180):
        job = capture.WindowsJob()
        try:
            handle, log = self.start(job,args,environment,cwd)
            require(self.wait(job, handle, seconds) == 0)
            self.empty(job)
            require(log.stat().st_size <= 8*1024*1024)
            return log.read_bytes()
        finally:
            self.close_job(job)
    def scan(self, scanner):
        config, ignore = self.root/"default-gitleaks.toml", self.root/"empty.ignore"
        write_new(config,b"[extend]\nuseDefault = true\n"); write_new(ignore,b"")
        failed = False
        for log in sorted(self.logs.glob("*.log")):
            try:
                require(log.stat().st_size <= 8*1024*1024)
                with checked(log).open("rb") as stream:
                    result = subprocess.run([str(scanner),"stdin","--config",str(config),"--gitleaks-ignore-path",str(ignore),
                        "--ignore-gitleaks-allow","--redact=100","--no-banner"],stdin=stream,
                        stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,cwd=self.root,
                        env=self.environment,timeout=60,check=False)
                if result.returncode: failed = True
            except (Failure,OSError,subprocess.SubprocessError):
                failed = True
        require(not failed)

def export_source(commands, source, archive, version, label):
    destination = commands.root / (label+"-source"); destination.mkdir()
    with tarfile.open(fileobj=io.BytesIO(archive),mode="r:") as tar:
        for member in tar.getmembers():
            name = Path(member.name)
            require(not name.is_absolute() and ".." not in name.parts and (member.isdir() or member.isfile()))
        tar.extractall(destination,filter="data")
    for name in revision.UPDATE_VERSION_PATHS:
        path = destination/name
        path.write_bytes(revision.versioned_source_bytes(name,path.read_bytes(),version))
    proof = revision.verify_versioned_export(destination,archive,version)
    return destination,proof

def instrumented_entry(data, nonce):
    """Inspect only PE bytes; never probe an unknown CLI by launching it.

    The nonce section is emitted inside main's compile-only FIRST dispatch
    statement. Omitted build/main instrumentation has no such section. The
    ordinary shipping five-field identity and format remain unchanged.
    """
    require(type(nonce) is str and re.fullmatch(r"[0-9a-f]{32}",nonce))
    identity = producer.pe_identity(data)  # Full existing native section/table bounds.
    expected = ENTRY_MAGIC + nonce.encode("ascii")
    pe, = struct.unpack_from("<I",data,60)
    count, = struct.unpack_from("<H",data,pe+6)
    optional, = struct.unpack_from("<H",data,pe+20)
    table = pe+24+optional
    found = 0
    for index in range(count):
        offset = table+index*40
        if data[offset:offset+8] != b".mrphqa\0":
            continue
        used, = struct.unpack_from("<I",data,offset+8)
        size,start = struct.unpack_from("<II",data,offset+16)
        flags, = struct.unpack_from("<I",data,offset+36)
        require(used==len(expected) and used<=size
                and flags & 0xE2000040 == 0x40000040
                and data[start:start+used]==expected and not any(data[start+used:start+size]))
        found += 1
    require(found==1)
    return identity


def build(commands, source, archive, version, nonce, label, cargo, notice_bundle, notice_receipt, reviewed):
    exported,proof = export_source(commands,source,archive,version,label)
    notice_dir=commands.root/(label+"-notices"); notice_dir.mkdir(mode=0o700)
    notice_contents=notices.prepare(notice_bundle,notice_receipt,notice_dir,revision=reviewed,
                                    platform="win32",version=version,working=exported)
    target = commands.root/"native-target"
    environment = {"MORPHEUS_WINDOWS_UPDATE_ACCEPTANCE_BUILD":nonce,"MORPHEUS_PACKAGE_FORMAT":"nsis-payload-v2"}
    commands.run([cargo,"build","--release","--locked","-p","morpheus-app","--features","native-calls",
        "--target-dir",target],environment=environment,cwd=exported,seconds=3600)
    require(revision.verify_versioned_export(exported,archive,version) == proof)
    binary = exported/"target/release/morpheus-app.exe"
    binary.parent.mkdir(parents=True); shutil.copyfile(target/"release/morpheus-app.exe",binary)
    identity = instrumented_entry(read(binary,LIMIT),nonce); require(identity["version"] == version)
    output = commands.root/(label+"-package"); output.mkdir()
    config = package.package_config(exported,"win32")
    config.update(version=version,**{"out-dir":str(output)})
    stage = commands.root/(label+"-producer"); stage.mkdir()
    prepared,payload = producer.prepare(config,exported,stage,notice_contents=notice_contents)
    commands.run([cargo,"packager","--release","--formats","nsis","--config",json.dumps(prepared)],
        environment=environment,cwd=exported,seconds=900)
    setups = list(output.glob("*.exe")); require(len(setups)==1)
    final = output/payload.name; shutil.copyfile(payload,final)
    return {"setup":checked(setups[0]),"payload":checked(final),"identity":identity,
        "executable_sha256":digest(binary),"setup_sha256":digest(setups[0]),"payload_sha256":digest(final),"source":proof}

def absent_registry():
    import winreg as W
    for hive in (W.HKEY_CURRENT_USER,W.HKEY_LOCAL_MACHINE):
        for view in (W.KEY_WOW64_64KEY,W.KEY_WOW64_32KEY):
            for name in (KEY,MANUFACTURER):
                try:
                    handle = W.OpenKey(hive,name,0,W.KEY_READ|view)
                except FileNotFoundError:
                    continue
                else:
                    W.CloseKey(handle)
                    raise Failure()

def registration(install, versions):
    import winreg as W
    with W.OpenKey(W.HKEY_CURRENT_USER,KEY,0,W.KEY_READ|W.KEY_WOW64_64KEY) as handle:
        values = {}
        count = W.QueryInfoKey(handle)[1]
        require(count == 9)
        for index in range(count):
            name,value,kind = W.EnumValue(handle,index)
            require(name not in values); values[name]=(value,kind)
        expected={"DisplayName":"Morpheus","Publisher":"Morpheus",
            "InstallLocation":'"'+str(install)+'"',"UninstallString":'"'+str(install/"uninstall.exe")+'"',
            "DisplayIcon":'"'+str(install/"morpheus-app.exe")+'"'}
        require(set(values)==set(expected)|{"DisplayVersion","NoModify","NoRepair","EstimatedSize"})
        require(all(values[k]==(v,W.REG_SZ) for k,v in expected.items()))
        require(values["DisplayVersion"][0] in versions and values["DisplayVersion"][1]==W.REG_SZ)
        require(values["NoModify"]==(1,W.REG_DWORD) and values["NoRepair"]==(1,W.REG_DWORD)
                and values["EstimatedSize"][1]==W.REG_DWORD and type(values["EstimatedSize"][0]) is int)
    with W.OpenKey(W.HKEY_CURRENT_USER,MANUFACTURER,0,W.KEY_READ|W.KEY_WOW64_64KEY) as handle:
        require(W.QueryValueEx(handle,"") == (str(install),W.REG_SZ))
    return values

def fact(case, name, phase, identity, nonce):
    value=json.loads(read(case/name,4096))
    require(value=={"schema":1,"nonce":nonce,"case":case.name,"phase":phase,
        "version":identity["version"],"cohort":identity["persistence"]})

def await_file(path, job, seconds=15):
    end=time.monotonic()+seconds
    while not path.exists():
        require(job.active()>0 and time.monotonic()<end)
        time.sleep(.025)
    checked(path)

def actor_environment(case, plan, base):
    raw=json.dumps(plan,sort_keys=True,separators=(",",":")).encode()+b"\n"
    write_new(case/"plan.json",raw)
    return private_environment(base,case)|{PLAN:str(case/"plan.json"),PLAN_HASH:hashlib.sha256(raw).hexdigest()}

class ManufacturerFixture:
    """Retained handles to exact synthetic keys on the already admitted runner.

    These four uninstall cases use the same old setup. Foreign named/default
    values and a subkey must survive the real uninstaller; only our exact nonce
    values are later removed, and only count-proven empty fixture keys deleted.
    """
    MODES = {"invalid-signature":"named-values", "truncated":"foreign-default", "discard":"subkey",
             "call-exclusion":"vendor-value"}
    def __init__(self, install, case, nonce):
        import winreg as W
        self.W, self.mode = W, self.MODES[case]
        self.name, self.value = "Morpheus-update-qa-"+nonce, "synthetic-preserved-"+nonce
        self.product, self.vendor, self.child = None, None, None
        try:
            access = W.KEY_READ|W.KEY_SET_VALUE|W.KEY_CREATE_SUB_KEY|W.KEY_WOW64_64KEY
            self.product = W.OpenKey(W.HKEY_CURRENT_USER,MANUFACTURER,0,access)
            self.vendor = W.OpenKey(W.HKEY_CURRENT_USER,r"Software\Morpheus",0,access)
            require(W.QueryInfoKey(self.product)[:2]==(0,1)
                    and W.QueryValueEx(self.product,"")==(str(install),W.REG_SZ)
                    and W.QueryInfoKey(self.vendor)[:2]==(1,0)
                    and W.EnumKey(self.vendor,0)=="Morpheus")
            if self.mode=="vendor-value":
                W.SetValueEx(self.vendor,self.name,0,W.REG_SZ,self.value)
                # Let the genuine uninstaller fully remove the empty product
                # child; the retained vendor key then has a value, no subkeys.
                W.CloseKey(self.product); self.product=None
            elif self.mode=="named-values":
                W.SetValueEx(self.product,self.name,0,W.REG_SZ,self.value)
                W.SetValueEx(self.vendor,self.name,0,W.REG_SZ,self.value)
            elif self.mode=="foreign-default":
                W.SetValueEx(self.product,"",0,W.REG_SZ,self.value)
            else:
                try:
                    existing = W.OpenKey(self.product,self.name,0,W.KEY_READ)
                except FileNotFoundError:
                    pass
                else:
                    W.CloseKey(existing); raise Failure()
                self.child = W.CreateKeyEx(self.product,self.name,0,W.KEY_READ|W.KEY_SET_VALUE)
                require(W.QueryInfoKey(self.child)[:2]==(0,0))
                W.SetValueEx(self.child,self.name,0,W.REG_SZ,self.value)
        except BaseException:
            self.close(); raise
    def exact_value(self, handle, name):
        require(self.W.QueryValueEx(handle,name)==(self.value,self.W.REG_SZ))
    def finish(self):
        W = self.W
        require(W.QueryInfoKey(self.vendor)[:2]==(0 if self.mode=="vendor-value" else 1,
                                                1 if self.mode in {"named-values","vendor-value"} else 0))
        if self.mode=="vendor-value":
            try:
                unexpected = W.OpenKey(W.HKEY_CURRENT_USER,MANUFACTURER,0,W.KEY_READ|W.KEY_WOW64_64KEY)
            except FileNotFoundError:
                pass
            else:
                W.CloseKey(unexpected); raise Failure()
            self.exact_value(self.vendor,self.name)
            W.DeleteValue(self.vendor,self.name)
            require(W.QueryInfoKey(self.vendor)[:2]==(0,0))
            W.CloseKey(self.vendor); self.vendor=None
            W.DeleteKeyEx(W.HKEY_CURRENT_USER,r"Software\Morpheus",W.KEY_WOW64_64KEY,0)
            return
        if self.mode=="foreign-default":
            require(W.QueryInfoKey(self.product)[:2]==(0,1))
            self.exact_value(self.product,"")
            W.DeleteValue(self.product,"")
        else:
            try:
                W.QueryValueEx(self.product,"")
            except FileNotFoundError:
                pass
            else:
                raise Failure()
            if self.mode=="named-values":
                require(W.QueryInfoKey(self.product)[:2]==(0,1))
                self.exact_value(self.product,self.name); self.exact_value(self.vendor,self.name)
                W.DeleteValue(self.product,self.name); W.DeleteValue(self.vendor,self.name)
            else:
                require(W.QueryInfoKey(self.product)[:2]==(1,0) and W.QueryInfoKey(self.child)[:2]==(0,1))
                self.exact_value(self.child,self.name)
                W.DeleteValue(self.child,self.name)
                require(W.QueryInfoKey(self.child)[:2]==(0,0))
                W.CloseKey(self.child); self.child=None
                W.DeleteKeyEx(self.product,self.name,W.KEY_WOW64_64KEY,0)
        require(W.QueryInfoKey(self.product)[:2]==(0,0))
        W.CloseKey(self.product); self.product=None
        W.DeleteKeyEx(W.HKEY_CURRENT_USER,MANUFACTURER,W.KEY_WOW64_64KEY,0)
        require(W.QueryInfoKey(self.vendor)[:2]==(0,0))
        W.CloseKey(self.vendor); self.vendor=None
        W.DeleteKeyEx(W.HKEY_CURRENT_USER,r"Software\Morpheus",W.KEY_WOW64_64KEY,0)
    def close(self):
        failed = False
        for name in ("child","product","vendor"):
            handle = getattr(self,name,None)
            if handle is not None:
                try: self.W.CloseKey(handle)
                except OSError: failed=True
                setattr(self,name,None)
        require(not failed)


def uninstall(commands, case, uninstaller_hash, versions, environment, preservation=None):
    install=case/"installed"
    if not install.exists():
        absent_registry(); return
    registration(install,versions)
    require(digest(install/"uninstall.exe",16*1024*1024)==uninstaller_hash)
    state=case/"installed.morpheus-update"
    require(not (state/"journal.json").exists())
    fixture = ManufacturerFixture(install,case.name,preservation) if preservation is not None else None
    try:
        commands.run([install/"uninstall.exe","/S"],environment=environment,cwd=case,seconds=90)
        require(not install.exists() and not state.exists())
        if fixture is not None: fixture.finish()
        absent_registry()
    finally:
        if fixture is not None: fixture.close()

def cleanup_case(commands, case, old, new, uninstaller_hash, environment):
    """Settle only our authenticated transaction, then use the real uninstaller.

    A single copied-recovery startup may exit and relaunch the restored image;
    the separate terminal probe establishes cleanup before uninstall. Unknown
    identity/registry/journal state stays private and failed on the fresh runner.
    """
    require(not commands.cleanup_failed)
    install = case / "installed"
    if not install.exists():
        absent_registry()
        return
    require(digest(install / "morpheus-app.exe") in {old["executable_sha256"],new["executable_sha256"]})
    registration(install, {old["identity"]["version"],new["identity"]["version"]})
    require(digest(install / "uninstall.exe",16*1024*1024) == uninstaller_hash)
    if (case / "installed.morpheus-update/journal.json").exists():
        commands.run([install / "morpheus-app.exe","--morpheus-windows-qa-recover"],
                     environment=environment,cwd=case,seconds=90)
    commands.run([install / "morpheus-app.exe","--morpheus-windows-qa-cleanup"],
                 environment=environment,cwd=case,seconds=45)
    require(not (case / "installed.morpheus-update/journal.json").exists())
    uninstall(commands,case,uninstaller_hash,{old["identity"]["version"],new["identity"]["version"]},environment)


def run_case(commands, case_name, old, new, nonce, signatures):
    absent_registry()
    case=commands.root/case_name; case.mkdir()
    install=case/"installed"
    base=private_environment(commands.environment,case)
    environment, uninstall_hash = None, None
    completed = False
    try:
        commands.run([old["setup"],"/S","/NS","/D="+str(install)],environment=base,cwd=case,seconds=90)
        require(digest(install/"morpheus-app.exe")==old["executable_sha256"])
        registration(install,{old["identity"]["version"]})
        uninstall_hash=digest(install/"uninstall.exe",16*1024*1024)
        plan={"schema":1,"nonce":nonce,"case":case_name,"root":str(case),"install":str(install),
            "payload":str(new["payload"]),"payload_sha256":new["payload_sha256"],"old":old["identity"],"new":new["identity"],
            "old_executable_sha256":old["executable_sha256"],"new_executable_sha256":new["executable_sha256"],
            "signature":signatures[1],"wrong_signature":signatures[0],"public_key":signatures[2]}
        environment=actor_environment(case,plan,base)
        job=capture.WindowsJob()
        try:
            handle,_=commands.start(job,[install/"morpheus-app.exe","--morpheus-windows-qa-stage"],environment,cwd=case)
            require(commands.wait(job,handle,30)==0)
            handoff = case_name in {"failed-trial","interrupted-marker","upgrade"}
            fact(case,"stage.json","handed-off" if handoff else "unchanged",old["identity"],nonce)
            if case_name=="interrupted-marker":
                await_file(case/"checkpoint.json",job)
                fact(case,"checkpoint.json","marker-durable-before-exe",old["identity"],nonce)
                commands.close_job(job)  # Deliberate whole-owned-Job interruption, never a PID.
                job=capture.WindowsJob()
                handle,_=commands.start(job,[install/"morpheus-app.exe"],environment,cwd=case)
                require(commands.wait(job,handle,30)==0)
            elif case_name=="upgrade":
                await_file(case/"trial.json",job)
                fact(case,"trial.json","committed-before-store",new["identity"],nonce)
                write_new(case/"release-trial",nonce.encode())
            commands.empty(job,90)
        finally:
            commands.close_job(job)
        if case_name=="failed-trial":
            fact(case,"trial.json","rejected-before-store",new["identity"],nonce)
        if case_name in {"failed-trial","interrupted-marker"}:
            fact(case,"restart.json","restarted",old["identity"],nonce)
        commands.run([install/"morpheus-app.exe","--morpheus-windows-qa-probe"],environment=environment,cwd=case,seconds=45)
        expected=new if case_name=="upgrade" else old
        fact(case,"probe.json","terminal-clean",expected["identity"],nonce)
        require(digest(install/"morpheus-app.exe")==expected["executable_sha256"])
        registration(install,{expected["identity"]["version"]})
        preservation = nonce if case_name in ManufacturerFixture.MODES else None
        uninstall(commands,case,uninstall_hash,{old["identity"]["version"],new["identity"]["version"]},environment,preservation)
        completed = True
        return {"case":case_name,"passed":True,"installed_executable_sha256":expected["executable_sha256"],
                "uninstaller_sha256":uninstall_hash,"registry_contract_verified":True,
                "manufacturer_preservation":ManufacturerFixture.MODES.get(case_name,"empty-owned-key-cleanup")}
    finally:
        if not completed and install.exists():
            require(environment is not None and uninstall_hash is not None)
            cleanup_case(commands,case,old,new,uninstall_hash,environment)

def remove_owned_root(root, identity):
    info=root.lstat(); require((info.st_dev,info.st_ino)==identity)
    count=0
    for directory, dirs, files in os.walk(root,followlinks=False):
        for name in dirs+files:
            path=Path(directory)/name; info=path.lstat(); count+=1
            require(count<=300000 and not stat.S_ISLNK(info.st_mode)
                    and not getattr(info,"st_file_attributes",0)&0x400)
    shutil.rmtree(root)
    require(not root.exists())

def execute(args):
    guard(os.environ)
    require(args.execute_on_free_hosted_runner)
    source=checked(args.source,directory=True)
    reviewed=revision.verify_checkout(source,revision.expected_revision(os.environ))
    scanner=checked(args.gitleaks)
    require(type(args.gitleaks_sha256) is str and re.fullmatch(r"[0-9a-f]{64}",args.gitleaks_sha256)
            and digest(scanner,32*1024*1024)==args.gitleaks_sha256)
    nonce=secrets.token_hex(16)
    root=Path(os.environ["RUNNER_TEMP"]).resolve()/ ("morpheus-windows-updateqa-"+nonce)
    root.mkdir(); identity=(root.stat().st_dev,root.stat().st_ino)
    commands=Commands(root,private_environment(os.environ,root/"tools-home"))
    stage,results,scan,clean="source-export",[],False,False
    keys = {}
    try:
        # Use the already provisioned exact toolchain binary, not a rustup
        # proxy that could select or auto-install an ambient toolchain.
        cargo=checked(Path(commands.environment["RUSTUP_HOME"]) /
                      "toolchains/1.99.0-x86_64-pc-windows-msvc/bin/cargo.exe")
        git=checked(Path(shutil.which("git") or "").resolve())
        require(commands.run([cargo,"packager","--version"]).strip()==b"cargo-packager 0.11.8")
        archive_path=root/"source.tar"
        commands.run([git,"archive","--format=tar","--output="+str(archive_path),reviewed],cwd=source)
        archive=read(archive_path,revision.MAX_EXPORT_BYTES)
        config=tomllib.loads((source/"crates/morpheus-app/Cargo.toml").read_text())
        version=config["package"]["version"].split("-",1)[0]
        stage="build-old"; old=build(commands,source,archive,version+"-updateqa.1",nonce,"old",cargo,args.native_source_bundle,args.native_source_receipt,reviewed)
        stage="build-new"; new=build(commands,source,archive,version+"-updateqa.2",nonce,"new",cargo,args.native_source_bundle,args.native_source_receipt,reviewed)
        require({k:v for k,v in old["identity"].items() if k!="version"}
                =={k:v for k,v in new["identity"].items() if k!="version"})
        stage="signing"; key=root/"fixture.key"
        commands.run([cargo,"packager","signer","generate","--ci","--path",key],
            environment={"CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD":""})
        for path in (key,key.with_name(key.name+".pub")):
            checked(path).chmod(0o600)
            info = path.stat()
            keys[path] = ((info.st_dev,info.st_ino),digest(path,4096))
        signatures=[]
        for item in (old,new):
            commands.run([cargo,"packager","signer","sign",item["payload"]],environment={
                "CARGO_PACKAGER_SIGN_PRIVATE_KEY":str(key),"CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD":""})
            signatures.append(read(item["payload"].with_name(item["payload"].name+".sig"),4096).decode().strip())
        signatures.append(read(key.with_name(key.name+".pub"),4096).decode().strip())
        for case in CASES:
            stage=case; results.append(run_case(commands,case,old,new,nonce,signatures))
        # Reinstall/uninstall is exercised by every following genuine NSIS case.
        stage="complete"
    finally:
        try:
            commands.scan(scanner); scan=True
        finally:
            # Generated fixture credentials are removed even if a transaction
            # remains pending; never remove a changed/foreign file or registry.
            key_failure = False
            for path, (identity_key, expected_hash) in keys.items():
                try:
                    info = checked(path).stat()
                    require((info.st_dev,info.st_ino)==identity_key and digest(path,4096)==expected_hash)
                    path.unlink()
                except (Failure,OSError):
                    key_failure = True
            require(not key_failure)
            # A pending/foreign registration retains this owned tree privately;
            # such a cleanup is a failure, never an installed acceptance pass.
            require(not commands.cleanup_failed)
            absent_registry()
            remove_owned_root(root,identity); clean=True
    require(stage=="complete" and len(results)==len(CASES) and scan and clean)
    print(json.dumps({"check":"windows_installed_update_mechanism","passed":True,"source_revision":reviewed,
        "cohort":old["identity"]["persistence"],"old_source":old["source"],"new_source":new["source"],
        "old_setup_sha256":old["setup_sha256"],
        "new_setup_sha256":new["setup_sha256"],"payload_sha256":new["payload_sha256"],"cases":results,
        "cleanup_passed":True,"private_log_scan_passed":True,"production_activation":False,
        "normal_ui_exit_verified":False,"live_calls_verified":False,"http_download_verified":False,
        "distribution_ready":False},sort_keys=True),flush=True)

def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source",type=Path,required=True)
    parser.add_argument("--native-source-bundle",type=Path,required=True)
    parser.add_argument("--native-source-receipt",type=Path,required=True)
    parser.add_argument("--gitleaks",type=Path,required=True)
    parser.add_argument("--gitleaks-sha256",required=True,help="Exact independently reviewed host scanner SHA256")
    parser.add_argument("--execute-on-free-hosted-runner",action="store_true")
    execute(parser.parse_args(argv))

if __name__=="__main__":
    try:
        main()
    except BaseException:
        print('MORPHEUS_WINDOWS_UPDATE_QA {"passed":false,"diagnostics":"withheld","distribution_ready":false}',file=sys.stderr,flush=True)
        raise SystemExit(1)
