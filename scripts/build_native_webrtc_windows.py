#!/usr/bin/env python3
"""Build prospective pinned WebRTC on an admitted fresh Windows x64 runner.

Provisioning, recursive DEPS/tool/source review, network isolation and hosted
execution admission belong to the calling workflow. Every command is retained
in the shared reviewed Windows Job owner with live output bounds.
This helper never provisions, fetches, applies patches or selects a fallback.
The caller must supply an independently reviewed SHA256 of the complete input
manifest. A receipt produced here records a build, not legal release approval.
"""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import sys
import types
import zipfile
import xml.etree.ElementTree as ET

NATIVE_REVISION = "aaeeee8077eb0a4cad1c9494e9c6433c751ef663"
NATIVE_TREE = "fecdf55fdd1e9cafc5531abab7ff14e35a1159f5"
DEPOT_REVISION = "b52fe87d04af61df3dd07748ff2c2c0b649dca09"
DEPOT_TREE = "5c857570433c574a02614b3aab9be1f34493095d"
NESTED_DEPOT_REVISION = "8efa575d754b8703d99b0f827528e45aeaa167aa"
FIND_DEPOT_TOOLS_SHA256 = "f42b1d5f2c8e2f26d50a679a5e5dc7595dd4c1b64af23f6684c22207f5c98b73"
GN_PIN = "git_revision:4619125bd337d259c0dc9f958d0102adc99d2543"
NINJA_PIN = "version:3@1.12.1.chromium.4"
SCHEMA = "morpheus-native-webrtc-windows-inputs-v4"
OWNER_HELPER_SHA = "e99c8aadd7db5e53229ad836b2bba115bfc0a11d9cda6b00d9166759ba6a0211"
OWNER = None
COMMAND_OUTPUT_BYTES = 0
MAX_TOTAL_COMMAND_OUTPUT = 512 * 1024 * 1024
MAX_COMMAND_OUTPUT = 64 * 1024 * 1024
MAX_MANIFEST = 128 * 1024 * 1024
MAX_FILES = 1_000_000
MAX_FILE = 64 * 1024 * 1024 * 1024
MAX_TOTAL = 256 * 1024 * 1024 * 1024
BUILD_NAME = "out-morpheus-win-x64-release"
TEST_BUILD_NAME = "out-morpheus-win-x64-native-security-tests"
PACKAGE_NAME = "win-x64-release"
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
SECURITY_PLAN_SHA = "a9f2220dfb0e527690ace518911e3e92c12124ab8c37e6cb391a47c89f19c080"
PRODUCER_INTERFACE_SHA = "ebfdd22dd8eaa6ee893adbdc9fe904f5e859d33b7353b067a66c32df526508bf"
SOURCE_NORMALIZER_SHA = "e99c8aadd7db5e53229ad836b2bba115bfc0a11d9cda6b00d9166759ba6a0211"
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
UPSTREAM_PATCHES = {
    "add_licenses.patch": "3744d07bce56a3cc281f70196ad52513b6395d353a73d2de97f2de616bc27625",
    "add_deps.patch": "da222e44f306137ed96b626879948325dc21857dcf054482b1e3df83e59fe5f4",
    "windows_silence_warnings.patch": "50dc2fa0db35abe9a4dd2d6849f70d12aa45c82af3371fcf8d6771fed9046887",
    "ssl_verify_callback_with_native_handle.patch": "55042fe99107439e88e56cddec4f5caf8c8a92e72f2e035ac8e37d52aaecb20c",
    "external_audio_source.patch": "83bfe8101270cc60be25991ec07bbabf51c071f6529b50895e4b1ef07314b81f",
}
# Original71e3 Windows release ABI/media args. Direct Ninja is selected explicitly
# because the prospective aaeeee .gn defaults use_siso to true.
GN_ARGS = (
    'is_debug=false is_clang=true target_cpu="x64" '
    'use_custom_libcxx=false rtc_libvpx_build_vp9=true enable_libaom=true '
    'rtc_include_tests=false rtc_build_examples=false rtc_build_tools=false '
    'is_component_build=false rtc_enable_protobuf=false rtc_use_h264=true '
    'ffmpeg_branding="Chrome" symbol_level=0 enable_iterator_debugging=false '
    'use_siso=false'
)
TEST_GN_ARGS = GN_ARGS.replace("rtc_include_tests=false", "rtc_include_tests=true") + " build_with_chromium=false"
GATES = {"source", "deps-hooks", "toolchain-rust", "patches", "native-test-source",
         "license-material", "license-gn-resolution", "wrapper-abi", "symbol-prefix",
         "generated-metadata", "python-vpython-cipd", "host-msvc-sdk"}
ENV_KEYS = {
    "VSINSTALLDIR", "VCINSTALLDIR", "VCToolsInstallDir", "VCToolsVersion",
    "WindowsSdkDir", "WindowsSDKVersion", "UniversalCRTSdkDir", "UCRTVersion",
    "INCLUDE", "LIB", "LIBPATH", "GYP_MSVS_OVERRIDE_PATH", "vs2022_install",
}
MATERIAL_ROLES = {
    "preferred-source", "source-inventory", "dependency-lock",
    "toolchain-inventory", "native-security-patches", "license-inputs",
    "offline-provisioning",
}


class RecipeError(Exception):
    pass


def require(condition: bool) -> None:
    if not condition:
        raise RecipeError()


def digest(path: Path, limit: int = MAX_FILE) -> tuple[int, str]:
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode))
    require(not (info.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT))
    require(info.st_nlink == 1 and 0 <= info.st_size <= limit)
    count = 0
    value = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            count += len(block)
            require(count <= limit)
            value.update(block)
        final = os.fstat(stream.fileno())
    named = path.lstat()
    identity = lambda item: (item.st_dev, item.st_ino, item.st_size, item.st_mtime_ns)
    require(identity(info) == identity(final) == identity(named))
    return count, value.hexdigest()


def plain_path(value: str) -> Path:
    require(isinstance(value, str) and 0 < len(value) <= 4096)
    require(not any(char in value for char in "\r\n\x00&|<>^"))
    path = Path(value)
    require(path.is_absolute() and not str(path).startswith("\\\\"))
    current = Path(path.anchor)
    for part in path.parts[1:]:
        require(part not in (".", "..") and ":" not in part and not part.endswith((".", " ")))
        require(not re.fullmatch(r"(?i)(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?", part))
        current /= part
        info = current.lstat()
        require(not (info.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT))
    return path


def below(path: Path, root: Path) -> bool:
    return path != root and path.is_relative_to(root)


def relative(value: str) -> str:
    require(isinstance(value, str) and 0 < len(value) <= 2048)
    require("\\" not in value and ":" not in value)
    path = PurePosixPath(value)
    require(not path.is_absolute() and path.as_posix() == value)
    for part in path.parts:
        require(part not in (".", "..") and part.casefold() != ".git" and not part.endswith((" ", ".")))
        require(not any(char in part for char in '<>"|?*\x00\r\n'))
        require(not re.fullmatch(r"(?i)(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?", part))
    return value


