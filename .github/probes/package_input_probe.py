"""Temporary owner-only hosted data retrieval proof. Never merge this driver."""
from pathlib import Path
import hashlib, json, os, shutil, signal, stat, subprocess, sys, tempfile, time
SOURCE = "93b08331aea82ce5358a548b44b82bb37f79b4c1"
BRANCH = "codex/package-input-probe-93b083-20261022"
SCANNER_SHA256 = "88f91962aa2f93ac6ab281d553b9e125f5197bbbce38f9f2437f7299c32e5509"

def child(source, root):
    sys.path.insert(0, str(source / "packaging/linux"))
    import package_appimage as P
    P.containment.assert_contained()
    progress = P.diagnostics.Progress(root)
    runner = P.OwnedRunner(root / "commands", progress=progress)
    try:
        progress.wrapper("build")
        environment = P.controlled_environment(os.environ)
        spec = P.runtime_builder.manifest()
        pin = spec["archives"]["zlib"]
        class InspectedRunner(P.OwnedRunner):
            def run(self, stage, argv, **options):
                output = super().run(stage, argv, **options)
                if stage != "download-runtime-zlib":
                    return output
                # Capture fixed metadata only, after the real owned-group drain.
                match = P.download.__globals__["PACKAGE_INPUT_STATUS"].search(output)
                payload = output[:match.start()] if match else b""
                facts = {"frame": match is not None, "http": 0, "curl": 0,
                         "received_bytes": len(payload), "size_matches": False, "digest_matches": False}
                if match is not None:
                    http, code = (int(item) for item in match.groups())
                    facts.update(http=http if 0 <= http <= 599 else 0,
                                 curl=code if 0 <= code <= 99 else 0,
                                 size_matches=len(payload) == pin["size"],
                                 digest_matches=hashlib.sha256(payload).hexdigest() == pin["sha256"])
                temporary = root / ("transport-%d.json" % self.sequence)
                P.private_receipt(temporary, facts)
                os.replace(temporary, root / "transport.json")
                return output  # Original bytes enter the unchanged production validator.
        runner = InspectedRunner(root / "inspected-commands", progress=progress)
        payloads, archives, patch = P.download_inputs(runner, root, environment)
        del payloads, archives, patch
        count = 6
        for key, item in spec["archives"].items():
            value = P.runtime_builder.download(runner, url=item["url"], maximum=item["size"],
                expected_size=item["size"], expected_sha256=item["sha256"], root=root,
                environment=environment, stage="download-runtime-" + key)
            del value
            count += 1
        if count != 13:
            raise P.PackageFailure("identity")
        progress.wrapper("complete")
        progress.cleanup(True)
        P.private_receipt(root / "passed.json", {"source": SOURCE, "inputs": count, "passed": True})
        return 0
    except BaseException as error:
        progress.failed(error)
        return 1

def scan(root, scanner):
    config, ignore = root / "scanner.toml", root / "empty.ignore"
    config.write_text("[extend]\nuseDefault = true\n")
    ignore.write_bytes(b"")
    for path in sorted(root.rglob("*.log")):
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1
                or info.st_mode & 0o077 or not 0 <= info.st_size <= 8 * 1024 * 1024):
            raise RuntimeError("scan")
        with path.open("rb") as raw:
            result = subprocess.run([str(scanner), "stdin", "--config", str(config),
                "--gitleaks-ignore-path", str(ignore), "--ignore-gitleaks-allow", "--redact=100",
                "--max-target-megabytes", "0", "--no-banner"], stdin=raw,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=root, timeout=30, check=False,
                env={"PATH": "/usr/bin:/bin", "HOME": str(root)})
        if result.returncode != 0:
            raise RuntimeError("scan")

