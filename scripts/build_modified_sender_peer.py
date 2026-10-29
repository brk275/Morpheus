"""Held, owner-reviewed-only build producer for MODIFIED_PEER experiments.

This does not run acceptance, publish assets, change an official image, or
qualify the unmodified peer. The new Element build graph needs separate review.
"""
from pathlib import Path
import argparse
import hashlib
import json
import os
import re
import stat
import tempfile
import tarfile
import types
import urllib.request

SDK_INPUTS = "dac3d8e9f703ecae657cf50455401cb2550390c7a152d2571324cecd2c30acfa"
ELEMENT_INPUTS = "ac748c1ebfd3b520853985128ff7b708ec735ae75f5c7155959eaabaa323e6da"
TOOLS_SHA = "8a249395957208e2876a3c55826bf1c427257e33fdc98a0ab444731ea0f18be0"
ELEMENT_REV = "ed6a0e1341a8700980506a883749db16192dc6f7"
CONFIG_SHA = "417f2e00f92e9be685a95750f75f4a8d3118a3a2efa06087bd41908293ef948b"


def need(value):
    if not value:
        raise RuntimeError("modified_peer_build")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def regular(path, limit, *, single_link=True):
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid()
         and (not single_link or info.st_nlink == 1) and 0 <= info.st_size <= limit)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    try:
        opened = os.fstat(fd)
        identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_uid,
                                  value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
        need(identity(opened) == identity(info))
        data = bytearray()
        while chunk := os.read(fd, min(65536, info.st_size - len(data) + 1)):
            data.extend(chunk)
            need(len(data) <= info.st_size)
        need(len(data) == info.st_size and identity(os.fstat(fd)) == identity(info)
             and identity(path.lstat()) == identity(info))
        return bytes(data)
    finally:
        os.close(fd)


def copy_tree(source, destination, write, *, omit_config=False):
    """Clone only bounded owned regular output; no links or shared output inode."""
    destination.mkdir(mode=0o700)
    rows = []
    total = entries = 0

    def visit(src, dst, prefix, depth):
        nonlocal total, entries
        need(depth <= 16)
        info = src.lstat()
        need(stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid())
        for path in sorted(src.iterdir()):
            entries += 1
            need(entries <= 10000 and path.name not in (".", "..")
                 and not any(c in path.name for c in ("\n", "\r", "\\")))
            name = prefix + path.name
            item = path.lstat()
            need(item.st_uid == os.geteuid())
            if stat.S_ISDIR(item.st_mode):
                (dst / path.name).mkdir(mode=0o700)
                visit(path, dst / path.name, name + "/", depth + 1)
            else:
                data = regular(path, 32 * 1024 * 1024)
                total += len(data)
                need(total <= 256 * 1024 * 1024 and len(rows) < 5000)
                if omit_config and (name == "config.json" or name.startswith("config.json.")):
                    continue
                write(dst / path.name, data)
                rows.append({"path": name, "bytes": len(data), "sha256": sha(data)})
    visit(source, destination, "", 0)
    if omit_config:
        write(destination / "config.json", b"")
        rows.append({"path": "config.json", "bytes": 0, "sha256": sha(b"")})
    return sorted(rows, key=lambda row: row["path"])