def file_record(record: dict) -> tuple[Path, dict]:
    require(set(record) == {"path", "bytes", "sha256"})
    require(type(record["bytes"]) is int and 0 <= record["bytes"] <= MAX_FILE)
    require(isinstance(record["sha256"], str) and SHA256.fullmatch(record["sha256"]))
    path = plain_path(record["path"])
    size, value = digest(path)
    require((size, value) == (record["bytes"], record["sha256"]))
    return path, {"bytes": size, "sha256": value}


def inventory(root: Path, rows: list, skip_build: bool = False, build_prefix: str = "") -> dict:
    build_paths = {build_prefix + name for name in (BUILD_NAME, TEST_BUILD_NAME)} if skip_build else set()
    require(type(rows) is list and 0 < len(rows) <= MAX_FILES)
    expected = {}
    folded = set()
    for row in rows:
        require(set(row) == {"path", "bytes", "sha256"})
        name = relative(row["path"])
        require(name.casefold() not in folded)
        require(not any(name == build or name.startswith(build + "/") for build in build_paths))
        require(type(row["bytes"]) is int and 0 <= row["bytes"] <= MAX_FILE)
        require(isinstance(row["sha256"], str) and SHA256.fullmatch(row["sha256"]))
        folded.add(name.casefold())
        expected[name] = {"bytes": row["bytes"], "sha256": row["sha256"]}
    observed = {}
    pending = [root]
    entries = 0
    total = 0
    while pending:
        directory = pending.pop()
        with os.scandir(directory) as listing:
            for entry in listing:
                entries += 1
                require(entries <= MAX_FILES)
                info = entry.stat(follow_symlinks=False)
                require(not (info.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT))
                require(entry.name.casefold() != ".git")
                path = Path(entry.path)
                name = path.relative_to(root).as_posix()
                if name in build_paths:
                    require(stat.S_ISDIR(info.st_mode))
                    continue
                relative(name)
                if stat.S_ISDIR(info.st_mode):
                    pending.append(path)
                else:
                    require(name in expected)
                    size, value = digest(path)
                    total += size
                    require(total <= MAX_TOTAL)
                    observed[name] = {"bytes": size, "sha256": value}
    require(observed == expected)
    return expected


def unique_json(pairs: list) -> dict:
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result


def read_inputs(path: Path, expected: str) -> dict:
    require(SHA256.fullmatch(expected))
    require(digest(path, MAX_MANIFEST)[1] == expected)
    with path.open("rb") as stream:
        raw = stream.read(MAX_MANIFEST + 1)
    require(len(raw) <= MAX_MANIFEST and hashlib.sha256(raw).hexdigest() == expected)
    return json.loads(raw, object_pairs_hook=unique_json)


def copy_checked(source: Path, destination: Path, expected: dict) -> dict:
    plain_path(str(source))
    require(digest(source) == (expected["bytes"], expected["sha256"]))
    destination.parent.mkdir(parents=True, exist_ok=True)
    plain_path(str(destination.parent))
    count = 0
    with source.open("rb") as origin, destination.open("xb") as target:
        while block := origin.read(1024 * 1024):
            count += len(block)
            require(count <= expected["bytes"])
            target.write(block)
    require(count == expected["bytes"])
    size, value = digest(destination)
    require((size, value) == (expected["bytes"], expected["sha256"]))
    return {"bytes": size, "sha256": value}


def save_json(path: Path, data: dict) -> None:
    raw = (json.dumps(data, sort_keys=True, indent=2) + "\n").encode("utf-8")
    require(len(raw) <= MAX_MANIFEST)
    with path.open("xb") as stream:
        stream.write(raw)


def load_process_owner():
    # Original shared Windows Job implementation; no manifest selects code.
    path = Path(__file__).resolve().parent / "native_webrtc/git_source_stager.py"
    raw = bounded_bytes(plain_path(str(path)), 256 * 1024)
    require(hashlib.sha256(raw).hexdigest() == OWNER_HELPER_SHA)
    value = types.ModuleType("morpheus_windows_owned_command")
    value.__file__ = str(path)
    sys.modules[value.__name__] = value
    exec(compile(raw, str(path), "exec"), value.__dict__)
    return value


def run(command: list[str], source: Path, env: dict, log: Path) -> None:
    # Fixed reviewed executables only. This is not a hostile arbitrary-code sandbox.
    # The original no-breakaway Job is assigned at process creation, stdout/stderr
    # are drained through live individual+combined limits, and original descendants
    # are stopped and joined before any next command or receipt.
    global OWNER, COMMAND_OUTPUT_BYTES
    if OWNER is None:
        OWNER = load_process_owner()
    require(OWNER.UNRETIRED_COMMAND is None)
    require(type(command) is list and 0 < len(command) <= 64
            and all(type(arg) is str and len(arg) <= 32767 and "\0" not in arg for arg in command))
    # Git identity/metadata and license/GN operations are short; only the actual
    # Ninja builds receive the bounded longer budget. No shell or .cmd execution.
    timeout = 5400 if "-C" in command else 900
    remaining = MAX_TOTAL_COMMAND_OUTPUT - COMMAND_OUTPUT_BYTES
    require(remaining >= OWNER.MAX_METADATA)
    result = OWNER.run_git_owned(command, source, env, timeout, log,
        log.with_name(log.name + ".stderr"), min(MAX_COMMAND_OUTPUT, remaining), None)
    COMMAND_OUTPUT_BYTES += result["stdout_bytes"] + result["stderr_bytes"]
    require(COMMAND_OUTPUT_BYTES <= MAX_TOTAL_COMMAND_OUTPUT)
    require(OWNER.UNRETIRED_COMMAND is None)


def bounded_bytes(path: Path, limit: int) -> bytes:
    size, value = digest(path, limit)
    with path.open("rb") as stream:
        raw = stream.read(limit + 1)
    require(len(raw) == size <= limit and hashlib.sha256(raw).hexdigest() == value)
    return raw


def typed_files(rows: list) -> list:
    require(type(rows) is list and 0 < len(rows) <= MAX_FILES)
    result = []
    folded = set()
    for row in rows:
        require(set(row) == {"path", "type", "mode", "bytes", "sha256"})
        name = relative(row["path"])
        require(row["type"] == "file" and row["mode"] in ("100644", "100755"))
        require(name.casefold() not in folded)
        folded.add(name.casefold())
        result.append({k: row[k] for k in ("path", "bytes", "sha256")})
    return result


def package_source_headers(original_files: dict, complete_source: dict) -> list:
    # Original patched Git source owns the public native/wrapper header closure.
    # Projected compiler/Rust headers remain separately inventoried build DATA.
    require(all(complete_source.get(name) == fact for name, fact in original_files.items()))
    return sorted(name for name in original_files if PurePosixPath(name).suffix in (".h", ".inc"))


def material_json(record: dict, producer: dict | None = None):
    path = plain_path(record["path"])
    require(below(path, plain_path(os.environ["RUNNER_TEMP"])))
    path, fact = file_record(record)
    if producer is not None:
        require(fact == producer)
    return read_inputs(path, fact["sha256"])


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
    require(plan["schema"] == "morpheus-native-webrtc-sources-v3")
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


