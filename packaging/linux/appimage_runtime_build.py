"""Build the AppImage runtime from fixed sources in one owned private prefix.

This is a packaging recipe, not a runtime download, installer or build service.
Only the caller's already reviewed OwnedRunner may execute commands. Source
review, fresh hosted tool admission and installed acceptance remain separate.
"""
from __future__ import annotations

import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import stat
import struct
import tarfile
import time

import appimage_client as client
from appimage_process import download_package_input as download


MANIFEST_SHA256 = "8e6ebc154c79b7b35d35289e7e4f915cd1732b1bfa15faa13562db8a97bbc24e"
NOTICE_DIR = "usr/share/doc/morpheus-app/appimage-runtime"
MAX_RUNTIME = 4 * 1024 * 1024


def fail(reason="source"):
    raise client.PackageFailure(reason)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def manifest():
    data = client.read_regular(Path(__file__).with_name("runtime-build.json"), 32768)
    if digest(data) != MANIFEST_SHA256:
        fail()
    value = json.loads(data)
    if value.get("schema") != 1 or set(value.get("archives", {})) != {
            "runtime", "squashfuse", "musl", "fuse", "zlib", "zstd", "mimalloc"}:
        fail()
    return value


def static_identity(data):
    """Require bounded x86-64 static PIE, NX stack and its reserved MD5 slot."""
    if (type(data) is not bytes or not 1024 <= len(data) <= MAX_RUNTIME
            or data[:7] != b"\x7fELF\x02\x01\x01" or data[8:11] != b"AI\x02"):
        fail("tool")
    kind, machine, version, entry, phoff, shoff, flags, ehsize, phsize, phnum, shsize, shnum, names = struct.unpack_from(
        "<HHIQQQIHHHHHH", data, 16)
    if (kind != 3 or machine != 62 or version != 1 or ehsize != 64
            or phsize != 56 or not 1 <= phnum <= 128 or phoff + phnum * 56 > len(data)
            or shsize != 64 or not 1 <= shnum <= 512 or not 0 < names < shnum
            or shoff + shnum * 64 > len(data)):
        fail("tool")
    stack = False
    for index in range(phnum):
        ptype, pflags, offset, va, pa, size, memory, align = struct.unpack_from("<IIQQQQQQ", data, phoff + index * 56)
        if offset + size > len(data) or ptype == 3 or (ptype == 1 and pflags & 3 == 3):
            fail("tool")
        if ptype == 0x6474e551:
            if pflags & 1:
                fail("tool")
            stack = True
        if ptype == 2:
            if size % 16 or size > 65536:
                fail("tool")
            terminated = False
            for at in range(offset, offset + size, 16):
                tag, value = struct.unpack_from("<qQ", data, at)
                if tag == 1:
                    fail("tool")
                if tag == 0:
                    terminated = True
                    break
            if not terminated:
                fail("tool")
    if not stack:
        fail("tool")
    headers = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + index * 64) for index in range(shnum)]
    name_header = headers[names]
    if name_header[1] != 3 or name_header[4] + name_header[5] > len(data):
        fail("tool")
    strings = data[name_header[4]:name_header[4] + name_header[5]]
    md5 = []
    for header in headers:
        name, stype, sflags, va, offset, size, link, info, align, entsize = header
        if name >= len(strings) or (stype != 8 and offset + size > len(data)):
            fail("tool")
        end = strings.find(b"\0", name)
        if end == -1:
            fail("tool")
        if strings[name:end] == b".digest_md5":
            if (stype != 1 or size != 16 or offset + size > len(data)
                    or sflags & 4 or data[offset:offset + size] != bytes(16)):
                fail("tool")
            md5.append((offset, size))
    if len(md5) != 1:
        fail("tool")
    return {"sha256": digest(data), "size": len(data), "static_pie": True,
            "needed": [], "interpreter": None, "md5_slot": list(md5[0])}


