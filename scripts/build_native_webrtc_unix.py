#!/usr/bin/env python3
"""Build admitted WebRTC inputs on a fresh public hosted Unix runner.

Source/tool provisioning and patch application remain external. Admitted Linux
inputs use the pinned CIPD client and genuine vpython to realize exact reviewed
Python environments before GN. No source fetch, self-update, mutable ref or
fallback is provided. A hash-bound admission is mandatory; this program grants
no execution authorization.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import types
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import stat
import sys
import time
import xml.etree.ElementTree as ET
import zipfile

SOURCE_REV = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
SOURCE_TREE = "fecdf55fdd1e9cafc5531abab7ff14e35a1159f5"
SDK_REV = "71e3a03e53205b65c6ca04f21339a58fc1a40eae"
DEPOT_REV = "b52fe87d04af61df3dd07748ff2c2c0b649dca09"
DEPOT_TREE = "5c857570433c574a02614b3aab9be1f34493095d"
NESTED_DEPOT_REV = "8efa575d754b8703d99b0f827528e45aeaa167aa"
TARGETS = {
    "linux-x64": ("Linux", "x86_64", "linux", "x64", "linux-x64-release"),
    "mac-arm64": ("Darwin", "arm64", "mac", "arm64", "mac-arm64-release"),
    "mac-x64": ("Darwin", "x86_64", "mac", "x64", "mac-x64-release"),
}
SECURITY_PLAN_SHA = "a9f2220dfb0e527690ace518911e3e92c12124ab8c37e6cb391a47c89f19c080"
PRODUCER_INTERFACE_SHA = "bc3d31fd5a9ea468c603847f6b699d24b8f958e5734d75da52913dd3b90ac898"
FINDER_SHA = "f42b1d5f2c8e2f26d50a679a5e5dc7595dd4c1b64af23f6684c22207f5c98b73"
LASTCHANGE_GNI_SHA = "63cb16e1cdd6632bab19f820e485dc88686d1fc31637d8539e0f40a1abe7117f"
TESTS = (
    "SifPayload.MatchesJsSdkHashes",
    "SifPayload.AcceptsServerBlankFrames",
    "SifPayload.RejectsAnythingElse",
    "SifTransform.ForwardsKnownAudioAndVideoWithoutKeyAndStripsTrailer",
    "SifTransform.RejectsForeignTrailerPayloadWithAndWithoutKeyOnBothMediaPaths",
    "SifTransform.KeepsDisabledAndEmptyBehaviorForBothDiscardPolicies",
    "SifTransform.OrdinaryAuthenticatedMediaStillRoundTripsOnBothMediaPaths",
    "KeyProvider.DerivationLogsOnlySizesForBothAlgorithms",
    "KeyProvider.CopiesPreserveDiscardFrameWhenCryptorNotReady",
    "SifTransform.RejectsShortEncryptedHeadersIvAndTags",
    "SifTransform.RejectsRbspWhichShrinksBelowTheAuthenticatedRecord",
    "KeyProvider.SifTrailerSnapshotsStayCoherentDuringUpdates",
)
BACKPORTS = {
    "01-sif-validation.patch": (
        "4f5ba9a827220170080ae4d1183e01b437eb7014c391bda46cd258ae0c5f5169"
    ),
    "02-key-log-removal.patch": (
        "793e25908a56d8993cb082e40728ad1bd39919a0abf988b1921384d15dd601fd"
    ),
    "03-root-refresh.patch": (
        "6f10c7bf1f0396e5a6e287343a93049d3b7e9387f1cffc9fe7f70a580d0bf508"
    ),
    "04-copy-discard-option.patch": (
        "ad5f64e55a6c4187b242535f84149c46590565d0e1017f1340856a8fcbd06835"
    ),
    "05-frame-bounds.patch": (
        "3e9934378b8c164314907ba7161f9b223554201fda47dad50744a1ae15c61b1d"
    ),
    "06-sif-trailer-snapshot.patch": (
        "df26ab1d8a31ca5cab1f39fda912e0c3e73a726c59cfcde4457e79415094edfd"
    ),
    "07-h264-prefix-validation.patch": (
        "1de7bf5402089c13f93f108f3008aaa8c025e2bdbd7b2375694417c186c86a17"
    ),
    "08-rbsp-decoded-iv.patch": (
        "dbefa1b81cd8b24eb0bcd41ec996e6b14265d66ffdad38cf1112e646821d4e09"
    ),
}
POSTIMAGES = {
    "api/crypto/BUILD.gn": (
        "3b5b3fc8b9522423078f7b3ec803b0f154c376ae51dc1db9ca9d335f279a017b"
    ),
    "api/crypto/frame_crypto_transformer.cc": (
        "81b1bc99719a7f67fa5d7fae7504f4db7c4245d4d97318df3bebf417b98af0c7"
    ),
    "api/crypto/frame_crypto_transformer.h": (
        "b3b050ed5553f0a132cbc20300366668b155dd4248dfb0c1dee71080e3bc2587"
    ),
    "api/crypto/frame_crypto_transformer_unittest.cc": (
        "2b1356142ecc88758b03e89ee615694edb03db4bc15b610bbdfe89d551860e56"
    ),
    "api/crypto/sif_payload_unittest.cc": (
        "f79355710f7350a9de13ae07d6e13144bb913a02c989c01772c28738422cac7c"
    ),
    "rtc_base/ssl_roots.h": (
        "f54188260ea8fd409ecd789a463931e9b1bde5a37a0c699a2535044c4ba9b7f3"
    ),
}
REFERENCE_PATCHES = {
    "add_licenses.patch": "3744d07bce56a3cc281f70196ad52513b6395d353a73d2de97f2de616bc27625",
    "fix_license_json_parsing.patch": "602275f0ebe9b5122496eaf3ecf624068316c87fdbcfee9d8fa5495463ca471b",
    "ssl_verify_callback_with_native_handle.patch": "55042fe99107439e88e56cddec4f5caf8c8a92e72f2e035ac8e37d52aaecb20c",
    "add_deps.patch": "da222e44f306137ed96b626879948325dc21857dcf054482b1e3df83e59fe5f4",
    "external_audio_source.patch": "83bfe8101270cc60be25991ec07bbabf51c071f6529b50895e4b1ef07314b81f",
}
LINUX_PATCHES = {
    "fix_desktop_capture_compile.patch": "e768451621cffb76232bac65b926e197dc49947fd8cb31e5f976bd480a659c85",
    "disable_crel.patch": "9c1c89c4f488209bf3397900754a5980b23e30702df579cca02ba96c164e8d2c",
    "david_disable_gun_source_macro.patch": "a4789ab8a70cd1d17f15a486caf02736c5ea350facdd4718b7504aa3bf8f66f1",
    "disable_sme_for_libyuv.patch": "e19d3de930350f9257e8ebbd50b7a94677152f999ef94df70434258fcec37a2d",
}
PREFIX_SHA = "740c1e02bbfcce3b053c43442a2f66341424b78cbe861deb0aa1ba4198d10b6d"
MAC_TARGETS = (
    "api/audio_codecs:builtin_audio_decoder_factory", "api/task_queue:default_task_queue_factory",
    "sdk:native_api", "sdk:default_codec_factory_objc", "pc:peer_connection",
    "sdk:videocapture_objc", "sdk:mac_framework_objc", "desktop_capture_objc",
    "modules/audio_device:audio_device",
)
GATES = {"source", "deps-hooks", "toolchain-rust", "patches", "native-test-source",
         "license-material", "license-gn-resolution", "wrapper-abi", "symbol-prefix",
         "generated-metadata"}
HEX = re.compile(r"[0-9a-f]{64}\Z")
MAC_MATERIAL_HELPER_SHA = "8a2f35fbf33b0d0121a496acb620db9a61cf47b953ea01fcf6ce3f7944e32f3c"
COMMAND_PROCESS_HELPER_SHA = "5debe4d363c339de898b965dee15660867b9a0bc4a96380f434bf19deab20c7b"
MAX_LOG = 256 * 1024 * 1024
MAX_INVENTORY = 128 * 1024 * 1024
MAX_FILES = 1_000_000
PYTHON_PLAN_SHA = "b3cd3a5066338da7fa450fe48580d987dbb88f90d9497dc2106802ab989c0aea"
PYTHON_DATA_BINDING_SHA = "cfa2923072c9297a92740d3b6451e18fedc07fd401f26963d448021371a2ec21"
PYTHON_STAGER_SHA = "4de9a016a988535b7576f2e0d7561afa290529dae7b1eb267bb8382956d49d02"
CIPD_BINARY_SHA = "a4245bbd790e00c43756c1c202c21a3d2b5f0a4fe655fdb4d100029dfe9f223a"
INSTRUMENTED_PLAN_SHA = "927a95029b9ddc8f4b4cce20f07975505b343260d913b9c3488bc6f6182ad0f9"
INSTRUMENTED_DATA_BINDING_SHA = "343c10863e454774b759b078b4e56be87774578eba68e05831560844b5e6b913"
INSTRUMENTED_STAGER_SHA = "8903aef2de2ea5a7778ad4ba21047909ad7595d3e1359f151829e489910fc84f"
INSTRUMENTED_SOURCE_PREFIX = "third_party/instrumented_libs/binaries"
LINUX_MATERIAL_HELPER_SHA = "acca8d027e3e2f1ce3a38181345b522a2906e5eeb1a1fc39cd3c1d2ff3bf0b51"
LINUX_LITERAL_PATHS = {
    prefix + "lib/systemd/system/system-systemd\\x2dcryptsetup.slice"
    for prefix in ("build/linux/debian_bullseye_amd64-sysroot/",
                   "build/linux/debian_bullseye_i386-sysroot/",
                   "checkout/src/build/linux/debian_bullseye_amd64-sysroot/",
                   "checkout/src/build/linux/debian_bullseye_i386-sysroot/",
                   "linux-inputs/sysroot-amd64/", "linux-inputs/sysroot-i386/")
}



class Refused(Exception):
    pass


def require(ok):
    if not ok:
        raise Refused()


def rel(value):
    require(isinstance(value, str) and bool(value) and (
        "\\" not in value or (value in LINUX_LITERAL_PATHS and platform.system() == "Linux"
                                and platform.machine() == "x86_64")))
    p = PurePosixPath(value)
    require(not p.is_absolute() and all(x not in ("", ".", "..") for x in p.parts))
    require(str(p) == value)
    return p


def read(path, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_size <= limit)
        with os.fdopen(fd, "rb", closefd=False) as f:
            data = f.read(limit + 1)
        after = os.fstat(fd)
        require(len(data) == before.st_size and before.st_dev == after.st_dev
                and before.st_ino == after.st_ino and before.st_size == after.st_size
                and before.st_mtime_ns == after.st_mtime_ns
                and before.st_ctime_ns == after.st_ctime_ns)
        return data
    finally:
        os.close(fd)


def digest(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode))
        h = hashlib.sha256()
        while True:
            data = os.read(fd, 1024 * 1024)
            if not data:
                break
            h.update(data)
        after = os.fstat(fd)
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns,
                 before.st_ctime_ns) == (after.st_dev, after.st_ino, after.st_size,
                                         after.st_mtime_ns, after.st_ctime_ns))
        return h.hexdigest()
    finally:
        os.close(fd)


def bound_json(path, expected, limit=MAX_INVENTORY):
    require(isinstance(expected, str) and HEX.fullmatch(expected))
    data = read(path, limit)
    require(hashlib.sha256(data).hexdigest() == expected)
    return json.loads(data)


def write_new(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())


def json_new(path, value):
    write_new(path, (json.dumps(value, sort_keys=True, indent=2) + "\n").encode())


def inventory(root, rows):
    require(root.is_absolute() and root == root.resolve() and root.is_dir())
    require(isinstance(rows, list) and 0 < len(rows) <= MAX_FILES)
    expected = {}
    for row in rows:
        name = str(rel(row["path"]))
        require(name not in expected and ".git" not in PurePosixPath(name).parts)
        p = root / name
        require(p.parent.resolve().is_relative_to(root))
        st = p.lstat()
        if row["type"] == "file":
            require(stat.S_ISREG(st.st_mode) and st.st_size == row["bytes"])
            if "mode" in row:
                require(row["mode"] in {"100644", "100755"}
                        and stat.S_IMODE(st.st_mode) == int(row["mode"], 8) & 0o777)
            require(digest(p) == row["sha256"])
        elif row["type"] == "symlink":
            require(stat.S_ISLNK(st.st_mode) and os.readlink(p) == row["target"])
            require(p.resolve(strict=True).is_relative_to(root))
        else:
            raise Refused()
        expected[name] = row
    seen = set()
    for directory, dirs, files in os.walk(root, followlinks=False, onerror=lambda _: require(False)):
        require(".git" not in dirs and ".git" not in files)
        for name in list(dirs):
            p = Path(directory) / name
            if p.is_symlink():
                seen.add(str(p.relative_to(root)))
                dirs.remove(name)
        for name in files:
            seen.add(str((Path(directory) / name).relative_to(root)))
    require(seen == set(expected))
    return expected


def gitlink_name(value):
    require(type(value) is str and 0 < len(value) <= 1024 and "\\" not in value and ":" not in value)
    path = PurePosixPath(value)
    require(not path.is_absolute() and str(path) == value and len(path.parts) <= 64)
    for part in path.parts:
        require(part not in (".", "..") and part.casefold() != ".git" and not part.endswith((" ", ".")))
        require(not any(char in part for char in '<>"|?*\r\n\x00')
                and all(32 <= ord(char) != 127 for char in part))
        require(not re.fullmatch(r"(?i)(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?", part))
    return value


def gitlink_material(plan, rows, repositories, materials, receipt):
    require(plan["schema"] == "morpheus-native-webrtc-sources-v2")
    require(type(plan["gitlinks"]) is list and len(plan["gitlinks"]) <= 1024)
    require(type(rows) is list and len(rows) <= 1024)
    original = {row["mount"]: row for row in repositories}
    require(len(original) == len(repositories)
            and len({name.casefold() for name in original}) == len(original))
    planned = {row["mount"]: row for row in plan["repositories"]}
    require(len(planned) == len(plan["repositories"]) == len(original)
            and set(planned) == set(original))
    for mount, owner in original.items():
        gitlink_name(mount)
        require(all(owner[key] == planned[mount][key] for key in ("mount", "url", "revision", "tree")))
        require(re.fullmatch(r"[0-9a-f]{40}", owner["revision"])
                and re.fullmatch(r"[0-9a-f]{40}", owner["tree"]))
    declarations = {}
    for row in plan["gitlinks"]:
        require(type(row) is dict and set(row) == {"mount", "path", "mode", "type", "oid", "action"})
        mount, path = gitlink_name(row["mount"]), gitlink_name(row["path"])
        name = gitlink_name(mount + "/" + path)
        require(mount in original and row["mode"] == "160000" and row["type"] == "commit"
                and type(row["oid"]) is str and re.fullmatch(r"[0-9a-f]{40}", row["oid"])
                and row["action"] == "uninitialized-directory")
        require(name.casefold() not in {key.casefold() for key in declarations})
        declarations[name] = row
    roots = {name.casefold() for name in declarations}
    prefixes = set(roots)
    for name, row in declarations.items():
        parents = {str(path) for path in PurePosixPath(name.casefold()).parents if str(path) != "."}
        require(not (parents & roots))
        prefixes.update(parents)
        owner = row["mount"].casefold()
        for mount in original:
            other, marker = mount.casefold(), name.casefold()
            if other == owner or owner.startswith(other + "/"):
                continue
            require(not (other == marker or other.startswith(marker + "/") or marker.startswith(other + "/")))
    records = {}
    for row in rows:
        require(type(row) is dict and set(row) == {"mount", "path", "mode", "type", "oid", "action",
                "repository_revision", "repository_tree", "exported_path", "materialized_type",
                "materialized_empty", "source_present", "submodule_initialized"})
        name = gitlink_name(row["exported_path"])
        require(name in declarations and name not in records)
        declaration = declarations[name]
        owner = original[declaration["mount"]]
        expected = {**declaration, "repository_revision": owner["revision"], "repository_tree": owner["tree"],
                    "exported_path": name, "materialized_type": "directory", "materialized_empty": True,
                    "source_present": False, "submodule_initialized": False}
        require(row == expected and row["materialized_empty"] is True
                and row["source_present"] is False and row["submodule_initialized"] is False)
        records[name] = row
    require(set(records) == set(declarations))
    seen = set()
    for mount, owner in original.items():
        require(type(owner.get("gitlinks")) is list and len(owner["gitlinks"]) <= 1024)
        for row in owner["gitlinks"]:
            require(type(row) is dict and row.get("mount") == mount)
            name = row.get("exported_path")
            require(type(name) is str and name not in seen and row == records.get(name)
                    and row["materialized_empty"] is True and row["source_present"] is False
                    and row["submodule_initialized"] is False)
            seen.add(name)
    require(seen == set(records))
    for value in (materials, receipt):
        require(type(value.get("uninitialized_gitlinks")) is int
                and value["uninitialized_gitlinks"] == len(records)
                and value.get("submodule_sources_materialized") is False)
    return {"entries": records, "rows": rows, "roots": roots, "prefixes": prefixes}


def outside_gitlinks(name, policy):
    path = PurePosixPath(name.casefold())
    require(str(path) not in policy["prefixes"]
            and not any(str(parent) in policy["roots"] for parent in path.parents))


def package_source_headers(original_rows, complete_source):
    # Public native/wrapper headers come from the original Git repositories.
    # Projected compiler and sysroot headers stay complete build-input DATA.
    original = {row["path"]: row for row in original_rows}
    require(len(original) == len(original_rows))
    require(all(complete_source.get(name) == row for name, row in original.items()))
    return {name for name in original if name.endswith((".h", ".inc"))}


class Build:
    def __init__(self, args):
        host, arch, target_os, cpu, package = TARGETS[args.target]
        require(platform.system() == host and platform.machine() == arch)
        require(os.environ.get("GITHUB_ACTIONS") == "true"
                and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
                and os.environ.get("GITHUB_REPOSITORY") == "brk275/Morpheus")
        temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
        self.work = Path(args.runner_root)
        require(self.work.is_absolute() and not self.work.exists()
                and self.work.parent.resolve(strict=True).is_relative_to(temp))
        self.m = bound_json(Path(args.admission), args.admission_sha256)
        require(self.m["schema"] == (5 if host == "Linux" else 6)
                and self.m["target"] == args.target)
        require(("python_runtime" in self.m) is (host == "Linux")
                and ("instrumented_data" in self.m) is (host == "Linux")
                and ("linux_materials" in self.m) is (host == "Linux")
                and ("mac_materials" in self.m) is (host == "Darwin")
                and ("mac_host" in self.m) is (host == "Darwin"))
        require(self.m["recipe_sha256"] == digest(Path(__file__).resolve()))
        require(self.m["source_revision"] == SOURCE_REV and self.m["source_tree"] == SOURCE_TREE
                and self.m["sdk_revision"] == SDK_REV
                and self.m["depot_revision"] == DEPOT_REV and self.m["depot_tree"] == DEPOT_TREE)
        require(set(self.m["gates"]) == GATES | (
            {"python-runtime", "instrumented-data"} if host == "Linux" else {"mac-python-runtime", "mac-host-sdk"}))
        for gate_name, gate in self.m["gates"].items():
            receipt = bound_json(Path(gate["path"]), gate["sha256"])
            require(receipt["status"] == "approved-for-isolated-native-build"
                    and receipt["source_revision"] == SOURCE_REV
                    and receipt["target"] == args.target)
            if gate_name == "generated-metadata":
                require(receipt["metadata_policy_sha256"] == self.m["gn_metadata"]["sha256"])
            if gate_name == "python-runtime":
                require(receipt["python_data_binding_sha256"] == PYTHON_DATA_BINDING_SHA
                        and receipt["python_stager_sha256"] == PYTHON_STAGER_SHA
                        and receipt["python_plan_sha256"] == PYTHON_PLAN_SHA
                        and receipt["python_materials_sha256"]
                            == self.m["python_runtime"]["materials"]["sha256"])
            if gate_name == "instrumented-data":
                require(receipt["instrumented_data_binding_sha256"] == INSTRUMENTED_DATA_BINDING_SHA
                        and receipt["instrumented_stager_sha256"] == INSTRUMENTED_STAGER_SHA
                        and receipt["instrumented_plan_sha256"] == INSTRUMENTED_PLAN_SHA
                        and receipt["instrumented_materials_sha256"]
                            == self.m["instrumented_data"]["materials"]["sha256"])
            if gate_name == "deps-hooks" and host == "Linux":
                require(receipt["dependency_decisions_sha256"]
                        == self.m["linux_materials"]["dependency_decisions"]["sha256"]
                        and receipt["declaration_ledger_sha256"]
                        == "2ea59842c90033f32529753950bff4172ef1322107784b7aea3cc00fa2478d58")
            if gate_name == "toolchain-rust" and host == "Linux":
                require(receipt["linux_plan_sha256"] == self.m["linux_materials"]["plan"]["sha256"]
                        and receipt["linux_materials_sha256"] == self.m["linux_materials"]["linux_materials"]["sha256"]
                        and receipt["core_materials_sha256"] == self.m["linux_materials"]["core_materials"]["sha256"]
                        and receipt["source_overlay_sha256"] == self.m["source_provision"]["source_overlay"]["sha256"])
            if gate_name in {"mac-python-runtime", "mac-host-sdk"}:
                require(receipt["mac_material_plan_sha256"] == self.m["mac_materials"]["plan"]["sha256"]
                        and receipt["mac_host_binding_sha256"] == self.m["mac_host"]["sha256"]
                        and receipt["mac_material_helper_sha256"] == MAC_MATERIAL_HELPER_SHA)
            if gate_name in {"patches", "native-test-source"}:
                require(receipt["native_security_binding_sha256"]
                        == self.m["native_security"]["sha256"])
        self.roots, self.files = {}, {}
        for name, row in self.m["roots"].items():
            root = Path(row["path"])
            data = bound_json(Path(row["inventory"]), row["inventory_sha256"])
            self.files[name] = inventory(root, data)
            self.roots[name] = root
        require({"source", "depot", "tools", "sdk", "reference", "provenance"} <= set(self.roots))
        require(("python-data" in self.roots) is (host == "Linux")
                and ("instrumented-data" in self.roots) is (host == "Linux")
                and ("mac-data" in self.roots) is (host == "Darwin"))
        require(all(not self.work.is_relative_to(root) for root in self.roots.values()))
        self.target = args.target
        self.host_tools = {}
        if host == "Darwin":
            self.validate_mac_host()
        self.src = self.roots["source"]
        self.tools = self.m["tools"]
        roles = {"git", "python", "vpython3", "gn", "ninja", "llvm-ar",
                 "llvm-objcopy", "clang++", "rustc", "cargo", "license-gn"}
        require(set(self.tools) == roles | {"cipd"})
        for role in roles | {"cipd"}:
            t = self.tools[role]
            path = self.item(t)
            require(os.access(path, os.X_OK) and digest(path) == t["sha256"])
        require(self.tools["license-gn"]["sha256"] == self.tools["gn"]["sha256"])
        self.work.mkdir(mode=0o700)
        for name in ("logs", "home", "tmp", "prod", "tests", "license", "bin"):
            (self.work / name).mkdir(mode=0o700)
        launchers = {"python3": "python", "vpython3": "vpython3", "gn": "gn",
                     "ninja": "ninja", "git": "git", "rustc": "rustc",
                     "cargo": "cargo", "clang++": "clang++"}
        launchers["cipd"] = "cipd"
        for name, role in launchers.items():
            (self.work / "bin" / name).symlink_to(self.item(self.tools[role]))
        self.env = {
            "HOME": str(self.work / "home"), "TMPDIR": str(self.work / "tmp"),
            "PATH": os.pathsep.join([str(self.work / "bin"), "/usr/bin", "/bin"]),
            "LANG": "C.UTF-8" if host == "Linux" else "en_US.UTF-8",
            "LC_ALL": "C", "PYTHONDONTWRITEBYTECODE": "1", "PYTHONNOUSERSITE": "1",
            "DEPOT_TOOLS_UPDATE": "0", "DEPOT_TOOLS_METRICS": "0",
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_OPTIONAL_LOCKS": "0", "GIT_NO_REPLACE_OBJECTS": "1",
            "GIT_TERMINAL_PROMPT": "0", "GIT_ALLOW_PROTOCOL": "",
            "GIT_NO_LAZY_FETCH": "1", "GIT_ATTR_NOSYSTEM": "1",
        }
        tc = self.m["toolchain"]
        require(tc["rust_enabled"] is True and tc["rust_cxx_enabled"] is True
                and tc["chromium_prelude_enabled"] is True)
        require(self.item(tc["sysroot_or_sdk"]).is_dir())
        if host == "Darwin":
            self.env["DEVELOPER_DIR"] = str(self.item(tc["developer_dir"]))
            self.env["SDKROOT"] = str(self.item(tc["sysroot_or_sdk"]))
        self.jobs, self.target, self.package = args.jobs, args.target, package
        self.target_os, self.cpu = target_os, cpu
        self.commands = []
        self.python_runtime_proof = None
        if host == "Linux":
            self.validate_python_materials()
            self.validate_instrumented_materials()
            self.validate_linux_materials()
        else:
            self.validate_mac_materials()
        self.validate_source_export()
        self.validate_patches()

    def item(self, row):
        if set(row) == {"host", "sha256"}:
            require(self.target in ("mac-arm64", "mac-x64") and row["host"] == "git"
                    and row["sha256"] == self.host_tools["git"]["sha256"])
            return Path(self.host_tools["git"]["path"])
        root = self.roots[row["root"]]
        path = root / str(rel(row["path"]))
        resolved = path.resolve(strict=True)
        require(resolved.is_relative_to(root))
        return resolved

    def tool(self, role):
        return str(self.item(self.tools[role]))

    def command(self, label, argv, timeout, cwd=None):
        require(re.fullmatch(r"[a-z0-9-]{1,64}", label) is not None)
        if self.target in ("mac-arm64", "mac-x64"):
            self.verify_mac_host()
        # Keep one process owner for the complete build. An unproved cleanup
        # leaves its retained authority fatal for every later command.
        runner = getattr(self, "_native_command_runner", None)
        if runner is None:
            runner = self.native_module("git_source_stager.py", COMMAND_PROCESS_HELPER_SHA)
            self._native_command_runner = runner
        require(os.name == "posix" and runner.MAX_METADATA == MAX_LOG)
        log = self.work / "logs" / (label + ".log")
        stdout = self.work / "logs" / (label + ".capture.stdout")
        stderr = self.work / "logs" / (label + ".capture.stderr")
        # Refuse a repeated label before executing anything. All three files
        # are private, exclusive outputs; only the owned pipe readers write
        # capture files, within one shared pre-write MAX_LOG budget.
        fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as output:
            capture = runner.run_git_owned(argv, cwd or self.src, self.env, timeout,
                                           stdout, stderr, MAX_LOG, None)
            require(capture["original_joined"] is True
                    and capture["process_contract"] == "trusted_git_fresh_process_group"
                    and capture["combined_limit_bytes"] == MAX_LOG)
            # Preserve the existing complete log consumed by Git/GTest/archive
            # parsers. Both captured streams are retained privately; the log
            # concatenates stdout then stderr after the original group is killed,
            # its leader joined and both readers finished. It is not a hostile
            # setsid-escape sandbox and grants no tool execution admission.
            total = 0
            for path, expected in ((stdout, capture["stdout_bytes"]),
                                   (stderr, capture["stderr_bytes"])):
                initial = path.lstat()
                require(stat.S_ISREG(initial.st_mode) and initial.st_nlink == 1
                        and initial.st_uid == os.geteuid()
                        and stat.S_IMODE(initial.st_mode) == 0o600
                        and initial.st_size == expected <= MAX_LOG)
                incoming = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
                with os.fdopen(incoming, "rb") as source:
                    identity = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
                    require(identity(initial) == identity(os.fstat(source.fileno())))
                    count = 0
                    while block := source.read(64 * 1024):
                        count += len(block)
                        total += len(block)
                        require(count <= expected and total <= MAX_LOG)
                        output.write(block)
                    require(count == expected and identity(initial)
                            == identity(os.fstat(source.fileno())) == identity(path.lstat()))
            output.flush()
            os.fsync(output.fileno())
        if self.target in ("mac-arm64", "mac-x64"):
            self.verify_mac_host()
        self.commands.append({"label": label, "argv": argv, "exit": 0,
                              "log_sha256": digest(log), "process_capture": capture})
        return log

    def native_module(self, name, expected):
        path = Path(__file__).resolve().parent / "native_webrtc" / name
        raw = read(path, 256 * 1024)
        require(hashlib.sha256(raw).hexdigest() == expected)
        module = types.ModuleType("morpheus_mac_" + name.replace(".", "_"))
        module.__file__ = str(path)
        exec(compile(raw, str(path), "exec"), module.__dict__)
        return module

    def validate_mac_host(self):
        binding = bound_json(Path(self.m["mac_host"]["path"]), self.m["mac_host"]["sha256"], 1024 * 1024)
        require(binding["schema"] == "morpheus-native-macos-host-v1"
                and binding["target"] == self.target and binding["sdk_redistribution"] is False)
        sdk = self.roots["sdk"]
        require(sdk == sdk.resolve(strict=True) and sdk.parent == Path("/Applications")
                and re.fullmatch(r"Xcode(?:_[A-Za-z0-9_.-]+)?\.app", sdk.name))
        require(binding["sdk_inventory_sha256"] == self.m["roots"]["sdk"]["inventory_sha256"]
                and binding["developer_dir"] == self.m["toolchain"]["developer_dir"]
                and binding["sdk_path"] == self.m["toolchain"]["sysroot_or_sdk"]
                and binding["developer_dir"]["root"] == "sdk" and binding["sdk_path"]["root"] == "sdk")
        require(set(binding["tools"]) == {"git", "xcodebuild", "xcrun", "xcode-select", "sw_vers"})
        require(binding["mac_sdk_min"] == "15"
                and binding["mac_sdk_gni_sha256"] == "b3962e7ad03db5784e84dbce294b2f1d5f27528f360ab35c24aa1c9c272dceda"
                and binding["find_sdk_sha256"] == "fcfdded28669fa5d964df92f5874f100e271f0a1f966151d8a084f71c3bf866a")
        require(digest(self.roots["source"] / "build/config/mac/mac_sdk.gni") == binding["mac_sdk_gni_sha256"]
                and digest(self.roots["source"] / "build/mac/find_sdk.py") == binding["find_sdk_sha256"])
        # The original native override runs this selector: absent the force
        # environment and corporate-host directory, the genuine path is system
        # Xcode. The clean command environment never inherits a force flag.
        require(not os.path.lexists("/Library/GoogleCorpSupport")
                and digest(self.roots["source"] / "build/mac/should_use_hermetic_xcode.py") == "9f71d27d1af372205d07318c240622c156ee18f67e7499a6356a697c238f7691"
                and digest(self.roots["source"] / "build_overrides/build.gni") == "5e2c94a0428fc6ff417a7fd71843c040d31c2b16e38a5d3a09b2bb4488a3a183")
        developer = self.item(binding["developer_dir"])
        sdk_dir = developer / "Platforms/MacOSX.platform/Developer/SDKs"
        candidates = []
        for child in sdk_dir.iterdir():
            match = re.fullmatch(r"MacOSX([0-9]+)\.([0-9]+)\.sdk", child.name)
            if match and child.is_dir():
                version = tuple(int(part) for part in match.groups())
                if version >= (15, 0): candidates.append((version, child.resolve(strict=True)))
        require(0 < len(candidates) <= 32
                and self.item(binding["sdk_path"]) == sorted(candidates)[0][1])
        helper = self.native_module("macos_materials.py", MAC_MATERIAL_HELPER_SHA)
        _, directories = helper.sdk_inventory(sdk)
        require(hashlib.sha256(helper.canonical(directories)).hexdigest() == binding["directory_topology_sha256"])
        self.host_tools = binding["tools"]
        self.verify_mac_host()

    def verify_mac_host(self):
        for role, row in self.host_tools.items():
            path = Path("/usr/bin") / role
            require(set(row) == {"path", "bytes", "sha256"} and Path(row["path"]) == path)
            actual = path.resolve(strict=True)
            require(actual.is_relative_to(Path("/usr")) and actual.is_file())
            info = actual.stat()
            require(info.st_uid == 0 and info.st_mode & 0o022 == 0
                    and info.st_size == row["bytes"] and digest(actual) == row["sha256"])

    def validate_mac_materials(self):
        data = self.m["mac_materials"]
        require(set(data) == {"schema", "plan", "materials", "overlay"}
                and data["schema"] == "morpheus-macos-material-consumer-v1"
                and all(data[key]["root"] == "mac-data" for key in ("plan", "materials"))
                and data["plan"]["path"] == "material-plan.json"
                and data["materials"]["path"] == "tool-materials.json")
        self.mac_helper = self.native_module("macos_materials.py", MAC_MATERIAL_HELPER_SHA)
        self.mac_material = self.native_module("linux_materials.py", LINUX_MATERIAL_HELPER_SHA)
        python = self.native_module("python_stager.py", PYTHON_STAGER_SHA)
        transport = self.native_module("linux_stager.py", "f3a908bc20170ee01f2643169ce60397627981d86de11ffaedf03dd8ea9e1c8c")
        core_rows, _ = self.mac_helper.core_profile(self.roots["tools"], self.target, self.mac_material, transport)
        self.mac_plan, expected, _ = self.mac_helper.profile(self.roots["mac-data"],
            data["plan"]["sha256"], self.target, self.mac_material, python)
        require(set(self.files["mac-data"]) == set(expected))
        for role, ref in self.mac_plan["roles"].items():
            require(self.tools[role] == {"root": "mac-data", **ref})
        for spec in self.mac_plan["specs"]:
            path = self.src / (".vpython3" if spec["name"] == "native" else "third_party/depot_tools/.vpython3")
            require(read(path, 65536) == spec["source"].encode())
        require(digest(self.item(data["materials"])) == data["materials"]["sha256"])
        self.mac_overlay = bound_json(self.item(data["overlay"]), data["overlay"]["sha256"])
        require(data["overlay"]["sha256"] == self.m["source_provision"]["source_overlay"]["sha256"])
        projected = []
        for rows, prefix, mount in ((core_rows, "tools/gn/", "buildtools/mac/"),
                (core_rows, "tools/ninja/", "third_party/ninja/"),
                (core_rows, "tools/clang/", "third_party/llvm-build/Release+Asserts/"),
                (expected, "runtime/rust/", "third_party/rust-toolchain/")):
            for name, row in rows.items():
                if name.startswith(prefix):
                    projected.append({"path": mount + name[len(prefix):], "type": "file",
                        "mode": "100755" if row["mode"] & 0o111 else "100644",
                        "bytes": row["bytes"], "sha256": row["sha256"]})
        require(self.mac_overlay == sorted(projected, key=lambda row: row["path"]))

    def prepare_mac_runtime(self):
        self.verify_gitlinks()
        for name in ("python-cache", "python-cache/cipd", "python-cache/cipd/instances",
                     "python-cache/vpython", "python-results"):
            (self.work / name).mkdir(mode=0o700)
        self.env.update({"CIPD_CACHE_DIR": str(self.work / "python-cache/cipd"),
                         "VPYTHON_VIRTUALENV_ROOT": str(self.work / "python-cache/vpython"),
                         "CIPD_SERVICE_URL": "https://chrome-infra-packages.appspot.com",
                         "CIPD_MAX_THREADS": "2", "CIPD_PARALLEL_DOWNLOADS": "2"})
        pins = []
        for row in self.mac_plan["materials"]:
            if not row["seed_cache"]: continue
            original = self.roots["mac-data"] / "cipd-cache/instances" / row["instance_id"]
            raw = read(original, row["bytes"])
            require(len(raw) == row["bytes"] and hashlib.sha256(raw).hexdigest() == row["sha256"])
            write_new(self.work / "python-cache/cipd/instances" / row["instance_id"], raw)
            output = self.work / "python-results" / (str(len(pins)).zfill(3) + ".json")
            old_umask = os.umask(0o077)
            try:
                self.command("mac-python-resolve-" + str(len(pins)), [self.tool("cipd"), "resolve",
                    "-cache-dir=" + self.env["CIPD_CACHE_DIR"], "-service-url=" + self.env["CIPD_SERVICE_URL"],
                    "-version=" + row["version"], "-json-output=" + str(output), row["package"]], 60)
            finally: os.umask(old_umask)
            require(json.loads(read(output, 65536)) == {"result": {"": [{"package": row["package"],
                "pin": {"package": row["package"], "instance_id": row["instance_id"]}}]}})
            pins.append({"package": row["package"], "version": row["version"], "instance_id": row["instance_id"],
                         "archive_sha256": row["sha256"], "result_sha256": digest(output)})
        for spec in self.mac_plan["specs"]:
            path = self.src / (".vpython3" if spec["name"] == "native" else "third_party/depot_tools/.vpython3")
            old_umask = os.umask(0o077)
            try:
                self.command("mac-python-install-" + spec["name"], [self.tool("vpython3"), "-vpython-root",
                    self.env["VPYTHON_VIRTUALENV_ROOT"], "-vpython-spec", str(path), "-vpython-tool", "install"], 900)
            finally: os.umask(old_umask)
        self.verify_mac_instances()
        self.verify_gitlinks()
        self.python_runtime_proof = {"schema": "morpheus-genuine-macos-vpython-setup-v1",
            "material_plan_sha256": self.m["mac_materials"]["plan"]["sha256"], "resolved_original_pins": pins,
            "genuine_install_specs": [{"name": s["name"], "sha256": s["sha256"]} for s in self.mac_plan["specs"]],
            "cache_scope": "same-owned-work-root-through-GN-Ninja", "standalone_pip": False,
            "native_source_readiness_inferred": False}
        json_new(self.work / "python-realization-receipt.json", self.python_runtime_proof)

    def verify_mac_instances(self):
        for row in self.mac_plan["materials"]:
            if not row["seed_cache"]: continue
            path = self.work / "python-cache/cipd/instances" / row["instance_id"]
            info = path.lstat()
            require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid() and info.st_nlink == 1
                    and stat.S_IMODE(info.st_mode) == 0o600 and info.st_size == row["bytes"]
                    and digest(path) == row["sha256"])

    def validate_python_materials(self):
        runtime = self.m["python_runtime"]
        require(set(runtime) == {"schema", "plan", "materials", "core_manifest"}
                and runtime["schema"] == "morpheus-linux-vpython-consumer-v1"
                and "python-data" in self.roots)
        root = self.roots["python-data"]
        require(root.parent == Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
                and re.fullmatch(r"morpheus-webrtc-tools-[0-9a-f]{16}", root.name))
        for name, other in self.roots.items():
            if name != "python-data":
                require(not root.is_relative_to(other) and not other.is_relative_to(root))
        fixed = {"plan": "selected-python-inputs.json", "materials": "tool-materials.json",
                 "core_manifest": "selected-tool-manifest.json"}
        for name, path in fixed.items():
            require(runtime[name]["root"] == "python-data" and runtime[name]["path"] == path)
        require(runtime["plan"]["sha256"] == PYTHON_PLAN_SHA)
        plan = bound_json(self.item(runtime["plan"]), PYTHON_PLAN_SHA, 2 * 1024 * 1024)
        material = bound_json(self.item(runtime["materials"]), runtime["materials"]["sha256"], 2 * 1024 * 1024)
        core = self.item(runtime["core_manifest"])
        require(digest(core) == runtime["core_manifest"]["sha256"] == material["manifest_sha256"])
        require(plan["schema"] == "morpheus-native-linux-vpython-data-v1"
                and plan["baseline"] == SOURCE_REV and plan["tree"] == SOURCE_TREE
                and plan["target"] == "Linuxx64" and len(plan["materials"]) == 33)
        require(material["schema"] == 3 and material["status"] == "linux_python_data_staged"
                and material["target"] == "Linuxx64" and material["baseline"] == SOURCE_REV
                and material["python_inputs_sha256"] == PYTHON_PLAN_SHA
                and material["python_materials"] == plan["materials"]
                and material["raw_instance_data_seeded"] == 30
                and material["executed_tools"] == [] and material["tools_executed"] == [])
        require(all(material[key] is False for key in (
            "environment_complete", "genuine_tag_cache_created", "genuine_vpython_derivations_created",
            "native_sources_complete", "compiler_cipd_runtime_closure", "build_ready",
            "distribution_ready", "hooks_executed")))
        expected, directories = {}, {"."}
        def entry(path, size, sha, mode):
            name = str(rel(path))
            require(name not in expected and HEX.fullmatch(sha))
            expected[name] = (size, sha, mode)
            directories.update(str(p) for p in PurePosixPath(name).parents)
        for row in plan["materials"]:
            entry("python-archives/" + row["input_path"], row["bytes"], row["sha256"], 0o600)
            if row["seed_cache"]:
                entry("cipd-cache/instances/" + row["instance_id"], row["bytes"], row["sha256"], 0o600)
            if row["extraction"] != "archive-only":
                prefix = "python-runtime/" + row["name"] + "/"
                for member in row["members"]:
                    if member["kind"] == "regular":
                        entry(prefix + member["path"], member["bytes"], member["sha256"], member["output_mode"])
                    elif member["kind"] == "directory":
                        name = str(rel(prefix + member["path"]))
                        directories.add(name)
                        directories.update(str(p) for p in PurePosixPath(name).parents)
                for alias in row["alias_copies"]:
                    entry(prefix + alias["path"], alias["bytes"], alias["sha256"], alias["output_mode"])
        require([spec["name"] for spec in plan["specs"]] == ["native", "depot"])
        for spec in plan["specs"]:
            entry("python-specs/" + spec["name"] + "/.vpython3", spec["bytes"], spec["sha256"], 0o600)
            actual = self.src / (".vpython3" if spec["name"] == "native" else "third_party/depot_tools/.vpython3")
            require(read(actual, 65536) == spec["source"].encode("utf-8")
                    and digest(actual) == spec["sha256"])
        for name in fixed.values():
            path = root / name
            entry(name, path.stat().st_size, digest(path), 0o600)
        require(set(expected) == set(self.files["python-data"]))
        for name, (size, sha, mode) in expected.items():
            row = self.files["python-data"][name]
            require(set(row) == {"path", "type", "bytes", "sha256"}
                    and row["type"] == "file" and row["bytes"] == size and row["sha256"] == sha)
            st = (root / name).lstat()
            require(stat.S_ISREG(st.st_mode) and st.st_nlink == 1 and st.st_uid == os.geteuid()
                    and stat.S_IMODE(st.st_mode) == mode)
        seen, seen_files = set(), set()
        for directory, children, names in os.walk(root, followlinks=False, onerror=lambda _: require(False)):
            path = Path(directory)
            st = path.lstat()
            require(stat.S_ISDIR(st.st_mode) and st.st_uid == os.geteuid()
                    and stat.S_IMODE(st.st_mode) == 0o700)
            for child in children:
                child_path = path / child
                child_stat = child_path.lstat()
                require(str(child_path.relative_to(root)) in directories
                        and stat.S_ISDIR(child_stat.st_mode) and child_stat.st_uid == os.geteuid()
                        and stat.S_IMODE(child_stat.st_mode) == 0o700)
            seen.add(str(path.relative_to(root)))
            seen_files.update(str((path / name).relative_to(root)) for name in names)
        require(seen == directories and seen_files == set(expected))
        roles = {"cipd": "python-runtime/cipd-client/cipd", "vpython3": "python-runtime/vpython/vpython3",
                 "python": "python-runtime/vpython/3.11/bin/python3.11"}
        for role, path in roles.items():
            require(self.tools[role] == {"root": "python-data", "path": path, "sha256": expected[path][1]})
        require(self.tools["cipd"]["sha256"] == CIPD_BINARY_SHA
                and plan["cipd_client_binary_sha256"] == CIPD_BINARY_SHA)
        self.python_plan = plan

    def verify_python_instances(self):
        for row in self.python_plan["materials"]:
            if row["seed_cache"]:
                path = self.work / "python-cache/cipd/instances" / row["instance_id"]
                st = path.lstat()
                require(stat.S_ISREG(st.st_mode) and not stat.S_ISLNK(st.st_mode)
                        and st.st_nlink == 1 and st.st_uid == os.geteuid()
                        and stat.S_IMODE(st.st_mode) == 0o600 and st.st_size == row["bytes"]
                        and digest(path) == row["sha256"])

    def prepare_python_runtime(self):
        self.verify_gitlinks()
        for name in ("python-cache", "python-cache/cipd", "python-cache/cipd/instances",
                     "python-cache/vpython", "python-results"):
            (self.work / name).mkdir(mode=0o700)
        for row in self.python_plan["materials"]:
            if row["seed_cache"]:
                seed = read(self.roots["python-data"] / "cipd-cache/instances" / row["instance_id"], row["bytes"])
                require(len(seed) == row["bytes"] and hashlib.sha256(seed).hexdigest() == row["sha256"])
                write_new(self.work / "python-cache/cipd/instances" / row["instance_id"], seed)
                del seed
        self.verify_python_instances()
        self.env.update({"CIPD_CACHE_DIR": str(self.work / "python-cache/cipd"),
                         "VPYTHON_VIRTUALENV_ROOT": str(self.work / "python-cache/vpython"),
                         "CIPD_SERVICE_URL": "https://chrome-infra-packages.appspot.com",
                         "CIPD_MAX_THREADS": "2", "CIPD_PARALLEL_DOWNLOADS": "2"})
        pins = []
        for row in self.python_plan["materials"]:
            if not row["seed_cache"]:
                continue
            number = str(len(pins)).zfill(2)
            output = self.work / "python-results" / ("resolve-" + number + ".json")
            require(not output.exists() and not output.is_symlink())
            # The genuine client owns its genuine result/tag cache. Its0666
            # JSON output stays private through the unchanged private work root.
            previous_umask = os.umask(0o077)
            try:
                self.command("python-resolve-" + number, [self.tool("cipd"), "resolve",
                    "-cache-dir=" + self.env["CIPD_CACHE_DIR"],
                    "-service-url=" + self.env["CIPD_SERVICE_URL"], "-version=" + row["version"],
                    "-json-output=" + str(output), row["package"]], 60)
            finally:
                os.umask(previous_umask)
            result = json.loads(read(output, 65536))
            st = output.lstat()
            require(stat.S_ISREG(st.st_mode) and st.st_uid == os.geteuid()
                    and st.st_nlink == 1 and stat.S_IMODE(st.st_mode) == 0o600)
            expected = {"package": row["package"], "pin": {
                "package": row["package"], "instance_id": row["instance_id"]}}
            require(result == {"result": {"": [expected]}})
            pins.append({"package": row["package"], "version": row["version"],
                         "instance_id": row["instance_id"], "archive_sha256": row["sha256"],
                         "result_sha256": digest(output)})
        require(len(pins) == 30)
        specs = []
        for spec in self.python_plan["specs"]:
            path = self.src / (".vpython3" if spec["name"] == "native" else "third_party/depot_tools/.vpython3")
            previous_umask = os.umask(0o077)
            try:
                self.command("python-install-" + spec["name"], [self.tool("vpython3"), "-vpython-root",
                    self.env["VPYTHON_VIRTUALENV_ROOT"], "-vpython-spec", str(path),
                    "-vpython-tool", "install"], 900)
            finally:
                os.umask(previous_umask)
            specs.append({"name": spec["name"], "sha256": spec["sha256"]})
        self.verify_python_instances()
        self.verify_gitlinks()
        self.python_runtime_proof = {"schema": "morpheus-genuine-linux-vpython-setup-v1",
            "python_data_binding_sha256": PYTHON_DATA_BINDING_SHA, "python_stager_sha256": PYTHON_STAGER_SHA,
            "python_plan_sha256": PYTHON_PLAN_SHA, "cipd_binary_sha256": CIPD_BINARY_SHA,
            "python_materials_sha256": self.m["python_runtime"]["materials"]["sha256"],
            "resolved_original_pins": pins, "genuine_install_specs": specs,
            "cache_scope": "same-owned-work-root-through-GN-Ninja", "standalone_pip": False,
            "native_source_readiness_inferred": False}
        json_new(self.work / "python-realization-receipt.json", self.python_runtime_proof)

    def validate_instrumented_materials(self):
        data = self.m["instrumented_data"]
        require(set(data) == {"schema", "plan", "materials", "core_manifest"}
                and data["schema"] == "morpheus-linux-instrumented-consumer-v1")
        root = self.roots["instrumented-data"]
        require(root.parent == Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
                and re.fullmatch(r"morpheus-webrtc-tools-[0-9a-f]{16}", root.name))
        for name, other in self.roots.items():
            if name != "instrumented-data":
                require(not root.is_relative_to(other) and not other.is_relative_to(root))
        fixed = {"plan": "selected-instrumented-inputs.json", "materials": "tool-materials.json",
                 "core_manifest": "selected-tool-manifest.json"}
        for name, path in fixed.items():
            require(data[name]["root"] == "instrumented-data" and data[name]["path"] == path)
        require(data["plan"]["sha256"] == INSTRUMENTED_PLAN_SHA)
        plan = bound_json(self.item(data["plan"]), INSTRUMENTED_PLAN_SHA, 2 * 1024 * 1024)
        material = bound_json(self.item(data["materials"]), data["materials"]["sha256"], 2 * 1024 * 1024)
        require(digest(self.item(data["core_manifest"])) == data["core_manifest"]["sha256"]
                == material["manifest_sha256"]
                == self.m["python_runtime"]["core_manifest"]["sha256"])
        require(plan["schema"] == "morpheus-native-linux-instrumented-inputs-v1"
                and plan["baseline"] == SOURCE_REV and plan["tree"] == SOURCE_TREE
                and plan["target"] == "Linuxx64"
                and plan["instrumented_repository_revision"]
                    == "69015643b3f68dbd438c010439c59adc52cac808"
                and plan["root_eight_security_plan_sha256"] == SECURITY_PLAN_SHA)
        tools = plan["materials"]
        require([row["name"] for row in tools] == ["msan-chained-origins-noble-lib",
                                                 "msan-no-origins-noble-lib"])
        require(material["schema"] == 4 and material["status"] == "instrumented_library_data_staged"
                and material["target"] == "Linuxx64" and material["baseline"] == SOURCE_REV
                and material["instrumented_inputs_sha256"] == INSTRUMENTED_PLAN_SHA
                and material["materials"] == tools and material["executed_tools"] == []
                and material["normalization"] == "exact_gzip_library_alias_regular_copies_no_filesystem_links"
                and material["permission_model"] == "unix_private_directories_0700_files_0600_or_0700"
                and "python_inputs_sha256" not in material and "linux_inputs_sha256" not in material)
        false = ("environment_complete", "native_sources_complete", "compiler_cipd_runtime_closure",
                 "build_ready", "distribution_ready", "hooks_executed")
        require(all(material[key] is False for key in false)
                and all(plan[key] is False for key in
                        ("environment_complete", "build_ready", "distribution_ready", "hooks_executed")))
        expected, directories, projection = {}, {"."}, {}
        def entry(path, size, sha, mode):
            name = str(rel(path))
            require(name not in expected and type(size) is int and size >= 0
                    and HEX.fullmatch(sha) and mode in (0o600, 0o700))
            expected[name] = (size, sha, mode)
            directories.update(str(p) for p in PurePosixPath(name).parents)
        def source_entry(path, size, sha, mode):
            name = str(rel(INSTRUMENTED_SOURCE_PREFIX + "/" + path))
            require(name not in projection)
            projection[name] = {"path": name, "type": "file", "bytes": size,
                                "sha256": sha, "mode": "100755" if mode == 0o700 else "100644"}
        archives = []
        for tool in tools:
            require(tool["intended_source_mount"] == "src/" + INSTRUMENTED_SOURCE_PREFIX)
            entry("archives/" + tool["name"] + ".tar.gz", tool["bytes"], tool["sha256"], 0o600)
            prefix = "instrumented-inputs/"
            for directory in tool["implicit_directories"]:
                name = str(rel(prefix + directory))
                directories.add(name)
                directories.update(str(p) for p in PurePosixPath(name).parents)
            for member in tool["members"]:
                path = member["raw_path"]
                if member["kind"] == "regular":
                    mode = 0o700 if member["mode"] & 0o111 else 0o600
                    entry(prefix + path, member["bytes"], member["sha256"], mode)
                    source_entry(path, member["bytes"], member["sha256"], mode)
                elif member["kind"] == "directory":
                    name = str(rel(prefix + path))
                    directories.add(name)
                    directories.update(str(p) for p in PurePosixPath(name).parents)
                else:
                    require(member["kind"] == "symlink")
            for alias in tool["alias_copies"]:
                entry(prefix + alias["destination"], alias["bytes"], alias["sha256"], alias["output_mode"])
                source_entry(alias["destination"], alias["bytes"], alias["sha256"], alias["output_mode"])
            archives.append({key: tool[key] for key in ("name", "sha256", "bytes", "members",
                            "alias_copies", "omitted_aliases", "omission_evidence")})
        require(material["archives"] == archives and len(projection) == 2584)
        for name in fixed.values():
            path = root / name
            entry(name, path.stat().st_size, digest(path), 0o600)
        require(set(expected) == set(self.files["instrumented-data"]))
        for name, (size, sha, _) in expected.items():
            require(self.files["instrumented-data"][name]
                    == {"path": name, "type": "file", "bytes": size, "sha256": sha})
        self.instrumented_expected = expected
        self.instrumented_directories = directories
        self.instrumented_projection = projection
        self.verify_instrumented_materials()
        self.instrumented_proof = {
            "schema": "morpheus-linux-instrumented-consumer-material-v1",
            "instrumented_data_binding_sha256": INSTRUMENTED_DATA_BINDING_SHA,
            "instrumented_stager_sha256": INSTRUMENTED_STAGER_SHA,
            "instrumented_plan_sha256": INSTRUMENTED_PLAN_SHA,
            "instrumented_materials_sha256": data["materials"]["sha256"],
            "source_overlay_sha256": self.m["source_provision"]["source_overlay"]["sha256"],
            "source_regular_files": len(projection), "filesystem_links_materialized": False,
            "native_source_readiness_inferred": False,
        }

    def verify_instrumented_materials(self):
        # Complete byte inventories are separately checked before any tool and
        # after all build/license commands. Also preserve private DATA topology.
        root = self.roots["instrumented-data"]
        for name, (size, _, mode) in self.instrumented_expected.items():
            st = (root / name).lstat()
            require(stat.S_ISREG(st.st_mode) and st.st_size == size and st.st_nlink == 1
                    and st.st_uid == os.geteuid() and stat.S_IMODE(st.st_mode) == mode)
        seen, seen_files = set(), set()
        for directory, children, names in os.walk(root, followlinks=False, onerror=lambda _: require(False)):
            path = Path(directory)
            st = path.lstat()
            require(stat.S_ISDIR(st.st_mode) and st.st_uid == os.geteuid()
                    and stat.S_IMODE(st.st_mode) == 0o700)
            for child in children:
                child_path = path / child
                child_stat = child_path.lstat()
                require(str(child_path.relative_to(root)) in self.instrumented_directories
                        and stat.S_ISDIR(child_stat.st_mode) and child_stat.st_uid == os.geteuid()
                        and stat.S_IMODE(child_stat.st_mode) == 0o700)
            seen.add(str(path.relative_to(root)))
            seen_files.update(str((path / name).relative_to(root)) for name in names)
        require(seen == self.instrumented_directories and seen_files == set(self.instrumented_expected))

    def validate_instrumented_projection(self, overlay):
        prefix = PurePosixPath(INSTRUMENTED_SOURCE_PREFIX)
        actual = {}
        for row in overlay:
            name = str(rel(row["path"]))
            path = PurePosixPath(name)
            if path.is_relative_to(prefix) or prefix.is_relative_to(path):
                require(name not in actual)
                actual[name] = row
        require(actual == self.instrumented_projection)

    def validate_linux_materials(self):
        # The helper is source-pinned as part of this recipe, not selected by
        # an untrusted manifest. It performs only DATA verification here.
        path = Path(__file__).resolve().parent / "native_webrtc/linux_materials.py"
        raw = read(path, 128 * 1024)
        require(hashlib.sha256(raw).hexdigest() == LINUX_MATERIAL_HELPER_SHA)
        helper = types.ModuleType("morpheus_linux_materials")
        helper.__file__ = str(path)
        sys.modules[helper.__name__] = helper
        exec(compile(raw, str(path), "exec"), helper.__dict__)
        data = self.m["linux_materials"]
        require(set(data) == {"schema", "plan", "expected_overlay", "core_materials", "linux_materials",
                              "dependency_decisions"}
                and data["schema"] == "morpheus-linux-projected-materials-v1"
                and data["plan"] == {"root": "linux-data", "path": "selected-linux-build-inputs.json",
                                     "sha256": helper.LINUX_PLAN}
                and data["expected_overlay"]["sha256"] == helper.LINUX_OVERLAY
                and data["core_materials"]["root"] == "core-data"
                and data["linux_materials"]["root"] == "linux-data"
                and all(data[key]["path"] == "tool-materials.json"
                        for key in ("core_materials", "linux_materials")))
        require({"core-data", "linux-data"} <= set(self.roots))
        decisions = bound_json(self.item(data["dependency_decisions"]),
                               data["dependency_decisions"]["sha256"])
        require(decisions["schema"] == "morpheus-native-linux-dependency-decisions-v1"
                and decisions["source_revision"] == SOURCE_REV
                and decisions["gn_target_os"] == "linux" and decisions["gn_target_cpu"] == "x64"
                and decisions["gn_use_siso"] is False
                and decisions["production_targets"] == [":default"]
                and decisions["native_test_target"] == "//:rtc_unittests"
                and len(decisions["declarations"]) == 13
                and all(row["decision"] in ("real-material", "proven-inactive")
                        for row in decisions["declarations"]))
        projected, linux_projected, shared_paths = {}, {}, []
        for kind in ("core", "linux"):
            root = self.roots[kind + "-data"]
            for name in ("source", "provenance", "depot", "python-data", "instrumented-data"):
                other = self.roots[name]
                require(not root.is_relative_to(other) and not other.is_relative_to(root))
            files, _, rows = helper.profile(kind, root, data[kind + "_materials"]["sha256"])
            require(set(files) == set(self.files[kind + "-data"]))
            for name, row in files.items():
                require(all(self.files[kind + "-data"][name][key] == row[key]
                            for key in ("bytes", "sha256")))
            for name, row in rows.items():
                entry = {key: row[key] for key in ("path", "type", "bytes", "sha256", "mode")}
                if name in projected:
                    require(kind == "linux" and helper.shared_llvm_nm(projected[name], entry))
                    shared_paths.append(name)
                else:
                    projected[name] = entry
                if kind == "linux":
                    linux_projected[name] = entry
        require(shared_paths == ["third_party/llvm-build/Release+Asserts/bin/llvm-nm"])
        expected_linux = bound_json(self.item(data["expected_overlay"]), helper.LINUX_OVERLAY)
        require({row["path"]: row for row in expected_linux} == linux_projected)
        self.linux_projection = projected
        self.linux_materials_proof = {"schema": "morpheus-linux-projected-materials-v1",
            "core_manifest_sha256": helper.CORE_PLAN, "linux_plan_sha256": helper.LINUX_PLAN,
            "expected_linux_overlay_sha256": helper.LINUX_OVERLAY,
            "core_materials_sha256": data["core_materials"]["sha256"],
            "linux_materials_sha256": data["linux_materials"]["sha256"],
            "dependency_decisions_sha256": data["dependency_decisions"]["sha256"],
            "source_files": len(projected), "shared_identical_inputs": shared_paths,
            "filesystem_links_materialized": False,
            "native_source_readiness_inferred": False}

    def original_git_identity(self, store, revision, tree):
        # ORIGINAL objects only. The ordinary export never gains a fake HEAD.
        require(re.fullmatch(r"[0-9a-f]{40}", revision)
                and re.fullmatch(r"[0-9a-f]{40}", tree))
        args = [self.tool("git"), "--no-pager", "-c", "core.hooksPath=/dev/null",
                "-c", "core.fsmonitor=false", "-c", "core.attributesFile=/dev/null",
                "-c", "gc.auto=0", "-c", "maintenance.auto=false",
                "--git-dir=" + str(store)]
        label = "git-original-" + str(len(self.commands))
        kind = self.command(label, args + ["cat-file", "-t", revision], 20)
        require(read(kind, 1024) == b"commit\n")
        for suffix, expected in (("commit", revision), ("tree", tree)):
            log = self.command(label + "-" + suffix,
                               args + ["rev-parse", "--verify", revision + "^{" + suffix + "}"], 20)
            require(read(log, 1024).decode("ascii").strip() == expected)

    def provision_json(self, name, producer_record=None):
        ref = self.m["source_provision"][name]
        require(ref["root"] == "provenance")
        if producer_record is not None:
            require(ref["sha256"] == producer_record["sha256"])
            require(self.item(ref).stat().st_size == producer_record["bytes"])
        return bound_json(self.item(ref), ref["sha256"])

    def validate_source_export(self):
        provision = self.roots["provenance"]
        require(set(self.m["source_provision"]) == {"producer_interface", "receipt", "materials", "repositories",
                "source_inventory", "depot_inventory", "security_receipt", "source_overlay", "source_plan", "gitlinks"})
        require(self.src == provision / "checkout/src"
                and self.roots["depot"] == provision / "checkout/outer-depot")
        contract = self.m["source_provision"]["producer_interface"]
        require(contract["sha256"] == PRODUCER_INTERFACE_SHA)
        interface = bound_json(self.item(contract), PRODUCER_INTERFACE_SHA)
        require(interface["schema"] == "morpheus-native-source-producer-interface-v1"
                and interface["source_git_administrative_metadata"] is False
                and interface["required_security_plan_sha256"] == SECURITY_PLAN_SHA)
        receipt = self.provision_json("receipt")
        materials = self.provision_json("materials")
        require(receipt["schema"] == "morpheus-native-webrtc-source-provision-v1"
                and receipt["materials_sha256"] == self.m["source_provision"]["materials"]["sha256"]
                and receipt["plan_sha256"] == materials["plan_sha256"])
        require(materials["schema"] == "morpheus-native-webrtc-git-materials-v1"
                and materials["target"] == self.target
                and materials["native_revision"] == SOURCE_REV and materials["native_tree"] == SOURCE_TREE
                and materials["outer_depot_revision"] == DEPOT_REV
                and materials["nested_depot_revision"] == NESTED_DEPOT_REV
                and materials["security_plan_sha256"] == SECURITY_PLAN_SHA
                and materials["selected_git_source_set_verified"] is True)
        for name in ("native_sources_complete", "build_ready", "distribution_ready"):
            require(receipt[name] is False and materials[name] is False)
        require(materials["compiler_cipd_runtime_closure"] is False and materials["hooks_executed"] is False)
        manifests = materials["manifests"]
        fixed_paths = {"receipt": "source-provision-receipt.json",
                       "materials": "materials/source-materials.json",
                       "repositories": "materials/repositories.json",
                       "source_inventory": "materials/post-patch-source-inventory.json",
                       "depot_inventory": "materials/outer-depot-inventory.json",
                       "security_receipt": "materials/security-source-receipt.json",
                       "source_plan": "materials/plan.json", "gitlinks": "materials/gitlinks.json"}
        for name, path in fixed_paths.items():
            require(self.m["source_provision"][name]["path"] == path)
        native = self.provision_json("source_inventory", manifests["typed_source_files"])
        self.original_header_files = package_source_headers(native, self.files["source"])
        depot = self.provision_json("depot_inventory", manifests["typed_outer_files"])
        require(all(x["type"] == "file" and x["mode"] in {"100644", "100755"}
                    for x in native + depot))
        # Producer output is patched Git source only. Every later tool/stamp
        # addition needs a separate bound inventory; no source overwrite/extra.
        overlay_ref = self.m["source_provision"]["source_overlay"]
        overlay = bound_json(self.item(overlay_ref), overlay_ref["sha256"])
        expected = {}
        for row in native + overlay:
            name = str(rel(row["path"]))
            require(name not in expected and row["type"] == "file"
                    and row["mode"] in {"100644", "100755"})
            expected[name] = row
        require(expected == self.files["source"])
        if self.target_os == "linux":
            self.validate_instrumented_projection(overlay)
            require({row["path"]: row for row in overlay
                     if not row["path"].startswith(INSTRUMENTED_SOURCE_PREFIX + "/")}
                    == self.linux_projection)
        require({x["path"]: x for x in depot} == self.files["depot"])
        repositories = self.provision_json("repositories", manifests["repositories"])
        require(isinstance(repositories, list) and 3 <= len(repositories) <= 4096)
        planned = {x["mount"]: x for x in self.m["deps_repositories"]}
        require(len(planned) == len(self.m["deps_repositories"]))
        require(len({x["mount"] for x in repositories}) == len(repositories))
        require(set(planned) == {x["mount"] for x in repositories})
        self.original_repositories = []
        stores = set()
        for row in repositories:
            require(all(row[k] == planned[row["mount"]][k] for k in ("mount", "url", "revision", "tree")))
            store_path = row["object_store"]
            require(re.fullmatch(r"objects/[0-9]{3,4}\.git", store_path) and store_path not in stores)
            stores.add(store_path)
            store = provision / store_path
            require(store == store.resolve(strict=True) and store.is_dir())
            require(any(p.startswith(store_path + "/") for p in self.files["provenance"]))
            require(isinstance(row["files"], list) and bool(row["files"]))
            self.original_repositories.append({k: row[k] for k in ("mount", "url", "revision", "tree")})
        selected = {x["mount"]: x for x in repositories}
        require(selected["src"]["revision"] == SOURCE_REV and selected["src"]["tree"] == SOURCE_TREE)
        require(selected["outer-depot"]["revision"] == DEPOT_REV and selected["outer-depot"]["tree"] == DEPOT_TREE)
        require(selected["src/third_party/depot_tools"]["revision"] == NESTED_DEPOT_REV)
        require(self.m["source_provision"]["source_plan"]["sha256"] == materials["plan_sha256"])
        source_plan = self.provision_json("source_plan")
        require(source_plan["target"] == self.target)
        marker_rows = self.provision_json("gitlinks", manifests["gitlinks"])
        self.gitlinks = gitlink_material(source_plan, marker_rows, repositories, materials, receipt)
        for name in self.files["source"]:
            outside_gitlinks("src/" + name, self.gitlinks)
        for name in self.files["depot"]:
            outside_gitlinks("outer-depot/" + name, self.gitlinks)
        self.verify_gitlinks()
        # Marker/source preflight completes before the first actual tool command.
        for row in repositories:
            self.original_git_identity(provision / row["object_store"], row["revision"], row["tree"])
        self.source_materials = materials
        self.security_receipt = self.provision_json("security_receipt", manifests["security"])
        self.validate_gn_metadata(selected)

    def verify_gitlinks(self):
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
        root = self.roots["provenance"] / "checkout"
        root_fd = os.open(root, flags)
        try:
            for name in self.gitlinks["entries"]:
                fd = os.dup(root_fd)
                try:
                    for part in PurePosixPath(name).parts:
                        next_fd = os.open(part, flags, dir_fd=fd)
                        os.close(fd)
                        fd = next_fd
                    before = os.fstat(fd)
                    require(stat.S_ISDIR(before.st_mode) and stat.S_IMODE(before.st_mode) == 0o700
                            and before.st_uid == os.geteuid())
                    require(os.listdir(fd) == [])
                    after = os.fstat(fd)
                    identity = lambda st: (st.st_dev, st.st_ino, st.st_mode, st.st_uid, st.st_mtime_ns, st.st_ctime_ns)
                    require(identity(before) == identity(after))
                finally:
                    os.close(fd)
        finally:
            os.close(root_fd)

    def validate_gn_metadata(self, repositories):
        ref = self.m["gn_metadata"]
        metadata = bound_json(self.item(ref), ref["sha256"])
        # Explicit upstream dummy metadata is NOT a native source identity.
        # Never invoke lastchange.py in a Git-free export or infer native time.
        require(metadata["schema"] == "morpheus-webrtc-gn-metadata-v1"
                and metadata["policy"] == "pinned-default-dummy"
                and metadata["is_official_build"] is False
                and metadata["use_dummy_lastchange"] is True
                and metadata["unresolved_consumers"] == [])
        require(digest(self.src / "build/util/lastchange.gni") == LASTCHANGE_GNI_SHA)
        stamps = metadata["stamps"]
        require(len(stamps) == 1 and stamps[0]["path"] == "build/util/LASTCHANGE.dummy"
                and stamps[0]["root"] == "source")
        original = {x["path"]: x for x in repositories["src/build"]["files"]}
        stamp = stamps[0]
        require(stamp["sha256"] == original["util/LASTCHANGE.dummy"]["sha256"]
                and stamp["bytes"] == original["util/LASTCHANGE.dummy"]["bytes"]
                and digest(self.item(stamp)) == stamp["sha256"])
        require(metadata["source_revision_is_dummy"] is False
                and metadata["source_revision"] == SOURCE_REV
                and metadata["production_targets"] == self.production_targets()
                and metadata["test_target"] == "//:rtc_unittests"
                and metadata["caller_source_sha256"] == "65cce5c8a3a138da9b7f2883b1988b60fb59ae1ad438de1c5ece0d21cdd15cd7")
        require(digest(self.src / "build/util/BUILD.gn") == metadata["caller_source_sha256"])
        self.gn_metadata = {k: metadata[k] for k in ("schema", "policy", "is_official_build",
                            "use_dummy_lastchange", "unresolved_consumers", "stamps",
                            "source_revision_is_dummy", "source_revision", "production_targets",
                            "test_target", "caller_source_sha256")}

    def production_targets(self):
        return [":default"] + (list(MAC_TARGETS) if self.target_os == "mac" else [])

    def validate_patches(self):
        required = REFERENCE_PATCHES | (LINUX_PATCHES if self.target_os == "linux" else {})
        require(set(self.m["reference_patches"]) == set(required))
        for name, sha in required.items():
            entry = self.m["reference_patches"][name]
            require(digest(self.item(entry)) == sha and entry["postimages"])
            for image in entry["postimages"]:
                require(digest(self.item(image)) == image["sha256"])
        require(self.m["native_security"]["sha256"] == SECURITY_PLAN_SHA)
        security = bound_json(self.item(self.m["native_security"]), SECURITY_PLAN_SHA)
        require(security["schema"] == "morpheus-native-security-source-plan-v1"
                and security["baseline_commit"] == SOURCE_REV
                and security["baseline_tree"] == SOURCE_TREE)
        patches = security["patches"]
        require([(p["name"], p["sha256"]) for p in patches] == list(BACKPORTS.items())
                and [p["order"] for p in patches] == list(range(1, 9)))
        require(list(self.m["backports"]) == list(BACKPORTS))
        self.backports = {}
        for patch in patches:
            name, sha = patch["name"], patch["sha256"]
            path = self.item(self.m["backports"][name])
            require(digest(path) == sha and path.stat().st_size == patch["bytes"])
            self.backports[name] = sha
        self.postimages = {str(rel(p["path"])): p["sha256"] for p in security["final_security_paths"]}
        require(len(security["final_security_paths"]) == 6 and self.postimages == POSTIMAGES)
        for image in security["final_security_paths"]:
            path = self.src / image["path"]
            require(digest(path) == image["sha256"] and path.stat().st_size == image["bytes"])
        produced = self.security_receipt
        require(produced["source_plan_sha256"] == SECURITY_PLAN_SHA
                and produced["baseline_commit"] == SOURCE_REV and produced["baseline_tree"] == SOURCE_TREE
                and produced["security_only_tree"] == security["security_only_tree"])
        require(produced["patches"] == [{k: p[k] for k in ("name", "order", "bytes", "sha256")} for p in patches])
        require(produced["final_security_paths"] == [{k: p[k] for k in ("path", "bytes", "sha256", "mode")} for p in security["final_security_paths"]])
        self.tests = tuple(security["gtest_filters"])
        require(self.tests == TESTS and len(self.tests) == 12
                and self.m["native_test_names"] == list(self.tests))
        resolver = self.m["license_resolver"]
        require(resolver["depot_revision"] == NESTED_DEPOT_REV)
        require(resolver["finder_sha256"] == FINDER_SHA
                and digest(self.src / "build/find_depot_tools.py") == FINDER_SHA)
        require((self.src / "third_party/depot_tools/gclient.py").is_file())
        require(digest(self.src / "third_party/depot_tools/gn.py") == resolver["gn_py_sha256"])
        require(resolver["native_gn_sha256"] == self.tools["license-gn"]["sha256"])
        require(digest(self.item(self.m["boringssl_prefix_map"])) == PREFIX_SHA)
        require(digest(self.item(self.m["source_bundle"])) == self.m["source_bundle"]["sha256"])
        require(self.m["license_material"])
        for material in self.m["license_material"]:
            require(digest(self.item(material)) == material["sha256"])

    def args(self, tests):
        common = ["is_debug=false", "is_official_build=false", "build_with_chromium=false", "use_siso=false",
                  "enable_rust=true", "enable_rust_cxx=true", "enable_chromium_prelude=true",
                  'target_os="' + self.target_os + '"', 'target_cpu="' + self.cpu + '"',
                  "rtc_enable_protobuf=false", "treat_warnings_as_errors=false",
                  "use_custom_libcxx=false", "use_clang_modules=false",
                  "rtc_include_tests=" + str(tests).lower(), "rtc_build_tools=false",
                  "rtc_build_examples=false", "rtc_libvpx_build_vp9=true", "enable_libaom=true",
                  "is_component_build=false", "enable_stripping=true",
                  "rtc_use_h264=true", "rtc_use_h265=true", "use_rtti=true"]
        if self.target_os == "linux":
            common += ["use_llvm_libatomic=false", "use_custom_libcxx_for_host=false",
                       "rtc_use_pipewire=true", "symbol_level=0", "enable_iterator_debugging=false",
                       "rtc_use_x11=true", 'ffmpeg_branding="Chrome"']
        else:
            common += ["enable_dsyms=false", 'mac_deployment_target="10.15"',
                       'mac_min_system_version="10.15"', "rtc_enable_symbol_export=true",
                       "rtc_enable_objc_symbol_export=false",
                       "rtc_include_dav1d_in_internal_decoder_factory=true",
                       "clang_use_chrome_plugins=false", "use_lld=false",
                       "rtc_include_internal_audio_device=true"]
        return " ".join(common)

    def build(self):
        if self.target_os == "linux":
            self.prepare_python_runtime()
        else:
            self.prepare_mac_runtime()
        prod, tests = self.work / "prod", self.work / "tests"
        for name, directory, testing in (("production", prod, False), ("tests", tests, True)):
            self.command("gn-" + name, [self.tool("gn"), "gen", str(directory),
                         "--root=" + str(self.src), "--fail-on-unused-args",
                         "--args=" + self.args(testing)], 900)
        targets = self.production_targets()
        self.command("ninja-production", [self.tool("ninja"), "-C", str(prod),
                     "-j", str(self.jobs), "-l", str(self.jobs)] + targets, 10800)
        self.command("ninja-tests", [self.tool("ninja"), "-C", str(tests),
                     "-j", str(self.jobs), "-l", str(self.jobs), "rtc_unittests"], 10800)
        binary = tests / "rtc_unittests"
        require(binary.is_file() and not binary.is_symlink())
        list_log = self.command("native-test-list", [str(binary), "--gtest_list_tests",
                                  "--gtest_filter=" + ":".join(self.tests)], 60)
        listed, suite = [], ""
        for line in read(list_log, 8 * 1024 * 1024).decode("utf-8").splitlines():
            if re.fullmatch(r"[A-Za-z0-9_]+\.", line):
                suite = line[:-1]
            elif re.fullmatch(r"  [A-Za-z0-9_]+", line) and suite:
                listed.append(suite + "." + line.strip())
        require(Counter(listed) == Counter(self.tests))
        xml = self.work / "native-tests.xml"
        self.command("native-test-run", [str(binary), "--gtest_filter=" + ":".join(self.tests),
                     "--gtest_output=xml:" + str(xml)], 600)
        data = read(xml, 4 * 1024 * 1024)
        require(b"<!DOCTYPE" not in data and b"<!ENTITY" not in data)
        result = ET.fromstring(data)
        cases = list(result.iter("testcase"))
        require(Counter(c.get("classname", "") + "." + c.get("name", "") for c in cases)
                == Counter(self.tests))
        require(all(c.get("status") == "run" and c.get("result") == "completed"
                    and not list(c) for c in cases))
        require(all(int(result.get(k, "-1")) == 0 for k in ("failures", "disabled", "errors")))
        require(int(result.get("tests", "-1")) == len(self.tests))
        self.verify_gitlinks()
        if self.target_os == "linux":
            self.verify_python_instances()
            self.verify_instrumented_materials()
        self.package_output(prod, targets)

    def copy(self, src, dst):
        if self.target_os == "mac":
            require(not src.resolve(strict=True).is_relative_to(self.roots["sdk"]))
        require(src.is_file() and not src.is_symlink() and not dst.exists())
        dst.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(dst, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
        with os.fdopen(fd, "wb") as output, open(src, "rb") as original:
            while True:
                chunk = original.read(1024 * 1024)
                if not chunk:
                    break
                output.write(chunk)
        require(digest(src) == digest(dst))

    def package_output(self, prod, targets):
        package = self.work / self.package
        package.mkdir(mode=0o700)
        (package / "lib").mkdir()
        objects = []
        for directory, dirs, files in os.walk(prod / "obj", followlinks=False):
            require(not any((Path(directory) / d).is_symlink() for d in dirs))
            for name in files:
                path = Path(directory) / name
                if path.suffix != ".o":
                    continue
                relative = path.relative_to(prod).as_posix()
                if "/third_party/nasm/" in relative:
                    continue
                require(not path.is_symlink() and re.fullmatch(r"[A-Za-z0-9_./+-]+", relative))
                require(not any(x in relative.lower() for x in ("googletest", "gtest", "gmock", "test_main", "_unittest")))
                objects.append(relative)
        objects.sort()
        require(0 < len(objects) <= 100_000)
        rsp = self.work / "objects.rsp"
        write_new(rsp, ("\n".join(objects) + "\n").encode("ascii"))
        library = package / "lib/libwebrtc.a"
        fmt = "darwin" if self.target_os == "mac" else "gnu"
        self.command("archive-create", [self.tool("llvm-ar"), "--format=" + fmt,
                     "qcD", str(library), "@" + str(rsp)], 900, cwd=prod)
        self.command("archive-index", [self.tool("llvm-ar"), "sD", str(library)], 900)
        log = self.command("archive-members", [self.tool("llvm-ar"), "t", str(library)], 60)
        require(Counter(read(log, 16 * 1024 * 1024).decode("utf-8").splitlines())
                == Counter(Path(x).name for x in objects))
        if self.target_os == "linux":
            self.command("prefix-boringssl", [self.tool("llvm-objcopy"), "--redefine-syms="
                         + str(self.item(self.m["boringssl_prefix_map"])), str(library)], 900)
        # Resolve Ninja's unique short aliases to actual GN labels for licensing.
        labels_log = self.command("gn-license-labels", [self.tool("gn"), "ls", str(prod), "//..."], 120)
        labels = [line for line in read(labels_log, 32 * 1024 * 1024).decode("utf-8").splitlines()
                  if re.fullmatch(r"//[A-Za-z0-9_./+-]*:[A-Za-z0-9_.+-]+", line)]
        license_targets = []
        for target in targets:
            if ":" in target:
                matches = [label for label in labels if label == "//" + target]
            else:
                matches = [label for label in labels if label.endswith(":" + target)]
            require(len(matches) == 1)
            license_targets.append(matches[0])
        # Generate licenses for the actual union, not merely one convenient target.
        command = [self.tool("python"), str(self.src / "tools_webrtc/libs/generate_licenses.py")]
        for target in license_targets:
            command += ["--target", target]
        command += [str(self.work / "license"), str(prod)]
        self.command("generate-licenses", command, 900)
        license_file = self.work / "license/LICENSE.md"
        require(0 < len(read(license_file, 64 * 1024 * 1024)))
        self.copy(license_file, package / "LICENSE.md")
        for name in ("LICENSE", "PATENTS", "AUTHORS"):
            self.copy(self.src / name, package / name)
        for name, src in (("args.gn", prod / "args.gn"),
                          ("webrtc.ninja", prod / "obj/webrtc.ninja"),
                          ("desktop_capture.ninja", prod / "obj/modules/desktop_capture/desktop_capture.ninja")):
            require(0 < len(read(src, 32 * 1024 * 1024)))
            self.copy(src, package / name)
        for material in self.m["license_material"]:
            self.copy(self.item(material), package / "LICENSES" / str(rel(material["output"])))
        # Preserve the original Git-source native/wrapper API header closure.
        # Compiler, Rust, sysroot and instrumented DATA projections are build
        # inputs, not public API headers; their full inventories and licensing
        # obligations remain bound separately and are never discarded.
        for name in sorted(self.original_header_files):
            if name.endswith((".h", ".inc")):
                self.copy((self.src / name).resolve(strict=True), package / "include" / name)
        # Generated headers remain relative to gen, as in GN's include roots.
        if (prod / "gen").exists():
            for directory, dirs, files in os.walk(prod / "gen", followlinks=False):
                require(not any((Path(directory) / d).is_symlink() for d in dirs))
                for name in files:
                    path = Path(directory) / name
                    if path.suffix in (".h", ".inc"):
                        dest = package / "include" / path.relative_to(prod / "gen")
                        if dest.exists():
                            require(digest(path) == digest(dest))
                        else:
                            self.copy(path, dest)
        self.copy(self.item(self.m["source_provision"]["gitlinks"]), package / "SOURCE_GITLINKS.json")
        require(digest(package / "SOURCE_GITLINKS.json") == self.m["source_provision"]["gitlinks"]["sha256"])
        self.verify_gitlinks()
        # Provisioned inputs must not have been rewritten by build hooks.
        for name, root in self.roots.items():
            inventory(root, list(self.files[name].values()))
        members = []
        for path in sorted(package.rglob("*")):
            if path.is_file():
                members.append({"path": path.relative_to(package).as_posix(),
                                "bytes": path.stat().st_size, "sha256": digest(path)})
        self.verify_gitlinks()
        if self.target_os == "linux":
            self.verify_python_instances()
            self.verify_instrumented_materials()
        if self.target_os == "mac":
            self.verify_mac_instances()
            self.mac_helper.profile(self.roots["mac-data"], self.m["mac_materials"]["plan"]["sha256"],
                                    self.target, self.mac_material, self.native_module("python_stager.py", PYTHON_STAGER_SHA))
            self.verify_mac_host()
        json_new(package / "PROVENANCE.json", {
            "target": self.target, "source_revision": SOURCE_REV, "source_tree": SOURCE_TREE,
            "sdk_reference_revision": SDK_REV, "outer_depot_revision": DEPOT_REV,
            "nested_license_depot_revision": NESTED_DEPOT_REV,
            "original_repositories": self.original_repositories,
            "source_provision_materials_sha256": self.m["source_provision"]["materials"]["sha256"],
            "source_export_has_git_metadata": False,
            "source_plan_schema": "morpheus-native-webrtc-sources-v2",
            "source_plan_sha256": self.m["source_provision"]["source_plan"]["sha256"],
            "gitlinks_material_sha256": self.m["source_provision"]["gitlinks"]["sha256"],
            "uninitialized_gitlinks": len(self.gitlinks["rows"]),
            "submodule_sources_materialized": False, "gitlinks": self.gitlinks["rows"],
            "gn_metadata_policy": self.gn_metadata,
            **({"python_runtime_setup": self.python_runtime_proof,
                "instrumented_source_material": self.instrumented_proof,
                "linux_projected_materials": self.linux_materials_proof} if self.target_os == "linux" else {}),
            **({"python_runtime_setup": self.python_runtime_proof,
                "mac_material_plan_sha256": self.m["mac_materials"]["plan"]["sha256"],
                "host_sdk_binding_sha256": self.m["mac_host"]["sha256"], "apple_sdk_redistributed": False}
               if self.target_os == "mac" else {}),
            "admission_sha256": self.admission_sha, "recipe_sha256": digest(Path(__file__).resolve()),
            "production_gn_args": self.args(False), "test_gn_args": self.args(True),
            "production_targets": targets, "license_targets": license_targets,
            "native_backport_patches": self.backports, "native_backport_postimages": self.postimages,
            "native_security_binding_sha256": self.m["native_security"]["sha256"],
            "sdk_reference_patches": REFERENCE_PATCHES | (LINUX_PATCHES if self.target_os == "linux" else {}),
            "tools_sha256": {name: row["sha256"] for name, row in self.tools.items()},
            "input_inventories_sha256": {name: row["inventory_sha256"] for name, row in self.m["roots"].items()},
            "source_bundle_sha256": self.m["source_bundle"]["sha256"],
            "native_test_names": list(self.tests),
            "native_test_xml_sha256": digest(self.work / "native-tests.xml"),
            "objects": [{"path": x, "sha256": digest(prod / x)} for x in objects],
            "members": members,
            "limits": "New baseline only; not old binary source proof, physical media, installed app or ABI/runtime acceptance.",
        })
        json_new(self.work / "private-command-receipts.json", self.commands)
        final = self.work / ("webrtc-" + self.package + ".zip")
        temporary = self.work / ".package.zip.part"
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            with os.fdopen(fd, "wb") as output:
                with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_STORED) as archive:
                    for path in sorted(package.rglob("*")):
                        if not path.is_file():
                            continue
                        info = zipfile.ZipInfo(self.package + "/" + path.relative_to(package).as_posix(), (1980, 1, 1, 0, 0, 0))
                        info.external_attr = 0o100644 << 16
                        with archive.open(info, "w", force_zip64=True) as dest, open(path, "rb") as src:
                            while True:
                                chunk = src.read(1024 * 1024)
                                if not chunk:
                                    break
                                dest.write(chunk)
                output.flush()
                os.fsync(output.fileno())
            self.verify_gitlinks()
            os.link(temporary, final)
        finally:
            temporary.unlink(missing_ok=True)
        self.verify_gitlinks()
        json_new(self.work / "result.json", {"status": "built-and-native-tests-passed",
                 "target": self.target, "archive": final.name, "archive_sha256": digest(final),
                 "admission_sha256": self.admission_sha,
                 "publication_admission": False, "logs_independently_scanned": False})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--runner-root", required=True)
    parser.add_argument("--admission", required=True)
    parser.add_argument("--admission-sha256", required=True)
    parser.add_argument("--jobs", type=int, choices=range(1, 5), default=2)
    args = parser.parse_args()
    try:
        build = Build(args)
        build.admission_sha = args.admission_sha256
        build.build()
    except BaseException:
        # Private command logs stay private. No raw exception, path, environment,
        # stdout/stderr, key bytes or SDK payload is printed on failure.
        sys.stderr.write("MORPHEUS_NATIVE_WEBRTC_BUILD failed\n")
        return 1
    sys.stdout.write("MORPHEUS_NATIVE_WEBRTC_BUILD built_and_native_tests_passed\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