def source_normalizer():
    import types
    path = Path(__file__).resolve().parent / "native_webrtc/git_source_stager.py"
    size, value = digest(path)
    require(size <= 256 * 1024 and value == SOURCE_NORMALIZER_SHA)
    raw = path.read_bytes()
    require(len(raw) == size and hashlib.sha256(raw).hexdigest() == SOURCE_NORMALIZER_SHA)
    module = types.ModuleType("morpheus_source_normalizer")
    module.__file__ = str(path)
    exec(compile(raw, str(path), "exec"), module.__dict__)
    return module


def verify_gitlinks(export):
    export["alias_helper"].verify_aliases(export["root"] / "checkout", export["aliases"], export["root"])
    # Read-only OS queries. Hold every ancestor without write/delete sharing
    # during a marker check; never change permissions or infer them from mode bits.
    if not export["gitlinks"]["entries"]:
        return
    import ctypes as c
    from ctypes import wintypes as w
    k, a = c.WinDLL("kernel32", use_last_error=True), c.WinDLL("advapi32", use_last_error=True)
    pointer = c.POINTER(w.LPVOID)
    k.GetCurrentProcess.argtypes, k.GetCurrentProcess.restype = (), w.HANDLE
    k.CreateFileW.argtypes = (w.LPCWSTR, w.DWORD, w.DWORD, w.LPVOID, w.DWORD, w.DWORD, w.HANDLE)
    k.CreateFileW.restype = w.HANDLE
    k.CloseHandle.argtypes, k.CloseHandle.restype = (w.HANDLE,), w.BOOL
    k.LocalFree.argtypes, k.LocalFree.restype = (w.HLOCAL,), w.HLOCAL
    k.GetFileInformationByHandleEx.argtypes = (w.HANDLE, c.c_int, w.LPVOID, w.DWORD)
    k.GetFileInformationByHandleEx.restype = w.BOOL
    a.OpenProcessToken.argtypes, a.OpenProcessToken.restype = (w.HANDLE, w.DWORD, c.POINTER(w.HANDLE)), w.BOOL
    a.GetTokenInformation.argtypes = (w.HANDLE, c.c_int, w.LPVOID, w.DWORD, c.POINTER(w.DWORD))
    a.GetTokenInformation.restype = w.BOOL
    a.GetSecurityInfo.argtypes = (w.HANDLE, c.c_int, w.DWORD, pointer, pointer, pointer, pointer, pointer)
    a.GetSecurityInfo.restype = w.DWORD
    a.GetSecurityDescriptorControl.argtypes = (w.LPVOID, c.POINTER(w.WORD), c.POINTER(w.DWORD))
    a.GetSecurityDescriptorControl.restype = w.BOOL
    a.GetSecurityDescriptorLength.argtypes, a.GetSecurityDescriptorLength.restype = (w.LPVOID,), w.DWORD
    a.IsValidSecurityDescriptor.argtypes, a.IsValidSecurityDescriptor.restype = (w.LPVOID,), w.BOOL
    a.IsValidAcl.argtypes, a.IsValidAcl.restype = (w.LPVOID,), w.BOOL
    a.GetAclInformation.argtypes, a.GetAclInformation.restype = (w.LPVOID, w.LPVOID, w.DWORD, c.c_int), w.BOOL
    a.GetAce.argtypes, a.GetAce.restype = (w.LPVOID, w.DWORD, pointer), w.BOOL
    a.IsValidSid.argtypes, a.IsValidSid.restype = (w.LPVOID,), w.BOOL
    a.GetLengthSid.argtypes, a.GetLengthSid.restype = (w.LPVOID,), w.DWORD
    a.EqualSid.argtypes, a.EqualSid.restype = (w.LPVOID, w.LPVOID), w.BOOL
    a.ConvertStringSidToSidW.argtypes, a.ConvertStringSidToSidW.restype = (w.LPCWSTR, pointer), w.BOOL

    class SidAndAttributes(c.Structure):
        _fields_ = [("Sid", w.LPVOID), ("Attributes", w.DWORD)]

    class TokenUser(c.Structure):
        _fields_ = [("User", SidAndAttributes)]

    class AttributeTag(c.Structure):
        _fields_ = [("FileAttributes", w.DWORD), ("ReparseTag", w.DWORD)]

    class AclSize(c.Structure):
        _fields_ = [("AceCount", w.DWORD), ("AclBytesInUse", w.DWORD), ("AclBytesFree", w.DWORD)]

    class AceHeader(c.Structure):
        _fields_ = [("AceType", w.BYTE), ("AceFlags", w.BYTE), ("AceSize", w.WORD)]

    token, allocated = w.HANDLE(), []
    closed = True
    try:
        require(a.OpenProcessToken(k.GetCurrentProcess(), 0x0008, c.byref(token)))  # TOKEN_QUERY only.
        size = w.DWORD()
        require(not a.GetTokenInformation(token, 1, None, 0, c.byref(size))
                and c.get_last_error() == 122 and c.sizeof(TokenUser) <= size.value <= 65536)
        token_data = c.create_string_buffer(size.value)
        require(a.GetTokenInformation(token, 1, token_data, len(token_data), c.byref(size)))
        user = c.cast(token_data, c.POINTER(TokenUser)).contents.User.Sid
        require(user and a.IsValidSid(user))
        known = []
        # SYSTEM, local Administrators and OWNER RIGHTS. The descriptor owner
        # must equal this process user; OWNER RIGHTS never names a different owner.
        for text in ("S-1-5-18", "S-1-5-32-544", "S-1-3-4"):
            sid = w.LPVOID()
            require(a.ConvertStringSidToSidW(text, c.byref(sid)))
            allocated.append(sid)
            require(a.IsValidSid(sid))
            known.append(sid)
        for name in export["gitlinks"]["entries"]:
            path = plain_path(str(export["root"] / "checkout" / name))
            components = [*reversed(path.parents), path]
            require(len(components) <= 128)
            handles, descriptor = [], w.LPVOID()
            local_closed = True
            try:
                for component in components:
                    # READ_ATTRIBUTES on ancestors; marker additionally requests
                    # READ_CONTROL|FILE_LIST_DIRECTORY. Share READ only, never WRITE/DELETE.
                    access = 0x20081 if component == path else 0x80
                    handle = k.CreateFileW(str(component), access, 1, None, 3, 0x02200000, None)
                    require(handle not in (None, c.c_void_p(-1).value))
                    handles.append(handle)
                    tag = AttributeTag()
                    require(k.GetFileInformationByHandleEx(handle, 9, c.byref(tag), c.sizeof(tag)))
                    require(tag.FileAttributes & 0x10 and not tag.FileAttributes & 0x400)
                owner, dacl = w.LPVOID(), w.LPVOID()
                require(a.GetSecurityInfo(handles[-1], 1, 0x0005, c.byref(owner), None,
                                          c.byref(dacl), None, c.byref(descriptor)) == 0)
                require(owner and dacl and descriptor and a.IsValidSecurityDescriptor(descriptor)
                        and 20 <= a.GetSecurityDescriptorLength(descriptor) <= 65536
                        and a.IsValidSid(owner) and a.EqualSid(owner, user) and a.IsValidAcl(dacl))
                control, revision = w.WORD(), w.DWORD()
                require(a.GetSecurityDescriptorControl(descriptor, c.byref(control), c.byref(revision)))
                require(revision.value == 1 and control.value & 0x0004 and control.value & 0x1000)
                info = AclSize()
                require(a.GetAclInformation(dacl, c.byref(info), c.sizeof(info), 2))
                require(0 < info.AceCount <= 64 and 8 <= info.AclBytesInUse <= 65535)
                owner_can_list = False
                for index in range(info.AceCount):
                    ace = w.LPVOID()
                    require(a.GetAce(dacl, index, c.byref(ace)) and ace)
                    header = c.cast(ace, c.POINTER(AceHeader)).contents
                    require(header.AceType == 0 and header.AceFlags & ~0x1F == 0
                            and 16 <= header.AceSize <= info.AclBytesInUse)
                    mask = c.cast(ace.value + 4, c.POINTER(w.DWORD)).contents.value
                    sid = w.LPVOID(ace.value + 8)
                    require(a.IsValidSid(sid) and 8 <= a.GetLengthSid(sid) <= 68
                            and 8 + a.GetLengthSid(sid) <= header.AceSize)
                    require(mask != 0 and mask & ~0xF01F01FF == 0)
                    own = bool(a.EqualSid(sid, owner) or a.EqualSid(sid, known[2]))
                    require(own or any(a.EqualSid(sid, item) for item in known[:2]))
                    if own and not header.AceFlags & 0x08 and mask & (0x80000000 | 0x10000000 | 0x0001):
                        owner_can_list = True
                require(owner_can_list)
                with os.scandir(path) as listing:
                    require(next(listing, None) is None)
                tag = AttributeTag()
                require(k.GetFileInformationByHandleEx(handles[-1], 9, c.byref(tag), c.sizeof(tag)))
                require(tag.FileAttributes & 0x10 and not tag.FileAttributes & 0x400)
            finally:
                if descriptor:
                    local_closed = not bool(k.LocalFree(descriptor)) and local_closed
                for handle in reversed(handles):
                    local_closed = bool(k.CloseHandle(handle)) and local_closed
                require(local_closed)
    finally:
        for sid in reversed(allocated):
            closed = not bool(k.LocalFree(sid)) and closed
        if token:
            closed = bool(k.CloseHandle(token)) and closed
        require(closed)


