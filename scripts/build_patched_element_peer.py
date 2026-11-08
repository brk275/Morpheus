#!/usr/bin/env python3
"""TEMP-only reviewed-source Element frontend builder; never stock acceptance."""
from pathlib import Path, PurePosixPath
import argparse
import hashlib
import json
import os
import shutil
import stat
import sys
import tarfile
import tempfile
import urllib.request

# Isolated Python omits the script directory. Import only these reviewed
# sibling helpers from this exact checkout, never from PYTHONPATH/user packages.
sys.path.insert(0, str(Path(__file__).resolve().parent))

from check_sender_proposal import (NoRedirect, apply_patch, checked_command, digest,
                                   need, regular, selected_binary, source_tree, write)
from element_peer_fixture import (MAX_ASSET_FILES, MAX_ASSET_FILE_BYTES,
                                  MAX_ASSET_TOTAL_BYTES, PATCHED_SDK_KIND, REVISION,
                                  SDK_PATCH_SHA256, SDK_PRODUCTION_FILES, SDK_REVISION,
                                  patched_sdk_assets)

ELEMENT_URL = "https://codeload.github.com/element-hq/element-call/tar.gz/" + REVISION
ELEMENT_PREFIX = "element-call-" + REVISION


def fetch(spec, destination):
    # Static primary source families only; no caller-provided mirrors or auth.
    need(spec["url"].startswith(("https://nodejs.org/download/release/",
                                "https://registry.npmjs.org/"))
         or spec["url"] in (ELEMENT_URL, "https://codeload.github.com/matrix-org/matrix-js-sdk/tar.gz/" + SDK_REVISION))
    need(type(spec["bytes"]) is int and 0 < spec["bytes"] <= 128 * 1024 * 1024)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(urllib.request.Request(spec["url"], headers={"User-Agent": "Morpheus-source-check/1.0"}), timeout=60) as response:
        need(response.status == 200)
        body = response.read(spec["bytes"] + 1)
    need(len(body) == spec["bytes"] and digest(body) == spec["sha256"])
    write(destination, body)


def extract_regular_tree(archive, expected, destination, *, prefix, total_limit):
    """Exact archive leaves, no hooks, links, permissions or tar extraction."""
    seen = set()
    total = 0
    with tarfile.open(archive, "r:*") as source:
        for index, member in enumerate(source):
            need(index < 4000 and not member.issparse())
            if member.name == prefix:
                need(member.isdir())
                continue
            need(member.name.startswith(prefix + "/"))
            relative = member.name[len(prefix) + 1:]
            need(all(part not in ("", ".", "..") for part in relative.split("/")))
            path = destination / relative
            if member.isdir():
                path.mkdir(mode=0o700, parents=True, exist_ok=True)
                continue
            need(relative in expected and relative not in seen and member.isfile())
            fact = expected[relative]
            need(member.size == fact["bytes"] and member.size <= 16 * 1024 * 1024)
            content = source.extractfile(member).read(member.size + 1)
            need(len(content) == member.size and digest(content) == fact["sha256"])
            total += len(content)
            need(total <= total_limit)
            if prefix == ELEMENT_PREFIX and relative == "embedded/android/gradlew.bat":
                # GitHub applies this unexecuted Windows script's eol=crlf
                # export rule. Reconstruct and verify the exact pinned Git leaf.
                content = content.replace(b"\r\n", b"\n")
            if prefix == ELEMENT_PREFIX:
                need(hashlib.sha1(b"blob " + str(len(content)).encode() + b"\0" + content).hexdigest() == fact["git_blob"])
            need(fact["mode"] in ("100644", "100755"))
            path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            write(path, content, fact["mode"] == "100755")
            seen.add(relative)
    need(seen == set(expected))


