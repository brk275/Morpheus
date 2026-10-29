#!/usr/bin/env python3
"""Manual Mac ARM/Intel DATA producer for the existing native Unix builder.

No SDK is copied to the handoff. Exact Mac source/runtime/host inputs and a
separate build admission are mandatory. Missing/UNBOUND inputs fail closed.
"""
from __future__ import annotations
import argparse
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import sys
import tarfile
import types

REPOSITORY = "brk275/Morpheus"
BASELINE = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
COMMON_SHA = "abae5a24963b232423e12d906a26dfec1b47b14fc778c2f2f4b47ec1ab086a79"
BUILDER_SHA = "e664095571eb5239fecdc9410b77f2e34fc7907da9ae67d399ba53206547b1fb"
MAC_HELPER_SHA = "3c526b2fe1b1618b212f0b37ee060d191912f1a0f5e784a18dc3c513d2c1457f"
TARGETS = {"mac-arm64": ("ARM64", "arm64", "MacARM", "macos-15"),
           "mac-x64": ("X64", "x86_64", "MacIntel", "macos-15-intel")}

def module(path, expected, name):
    before = path.lstat()
    if not __import__("stat").S_ISREG(before.st_mode) or before.st_size > 256*1024: raise ValueError()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        opened = os.fstat(fd)
        with os.fdopen(fd, "rb", closefd=False) as source: raw = source.read(256*1024+1)
        after = os.fstat(fd); final = path.lstat()
        identity = lambda item: (item.st_dev,item.st_ino,item.st_size,item.st_mtime_ns,item.st_ctime_ns)
        if identity(before) != identity(opened) or identity(opened) != identity(after) or identity(after) != identity(final): raise ValueError()
    finally: os.close(fd)
    if len(raw) > 256 * 1024 or hashlib.sha256(raw).hexdigest() != expected: raise ValueError()
    result = types.ModuleType(name); result.__file__ = str(path)
    sys.modules[name] = result
    exec(compile(raw, str(path), "exec"), result.__dict__)
    return result

def runner(target, common):
    need = common.need
    arch, machine, _, _ = TARGETS[target]
    need(os.name == "posix" and platform.system() == "Darwin" and platform.machine() == machine
         and os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
         and os.environ.get("RUNNER_OS") == "macOS" and os.environ.get("RUNNER_ARCH") == arch
         and os.environ.get("GITHUB_REPOSITORY") == REPOSITORY and os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
         and os.environ.get("GITHUB_REF") == "refs/heads/main" and os.environ.get("GITHUB_ACTOR") == "brk275"
         and os.environ.get("GITHUB_TRIGGERING_ACTOR") == "brk275")
    repo = common.api("/repos/" + REPOSITORY)
    need(repo["full_name"] == REPOSITORY and repo["private"] is False and repo["default_branch"] == "main")
    need(sys.version_info >= (3, 13) and all(hasattr(os, name) for name in ("waitid", "WNOWAIT", "P_PID", "WEXITED")))
    temp = Path(os.environ["RUNNER_TEMP"])
    need(temp.is_absolute() and temp == temp.resolve(strict=True))
    return temp

def dependency_decisions(assets, plan, common):
    _, ledger = common.asset(assets, plan["declaration_ledger"])
    _, decisions = common.asset(assets, plan["dependency_decisions"])
    common.need(ledger["schema"] == "morpheus-native-macos-declarations-v1"
        and ledger["target"] == plan["target"] and ledger["baseline"] == BASELINE
        and decisions["schema"] == "morpheus-native-macos-dependency-decisions-v1"
        and decisions["target"] == plan["target"] and decisions["source_revision"] == BASELINE
        and decisions["production_targets"] == plan["production_targets"]
        and decisions["native_test_target"] == "//:rtc_unittests")
    originals = {row["path"]: row for row in ledger["declarations"]}
    rows = decisions["declarations"]
    common.need(len(originals) == len(ledger["declarations"]) and len(rows) == len(originals)
                and {row["path"] for row in rows} == set(originals))
    for row in rows:
        raw = (json.dumps(originals[row["path"]], sort_keys=True, separators=(",", ":"))+"\n").encode()
        common.need(row["declaration_sha256"] == common.sha(raw)
                    and row["decision"] in ("real-material", "proven-inactive", "unresolved") and row["evidence"])
        for evidence in row["evidence"]: common.asset(assets, evidence)
    return decisions