def source_export(inputs: dict, source: Path, depot: Path, runner: Path,
                  parent: Path, source_files: dict, depot_files: dict) -> dict:
    specification = inputs["provenance"]
    require(set(specification) == {"root", "files"})
    provenance = plain_path(specification["root"])
    require(below(provenance, runner) and not parent.is_relative_to(provenance)
            and not provenance.is_relative_to(parent))
    require(source == provenance / "checkout/src" and depot == provenance / "checkout/outer-depot")
    provenance_files = inventory(provenance, specification["files"])
    refs = inputs["source_provision"]
    require(set(refs) == {"producer_interface", "receipt", "materials", "repositories",
                          "source_inventory", "depot_inventory", "security_receipt", "source_overlay", "source_plan", "gitlinks", "aliases"})
    require(refs["producer_interface"]["sha256"] == PRODUCER_INTERFACE_SHA)
    interface = material_json(refs["producer_interface"])
    require(interface["schema"] == "morpheus-native-source-producer-interface-v2"
            and interface["source_git_administrative_metadata"] is False
            and interface["required_security_plan_sha256"] == SECURITY_PLAN_SHA)
    paths = {"receipt": "source-provision-receipt.json",
             "materials": "materials/source-materials.json",
             "repositories": "materials/repositories.json",
             "source_inventory": "materials/post-patch-source-inventory.json",
             "depot_inventory": "materials/outer-depot-inventory.json",
             "security_receipt": "materials/security-source-receipt.json",
             "aliases": "materials/source-aliases.json"}
    for name, relative_path in paths.items():
        require(plain_path(refs[name]["path"]) == provenance / relative_path)
    receipt = material_json(refs["receipt"])
    materials = material_json(refs["materials"])
    require(receipt["schema"] == "morpheus-native-webrtc-source-provision-v1"
            and materials["schema"] == "morpheus-native-webrtc-git-materials-v1")
    require(receipt["materials_sha256"] == refs["materials"]["sha256"]
            and receipt["plan_sha256"] == materials["plan_sha256"])
    require(materials["target"] == "win-x64" and materials["native_revision"] == NATIVE_REVISION
            and materials["native_tree"] == NATIVE_TREE
            and materials["outer_depot_revision"] == DEPOT_REVISION
            and materials["nested_depot_revision"] == NESTED_DEPOT_REVISION
            and materials["security_plan_sha256"] == SECURITY_PLAN_SHA
            and materials["selected_git_source_set_verified"] is True)
    for key in ("native_sources_complete", "build_ready", "distribution_ready"):
        require(receipt[key] is False and materials[key] is False)
    require(materials["compiler_cipd_runtime_closure"] is False and materials["hooks_executed"] is False)
    records = materials["manifests"]
    native = typed_files(material_json(refs["source_inventory"], records["typed_source_files"]))
    outer = typed_files(material_json(refs["depot_inventory"], records["typed_outer_files"]))
    overlay_rows = material_json(refs["source_overlay"])
    require(type(overlay_rows) is list)
    overlay = typed_files(overlay_rows) if overlay_rows else []
    expected = {}
    folded = set()
    for row in native + overlay:
        require(row["path"].casefold() not in folded)
        folded.add(row["path"].casefold())
        expected[row["path"]] = {k: row[k] for k in ("bytes", "sha256")}
    require(expected == source_files)
    require({r["path"]: {k: r[k] for k in ("bytes", "sha256")} for r in outer} == depot_files)
    repositories = material_json(refs["repositories"], records["repositories"])
    require(type(repositories) is list and 3 <= len(repositories) <= 4096)
    planned = {r["mount"]: r for r in inputs["repositories"]}
    require(len(planned) == len(inputs["repositories"]) == len(repositories))
    require(set(planned) == {r["mount"] for r in repositories})
    selected = {}
    stores = set()
    for row in repositories:
        require(all(row[k] == planned[row["mount"]][k] for k in ("mount", "url", "revision", "tree")))
        require(re.fullmatch(r"[0-9a-f]{40}", row["revision"]) and re.fullmatch(r"[0-9a-f]{40}", row["tree"]))
        name = relative(row["object_store"])
        require(re.fullmatch(r"objects/[0-9]{3,4}\.git", name) and name not in stores)
        stores.add(name)
        require(name + "/config" in provenance_files)
        plain_path(str(provenance / name))
        require(type(row["files"]) is list and bool(row["files"]))
        selected[row["mount"]] = row
    require(selected["src"]["revision"] == NATIVE_REVISION and selected["src"]["tree"] == NATIVE_TREE)
    require(selected["outer-depot"]["revision"] == DEPOT_REVISION and selected["outer-depot"]["tree"] == DEPOT_TREE)
    require(selected["src/third_party/depot_tools"]["revision"] == NESTED_DEPOT_REVISION)
    require(refs["source_plan"]["sha256"] == materials["plan_sha256"])
    source_plan = material_json(refs["source_plan"])
    require(source_plan["target"] == "win-x64")
    marker_rows = material_json(refs["gitlinks"], records["gitlinks"])
    policy = gitlink_material(source_plan, marker_rows, repositories, materials, receipt)
    alias_helper = source_normalizer()
    alias_rows = material_json(refs["aliases"], records["aliases"])
    original_inventory = {prefix + row["path"]: {**row, "path": prefix + row["path"]}
        for prefix, rows in (("src/", native), ("outer-depot/", outer)) for row in rows}
    aliases = alias_helper.alias_materials(source_plan, alias_rows, repositories, materials,
        receipt, provenance / "checkout", original_inventory, provenance)
    alias_policy = alias_helper.alias_policy(source_plan["aliases"], selected,
        alias_helper.gitlink_policy(source_plan["gitlinks"], selected))
    for row in overlay:
        alias_helper.outside_aliases("src/" + row["path"], alias_policy)
    for name in source_files:
        outside_gitlinks("src/" + name, policy)
    for name in depot_files:
        outside_gitlinks("outer-depot/" + name, policy)
    export = {"alias_helper": alias_helper, "aliases": aliases, "root": provenance, "files": specification["files"], "repositories": repositories,
              "selected": selected, "materials_sha256": refs["materials"]["sha256"],
              "source_plan_sha256": refs["source_plan"]["sha256"],
              "gitlinks_sha256": refs["gitlinks"]["sha256"], "gitlinks": policy,
              "native_files": {r["path"]: {k: r[k] for k in ("bytes", "sha256")} for r in native},
              "security_receipt": material_json(refs["security_receipt"], records["security"])}
    verify_gitlinks(export)
    return export