def element_manager(spec, root, tools):
    need(spec["url"] == "https://registry.npmjs.org/pnpm/-/pnpm-11.21.0.tgz"
         and 0 < spec["bytes"] <= 16 * 1024 * 1024)
    tools.download(spec, root / "pnpm11.archive")
    destination = root / "pnpm11"
    destination.mkdir(mode=0o700)
    seen = set()
    total = 0
    with tarfile.open(root / "pnpm11.archive", "r:gz") as source:
        for index, member in enumerate(source):
            need(index < 1000)
            if member.isdir():
                continue
            need(member.isfile() and not member.issparse() and member.name in spec["files"]
                 and member.name not in seen and member.size <= 16 * 1024 * 1024)
            fact = spec["files"][member.name]
            need(member.size == fact["bytes"])
            data = source.extractfile(member).read(member.size + 1)
            need(len(data) == member.size and sha(data) == fact["sha256"])
            total += len(data)
            need(total <= 40 * 1024 * 1024)
            seen.add(member.name)
            if member.name.startswith(("package/bin/", "package/dist/")) or member.name == "package/package.json":
                relative = member.name.removeprefix("package/")
                need(not Path(relative).is_absolute() and all(part not in ("", ".", "..") for part in relative.split("/")))
                target = destination / relative
                target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                tools.write(target, data, fact["mode"] == "100755")
    need(seen == set(spec["files"]))
    need(sha(regular(destination / "dist/pnpm.mjs", 16 * 1024 * 1024))
         == "293dcced3c77f5a633352076fe4c9a768e60c6992ed58bc695b465f839fe9615")
    return destination / "bin/pnpm.mjs"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--scanner", type=Path, required=True)
    parser.add_argument("--variant", choices=("baseline44", "renewal44"), required=True)
    args = parser.parse_args()
    need(os.environ.get("GITHUB_ACTIONS") == "true"
         and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
         and os.environ.get("RUNNER_OS") == "Linux"
         and os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus"
         and os.environ.get("GITHUB_ACTOR") == os.environ.get("GITHUB_TRIGGERING_ACTOR") == "brk275")
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    need(args.root.is_absolute() and args.root.parent == runner_temp
         and re.fullmatch(r"morpheus-modified-sender-[a-zA-Z0-9_-]+", args.root.name)
         and not args.root.exists())
    scripts = Path(__file__).resolve().parent
    tool_source = regular(scripts / "sender_proposal_tools.py", 128 * 1024)
    need(sha(tool_source) == TOOLS_SHA)
    tools = types.ModuleType("admitted_sender_source_tools")
    exec(compile(tool_source, "admitted_sender_source_tools", "exec"), tools.__dict__)
    raw = regular(scripts / "modified-sender-sdk-inputs.json", 512 * 1024)
    need(sha(raw) == SDK_INPUTS)
    sdk_inputs = json.loads(raw)
    raw = regular(scripts / "modified-sender-element-inputs.json", 512 * 1024)
    need(sha(raw) == ELEMENT_INPUTS)
    element_inputs = json.loads(raw)
    patch = regular(scripts / "modified-sender-sdk.patch", 128 * 1024)
    need(sha(patch) == tools.PROPOSAL == sdk_inputs["proposal_sha256"])
    need(sha(regular(args.scanner, 64 * 1024 * 1024)) == sdk_inputs["scanner_binary_sha256"])
    config = regular(scripts / "vite.modified-peer.config.ts", 128 * 1024)
    need(sha(config) == CONFIG_SHA == element_inputs["modified_config_sha256"])
    work = Path(tempfile.mkdtemp(prefix="morpheus-modified-sender-build-", dir=runner_temp))
    os.chmod(work, 0o700)
    sdk, element, bin_root = work / "sdk", work / "element", work / "bin"
    for path in (sdk, element, bin_root, work / "temp", work / "cache", work / "config", work / "store"):
        path.mkdir(mode=0o700)
    tools.write(work / "scanner.toml", b"[extend]\nuseDefault = true\n")
    tools.write(work / "scanner.ignore", b"")
    tools.write(work / "npmrc", b"registry=https://registry.npmjs.org/\nignore-scripts=true\n")
    for name in ("source", "node", "pnpm"):
        tools.download(sdk_inputs[name], work / (name + ".archive"))
    tools.source_tree(work / "source.archive", sdk_inputs["source_files"], sdk)
    if args.variant == "renewal44":
        tools.apply_patch(sdk, patch, sdk_inputs["proposal_files"])
    else:
        for name, fact in sdk_inputs["proposal_files"].items():
            need(sha(regular(sdk / name, 4 * 1024 * 1024)) == fact["before_sha256"])
    tools.selected_binary(work / "node.archive", sdk_inputs["node"]["binary"], bin_root / "node")
    tools.selected_binary(work / "pnpm.archive", sdk_inputs["pnpm"]["binary"], bin_root / "pnpm")
    spec = element_inputs["archive"]
    need(spec["url"] == "https://codeload.github.com/element-hq/element-call/tar.gz/" + ELEMENT_REV
         and 0 < spec["archive_bytes"] <= 64 * 1024 * 1024)
    with urllib.request.build_opener(tools.NoRedirect()).open(spec["url"], timeout=60) as response:
        need(response.status == 200)
        data = response.read(spec["archive_bytes"] + 1)
    need(len(data) == spec["archive_bytes"] and sha(data) == spec["archive_sha256"])
    tools.write(work / "element.archive", data)
    tools.SOURCE_PREFIX = "element-call-" + ELEMENT_REV
    tools.source_tree(work / "element.archive", element_inputs["source_files"], element)
    need(sha(regular(element / "pnpm-lock.yaml", 512 * 1024)) == element_inputs["lock_sha256"])
    tools.write(element / "vite.modified-peer.config.ts", config)
    environment = {"PATH": str(bin_root) + ":/usr/bin:/bin", "CI": "true", "TZ": "UTC", "LANG": "C.UTF-8",
        "TMPDIR": str(work / "temp"), "XDG_CACHE_HOME": str(work / "cache"), "XDG_CONFIG_HOME": str(work / "config"),
        "PNPM_HOME": str(bin_root), "npm_config_userconfig": str(work / "npmrc"),
        "npm_config_globalconfig": str(work / "npmrc"), "npm_config_cache": str(work / "cache"),
        "npm_config_registry": "https://registry.npmjs.org/", "npm_config_ignore_scripts": "true",
        "npm_config_enable_pre_post_scripts": "false", "npm_config_manage_package_manager_versions": "false",
        "npm_config_update_notifier": "false", "NODE_DISABLE_COMPILE_CACHE": "1", "VITE_APP_VERSION": "CONTROLLED_PEER-sdk44-source-pair"}
    pnpm, node = str(bin_root / "pnpm"), str(bin_root / "node")
    element_pnpm = element_manager(element_inputs["element_pnpm"], work, tools)
    regression = regular(scripts / "modified_sender_no_lifecycle.mjs", 128 * 1024)
    need(sha(regression) == "20cf136378c26e95e0a799a9c175b30269587e531d849e01444f60c2053ca1e2")
    tools.write(work / "no-lifecycle.mjs", regression)
    tools.checked_command("manager_no_lifecycle", [node, str(work / "no-lifecycle.mjs"),
        str(element_pnpm.parent.parent / "dist/pnpm.mjs")],
        work, environment, work, args.scanner, 60)
    for label, directory in (("sdk_dependencies", sdk), ("element_dependencies", element)):
        lock = regular(directory / "pnpm-lock.yaml", 512 * 1024)
        manager = [pnpm] if directory == sdk else [node, str(element_pnpm)]
        tools.checked_command(label, [*manager, "install", "--frozen-lockfile", "--ignore-scripts", "--store-dir",
            str(work / "store" / label), "--reporter", "append-only"], directory, environment, work, args.scanner, 600)
        need(regular(directory / "pnpm-lock.yaml", 512 * 1024) == lock)
    # Invoke the reviewed transform explicitly; root prepare/build hooks never run.
    need(not (sdk / "lib").exists())
    need(sha(regular(sdk / "babel.config.cjs", 128 * 1024))
         == "d8487617a57ed6e8f76916f79b2ac697ce565aace57c6e4cf4798a8ae966be89")
    tools.checked_command("sdk_compile", [node, str(sdk / "node_modules/@babel/cli/bin/babel.js"),
        "--no-babelrc", "--config-file", str(sdk / "babel.config.cjs"), "--out-dir", str(sdk / "lib"),
        "--extensions", ".ts,.js", str(sdk / "src")], sdk, environment, work, args.scanner, 600)
    package_link = element / "node_modules/matrix-js-sdk"
    need(package_link.is_symlink())
    installed_sdk = package_link.resolve(strict=True)
    need(installed_sdk.is_relative_to((element / "node_modules").resolve(strict=True))
         and not (installed_sdk / "lib").exists())
    package = json.loads(regular(installed_sdk / "package.json", 128 * 1024, single_link=False))
    need(package["name"] == "matrix-js-sdk" and package["version"] == "44.0.0")
    # Leave Element's exact runtime dependency resolution intact, including
    # @babel/runtime8.0.0; only add output compiled from the corrected source.
    for name in ("src/matrixrtc/MembershipManager.ts", "src/matrixrtc/MembershipManagerActionScheduler.ts"):
        need(sha(regular(installed_sdk / name, 4 * 1024 * 1024, single_link=False))
             == sdk_inputs["proposal_files"][name]["before_sha256"])
    copy_tree(sdk / "lib", installed_sdk / "lib", tools.write)
    need(sha(regular(element / ".postcssrc.json", 128 * 1024))
         == "c19bd4dd47d1ec37e1a3a1a69fd474f631a549b56f0aa95c761bddcd136a74ed")
    tools.checked_command("modified_browser_build", [node, "--max-old-space-size=4096",
        str(element / "node_modules/vite/bin/vite.js"), "build", "--config", "vite.modified-peer.config.ts"],
        element, environment, work, args.scanner, 900)
    args.root.mkdir(mode=0o700)
    interop = args.root / "element-interop"
    interop.mkdir(mode=0o700)
    output = interop / "modified-peer"
    output.mkdir(mode=0o700)
    assets = copy_tree(element / "dist", output / "dist", tools.write, omit_config=True)
    receipt = {"schema": 1, "classification": "MODIFIED_PEER", "variant": args.variant,
        "proposal_applied": args.variant == "renewal44", "element_revision": ELEMENT_REV,
        "sdk_revision": "3f05dc4d104a3dfc753c5e3549a18c74fab4b076", "proposal_sha256": tools.PROPOSAL,
        "element_lock_sha256": element_inputs["lock_sha256"], "builder_source_sha256": sha(regular(Path(__file__), 128 * 1024)),
        "assets": assets}
    data = (json.dumps(receipt, sort_keys=True, indent=2) + "\n").encode()
    need(len(data) <= 2 * 1024 * 1024)
    tools.write(output / "build-receipt.json", data)
    print(json.dumps({"classification": "MODIFIED_PEER", "variant": args.variant, "manifest_sha256": sha(data),
                      "official_stock_acceptance": False, "beta_ready": False}), flush=True)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        print('{"classification":"MODIFIED_PEER","build":"failed","official_stock_acceptance":false,"beta_ready":false}')
        raise SystemExit(1)