def pack(args, plan, roots, temp, material, common):
    # Explicit allowlist excludes original Xcode/SDK roots and host inventory.
    common.need(set(roots) == {"source", "core", "mac", "control"})
    inventories = {}
    for role, root in roots.items():
        common.need(root.is_relative_to(temp) and root.parent == temp)
        files, dirs = common.scan_tree(material, root)
        inventories[root.name] = {"role": role, "files": list(files.values()), "directories": sorted(dirs)}
    handoff = {"schema": "morpheus-native-macos-handoff-v1", "target": args.target,
        "pipeline_plan_sha256": args.plan_sha256, "source_commit": os.environ["GITHUB_SHA"],
        "source_run_id": os.environ["GITHUB_RUN_ID"], "source_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "roots": inventories, "apple_sdk_redistributed": False, "build_admission_granted": False,
        "native_sources_complete": False, "build_ready": False, "distribution_ready": False}
    destination = temp / "native-webrtc-handoff"
    common.need(not os.path.lexists(destination)); destination.mkdir(mode=0o700)
    output = destination / "handoff.tar.gz"
    with open(output, "xb") as stream:
        os.chmod(output, 0o600)
        with tarfile.open(fileobj=common.BoundedWriter(stream, common.MAX_HANDOFF), mode="w:gz", format=tarfile.GNU_FORMAT) as archive:
            raw = (json.dumps(handoff, sort_keys=True, separators=(",", ":"))+"\n").encode()
            common.need(len(raw) <= common.MAX_JSON)
            info = tarfile.TarInfo("HANDOFF.json"); info.size = len(raw); info.mode = 0o600
            archive.addfile(info, io.BytesIO(raw))
            for name, spec in sorted(inventories.items()):
                for directory in spec["directories"]:
                    info = tarfile.TarInfo(name + ("/" + directory if directory != "." else ""))
                    info.type = tarfile.DIRTYPE; info.mode = 0o700; archive.addfile(info)
                for row in spec["files"]:
                    path = temp / name / row["path"]
                    material.file_fact(path, row, row["mode"])
                    info = tarfile.TarInfo(name + "/" + row["path"]); info.size = row["bytes"]; info.mode = row["mode"]
                    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
                    with os.fdopen(fd, "rb") as source: archive.addfile(info, source)
                    common.need(stream.tell() <= common.MAX_HANDOFF)
        stream.flush(); os.fsync(stream.fileno())
    for name, spec in inventories.items(): material.topology(temp / name, {r["path"]: r for r in spec["files"]}, set(spec["directories"]))
    fact = material.file_fact(output); common.need(fact["bytes"] <= common.MAX_HANDOFF)
    key = "native-webrtc-source-" + args.plan_sha256[:16] + "-" + fact["sha256"]
    material.write_json(destination / "handoff-receipt.json", {"schema": 1, "target": args.target,
        "key": key, "archive": fact, "apple_sdk_redistributed": False, "build_admission_granted": False})
    common.emit("cache_key", key); common.emit("handoff_sha256", fact["sha256"]); common.emit("handoff_bytes", str(fact["bytes"]))
    if args.publish_handoff:
        common.need(plan["cache_handoff_authorized"] is True)
        common.cache_capacity(key, fact["bytes"]); common.emit("cache_save", "true")
    else: common.emit("cache_save", "false")
    print("MORPHEUS_NATIVE_MAC_INPUT_RECEIPT " + json.dumps({"target": args.target, "handoff": fact,
        "key": key, "pipeline_plan_sha256": args.plan_sha256, "apple_sdk_redistributed": False,
        "build_admission_granted": False, "native_sources_complete": False}, sort_keys=True))