def security_inputs(inputs: dict, source: Path, export: dict, patch_facts: dict) -> dict:
    require(inputs["native_security"]["sha256"] == SECURITY_PLAN_SHA)
    plan = material_json(inputs["native_security"])
    require(plan["schema"] == "morpheus-native-security-source-plan-v1"
            and plan["baseline_commit"] == NATIVE_REVISION and plan["baseline_tree"] == NATIVE_TREE)
    require([(p["name"], p["sha256"]) for p in plan["patches"]] == list(BACKPORTS.items())
            and [p["order"] for p in plan["patches"]] == list(range(1, 9)))
    for patch in plan["patches"]:
        require(patch_facts.get(patch["name"]) == {k: patch[k] for k in ("bytes", "sha256")})
    require(len(plan["final_security_paths"]) == 6
            and {r["path"]: r["sha256"] for r in plan["final_security_paths"]} == POSTIMAGES)
    for row in plan["final_security_paths"]:
        require(digest(plain_path(str(source / relative(row["path"])))) == (row["bytes"], row["sha256"]))
    produced = export["security_receipt"]
    require(produced["source_plan_sha256"] == SECURITY_PLAN_SHA
            and produced["baseline_commit"] == NATIVE_REVISION and produced["baseline_tree"] == NATIVE_TREE
            and produced["security_only_tree"] == plan["security_only_tree"])
    require(produced["patches"] == [{k: p[k] for k in ("name", "order", "bytes", "sha256")} for p in plan["patches"]])
    require(produced["final_security_paths"] == [{k: p[k] for k in ("path", "bytes", "sha256", "mode")} for p in plan["final_security_paths"]])
    require(tuple(plan["gtest_filters"]) == TESTS and len(TESTS) == 12)
    return plan


def gn_metadata(inputs: dict, source: Path, export: dict) -> dict:
    metadata = material_json(inputs["gn_metadata"])
    require(metadata["schema"] == "morpheus-webrtc-gn-metadata-v1"
            and metadata["policy"] == "pinned-default-dummy"
            and metadata["is_official_build"] is False
            and metadata["use_dummy_lastchange"] is True
            and metadata["unresolved_consumers"] == [])
    require(digest(source / "build/util/lastchange.gni")[1] == LASTCHANGE_GNI_SHA)
    require(digest(source / "build/config/BUILDCONFIG.gn")[1] == "1553d0e8dc3d0f80301c0de316279319c784f199dfd80289c9fb04b3a20971f1")
    require(metadata["source_revision_is_dummy"] is False
            and metadata["source_revision"] == NATIVE_REVISION
            and metadata["production_targets"] == [":default"]
            and metadata["test_target"] == "//:rtc_unittests")
    require(metadata["caller_source_sha256"] == "65cce5c8a3a138da9b7f2883b1988b60fb59ae1ad438de1c5ece0d21cdd15cd7"
            and digest(source / "build/util/BUILD.gn")[1] == metadata["caller_source_sha256"])
    require(len(metadata["stamps"]) == 1)
    stamp = metadata["stamps"][0]
    require(plain_path(stamp["path"]) == source / "build/util/LASTCHANGE.dummy")
    original = {r["path"]: r for r in export["selected"]["src/build"]["files"]}
    require({k: stamp[k] for k in ("bytes", "sha256")} == {k: original["util/LASTCHANGE.dummy"][k] for k in ("bytes", "sha256")})
    file_record(stamp)
    return {k: metadata[k] for k in ("schema", "policy", "is_official_build", "use_dummy_lastchange",
                                     "unresolved_consumers", "source_revision_is_dummy", "source_revision",
                                     "production_targets", "test_target", "caller_source_sha256")} | {
        "stamps": [{"source_relative_path": "build/util/LASTCHANGE.dummy",
                    "bytes": stamp["bytes"], "sha256": stamp["sha256"]}]}


def original_objects(export: dict, git: Path, source: Path, env: dict, logs: Path) -> None:
    # Full explicit ORIGINAL commit/tree only. Never discover export HEAD.
    for index, row in enumerate(export["repositories"]):
        store = export["root"] / row["object_store"]
        command = [str(git), "--no-pager", "-c", "core.hooksPath=/dev/null",
                   "-c", "core.fsmonitor=false", "-c", "core.attributesFile=/dev/null",
                   "-c", "gc.auto=0", "-c", "maintenance.auto=false", "--git-dir=" + str(store)]
        log = logs / f"git-original-{index:04d}-kind.log"
        run(command + ["cat-file", "-t", row["revision"]], source, env, log)
        require(bounded_bytes(log, 1024) == b"commit\n")
        for suffix, expected in (("commit", row["revision"]), ("tree", row["tree"])):
            log = logs / f"git-original-{index:04d}-{suffix}.log"
            run(command + ["rev-parse", "--verify", row["revision"] + "^{" + suffix + "}"], source, env, log)
            require(bounded_bytes(log, 1024).decode("ascii").strip() == expected)