def parent(source, scanner):
    expected = {"GITHUB_ACTIONS":"true", "RUNNER_ENVIRONMENT":"github-hosted", "RUNNER_OS":"Linux",
                "GITHUB_REPOSITORY":"brk275/Morpheus", "GITHUB_REF":"refs/heads/" + BRANCH,
                "GITHUB_EVENT_NAME":"workflow_dispatch", "GITHUB_ACTOR":"brk275",
                "GITHUB_TRIGGERING_ACTOR":"brk275", "GITHUB_ACTOR_ID":"88346297"}
    if any(os.environ.get(key) != value for key, value in expected.items()):
        raise RuntimeError("ineligible")
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    if (event.get("repository", {}).get("private") is not False
            or event.get("repository", {}).get("owner", {}).get("id") != 88346297
            or event.get("sender", {}).get("id") != 88346297
            or hashlib.sha256(scanner.read_bytes()).hexdigest() != SCANNER_SHA256):
        raise RuntimeError("ineligible")
    sys.path.insert(0, str(source / "scripts"))
    import element_process_namespace as C
    sys.path.insert(0, str(source / "packaging/linux"))
    import appimage_diagnostics as diagnostics
    root = Path(tempfile.mkdtemp(prefix="morpheus-input-proof-", dir=os.environ["RUNNER_TEMP"]))
    os.chmod(root, 0o700)
    facts = {"source": SOURCE, "passed": False, "inputs": 0, "cleanup": False, "scanned": False}
    process = None
    owned_cleanup = True
    try:
        for name in ("home", "tmp"):
            (root / name).mkdir(mode=0o700)
        sterile = {"PATH":"/usr/bin:/bin", "HOME":str(root / "home"), "TMPDIR":str(root / "tmp"),
                   "LANG":"C.UTF-8", "LC_ALL":"C.UTF-8", "GITHUB_ACTIONS":"true", "RUNNER_OS":"Linux"}
        # Same PID namespace and detached-descendant kill proof used by package QA.
        original = dict(os.environ)
        try:
            os.environ.clear()
            os.environ.update(sterile)
            C.containment_check(root / "probe-namespace")
        finally:
            os.environ.clear()
            os.environ.update(original)
        command = C.namespace_command([sys.executable, "-I", "-B", str(Path(__file__).resolve()),
            "inside", str(source), str(root)], root / "namespace", sterile)
        fd = os.open(root / "namespace.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as output:
            process = subprocess.Popen(command, env=sterile, cwd=root, stdin=subprocess.DEVNULL,
                stdout=output, stderr=subprocess.STDOUT, start_new_session=True, umask=0o077)
            deadline = time.monotonic() + 1700
            while (code := C.status(process)) is None:
                if time.monotonic() >= deadline or (root / "namespace.log").stat().st_size > 1024 * 1024:
                    raise RuntimeError("bound")
                time.sleep(0.05)
        if code != 0:
            raise RuntimeError("child")
        receipt = root / "passed.json"
        info = receipt.lstat()
        if (not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_nlink != 1
                or info.st_uid != os.getuid() or not 0 < info.st_size <= 1024
                or json.loads(receipt.read_bytes()) != {"source":SOURCE, "inputs":13, "passed":True}):
            raise RuntimeError("receipt")
        facts.update(passed=True, inputs=13)
    except BaseException:
        facts["passed"] = False
    finally:
        if process is not None:
            try:
                C.stop_owned(process)
            except BaseException:
                owned_cleanup = False
                facts["passed"] = False
        try:
            scan(root, scanner)
            facts["scanned"] = True
            transport = root / "transport.json"
            if transport.exists():
                info = transport.lstat()
                if (not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_nlink != 1
                        or info.st_uid != os.getuid() or not 0 < info.st_size <= 1024):
                    raise RuntimeError("transport")
                value = json.loads(transport.read_bytes())
                if (type(value) is not dict or set(value) != {"frame", "http", "curl", "received_bytes", "size_matches", "digest_matches"}
                        or any(type(value[key]) is not bool for key in ("frame", "size_matches", "digest_matches"))
                        or any(type(value[key]) is not int for key in ("http", "curl", "received_bytes"))
                        or not 0 <= value["http"] <= 599 or not 0 <= value["curl"] <= 99
                        or not 0 <= value["received_bytes"] <= 64 * 1024 * 1024):
                    raise RuntimeError("transport")
                facts["transport"] = value
            if not facts["passed"]:
                facts["failure"] = diagnostics.public_failure(root, scan_passed=True,
                    outer_cleanup="passed" if owned_cleanup else "failed", outer_reason="namespace_exit")
        except BaseException:
            facts.update(passed=False, scanned=False)
        try:
            shutil.rmtree(root)
            facts["cleanup"] = owned_cleanup
        except BaseException:
            facts.update(passed=False, cleanup=False)
    # No raw log, URL, server body, signed redirect or exception is disclosed.
    print("MORPHEUS_PACKAGE_INPUTS " + json.dumps(facts, sort_keys=True), flush=True)
    return 0 if facts["passed"] and facts["scanned"] and facts["cleanup"] else 1

def interrupted(_signal, _frame):
    raise KeyboardInterrupt

if __name__ == "__main__":
    os.umask(0o077)
    signal.signal(signal.SIGTERM, interrupted)
    if len(sys.argv) != 4 or sys.argv[1] not in ("parent", "inside"):
        raise SystemExit(1)
    source = Path(sys.argv[2]).resolve(strict=True)
    raise SystemExit(child(source, Path(sys.argv[3])) if sys.argv[1] == "inside"
                     else parent(source, Path(sys.argv[3]).resolve(strict=True)))