def inventory(directory, *, total_limit=MAX_ASSET_TOTAL_BYTES):
    result = {}
    total = 0
    stack = [(directory, 0)]
    entries = 0
    while stack:
        root, depth = stack.pop()
        need(depth <= 16)
        for path in root.iterdir():
            entries += 1
            need(entries <= 2 * MAX_ASSET_FILES)
            info = path.lstat()
            need(info.st_uid == os.geteuid() and not stat.S_ISLNK(info.st_mode))
            if stat.S_ISDIR(info.st_mode):
                stack.append((path, depth + 1))
                continue
            content = regular(path, MAX_ASSET_FILE_BYTES)
            total += len(content)
            need(total <= total_limit)
            result[path.relative_to(directory).as_posix()] = {"bytes": len(content), "sha256": digest(content)}
            need(len(result) <= MAX_ASSET_FILES)
    return result


def resolved_package(tree, name, version, caller):
    """Node's ancestor node_modules package lookup, without executing a loader."""
    caller = caller.resolve(strict=True)
    need(caller.is_relative_to(tree))
    start = caller if caller.is_dir() else caller.parent
    for parent in (start, *start.parents):
        if not parent.is_relative_to(tree):
            break
        if parent.name == "node_modules":
            continue
        candidate = parent / "node_modules" / name
        if not candidate.exists() and not candidate.is_symlink():
            continue
        found = candidate.resolve(strict=True)
        need(found.is_dir() and found.is_relative_to(tree / "node_modules/.pnpm"))
        package = json.loads(regular(found / "package.json", 128 * 1024))
        need(package["name"] == name and package["version"] == version)
        return found
    raise RuntimeError("refused")


def installed_members(tree, facts, *, element=False):
    """Bind actual caller-selected JS/native files; no inferred pnpm suffix."""
    need(isinstance(facts, list) and facts)
    seen = set()
    vite = resolved_package(tree, "vite", "8.2.4", tree) if element else None
    roots = {}
    for fact in facts:
        name, version = fact["identity"].rsplit("@", 1)
        relative = fact["relative_entry"]
        need(not PurePosixPath(relative).is_absolute()
             and all(part not in ("", ".", "..") for part in relative.split("/")))
        if fact["phase"] == "ELEMENT_NATIVE":
            anchor_name = {"@esbuild/linux-x64": "esbuild",
                           "@rolldown/binding-linux-x64-gnu": "rolldown",
                           "lightningcss-linux-x64-gnu": "lightningcss"}[name]
            anchor_version = {"esbuild": "0.28.2", "rolldown": "1.2.15", "lightningcss": "1.33.0"}[anchor_name]
            anchor = resolved_package(tree, anchor_name, anchor_version, vite / "dist/node/chunks/node.js")
            loader = {"esbuild": "lib/main.js", "rolldown": "dist/shared/binding-BMtuZhvP.mjs",
                      "lightningcss": "node/index.js"}[anchor_name]
            package = resolved_package(tree, name, version, anchor / loader)
            fallback = anchor / {"esbuild": "lib/downloaded-@esbuild-linux-x64-esbuild",
                                 "rolldown": "dist/rolldown-binding.linux-x64-gnu.node",
                                 "lightningcss": "lightningcss.linux-x64-gnu.node"}[anchor_name]
            if fallback.exists() or fallback.is_symlink():
                content = regular(fallback, 32 * 1024 * 1024)
                need(len(content) == fact["bytes"] and digest(content) == fact["sha256"])
        else:
            caller = vite / "dist/node/chunks/node.js" if element and name in ("rolldown", "esbuild", "lightningcss") else tree
            package = resolved_package(tree, name, version, caller)
        path = package / relative
        need(path.resolve(strict=True) == path and path.is_relative_to(tree / "node_modules/.pnpm") and path not in seen)
        content = regular(path, 32 * 1024 * 1024)
        need(len(content) == fact["bytes"] and digest(content) == fact["sha256"])
        if name == "@esbuild/linux-x64":
            need(bool(path.stat().st_mode & 0o111))
        seen.add(path)
        roots[name] = package