def stage(args, plan, temp, common, material, mac):
    key = args.plan_sha256[:16]
    control = temp / ("morpheus-webrtc-control-" + key)
    common.need(not os.path.lexists(control)); control.mkdir(mode=0o700)
    workspace = Path(os.environ["GITHUB_WORKSPACE"]).resolve(strict=True)
    assets = common.input_assets(workspace, plan["assets"], control / "inputs", material)
    core_path, _ = common.asset(assets, plan["core_plan"], mac.CORE_PLAN)
    profile_path, profile = common.asset(assets, plan["mac_material_plan"])
    mac.validate(profile, args.target, material, common.module("python_stager.py"))
    source_path, source = common.asset(assets, plan["source_plan"])
    _, patches = common.asset(assets, plan["patch_manifest"])
    security_path, _ = common.asset(assets, plan["security_plan"], common.SECURITY_PLAN)
    common.need(source["schema"] == "morpheus-native-webrtc-sources-v3" and source["target"] == args.target)
    decisions = dependency_decisions(assets, plan, common)
    common.need(shutil.disk_usage(temp).free >= plan["minimum_free_bytes"])
    host_git = plan["host_git"]
    common.need(set(host_git) == {"path", "bytes", "sha256", "version"}
                and Path(host_git["path"]) == Path("/usr/bin/git").resolve(strict=True)
                and common.host_fact(Path(host_git["path"])) == {k: host_git[k] for k in ("bytes", "sha256")})
    for row in patches["patches"]:
        relative = row["file"].removeprefix("RUNNER_TEMP_INPUT_ROOT_UNBOUND/")
        common.need(relative != row["file"])
        actual = assets / "native/webrtc/linux" / material.relative(relative)
        common.need(material.file_fact(actual) == {k: row[k] for k in ("bytes", "sha256")})
        row["file"] = str(actual)
    patch_path = control / "patch-manifest.json"; patch_fact = material.write_json(patch_path, patches)
    common.need(set(source["git"]) == {"path", "bytes", "sha256", "version"}
                and all(value is None or "UNBOUND" in str(value) for value in source["git"].values())
                and source["patch_manifest"]["sha256"] == plan["patch_manifest"]["sha256"])
    source["git"] = host_git
    source["patch_manifest"] = {"path": str(patch_path), "sha256": patch_fact["sha256"]}
    source["security_plan"] = {"path": str(security_path), "sha256": common.SECURITY_PLAN}
    relocated = control / "source-plan.json"; source_fact = material.write_json(relocated, source)
    original = temp / ("morpheus-webrtc-original-" + key)
    common.cli(common.module("git_source_stager.py"), ["--plan", str(relocated), "--plan-sha256", source_fact["sha256"], "--output-root", str(original)])
    selections = material.read_json(original / "materials/deps-selection.json", material.file_fact(original / "materials/deps-selection.json")["sha256"])
    _, ledger = common.asset(assets, plan["declaration_ledger"])
    selected = sorted([row for block in selections for row in block["entries"] if row["kind"] != "git" and row["selected"]], key=lambda row: row["path"])
    common.need(ledger["declarations"] == selected)
    files, directories = common.scan_tree(material, original)
    assembled = temp / ("morpheus-webrtc-assembled-" + key)
    material.topology(original, files, directories, copy_to=assembled)
    roots = {"source": assembled, "control": control}
    for kind in ("core", "mac"):
        root = temp / ("morpheus-webrtc-tools-" + common.sha((key + ":" + kind).encode())[:16]); roots[kind] = root
        if kind == "core":
            admission = {"schema": 1, "purpose": "stage_verified_tool_data_only", "manifest_sha256": mac.CORE_PLAN,
                "target": TARGETS[args.target][2], "root": str(root), "public_repository": True,
                "standard_hosted_runner": True, "repository": REPOSITORY,
                "accept_exact_mac_dangling_alias_omissions": True, **common.job_fields()}
            path = control / "core-admission.json"; fact = material.write_json(path, admission)
            common.cli(common.module("linux_stager.py"), ["--target", TARGETS[args.target][2], "--manifest", str(core_path),
                "--manifest-sha256", mac.CORE_PLAN, "--admission", str(path), "--admission-sha256", fact["sha256"], "--root", str(root)])
        else: mac.stage_data(profile, common.read(profile_path), root, material, common.module("linux_stager.py"))
    core_rows, _ = mac.core_profile(roots["core"], args.target, material, common.module("linux_stager.py"))
    _, runtime_rows, _ = mac.profile(roots["mac"], plan["mac_material_plan"]["sha256"], args.target, material, common.module("python_stager.py"))
    marker_rows = material.read_json(assembled / "materials/gitlinks.json", material.file_fact(assembled / "materials/gitlinks.json")["sha256"])
    normalizer = common.module("git_source_stager.py")
    aliases = normalizer.verify_source_aliases(assembled)
    protected = [row["exported_path"] for row in marker_rows]
    protected += [row["mount"] + "/" + value for row in aliases
                  for value in (row["path"], row["target_path"])]
    overlay = mac.project(assembled, files, directories, protected,
                          (roots["core"], core_rows), (roots["mac"], runtime_rows), args.target, material)
    normalizer.verify_source_aliases(assembled)
    overlay_fact = material.write_json(assembled / "materials/macos-source-overlay.json", overlay)
    full_files, _ = common.scan_tree(material, assembled)
    material.write_json(control / "source-inventory.json", [{"path": name.removeprefix("checkout/src/"), "type": "file",
        "mode": "100755" if row["mode"] == 0o755 else "100644", "bytes": row["bytes"], "sha256": row["sha256"]}
        for name, row in sorted(full_files.items()) if name.startswith("checkout/src/")])
    material.write_json(control / "provenance-inventory.json", [{"path": name, "type": "file", "bytes": row["bytes"],
        "sha256": row["sha256"], **({"mode": "100755" if row["mode"] == 0o755 else "100644"} if row["mode"] in (0o644,0o755) else {})}
        for name,row in sorted(full_files.items())])
    material.write_json(control / "macos-projection-receipt.json", {"schema": "morpheus-native-macos-projection-v1",
        "target": args.target, "source_plan_sha256": source_fact["sha256"], "overlay": overlay_fact,
        "mac_material_plan_sha256": plan["mac_material_plan"]["sha256"], "unresolved_declarations":
        [r["path"] for r in decisions["declarations"] if r["decision"] == "unresolved"], "apple_sdk_redistributed": False,
        "downloaded_tools_executed": [], "trusted_host_git_used": True, "native_sources_complete": False,
        "build_ready": False, "distribution_ready": False})
    pack(args, plan, roots, temp, material, common)