def native_tests(source: Path, env: dict, output: Path, logs: Path) -> dict:
    executable = plain_path(str(output / "rtc_unittests.exe"))
    identity = digest(executable)
    require(identity[0] > 0)
    listing = logs / "native-test-list.log"
    run([str(executable), "--gtest_list_tests", "--gtest_filter=" + ":".join(TESTS)], source, env, listing)
    found, suite = [], ""
    for line in bounded_bytes(listing, 8 * 1024 * 1024).decode("utf-8").splitlines():
        if re.fullmatch(r"[A-Za-z0-9_]+\.", line):
            suite = line[:-1]
        elif re.fullmatch(r"  [A-Za-z0-9_]+", line) and suite:
            found.append(suite + "." + line.strip())
    require(Counter(found) == Counter(TESTS))
    xml = logs / "native-security-tests.xml"
    run([str(executable), "--gtest_filter=" + ":".join(TESTS), "--gtest_output=xml:" + str(xml)],
        source, env, logs / "native-test-run.log")
    raw = bounded_bytes(xml, 4 * 1024 * 1024)
    require(b"<!DOCTYPE" not in raw and b"<!ENTITY" not in raw)
    result = ET.fromstring(raw)
    cases = list(result.iter("testcase"))
    require(Counter(c.get("classname", "") + "." + c.get("name", "") for c in cases) == Counter(TESTS))
    require(all(c.get("status") == "run" and c.get("result") == "completed" and not list(c) for c in cases))
    require(all(int(result.get(k, "-1")) == 0 for k in ("failures", "disabled", "errors")))
    require(int(result.get("tests", "-1")) == len(TESTS) and digest(executable) == identity)
    return {"target": "//:rtc_unittests", "names": list(TESTS), "cases": len(TESTS),
            "xml": xml, "xml_sha256": digest(xml)[1], "executable_sha256": identity[1],
            "native_crypto_tests_passed": True}