def unchanged_source(tree, facts, patches=None, *, outputs=()):
    """Complete original leaf inventory, with only known generated roots omitted."""
    patches = patches or {}
    expected = {name for name, fact in facts.items() if fact["mode"] != "120000"}
    parents = {str(parent) for name in facts for parent in PurePosixPath(name).parents}
    seen = set()
    pending = [tree]
    entries = 0
    while pending:
        directory = pending.pop()
        for path in directory.iterdir():
            entries += 1
            need(entries <= 4 * len(facts))
            relative = path.relative_to(tree).as_posix()
            info = path.lstat()
            need(info.st_uid == os.getuid() and not stat.S_ISLNK(info.st_mode))
            if relative in ("node_modules", *outputs):
                need(stat.S_ISDIR(info.st_mode))
                continue
            if stat.S_ISDIR(info.st_mode):
                need(relative in parents)
                pending.append(path)
                continue
            need(relative in expected and relative not in seen)
            fact = facts[relative]
            need(bool(info.st_mode & 0o111) == (fact["mode"] == "100755"))
            body = regular(path, 8 * 1024 * 1024)
            expected_hash = patches.get(relative, {}).get("after_sha256", fact["sha256"])
            if tree.name == "element" and relative == "embedded/android/gradlew.bat":
                need(hashlib.sha1(b"blob " + str(len(body)).encode() + b"\0" + body).hexdigest() == fact["git_blob"])
            else:
                need(digest(body) == expected_hash)
            seen.add(relative)
    need(seen == expected)


CONFIG_NAMES = (
    "package.json", ".config.json", ".config.yaml", ".config.yml", ".config.js", ".config.ts", ".config.cjs", ".config.mjs",
    "browserslist-stats.json", "browserslist", ".browserslistrc", ".babelignore",
    *(".svgrrc" + suffix for suffix in ("", ".json", ".yaml", ".yml", ".js", ".ts", ".cjs", ".mjs")),
    *("svgr.config" + suffix for suffix in (".js", ".ts", ".cjs", ".mjs")),
    *(".postcssrc" + suffix for suffix in ("", ".json", ".yaml", ".yml", ".ts", ".cts", ".mts", ".js", ".cjs", ".mjs")),
    *("postcss.config" + suffix for suffix in (".ts", ".cts", ".mts", ".js", ".cjs", ".mjs")),
    *("babel.config" + suffix for suffix in (".js", ".cjs", ".mjs", ".json", ".cts", ".ts", ".mts")),
    *(".babelrc" + suffix for suffix in ("", ".js", ".cjs", ".mjs", ".json", ".cts")),
)


def no_ancestor_configuration(tree):
    """Fail closed on selected tools' config/stat searches outside pinned source."""
    parents = tree.parents
    need(len(parents) <= 32)
    for parent in parents:
        info = parent.lstat()
        need(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode))
        for name in CONFIG_NAMES:
            path = parent / name
            need(not path.exists() and not path.is_symlink())
        config = parent / ".config"
        if config.exists() or config.is_symlink():
            info = config.lstat()
            need(stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode))
            for suffix in ("", ".json", ".yaml", ".yml", ".js", ".ts", ".cjs", ".mjs"):
                path = config / ("svgrrc" + suffix)
                need(not path.exists() and not path.is_symlink())


def package_configuration(tree, source_files, *, sdk):
    """Exact pre-install config bytes, no dynamic YAML/config/plugin evaluation.

    The authenticated source review binds these configuration files and confirms
    no configDependencies except the SDK manager-lock document's empty mapping.
    Fresh HOME/XDG/fixed npmrc and the separate ancestor-config guard exclude
    inherited sources; this guard binds the package manager's own config bytes.
    """
    names = ("package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml", ".npmrc", "pnpmfile.cjs", ".pnpmfile.cjs")
    for name in names:
        path = tree / name
        if name not in source_files:
            need(not path.exists())
            continue
        fact = source_files[name]
        body = regular(path, 1024 * 1024)
        need(len(body) == fact["bytes"] and digest(body) == fact["sha256"])
        if name != "pnpm-lock.yaml":
            need(b"configDependencies" not in body)
    package = json.loads(regular(tree / "package.json", 128 * 1024))
    if sdk:
        need(package["devEngines"]["packageManager"] == {
            "name": "pnpm", "version": "12.11.2", "onFail": "download"})
        lock = regular(tree / "pnpm-lock.yaml", 1024 * 1024)
        need(lock.count(b"configDependencies:") == 1 and b"configDependencies: {}\n" in lock)
    else:
        need(package["packageManager"] == "pnpm@11.21.0+sha512.521705bce689924eac72f5a3587122f362689ef6571e55ba80076fd637c11132ecffada26fad4ea79c485bfddbfd3d5a2a5b05805a77e893de71ec8a6cca3bb1")
        need(b"configDependencies" not in regular(tree / "pnpm-lock.yaml", 1024 * 1024))