def verify_embedded(artifact, runtime):
    """No tool-selected substitute; only the reserved non-code MD5 may change."""
    identity = static_identity(runtime)
    image = client.read_regular(artifact, 512 * 1024 * 1024)
    if len(image) <= len(runtime):
        fail("deployment")
    offset, size = identity["md5_slot"]
    actual = image[:len(runtime)]
    if actual[:offset] != runtime[:offset] or actual[offset + size:] != runtime[offset + size:]:
        fail("deployment")
    return {"runtime_sha256": digest(runtime), "embedded_prefix_sha256": digest(actual),
            "runtime_bytes": len(runtime), "only_mutable_region": ".digest_md5:16",
            "code_and_all_other_runtime_bytes_exact": True}


def extract(data, destination, pin):
    """Validate the full map before writing; never create an archive link."""
    if len(data) != pin["size"] or digest(data) != pin["sha256"]:
        fail()
    entries = []
    paths = set()
    expanded = 0
    aliases = {"tests/cli-tests/bin/unzstd", "tests/cli-tests/bin/zstdcat"}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as source:
        for member in source:
            pure = PurePosixPath(member.name)
            if (not member.name or "\\" in member.name or "\0" in member.name
                    or pure.is_absolute() or ".." in pure.parts or not pure.parts
                    or pure.parts[0] != pin["prefix"] or pure in paths
                    or str(pure) != member.name.rstrip("/")):
                fail()
            paths.add(pure)
            relative = PurePosixPath(*pure.parts[1:])
            # These two upstream test-only aliases are preserved in the source
            # archive but unused by the static library build. Create no links.
            if member.issym() and pin["prefix"] == "zstd-1.5.6" and str(relative) in aliases and member.linkname == "zstd":
                continue
            if not (member.isdir() or member.isfile()) or member.size < 0 or member.size > 2 * 1024 * 1024:
                fail()
            expanded += member.size
            if len(paths) > 4000 or expanded > 128 * 1024 * 1024:
                fail()
            entries.append((member, relative))
        files = {path for member, path in entries if member.isfile()}
        if any(parent in files for path in files for parent in path.parents):
            fail()
        destination.mkdir(mode=0o700)
        for member, relative in entries:
            path = destination.joinpath(*relative.parts)
            if member.isdir():
                path.mkdir(mode=0o700, parents=True, exist_ok=True)
            else:
                path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                content = source.extractfile(member).read(member.size + 1)
                if len(content) != member.size:
                    fail()
                client.write_new(path, content, 0o700 if member.mode & 0o111 else 0o600)


def host_inputs(run, expected, *, uapi_paths=()):
    """Fail before source commands if fixed Ubuntu package/tool inputs drift."""
    data = client.read_regular(Path(__file__).with_name("runtime-build-host.json"), 262144)
    if digest(data) != expected:
        fail("tool")
    host = json.loads(data)
    packages = host["packages"]
    # Only the paths from the SHA-pinned source manifest extend the tool roots.
    # This is not an allowance for arbitrary host headers or a host sysroot.
    uapi_paths = frozenset(uapi_paths)
    if not uapi_paths <= host["files"].keys():
        fail("tool")
    names = [pin["package"] for pin in packages]
    query = client.R.trusted_tool("dpkg-query")
    output = run("tool-version", [query, "-W", "--showformat=${Package}\t${Version}\t${db:Status-Status}\n", *names], bound=16384)
    wanted = {pin["package"] + "\t" + pin["version"] + "\tinstalled" for pin in packages}
    lines = output.decode("ascii").splitlines()
    if len(lines) != len(wanted) or set(lines) != wanted:
        fail("tool")
    for name, pin in host["files"].items():
        path = Path(name)
        resolved = path.resolve(strict=True)
        if not (name.startswith(("/usr/bin/", "/usr/lib/", "/usr/libexec/", "/usr/share/"))
                or name in uapi_paths):
            fail("tool")
        for ancestor in (resolved, *resolved.parents):
            info = ancestor.stat()
            if info.st_uid != 0 or info.st_mode & 0o022:
                fail("tool")
        content = client.read_regular(resolved, pin["bytes"])
        if len(content) != pin["bytes"] or digest(content) != pin["sha256"]:
            fail("tool")
    return {"manifest_sha256": expected, "packages": packages,
            "hashed_inputs": len(host["files"]), "actual_bytes_match_pinned_packages": True,
            "base_interpreter_and_coreutils": "standard_fresh_ubuntu_host_not_a_bit_reproducibility_claim"}