def build(args: argparse.Namespace) -> None:
    require(os.name == "nt")
    require(os.environ.get("GITHUB_ACTIONS") == "true")
    require(os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted")
    require(os.environ.get("RUNNER_OS") == "Windows")
    require(os.environ.get("RUNNER_ARCH") == "X64")
    require(type(args.jobs) is int and 1 <= args.jobs <= 4)
    runner = plain_path(os.environ["RUNNER_TEMP"])
    source, depot = plain_path(args.source_root), plain_path(args.depot_tools_root)
    require(below(source, runner) and below(depot, runner))
    require(not source.is_relative_to(depot) and not depot.is_relative_to(source))
    system = plain_path(os.environ["SystemRoot"])
    package = Path(args.package_root)
    require(package.is_absolute() and package.name == PACKAGE_NAME)
    parent = plain_path(str(package.parent))
    require(below(parent, runner) and not parent.is_relative_to(source))
    require(not parent.is_relative_to(depot))
    stage = parent / (PACKAGE_NAME + ".partial")
    archive = parent / "webrtc-win-x64-release.zip"
    receipt = parent / "windows-build-receipt.json"
    logs = parent / "windows-build-logs"
    output = source / BUILD_NAME
    test_output = source / TEST_BUILD_NAME
    require(all(not item.exists() for item in (package, stage, archive, receipt, logs, output, test_output)))
    inputs_path = plain_path(args.inputs)
    require(below(inputs_path, runner))
    inputs = read_inputs(inputs_path, args.inputs_sha256)
    require(set(inputs) == {"schema", "target", "source", "depot_tools", "tools",
                            "toolchain", "license_tools", "patches", "materials", "gates",
                            "provenance", "source_provision", "repositories", "native_security", "gn_metadata"})
    require(inputs["schema"] == SCHEMA and inputs["target"] == "win-x64-release")
    require(set(inputs["source"]) == {"revision", "tree", "files"})
    require(set(inputs["depot_tools"]) == {"revision", "tree", "files"})
    require(inputs["source"]["revision"] == NATIVE_REVISION)
    require(inputs["source"]["tree"] == NATIVE_TREE)
    require(inputs["depot_tools"]["revision"] == DEPOT_REVISION)
    require(inputs["depot_tools"]["tree"] == DEPOT_TREE)
    require(set(inputs["gates"]) == GATES)
    for name, record in inputs["gates"].items():
        gate = material_json(record)
        require(gate["status"] == "approved-for-isolated-native-build"
                and gate["source_revision"] == NATIVE_REVISION and gate["target"] == "win-x64")
        if name in ("patches", "native-test-source"):
            require(gate["native_security_binding_sha256"] == SECURITY_PLAN_SHA)
        if name == "generated-metadata":
            require(gate["metadata_policy_sha256"] == inputs["gn_metadata"]["sha256"])
    source_files = inventory(source, inputs["source"]["files"])
    depot_files = inventory(depot, inputs["depot_tools"]["files"])
    export = source_export(inputs, source, depot, runner, parent, source_files, depot_files)
    metadata = gn_metadata(inputs, source, export)
    require(set(inputs["tools"]) == {"gn", "ninja", "python", "git"})
    tools = {}
    tool_facts = {}
    for name, record in inputs["tools"].items():
        require(set(record) == {"path", "bytes", "sha256", "version", "source_pin"})
        require(isinstance(record["version"], str) and 0 < len(record["version"]) <= 128)
        require(not any(char in record["version"] for char in "\r\n\x00"))
        path, fact = file_record({key: record[key] for key in ("path", "bytes", "sha256")})
        require(path.suffix.lower() == ".exe")
        tools[name], tool_facts[name] = path, {**fact, "version": record["version"],
                                             "source_pin": record["source_pin"]}
    require(tools["gn"] == source / "buildtools/win/gn.exe")
    require(tools["ninja"] == source / "third_party/ninja/ninja.exe")
    require(inputs["tools"]["gn"]["source_pin"] == GN_PIN)
    require(inputs["tools"]["ninja"]["source_pin"] == NINJA_PIN)
    license_tools = inputs["license_tools"]
    require(set(license_tools) == {"depot_kind", "depot_revision", "gn_wrapper_sha256",
                                  "find_depot_tools_sha256"})
    require(license_tools["find_depot_tools_sha256"] == FIND_DEPOT_TOOLS_SHA256)
    require(source_files["build/find_depot_tools.py"]["sha256"] == FIND_DEPOT_TOOLS_SHA256)
    # This exact be1 build resolver checks the nested DEPS checkout before PATH.
    # Require its real gclient.py discriminator and bind the actual gn.py there;
    # the outer bootstrap can never be attested as the license generator's tool.
    require(license_tools["depot_kind"] == "nested")
    require(license_tools["depot_revision"] == NESTED_DEPOT_REVISION)
    require(source_files["third_party/depot_tools/gclient.py"]["bytes"] > 0)
    wrapper = source_files["third_party/depot_tools/gn.py"]
    require(license_tools["gn_wrapper_sha256"] == wrapper["sha256"])
    for role in ("python", "git"):
        pin = inputs["tools"][role]["source_pin"]
        require(isinstance(pin, str) and 0 < len(pin) <= 256
                and not any(char in pin for char in "\r\n\x00"))
    toolchain = inputs["toolchain"]
    require(set(toolchain) == {"visual_studio_root", "visual_studio_version",
                              "windows_sdk_version", "clang_version", "rust_toolchain_version",
                              "roots", "environment", "path"})
    vs = plain_path(toolchain["visual_studio_root"])
    for name in ("visual_studio_version", "windows_sdk_version", "clang_version",
                 "rust_toolchain_version"):
        require(isinstance(toolchain[name], str) and 0 < len(toolchain[name]) <= 128)
        require(not any(char in toolchain[name] for char in "\r\n\x00"))
        require(re.search(r"[0-9]+\.[0-9]+", toolchain[name]))
    require(type(toolchain["roots"]) is list and 0 < len(toolchain["roots"]) <= 16)
    tool_roots = []
    for record in toolchain["roots"]:
        require(set(record) == {"root", "files"})
        root = plain_path(record["root"])
        require(root not in tool_roots and not root.is_relative_to(source))
        require(not source.is_relative_to(root) and not depot.is_relative_to(root))
        require(not root.is_relative_to(depot) and not parent.is_relative_to(root))
        require(all(not root.is_relative_to(old) and not old.is_relative_to(root)
                    for old in tool_roots))
        inventory(root, record["files"])
        tool_roots.append(root)
    require(any(vs == root or below(vs, root) for root in tool_roots))
    require(any(below(tools["python"], root) for root in tool_roots))
    require(any(below(tools["git"], root) for root in tool_roots))
    require(set(toolchain["environment"]) <= ENV_KEYS)
    for value in toolchain["environment"].values():
        require(isinstance(value, str) and 0 < len(value) <= 32768)
        require(not any(char in value for char in "\r\n\x00&|<>^"))
    require(plain_path(toolchain["environment"]["GYP_MSVS_OVERRIDE_PATH"]) == vs
            and plain_path(toolchain["environment"]["vs2022_install"]) == vs)
    require(type(toolchain["path"]) is list and 0 < len(toolchain["path"]) <= 16)
    path_parts = [plain_path(value) for value in toolchain["path"]]
    require(depot in path_parts and tools["python"].parent in path_parts)
    allowed_roots = [source, depot, *tool_roots]
    for path in path_parts:
        require(path in (system, system / "System32")
                or any(path == root or below(path, root) for root in allowed_roots))
    require(type(inputs["patches"]) is list and len(inputs["patches"]) <= 64)
    patch_facts = {}
    patch_sources = []
    for record in inputs["patches"]:
        path, fact = file_record(record)
        require(path.name not in patch_facts)
        patch_facts[path.name] = fact
        patch_sources.append((path, fact))
    require(all(patch_facts.get(name, {}).get("sha256") == value
                for name, value in UPSTREAM_PATCHES.items()))
    security_plan = security_inputs(inputs, source, export, patch_facts)
    require(type(inputs["materials"]) is list and len(inputs["materials"]) <= 64)
    materials = []
    roles = set()
    for record in inputs["materials"]:
        require(set(record) == {"role", "path", "bytes", "sha256"})
        require(record["role"] in MATERIAL_ROLES)
        path, fact = file_record({key: record[key] for key in ("path", "bytes", "sha256")})
        require(below(path, runner))
        roles.add(record["role"])
        materials.append((record["role"], path, fact))
    require(roles == MATERIAL_ROLES)
    require(sum(fact["bytes"] for _, _, fact in materials) <= MAX_TOTAL)
    for name in ("LICENSE", "PATENTS", "AUTHORS", "tools_webrtc/libs/generate_licenses.py",
                 "build/find_depot_tools.py", ".vpython3"):
        require(name in source_files)
    # No inherited credentials, PYTHONPATH, startup hooks or remote-service config.
    # This environment does not itself replace caller-enforced network isolation.
    env = {
        "SystemRoot": str(system), "WINDIR": str(system),
        "COMSPEC": str(system / "System32/cmd.exe"),
        "TEMP": str(runner), "TMP": str(runner),
        "PATH": os.pathsep.join(str(path) for path in path_parts),
        "DEPOT_TOOLS_UPDATE": "0", "DEPOT_TOOLS_WIN_TOOLCHAIN": "0",
        "GYP_GENERATORS": "ninja,msvs-ninja", "GYP_MSVS_VERSION": "2022",
        "vs2019_install": str(vs), "PYTHONDONTWRITEBYTECODE": "1",
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "NUL",
        "GIT_OPTIONAL_LOCKS": "0", "GIT_NO_REPLACE_OBJECTS": "1", "GIT_NO_LAZY_FETCH": "1",
        "GIT_TERMINAL_PROMPT": "0", "GIT_ALLOW_PROTOCOL": "", "GIT_ATTR_NOSYSTEM": "1",
        **toolchain["environment"],
    }
    stage.mkdir()
    logs.mkdir()
    verify_gitlinks(export)
    original_objects(export, tools["git"], source, env, logs)
    print('{"native_windows_build":"gn"}', flush=True)
    run([str(tools["gn"]), "gen", str(output), "--root=" + str(source),
         "--fail-on-unused-args", "--args=" + GN_ARGS], source, env, logs / "gn.log")
    run([str(tools["gn"]), "gen", str(test_output), "--root=" + str(source),
         "--fail-on-unused-args", "--args=" + TEST_GN_ARGS], source, env, logs / "gn-native-tests.log")
    print('{"native_windows_build":"ninja"}', flush=True)
    run([str(tools["ninja"]), "-C", str(output), "-j", str(args.jobs), ":default"],
        source, env, logs / "ninja.log")
    run([str(tools["ninja"]), "-C", str(test_output), "-j", str(args.jobs), "rtc_unittests"],
        source, env, logs / "ninja-native-tests.log")
    print('{"native_windows_build":"native_tests"}', flush=True)
    test_result = native_tests(source, env, test_output, logs)
    print('{"native_windows_build":"licenses"}', flush=True)
    run([str(tools["python"]), "-I", "-B",
         str(source / "tools_webrtc/libs/generate_licenses.py"), "--target", ":default",
         str(output), str(output)], source, env, logs / "licenses.log")
    verify_gitlinks(export)
    # Reject mutated sources/depot or a changed executed binary before packaging.
    inventory(source, inputs["source"]["files"], skip_build=True)
    inventory(depot, inputs["depot_tools"]["files"])
    inventory(export["root"], export["files"], skip_build=True, build_prefix="checkout/src/")
    for record in toolchain["roots"]:
        inventory(plain_path(record["root"]), record["files"])
    for name, path in tools.items():
        require(digest(path) == (tool_facts[name]["bytes"], tool_facts[name]["sha256"]))
    staged = {}
    inputs_size, inputs_value = digest(inputs_path, MAX_MANIFEST)
    require(inputs_value == args.inputs_sha256)
    staged["MORPHEUS_BUILD_INPUTS.json"] = copy_checked(
        inputs_path, stage / "MORPHEUS_BUILD_INPUTS.json",
        {"bytes": inputs_size, "sha256": inputs_value})
    required = {
        "lib/webrtc.lib": output / "obj/webrtc.lib",
        "webrtc.ninja": output / "obj/webrtc.ninja",
        "desktop_capture.ninja": output / "obj/modules/desktop_capture/desktop_capture.ninja",
        "args.gn": output / "args.gn", "LICENSE.md": output / "LICENSE.md",
        "LICENSE": source / "LICENSE", "PATENTS": source / "PATENTS", "AUTHORS": source / "AUTHORS",
        "native-security-tests.xml": test_result["xml"],
    }
    headers = package_source_headers(export["native_files"], source_files)
    require(headers and any(name.startswith("api/") for name in headers))
    estimated = (sum(source_files[name]["bytes"] for name in headers)
                 + sum(fact["bytes"] for _, _, fact in materials)
                 + sum(fact["bytes"] for _, fact in patch_sources))
    require(estimated <= MAX_TOTAL)
    for name, origin in required.items():
        plain_path(str(origin))
        size, value = digest(origin)
        require(size > 0)
        staged[name] = copy_checked(origin, stage / name, {"bytes": size, "sha256": value})
    gitlink_source, gitlink_fact = file_record(inputs["source_provision"]["gitlinks"])
    staged["SOURCE_GITLINKS.json"] = copy_checked(gitlink_source, stage / "SOURCE_GITLINKS.json", gitlink_fact)
    alias_source, alias_fact = file_record(inputs["source_provision"]["aliases"])
    staged["SOURCE_ALIASES.json"] = copy_checked(alias_source, stage / "SOURCE_ALIASES.json", alias_fact)
    with (stage / "lib/webrtc.lib").open("rb") as library:
        require(library.read(8) == b"!<arch>\n")
    for name in headers:
        staged["include/" + name] = copy_checked(source / name, stage / "include" / name,
                                                source_files[name])
    for index, (role, origin, fact) in enumerate(materials):
        name = f"materials/{index:02d}-{role}/" + relative(origin.name)
        staged[name] = copy_checked(origin, stage / name, fact)
    for index, (origin, fact) in enumerate(patch_sources):
        name = f"patches/{index:02d}.patch"
        staged[name] = copy_checked(origin, stage / name, fact)
    manifest = {
        "schema": "morpheus-native-webrtc-windows-materials-v1",
        "target": "win-x64-release", "source_revision": NATIVE_REVISION,
        "source_tree": NATIVE_TREE, "depot_revision": DEPOT_REVISION,
        "depot_tree": DEPOT_TREE, "admitted_inputs_sha256": args.inputs_sha256,
        "gn_args": GN_ARGS, "test_gn_args": TEST_GN_ARGS,
        "parallel_jobs": args.jobs, "tools": tool_facts,
        "source_provision_materials_sha256": export["materials_sha256"],
        "original_repositories": [{k: r[k] for k in ("mount", "url", "revision", "tree")} for r in export["repositories"]],
        "source_export_has_git_metadata": False, "gn_metadata_policy": metadata,
        "source_plan_schema": "morpheus-native-webrtc-sources-v3", "source_plan_sha256": export["source_plan_sha256"],
        "gitlinks_material_sha256": export["gitlinks_sha256"], "gitlinks": export["gitlinks"]["rows"],
        "uninitialized_gitlinks": len(export["gitlinks"]["rows"]), "submodule_sources_materialized": False,
        "source_aliases_sha256": inputs["source_provision"]["aliases"]["sha256"],
        "source_alias_normalizations": export["aliases"], "source_filesystem_symlinks_created": False,
        "native_security_plan_sha256": SECURITY_PLAN_SHA,
        "security_only_tree": security_plan["security_only_tree"],
        "native_security_test_names": test_result["names"],
        "native_security_test_xml_sha256": test_result["xml_sha256"],
        "native_security_test_executable_sha256": test_result["executable_sha256"],
        "license_tools": license_tools,
        "toolchain": {key: toolchain[key] for key in
                      ("visual_studio_version", "windows_sdk_version", "clang_version",
                       "rust_toolchain_version")},
        "materials": [{"role": role, "file": f"materials/{index:02d}-{role}/" + origin.name, **fact}
                      for index, (role, origin, fact) in enumerate(materials)],
        "patches": [{"name": origin.name, "file": f"patches/{index:02d}.patch", **fact}
                    for index, (origin, fact) in enumerate(patch_sources)],
        "files": staged, "source_materials_present": True,
        "complete_matching_source_verified": False,
        "distribution_ready": False, "license_compliance_verified": False,
        "native_security_tests_passed": test_result["native_crypto_tests_passed"],
        "native_security_scope": "exact12 native crypto tests only; not full ABI/media/interop/hardware acceptance",
    }
    verify_gitlinks(export)
    save_json(stage / "MORPHEUS_NATIVE_MATERIALS.json", manifest)
    verify_gitlinks(export)
    stage.rename(package)
    staged["MORPHEUS_NATIVE_MATERIALS.json"] = {
        "bytes": digest(package / "MORPHEUS_NATIVE_MATERIALS.json")[0],
        "sha256": digest(package / "MORPHEUS_NATIVE_MATERIALS.json")[1],
    }
    require(len(staged) <= MAX_FILES)
    require(sum(fact["bytes"] for fact in staged.values()) <= MAX_TOTAL)
    with archive.open("xb") as stream, zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED,
                                                     allowZip64=True) as zipped:
        for name, fact in sorted(staged.items()):
            path = plain_path(str(package / name))
            require(digest(path) == (fact["bytes"], fact["sha256"]))
            value = hashlib.sha256()
            count = 0
            with path.open("rb") as origin, zipped.open(PACKAGE_NAME + "/" + name, "w",
                                                       force_zip64=True) as target:
                while block := origin.read(1024 * 1024):
                    count += len(block)
                    require(count <= fact["bytes"])
                    value.update(block)
                    target.write(block)
            require((count, value.hexdigest()) == (fact["bytes"], fact["sha256"]))
    size, value = digest(archive)
    verify_gitlinks(export)
    save_json(receipt, {"schema": "morpheus-native-webrtc-windows-build-v1",
                        "source_revision": NATIVE_REVISION, "inputs_sha256": args.inputs_sha256,
                        "archive": {"file": archive.name, "bytes": size, "sha256": value},
                        "materials_sha256": digest(package / "MORPHEUS_NATIVE_MATERIALS.json")[1],
                        "distribution_ready": False,
                        "native_security_tests_passed": test_result["native_crypto_tests_passed"],
                        "native_security_test_xml_sha256": test_result["xml_sha256"],
                        "native_security_plan_sha256": SECURITY_PLAN_SHA})
    print('{"native_windows_build":"complete"}', flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--inputs", required=True)
    parser.add_argument("--inputs-sha256", required=True)
    parser.add_argument("--source-root", required=True)
    parser.add_argument("--depot-tools-root", required=True)
    parser.add_argument("--package-root", required=True)
    parser.add_argument("--jobs", type=int, choices=(1, 2, 3, 4), default=2)
    args = parser.parse_args()
    try:
        build(args)
    except BaseException:
        # No exception text, environment, command, source body or host path leaks.
        # No completion receipt is written on failure; caller must contain/join
        # the original native command tree before collecting private logs.
        print('{"native_windows_build":"failed"}', file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