def sdk_handoff(sdk, element, source_files):
    """Keep Element's own dependency graph; replace only SDK generated lib."""
    target = (element / "node_modules/matrix-js-sdk").resolve(strict=True)
    need(target.is_relative_to(element / "node_modules/.pnpm") and target.is_dir())
    package_body = regular(target / "package.json", 128 * 1024)
    need(digest(package_body) == source_files["package.json"]["sha256"])
    package = json.loads(package_body)
    need(package["name"] == "matrix-js-sdk" and package["version"] == "44.0.0"
         and package["main"] == "./lib/index.js" and package["browser"] == "./lib/browser-index.js")
    # Git dependency installation must not silently run prepare. Its source is
    # authenticated against the original upstream package before lib is added.
    need(not (target / "lib").exists())
    expected_src = {name[4:]: fact for name, fact in source_files.items() if name.startswith("src/")}
    actual_src = inventory(target / "src", total_limit=32 * 1024 * 1024)
    need(actual_src == {name: {"bytes": fact["bytes"], "sha256": fact["sha256"]} for name, fact in expected_src.items()})
    emitted = inventory(sdk / "lib", total_limit=64 * 1024 * 1024)
    need(emitted and "index.js" in emitted and "browser-index.js" in emitted)
    need(all(name.endswith((".js", ".js.map")) for name in emitted))
    # The library is generated only from the two-leaf patched source tree.
    # Preserve its maps for Vite's complete source-chain selection proof below.
    shutil.copytree(sdk / "lib", target / "lib", symlinks=False)
    need(inventory(target / "lib", total_limit=64 * 1024 * 1024) == emitted)
    return target, emitted