def build(args, plan, temp, common, material, mac):
    template = common.load(Path(args.admission), args.admission_sha256)
    common.need(template["schema"] == 6 and template["target"] == args.target and template["recipe_sha256"] == BUILDER_SHA)
    common.cache_download(args, temp, material, plan, mac_target=args.target)
    header, cache = common.restore(args, temp, material, mac_target=args.target)
    def relocate(value):
        if isinstance(value, dict): return {k: relocate(v) for k,v in value.items()}
        if isinstance(value, list): return [relocate(v) for v in value]
        if isinstance(value, str) and value.startswith("$RUNNER_TEMP/"): return str(temp / material.relative(value.removeprefix("$RUNNER_TEMP/")))
        if isinstance(value, str) and value.startswith("$GITHUB_WORKSPACE/"): return str(Path(os.environ["GITHUB_WORKSPACE"]) / material.relative(value.removeprefix("$GITHUB_WORKSPACE/")))
        return value
    admission = relocate(template)
    common.need(admission["mac_materials"]["plan"]["sha256"] == plan["mac_material_plan"]["sha256"])
    name = next(name for name,row in header["roots"].items() if row["role"] == "control")
    decisions = dependency_decisions(temp / name / "inputs", plan, common)
    common.need(all(row["decision"] != "unresolved" for row in decisions["declarations"]))
    control = temp / ("morpheus-webrtc-build-control-" + args.plan_sha256[:16]); common.need(not os.path.lexists(control)); control.mkdir(mode=0o700)
    # Recompute real host-only SDK DATA and require the separately admitted
    # preflight hash. No SDK file enters the cache or package projection.
    sdk_root = Path(admission["roots"]["sdk"]["path"])
    sdk_files, sdk_dirs = mac.sdk_inventory(sdk_root)
    sdk_bytes = mac.canonical(sdk_files)
    common.need(hashlib.sha256(sdk_bytes).hexdigest() == admission["roots"]["sdk"]["inventory_sha256"])
    host = common.load(Path(admission["mac_host"]["path"]), admission["mac_host"]["sha256"])
    common.need(hashlib.sha256(mac.canonical(sdk_dirs)).hexdigest() == host["directory_topology_sha256"])
    sdk_inventory_path = control / "host-sdk-inventory.json"
    common.need(Path(admission["roots"]["sdk"]["inventory"]) == sdk_inventory_path)
    fd = os.open(sdk_inventory_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as output:
        output.write(sdk_bytes); output.flush(); os.fsync(output.fileno())
    path = control / "admission.json"; fact = material.write_json(path, admission)
    recipe = module(Path(__file__).parent / "build_native_webrtc_unix.py", BUILDER_SHA, "morpheus_mac_builder")
    common.cli(recipe, ["--target", args.target, "--runner-root", str(temp / "morpheus-webrtc-build"),
        "--admission", str(path), "--admission-sha256", fact["sha256"], "--jobs", str(args.jobs)])
    material.write_json(control / "handoff-consumption.json", {"cache": cache, "handoff_sha256": args.handoff_sha256,
        "template_sha256": args.admission_sha256, "relocated_admission_sha256": fact["sha256"], "publication_admission": False})

def main():
    class FixedParser(argparse.ArgumentParser):
        def error(self, message): raise ValueError()
    parser = FixedParser(allow_abbrev=False)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--phase", choices=("stage", "build", "cache-receipt"), required=True)
    parser.add_argument("--plan", required=True); parser.add_argument("--plan-sha256", required=True)
    parser.add_argument("--publish-handoff", action="store_true")
    for key in ("cache-key", "cache-id", "handoff-sha256", "admission", "admission-sha256"): parser.add_argument("--"+key, default="")
    parser.add_argument("--handoff-bytes", type=int, default=0); parser.add_argument("--jobs", type=int, choices=(1,2,3,4), default=2)
    args = parser.parse_args()
    common = module(Path(__file__).parent / "run_native_webrtc_linux.py", COMMON_SHA, "morpheus_shared_native_producer")
    temp = runner(args.target, common); plan = common.load(Path(args.plan), args.plan_sha256)
    common.need(set(plan) == {"schema", "target", "pipeline_sha256", "recipe_sha256", "security_plan",
        "native_test_count", "security_patch_count", "production_targets", "assets", "core_plan", "mac_material_plan",
        "source_plan", "patch_manifest", "declaration_ledger", "dependency_decisions", "host_git", "host_python",
        "host_zstd", "minimum_free_bytes", "cache_handoff_authorized"})
    common.need(plan["production_targets"] == [":default", "api/audio_codecs:builtin_audio_decoder_factory",
        "api/task_queue:default_task_queue_factory", "sdk:native_api", "sdk:default_codec_factory_objc",
        "pc:peer_connection", "sdk:videocapture_objc", "sdk:mac_framework_objc", "desktop_capture_objc", "modules/audio_device:audio_device"])
    common.need(plan["schema"] == "morpheus-native-macos-pipeline-v1" and plan["target"] == args.target
        and plan["pipeline_sha256"] == common.sha(common.read(Path(__file__).resolve()))
        and plan["recipe_sha256"] == BUILDER_SHA and plan["native_test_count"] == 12 and plan["security_patch_count"] == 8
        and plan["security_plan"]["sha256"] == common.SECURITY_PLAN and type(plan["cache_handoff_authorized"]) is bool
        and type(plan["minimum_free_bytes"]) is int and plan["minimum_free_bytes"] >= 12*1024**3)
    material = common.module("linux_materials.py")
    mac = module(Path(__file__).parent / "native_webrtc/macos_materials.py", MAC_HELPER_SHA, "morpheus_mac_materials")
    if args.phase == "stage":
        common.need(not args.admission and not args.admission_sha256 and not args.cache_key and not args.cache_id
                    and not args.handoff_sha256 and args.handoff_bytes == 0)
        stage(args, plan, temp, common, material, mac)
    elif args.phase == "build":
        common.need(not args.publish_handoff and args.admission and args.admission_sha256)
        build(args, plan, temp, common, material, mac)
    else:
        common.need(not args.publish_handoff and not args.admission and not args.admission_sha256)
        record = common.cache_identity(args, material, after_save=True)
        material.write_json(temp / "native-webrtc-handoff/cache-identity.json", record)
        print(json.dumps({"native_source_cache_id": record["id"], "key": record["key"]}, sort_keys=True))

if __name__ == "__main__":
    try: main()
    except BaseException:
        print('{"native_macos_producer":"failed","build_or_release_acceptance":false}', file=sys.stderr)
        raise SystemExit(1)