def prepare(runner, root, environment):
    client.require_build_platform()
    root = client.private_directory(root)
    spec = manifest()
    work = root / "runtime-source-build"
    work.mkdir(mode=0o700)
    home = work / "home"
    home.mkdir(mode=0o700)
    temporary = work / "tmp"
    temporary.mkdir(mode=0o700)
    prefix = work / "prefix"
    prefix.mkdir(mode=0o700)
    source = work / "source"
    source.mkdir(mode=0o700)
    # No owner environment, flags, search path, credentials, compiler plugin or
    # loader override reaches the source build. No writes/install under /usr.
    env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "TMPDIR": str(temporary),
           "LANG": "C", "LC_ALL": "C", "SOURCE_DATE_EPOCH": "0"}
    deadline = time.monotonic() + spec["bounds"]["total_seconds"]
    commands = []
    progress = getattr(runner, "progress", None)

    def mark(stage):
        # Label Python-only work as well as commands; the last command alone
        # cannot identify a later filesystem failure. No paths enter progress.
        if progress is not None:
            progress.command(stage)

    def run(stage, argv, *, cwd=work, budget=120, bound=1024 * 1024):
        remaining = deadline - time.monotonic()
        if remaining < 1:
            fail("deadline")
        commands.append({"stage": stage, "argv": [value.replace(str(work), "$BUILD") for value in argv],
                         "cwd": str(cwd).replace(str(work), "$BUILD"),
                         "maximum_seconds": min(budget, remaining), "maximum_output": bound})
        return runner.run(stage, argv, cwd=cwd, env=dict(env), timeout=min(budget, remaining), output_limit=bound)

    host = host_inputs(run, spec["host_manifest_sha256"], uapi_paths=spec["kernel_headers"].values())
    tools = {name: client.R.trusted_tool(name) for name in
             ("gcc-13", "make", "ar", "ranlib", "strip", "readelf", "patch", "meson", "ninja", "autoreconf", "pkg-config")}
    host_spec = json.loads(client.read_regular(Path(__file__).with_name("runtime-build-host.json"), 262144))
    # Tool aliases must resolve to the exact packaged files checked above.
    for path in tools.values():
        if str(Path(path).resolve(strict=True)) not in host_spec["files"]:
            fail("tool")
    versions = {name: digest(run("tool-version", [path, "--version"], bound=16384)) for name, path in tools.items()}
    archives = {}
    for key, pin in spec["archives"].items():
        if deadline - time.monotonic() < 125:
            fail("deadline")
        archives[key] = download(runner, url=pin["url"], maximum=pin["size"], expected_size=pin["size"],
                                 expected_sha256=pin["sha256"], root=work, environment=env, stage="download-runtime-" + key)
        extract(archives[key], source / key, pin)
    patches = {}
    for name, pin in spec["patches"].items():
        data = client.read_regular(Path(__file__).with_name("runtime-patches") / name, 32768)
        if len(data) != pin["size"] or digest(data) != pin["sha256"]:
            fail()
        patches[name] = data

    def apply(key, name):
        path = work / name
        client.write_new(path, patches[name])
        run("distro-patch", [tools["patch"], "--batch", "--forward", "--fuzz=0", "--get=0", "--no-backup-if-mismatch", "-p1", "--input", str(path)], cwd=source / key)

    apply("musl", "musl-1.2.4-security-static-pie.patch")
    apply("squashfuse", "squashfuse-0.5.2-security.patch")
    apply("zlib", "zlib-1.3.2-CVE-2026-85091.patch")
    for key in ("musl", "squashfuse", "zlib"):
        for name, expected in spec[key + "_patched_files"].items():
            if digest(client.read_regular(source / key / name, 1048576)) != expected:
                fail()
    mount_patch = source / "runtime/patches/libfuse/mount.c.diff"
    run("distro-patch", [tools["patch"], "--batch", "--forward", "--fuzz=0", "--get=0", "-p1", "--input", str(mount_patch)], cwd=source / "fuse")
    flags = "-Os -fPIC -ffunction-sections -fdata-sections " + shlex.quote("-ffile-prefix-map=" + str(work) + "=/usr/src/morpheus-runtime")
    env.update(CC=tools["gcc-13"], AR=tools["ar"], RANLIB=tools["ranlib"], CFLAGS=flags)
    run("configure", [str(source / "musl/configure"), "--prefix=" + str(prefix), "--syslibdir=" + str(prefix / "lib"), "--disable-shared", "--enable-gcc-wrapper"], cwd=source / "musl")
    # musl echoes thousands of full compiler commands by default. Suppress
    # make's recipe echo, keeping compiler warnings/errors and the original
    # per-stream limits, exit checks, deadlines and owned-child cleanup.
    run("runtime-musl-build", [tools["make"], "-s", "-j2"], cwd=source / "musl", budget=300)
    run("runtime-musl-install", [tools["make"], "-s", "install"], cwd=source / "musl")
    # The source's only Linux UAPI include is closed to these13 pinned
    # declaration headers. No host glibc include/search directory is added.
    mark("runtime-headers")
    for destination, original in spec["kernel_headers"].items():
        content = client.read_regular(Path(original).resolve(strict=True), 65536)
        if digest(content) != host_spec["files"][original]["sha256"]:
            fail("tool")
        target = prefix / "include" / destination
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        client.write_new(target, content)
    wrapper = prefix / "bin/musl-gcc"
    if not wrapper.is_file() or wrapper.is_symlink():
        fail("tool")
    env.update(CC=str(wrapper), CPPFLAGS="-I" + shlex.quote(str(prefix / "include")),
               LDFLAGS="-static -L" + shlex.quote(str(prefix / "lib")),
               PKG_CONFIG=tools["pkg-config"], PKG_CONFIG_PATH="", PKG_CONFIG_LIBDIR=str(prefix / "lib/pkgconfig"),
               REALGCC=tools["gcc-13"])
    run("runtime-zlib-configure", [str(source / "zlib/configure"), "--static", "--prefix=" + str(prefix)], cwd=source / "zlib")
    run("runtime-zlib-build", [tools["make"], "-j2", "libz.a"], cwd=source / "zlib")
    run("runtime-zlib-install", [tools["make"], "install"], cwd=source / "zlib")
    run("runtime-zstd-build", [tools["make"], "-C", str(source / "zstd/lib"), "-j2", "libzstd.a"], budget=300)
    mark("runtime-zstd-stage")
    client.write_new(prefix / "lib/libzstd.a", client.read_regular(source / "zstd/lib/libzstd.a", 16 * 1024 * 1024))
    for name in ("zstd.h", "zstd_errors.h"):
        client.write_new(prefix / "include" / name, client.read_regular(source / "zstd/lib" / name, 1048576))
    # Upstream's supported single-object static target, with Alpine's secure
    # mode and musl definition; no C++/CMake/allocator download is introduced.
    mi = work / "mimalloc.o"
    run("runtime-mimalloc-build", [str(wrapper), "-std=c11", "-Os", "-fPIC", "-ftls-model=local-dynamic", "-pthread", "-DMI_SECURE=4", "-DMI_STATIC_LIB", "-DMI_MALLOC_OVERRIDE", "-DMI_LIBC_MUSL=1", "-DNDEBUG", "-I" + str(source / "mimalloc/include"), "-c", str(source / "mimalloc/src/static.c"), "-o", str(mi)])
    run("runtime-mimalloc-archive", [tools["ar"], "rcs", str(prefix / "lib/libmimalloc.a"), str(mi)])
    mark("runtime-native-config")
    native = work / "musl-native.ini"
    client.write_new(native, ("[binaries]\nc = " + repr([str(wrapper)]) + "\nar = " + repr(tools["ar"]) + "\nstrip = " + repr(tools["strip"]) + "\npkgconfig = " + repr(tools["pkg-config"]) + "\n[built-in options]\nc_link_args = ['-static']\n").encode())
    fb = work / "fuse-build"
    run("runtime-fuse-configure", [tools["meson"], "setup", str(fb), str(source / "fuse"), "--native-file", str(native), "--prefix=" + str(prefix), "--libdir=lib", "--default-library=static", "--wrap-mode=nodownload", "-Dutils=false", "-Dexamples=false", "-Dtests=false", "-Duseroot=false", "-Ddisable-libc-symbol-version=true", "-Dinitscriptdir="], budget=180)
    run("runtime-fuse-install", [tools["ninja"], "-C", str(fb), "-j2", "install"], budget=180)
    sq = source / "squashfuse"
    run("runtime-squashfuse-bootstrap", [tools["autoreconf"], "-i"], cwd=sq)
    run("runtime-squashfuse-configure", [str(sq / "configure"), "--prefix=" + str(prefix), "--disable-shared", "--enable-static", "--disable-demo", "--with-zlib=" + str(prefix), "--with-zstd=" + str(prefix), "--without-xz", "--without-lzo", "--without-lz4"], cwd=sq)
    run("runtime-squashfuse-build", [tools["make"], "-j2"], cwd=sq, budget=180)
    run("runtime-malformed-build", [tools["make"], "malformedtest"], cwd=sq)
    # The actual upstream malformed-image target must exist and return0;
    # skip77 is not success. Link it against the same freshly patched libs.
    malformed = sq / "malformedtest"
    if not malformed.is_file() or malformed.is_symlink():
        fail()
    malformed_bytes = client.read_regular(malformed, 4 * 1024 * 1024)
    if malformed_bytes[:7] != b"\x7fELF\x02\x01\x01":
        fail()
    run("runtime-malformed-test", [str(malformed)], cwd=sq, budget=60, bound=65536)
    run("runtime-squashfuse-install", [tools["make"], "install"], cwd=sq)
    # Upstream explicitly installs all public/internal headers for runtime.c.
    mark("runtime-squashfuse-headers")
    for path in sq.glob("*.h"):
        target = prefix / "include/squashfuse" / path.name
        if not target.exists():
            client.write_new(target, client.read_regular(path, 1048576))
    output = work / "runtime-x86_64"
    linker_map = work / "runtime.map"
    rs = source / "runtime/src/runtime"
    runtime_object = work / "runtime.o"
    args = [str(wrapper), "-std=gnu99", "-Os", "-fPIE", "-D_FILE_OFFSET_BITS=64", '-DGIT_COMMIT="' + spec["version_label"] + '"', "-ffunction-sections", "-fdata-sections", "-ffile-prefix-map=" + str(work) + "=/usr/src/morpheus-runtime", "-Wall", "-Werror", "-I" + str(prefix / "include/squashfuse"), "-I" + str(prefix / "include/fuse3"), "-c", str(rs / "runtime.c"), "-o", str(runtime_object)]
    run("runtime-compile", args, cwd=rs)
    run("runtime-link", [str(wrapper), "-T", str(rs / "data_sections.ld"), "-Wl,--gc-sections", "-static", "-static-pie", "-L" + str(prefix / "lib"), str(runtime_object), "-Wl,--start-group", "-lsquashfuse", "-lsquashfuse_ll", "-lzstd", "-lz", "-lfuse3", "-lmimalloc", "-Wl,--end-group", "-pthread", "-Wl,-Map," + str(linker_map), "-o", str(output)], cwd=rs)
    run("runtime-strip", [tools["strip"], "--strip-debug", "--strip-unneeded", str(output)])
    mark("runtime-verify")
    data = client.read_regular(output, MAX_RUNTIME)
    # Exact upstream type2 header construction, after strip, without dd/shell.
    data = data[:8] + b"AI\x02" + data[11:]
    output.write_bytes(data)
    output.chmod(0o500)
    identity = static_identity(data)
    mapping = client.read_regular(linker_map, 4 * 1024 * 1024)
    loaded = re.findall(rb"^LOAD ([^\r\n]+)$", mapping, re.MULTILINE)
    host_files = host_spec["files"]
    loaded_files = []
    for name in loaded:
        path = Path(name.decode()).resolve(strict=True)
        if path != runtime_object and not path.is_relative_to(prefix) and str(path) not in host_files:
            fail("tool")
        content = client.read_regular(path, 64 * 1024 * 1024)
        if str(path) in host_files and digest(content) != host_files[str(path)]["sha256"]:
            fail("tool")
        loaded_files.append({"path": str(path).replace(str(work), "$BUILD"),
                             "sha256": digest(content), "bytes": len(content)})
    required_libraries = {"libsquashfuse.a", "libsquashfuse_ll.a", "libzstd.a", "libz.a", "libfuse3.a", "libmimalloc.a", "libc.a"}
    if not loaded or not required_libraries <= {Path(name.decode()).name for name in loaded}:
        fail("tool")
    metadata = {"schema": 1, "runtime": identity, "runtime_revision": spec["runtime_revision"],
                "version_label": spec["version_label"], "sources": spec["archives"], "patches": spec["patches"],
                "security_backports": spec["security_backports"],
                "host_inputs": host, "tool_version_output_sha256": versions,
                "runtime_object_sha256": digest(client.read_regular(runtime_object, MAX_RUNTIME)),
                "link_map_sha256": digest(mapping), "malformedtest_returned_zero": True,
                "malformedtest_sha256": digest(malformed_bytes), "commands": commands,
                "linked_inputs": loaded_files,
                "fresh_private_prefix": True, "upstream_runtime_binary_downloaded": False,
                "reproducible_build_proof": False, "distribution_ready": False}
    mark("runtime-notices")
    stage = work / "notices"
    stage.mkdir(mode=0o700)
    for key, pin in spec["archives"].items():
        client.write_new(stage / pin["filename"], archives[key])
        for name, expected in pin["licenses"].items():
            license_data = client.read_regular(source / key / name, 65536)
            if len(license_data) != expected["bytes"] or digest(license_data) != expected["sha256"]:
                fail("license")
            client.write_new(stage / (key + "-" + name), license_data)
    for name, content in patches.items():
        client.write_new(stage / name, content)
    # Include the exact UAPI declarations actually used; their per-file SPDX
    # terms (including Linux-syscall-note) remain in the source header text.
    headers = io.BytesIO()
    with tarfile.open(fileobj=headers, mode="w") as archive:
        for destination in sorted(spec["kernel_headers"]):
            content = client.read_regular(prefix / "include" / destination, 65536)
            original = spec["kernel_headers"][destination]
            if digest(content) != host_spec["files"][original]["sha256"]:
                fail("tool")
            member = tarfile.TarInfo(destination)
            member.size = len(content)
            member.mode = 0o644
            archive.addfile(member, io.BytesIO(content))
    client.write_new(stage / "linux-uapi-headers.tar.gz", gzip.compress(headers.getvalue(), mtime=0))
    # Source plus the actual unstripped work object permits relinking against
    # a modified LGPL library; this object is data, never a launched program.
    client.write_new(stage / "runtime-relink.o.gz", gzip.compress(client.read_regular(runtime_object, MAX_RUNTIME), mtime=0))
    client.write_new(stage / "RELINKING.txt", (
        "The runtime and library source archives, patches and build recipe are provided here.\n"
        "Decompress runtime-relink.o.gz with gzip to recover the actual unstripped work object.\n"
        "The SOURCE.json commands describe the private-prefix build and link.\n"
        "This runtime statically uses libfuse under LGPL-2.1; see fuse-LICENSE and fuse-LGPL2.txt.\n"
        "To relink with modified libfuse, rebuild that library from its included source,\n"
        "then repeat the recorded final static link with this object and the other libraries.\n"
        "Release identity checks are not a prohibition on modifying or debugging the LGPL library.\n"
        "No upstream binary signature or bit-reproducible result is asserted.\n"
        "Compiler-runtime and wider product corresponding-source compliance remains a release gate.\n"
    ).encode())
    client.write_new(stage / "SOURCE.json", json.dumps(metadata, sort_keys=True, indent=2).encode() + b"\n")
    for name in ("runtime-build.json", "runtime-build-host.json", "appimage_runtime_build.py"):
        client.write_new(stage / name, client.read_regular(Path(__file__).with_name(name), 262144))
    return data, metadata, stage