def selected_sdk_sources(dist, sdk):
    matched = set()
    # Vite's source maps compose Babel maps. Check complete sourcesContent,
    # not a text signature in minified JS, and reject any older selected copy.
    for name in inventory(dist):
        if not name.endswith(".js.map"):
            continue
        mapping = json.loads(regular(dist / name, MAX_ASSET_FILE_BYTES))
        sources, contents = mapping.get("sources"), mapping.get("sourcesContent")
        need(isinstance(sources, list) and isinstance(contents, list) and len(sources) == len(contents))
        for source, content in zip(sources, contents):
            need(isinstance(source, str))
            for relative, expected in SDK_PRODUCTION_FILES.items():
                if source.replace("\\", "/").endswith("/" + relative):
                    need(isinstance(content, str) and digest(content.encode()) == expected)
                    need(regular(sdk / relative, 4 * 1024 * 1024) == content.encode())
                    matched.add(relative)
    need(matched == set(SDK_PRODUCTION_FILES))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--patch", type=Path, required=True)
    parser.add_argument("--scanner", type=Path, required=True)
    parser.add_argument("--receipt-path-file", type=Path, required=True)
    args = parser.parse_args()
    need(sys.platform == "linux" and os.environ.get("GITHUB_ACTIONS") == "true"
         and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
         and os.environ.get("RUNNER_OS") == "Linux" and os.environ.get("RUNNER_ARCH") == "X64"
         and os.uname().machine == "x86_64" and os.confstr("CS_GNU_LIBC_VERSION").startswith("glibc ")
         and os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus"
         and os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
         and os.environ.get("GITHUB_ACTOR") == "brk275"
         and os.environ.get("GITHUB_TRIGGERING_ACTOR") == "brk275")
    raw = regular(args.inputs, 1024 * 1024)
    need(digest(raw) == os.environ.get("MORPHEUS_ELEMENT_BUILD_INPUTS_SHA256"))
    inputs = json.loads(raw)
    need(inputs["source_review_clear"] is True and inputs["execution_admitted"] is True)
    patch = regular(args.patch, 128 * 1024)
    need(digest(patch) == SDK_PATCH_SHA256 and set(inputs["sdk_patch_files"]) == set(SDK_PRODUCTION_FILES))
    need(digest(regular(args.scanner, 64 * 1024 * 1024)) == inputs["scanner_binary_sha256"])
    need(os.getuid() == os.geteuid() != 0)
    requested_temp = Path(os.environ["RUNNER_TEMP"])
    need(requested_temp.is_absolute() and requested_temp == requested_temp.resolve(strict=True))
    temp_info = requested_temp.lstat()
    need(stat.S_ISDIR(temp_info.st_mode) and temp_info.st_uid == os.getuid() and not temp_info.st_mode & 0o022)
    runner_temp = requested_temp
    need(not any(char in str(runner_temp) for char in (",", "\r", "\n", "\x00")))
    receipt_path_file = args.receipt_path_file.absolute()
    need(receipt_path_file.parent.resolve(strict=True) == runner_temp and not receipt_path_file.exists() and not receipt_path_file.is_symlink())
    root = Path(tempfile.mkdtemp(prefix="morpheus-patched-sdk-build-", dir=runner_temp))
    os.chmod(root, 0o700)
    sdk, element, tools = root / "sdk", root / "element", root / "tools"
    for path in (sdk, element, tools, root / "sdk-store", root / "element-store", root / "home",
                 root / "temp", root / "cache", root / "config"):
        path.mkdir(mode=0o700)
    write(root / "scanner.toml", b"[extend]\nuseDefault = true\n")
    write(root / "scanner.ignore", b"")
    write(root / "npmrc", b"registry=https://registry.npmjs.org/\nignore-scripts=true\n")
    for name in ("sdk_source", "element_source", "node", "sdk_pnpm", "element_pnpm"):
        fetch(inputs[name], root / (name + ".archive"))
    source_tree(root / "sdk_source.archive", inputs["sdk_source_files"], sdk)
    apply_patch(sdk, patch, inputs["sdk_patch_files"])
    for relative, expected in SDK_PRODUCTION_FILES.items():
        need(digest(regular(sdk / relative, 4 * 1024 * 1024)) == expected)
    extract_regular_tree(root / "element_source.archive", inputs["element_source_files"], element,
                         prefix=ELEMENT_PREFIX, total_limit=16 * 1024 * 1024)
    selected_binary(root / "node.archive", inputs["node"]["binary"], tools / "node")
    selected_binary(root / "sdk_pnpm.archive", inputs["sdk_pnpm"]["binary"], tools / "sdk-pnpm")
    pnpm11 = tools / "pnpm11"
    pnpm11.mkdir(mode=0o700)
    extract_regular_tree(root / "element_pnpm.archive", inputs["element_pnpm_files"], pnpm11,
                         prefix="package", total_limit=48 * 1024 * 1024)
    environment = {"PATH": str(tools) + ":/usr/bin:/bin", "CI": "true", "TZ": "UTC", "LANG": "C.UTF-8",
        "HOME": str(root / "home"), "TMPDIR": str(root / "temp"), "XDG_CACHE_HOME": str(root / "cache"),
        "XDG_CONFIG_HOME": str(root / "config"), "NODE_DISABLE_COMPILE_CACHE": "1",
        "npm_config_userconfig": str(root / "npmrc"), "npm_config_globalconfig": str(root / "npmrc"),
        "npm_config_cache": str(root / "cache"), "npm_config_registry": "https://registry.npmjs.org/",
        "npm_config_ignore_scripts": "true", "npm_config_enable_pre_post_scripts": "false",
        "npm_config_manage_package_manager_versions": "false", "npm_config_update_notifier": "false"}
    # Fresh allowlisted env contains no credentials, BABEL/NODE_PATH/NODE_OPTIONS,
    # loader overrides, WebContainer fallbacks, VITE options or upload tokens.
    node = str(tools / "node")
    package_configuration(sdk, inputs["sdk_source_files"], sdk=True)
    sdk_lock = regular(sdk / "pnpm-lock.yaml", 512 * 1024)
    checked_command("sdk-install", [str(tools / "sdk-pnpm"), "install", "--pm-on-fail=error",
        "--runtime-on-fail=error", "--frozen-lockfile", "--ignore-scripts",
        "--ignore-pnpmfile", "--package-import-method=copy", "--store-dir", str(root / "sdk-store"),
        "--reporter", "append-only"], sdk, environment, root, args.scanner, 300)
    need(regular(sdk / "pnpm-lock.yaml", 512 * 1024) == sdk_lock)
    unchanged_source(sdk, inputs["sdk_source_files"], inputs["sdk_patch_files"])
    no_ancestor_configuration(sdk)
    installed_members(sdk, inputs["babel_members"])
    checked_command("sdk-babel", [node, "--max-old-space-size=4096", "node_modules/@babel/cli/bin/babel.js",
        "--config-file", str(sdk / "babel.config.cjs"), "--no-babelrc", "--delete-dir-on-start", "-d", "lib",
        "--verbose", "--extensions", ".ts,.js", "src"], sdk, environment, root, args.scanner, 600)
    unchanged_source(sdk, inputs["sdk_source_files"], inputs["sdk_patch_files"], outputs=("lib",))
    # No declaration emit: Vite consumes the JavaScript and its source maps.
    element_lock = regular(element / "pnpm-lock.yaml", 1024 * 1024)
    package_configuration(element, inputs["element_source_files"], sdk=False)
    checked_command("element-install", [node, str(pnpm11 / "bin/pnpm.mjs"), "install", "--pm-on-fail=error",
        "--config.runtime-on-fail=error", "--frozen-lockfile", "--ignore-scripts", "--ignore-pnpmfile",
        "--package-import-method=copy", "--store-dir", str(root / "element-store"), "--reporter", "append-only"],
        element, environment, root, args.scanner, 300)
    need(regular(element / "pnpm-lock.yaml", 1024 * 1024) == element_lock)
    unchanged_source(element, inputs["element_source_files"])
    sdk_target, emitted = sdk_handoff(sdk, element, inputs["sdk_source_files"])
    no_ancestor_configuration(element)
    installed_members(element, inputs["element_build_members"], element=True)
    checked_command("element-vite", [node, "--max-old-space-size=4096", "node_modules/vite/bin/vite.js",
        "build", "--mode", "production"], element, environment, root, args.scanner, 600)
    unchanged_source(element, inputs["element_source_files"], outputs=("dist",))
    unchanged_source(sdk, inputs["sdk_source_files"], inputs["sdk_patch_files"], outputs=("lib",))
    need(inventory(sdk_target / "lib", total_limit=64 * 1024 * 1024) == emitted)
    dist = element / "dist"
    selected_sdk_sources(dist, sdk)
    # Source deployment config is deliberately absent; nginx has a fixed alias
    # to the synthetic fixture config outside the served application directory.
    need(not (dist / "config.json").exists())
    os.chmod(dist, 0o700)
    assets = inventory(dist)
    receipt = {"schema": 1, "kind": PATCHED_SDK_KIND, "element_revision": REVISION,
        "sdk_revision": SDK_REVISION, "sdk_patch_sha256": SDK_PATCH_SHA256,
        "sdk_production_files": SDK_PRODUCTION_FILES, "stock_interoperability": False,
        "official_application_assets": False, "assets": str(dist), "inventory": assets,
        "build_inputs_sha256": digest(raw)}
    receipt_file = root / "asset-receipt.json"
    receipt_body = (json.dumps(receipt, sort_keys=True) + "\n").encode()
    write(receipt_file, receipt_body)
    patched_sdk_assets(receipt_file, digest(receipt_body))
    write(receipt_path_file, (str(receipt_file) + "\n" + digest(receipt_body) + "\n").encode())
    print(json.dumps({"patched_sdk_build": "success", "stock_interoperability": False,
                      "receipt_sha256": digest(receipt_body), "build_inputs_sha256": digest(raw)}), flush=True)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        print("Patched SDK frontend build failed; private diagnostics withheld", file=sys.stderr)
        raise SystemExit(1)
