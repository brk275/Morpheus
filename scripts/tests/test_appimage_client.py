"""Pure/private-draft tests: never invoke native tools, services or downloads."""

import hashlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "packaging/linux" / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


C = load("appimage_client")
B = load("appimage_bindings")


def elf(*, runpath=None, legacy=False, marker=b"a", needed="libc.so.6", soname=C.SONAME):
    data = bytearray(2048)
    data[:16] = b"\x7fELF\x02\x01\x01" + bytes(9)
    struct.pack_into("<HHIQQQIHHHHHH", data, 16, 3, 62, 1, 0, 64, 0, 0, 64, 56, 3, 0, 0, 0)
    note = struct.pack("<III", 4, 20, 3) + b"GNU\0" + marker * 20
    strings = b"\0" + soname.encode() + b"\0" + needed.encode() + b"\0"
    tags = [(5, 0x1000 + 1024), (14, 1), (1, len(soname) + 2)]
    if runpath is not None:
        tags.append((15 if legacy else 29, len(strings)))
        strings += runpath.encode() + b"\0"
    tags += [(10, len(strings)), (0, 0)]
    segments = [(1, 5, 0, 0x1000, 0, len(data), len(data), 4096),
                (4, 4, 256, 0, 0, len(note), len(note), 4),
                (2, 4, 320, 0, 0, len(tags) * 16, len(tags) * 16, 8)]
    for index, segment in enumerate(segments):
        struct.pack_into("<IIQQQQQQ", data, 64 + index * 56, *segment)
    data[256:256 + len(note)] = note
    data[1024:1024 + len(strings)] = strings
    for index, tag in enumerate(tags):
        struct.pack_into("<qQ", data, 320 + index * 16, *tag)
    return bytes(data)


def static_runtime():
    """Inert ELF fixture; never executed and not a real runtime build claim."""
    data = bytearray(2048)
    data[:16] = b"\x7fELF\x02\x01\x01\0AI\x02" + bytes(5)
    struct.pack_into("<HHIQQQIHHHHHH", data, 16, 3, 62, 1, 256, 64, 1600, 0, 64, 56, 3, 64, 3, 1)
    for index, segment in enumerate(((1, 5, 0, 0, 0, 1024, 1024, 4096),
                                     (1, 6, 1024, 1024, 0, 1024, 1024, 4096),
                                     (0x6474e551, 6, 0, 0, 0, 0, 0, 16))):
        struct.pack_into("<IIQQQQQQ", data, 64 + index * 56, *segment)
    names = b"\0.shstrtab\0.digest_md5\0"
    data[1536:1536 + len(names)] = names
    struct.pack_into("<IIQQQQIIQQ", data, 1664, 1, 3, 0, 0, 1536, len(names), 0, 0, 1, 0)
    struct.pack_into("<IIQQQQIIQQ", data, 1728, 11, 1, 3, 1280, 1280, 16, 0, 0, 1, 0)
    return bytes(data)


class ToolTests(unittest.TestCase):
    def test_official_pins_have_exact_identities(self):
        pins = C.pinned_tools()
        self.assertEqual({p["asset_id"] for p in pins}, {602438599, 602435573})
        self.assertEqual(len({p["sha256"] for p in pins}), 2)
        expected = {
            "linuxdeploy-x86_64.AppImage":
                "https://api.github.com/repos/linuxdeploy/linuxdeploy/releases/assets/602438599",
            "linuxdeploy-plugin-appimage.AppImage":
                "https://api.github.com/repos/linuxdeploy/linuxdeploy-plugin-appimage/releases/assets/602435573",
        }
        self.assertEqual({pin["cache_name"]: C.tool_download_url(pin) for pin in pins}, expected)
        original = json.loads((ROOT / "packaging/linux/appimage-tools.json").read_text())
        for changed in ({"asset_id": True}, {"asset_id": "657374641"}, {"asset_id": 0},
                        {"asset_id": 10 ** 20}, {"url": "https://github.com/other/project/asset"}):
            with self.subTest(changed=changed):
                invalid = json.loads(json.dumps(original))
                invalid["tools"][1].update(changed)
                with mock.patch.object(C, "read_regular", return_value=json.dumps(invalid).encode()), \
                        self.assertRaises(C.PackageFailure):
                    C.pinned_tools()
        invalid = json.loads(json.dumps(original))
        invalid["tools"].append({"cache_name": "runtime-x86_64"})
        with mock.patch.object(C, "read_regular", return_value=json.dumps(invalid).encode()), \
                self.assertRaises(C.PackageFailure):
            C.pinned_tools()

    def test_original_launcher_preserves_argv_exec_and_relative_runpath(self):
        source = C.launcher_bytes()
        self.assertIn(b'cd -P -- "${0%/*}"', source)
        self.assertIn(b'[ -L "$MORPHEUS_EXECUTABLE" ]', source)
        self.assertIn(b'"$MORPHEUS_APPDIR/usr/bin/morpheus-app"', source)
        self.assertTrue(source.endswith(b'exec "$MORPHEUS_EXECUTABLE" "$@"\n'))
        for forbidden in (b"LD_LIBRARY_PATH=", b"LD_PRELOAD=", b"$*", b"eval ", b"&\n"):
            self.assertNotIn(forbidden, source)

    def fake_pins(self):
        payloads = {name: b"0123456789abcdef" + name.encode() for name in C.TOOL_NAMES}
        pins = [{"cache_name": name, "size": len(data), "sha256": C.sha256(data)} for name, data in payloads.items()]
        return pins, payloads

    def test_bad_last_payload_writes_no_executables(self):
        pins, payloads = self.fake_pins()
        payloads[pins[-1]["cache_name"]] = b"wrong"
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(C, "pinned_tools", return_value=pins):
            root = Path(directory).resolve()
            with self.assertRaises(C.PackageFailure):
                C.seed_tools(root, payloads, static_runtime())
            self.assertEqual(list(root.iterdir()), [])

    def test_new_cache_and_only_header_derivation_allowed(self):
        pins, payloads = self.fake_pins()
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(C, "pinned_tools", return_value=pins):
            root = Path(directory).resolve()
            environment = C.seed_tools(root, payloads, static_runtime())
            self.assertEqual(set(environment), {"XDG_CACHE_HOME", "LDAI_RUNTIME_FILE"})
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads, static_runtime())

            path = root / "xdg-cache/.cargo-packager/AppImage/linuxdeploy-x86_64.AppImage"
            original = payloads[path.name]
            path.write_bytes(original[:8] + bytes(3) + original[11:])
            self.assertEqual(len(C.verify_tools_after(root, payloads, static_runtime())), 4)
            path.write_bytes(path.read_bytes() + b"unexpected")
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads, static_runtime())

    def test_existing_or_symlink_cache_is_rejected_before_writes(self):
        pins, payloads = self.fake_pins()
        for symlink in (False, True):
            with self.subTest(symlink=symlink), tempfile.TemporaryDirectory() as directory, \
                    tempfile.TemporaryDirectory() as elsewhere, mock.patch.object(C, "pinned_tools", return_value=pins):
                root = Path(directory).resolve()
                cache = root / "xdg-cache"
                if symlink:
                    cache.symlink_to(Path(elsewhere).resolve(), target_is_directory=True)
                else:
                    cache.mkdir(mode=0o700)
                with self.assertRaises(C.PackageFailure):
                    C.seed_tools(root, payloads, static_runtime())
                self.assertEqual(list(Path(elsewhere).iterdir()), [])
                self.assertEqual(list(cache.iterdir()), [])

    def test_after_check_rejects_symlink_ancestor_and_writable_tool(self):
        pins, payloads = self.fake_pins()
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(C, "pinned_tools", return_value=pins):
            root = Path(directory).resolve()
            C.seed_tools(root, payloads, static_runtime())
            cache = root / "xdg-cache"
            actual = root / "moved-cache"
            cache.rename(actual)
            cache.symlink_to(actual, target_is_directory=True)
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads, static_runtime())
            cache.unlink()
            actual.rename(cache)
            path = cache / ".cargo-packager/AppImage/linuxdeploy-x86_64.AppImage"
            original = payloads[path.name]
            path.write_bytes(original[:8] + bytes(3) + original[11:])
            path.chmod(0o777)
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads, static_runtime())

    def test_source_runtime_is_static_and_only_embedded_md5_can_change(self):
        runtime = load("appimage_runtime_build")
        original = static_runtime()
        self.assertEqual(runtime.static_identity(original)["needed"], [])
        for offset, value in ((8, 0), (64, 3), (124, 7)):
            with self.subTest(offset=offset):
                changed = bytearray(original)
                changed[offset] = value
                with self.assertRaises(C.PackageFailure):
                    runtime.static_identity(bytes(changed))
        with tempfile.TemporaryDirectory() as directory:
            image = Path(directory) / "candidate.AppImage"
            changed = bytearray(original)
            changed[1280:1296] = b"m" * 16
            image.write_bytes(bytes(changed) + b"hsqs")
            self.assertTrue(runtime.verify_embedded(image, original)["code_and_all_other_runtime_bytes_exact"])
            changed[300] ^= 1
            image.write_bytes(bytes(changed) + b"hsqs")
            with self.assertRaises(C.PackageFailure):
                runtime.verify_embedded(image, original)

    def test_zlib_release_mirror_preserves_the_original_source_pin(self):
        runtime = load("appimage_runtime_build")
        pin = runtime.manifest()["archives"]["zlib"]
        url = "https://github.com/madler/zlib/releases/download/v1.3.2/zlib-1.3.2.tar.gz"
        self.assertEqual(pin["url"], url)
        self.assertEqual(pin["size"], 1502830)
        self.assertEqual(pin["sha256"], "bb329a0a2cd0274d05519d61c667c062e06990d72e125ee2dfa8de64f0119d16")
        self.assertEqual(pin["filename"], "zlib-1.3.2.tar.gz")
        self.assertEqual(pin["prefix"], "zlib-1.3.2")
        data = (ROOT / "packaging/linux/runtime-build.json").read_bytes()
        # Even another HTTPS URL must fail the exact manifest identity gate.
        with mock.patch.object(runtime.client, "read_regular", return_value=data.replace(url.encode(), b"https://unreviewed.invalid/source")), \
                self.assertRaises(C.PackageFailure):
            runtime.manifest()

    def test_runtime_archive_map_rejects_partial_or_linked_sources_before_writes(self):
        runtime = load("appimage_runtime_build")

        def archive(entries):
            output = io.BytesIO()
            with tarfile.open(fileobj=output, mode="w") as stream:
                for name, kind, content in entries:
                    member = tarfile.TarInfo(name)
                    member.type = kind
                    if kind == tarfile.SYMTYPE:
                        member.linkname = content
                        stream.addfile(member)
                    else:
                        member.size = len(content)
                        stream.addfile(member, io.BytesIO(content))
            return output.getvalue()

        valid = [("source/file", tarfile.REGTYPE, b"fixed source")]
        invalid = [valid + valid, [("source/../escape", tarfile.REGTYPE, b"x")],
                   [("source/link", tarfile.SYMTYPE, "outside")],
                   [("source/file", tarfile.REGTYPE, b"x"),
                    ("source/file/child", tarfile.REGTYPE, b"x")]]
        for entries in invalid:
            with self.subTest(entries=[entry[0] for entry in entries]), tempfile.TemporaryDirectory() as folder:
                data = archive(entries)
                destination = Path(folder) / "source"
                pin = {"size": len(data), "sha256": hashlib.sha256(data).hexdigest(), "prefix": "source"}
                with self.assertRaises(C.PackageFailure):
                    runtime.extract(data, destination, pin)
                self.assertFalse(destination.exists())
        with tempfile.TemporaryDirectory() as folder:
            data = archive(valid)
            destination = Path(folder) / "source"
            pin = {"size": len(data), "sha256": hashlib.sha256(data).hexdigest(), "prefix": "source"}
            runtime.extract(data, destination, pin)
            self.assertEqual((destination / "file").read_bytes(), b"fixed source")

    def test_runtime_host_version_drift_refuses_before_source_commands(self):
        runtime = load("appimage_runtime_build")
        data = json.dumps({"packages": [{"package": "compiler", "version": "fixed"}], "files": {}}).encode()
        run = mock.Mock(return_value=b"compiler\tdifferent\tinstalled\n")
        with mock.patch.object(C, "read_regular", return_value=data), \
                mock.patch.object(C.R, "trusted_tool", return_value="/usr/bin/dpkg-query"), \
                self.assertRaises(C.PackageFailure):
            runtime.host_inputs(run, hashlib.sha256(data).hexdigest())
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[0], "tool-version")

    def test_runtime_host_headers_require_exact_source_manifest_membership(self):
        runtime = load("appimage_runtime_build")
        content = b"fixed UAPI declarations"
        allowed = "/usr/include/linux/types.h"

        class InertRootOwnedPath:
            def __init__(self, name): self.name = name
            def with_name(self, name): return self
            def resolve(self, *, strict): return self
            @property
            def parents(self): return ()
            def stat(self): return type("Metadata", (), {"st_uid": 0, "st_mode": 0o100644})()

        for name, admitted in ((allowed, True), ("/usr/include/linux/unrecorded.h", False)):
            with self.subTest(name=name):
                host = {"packages": [{"package": "compiler", "version": "fixed"}], "files": {
                    name: {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}}}
                if name != allowed:
                    host["files"][allowed] = {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
                data = json.dumps(host).encode()
                run = mock.Mock(return_value=b"compiler\tfixed\tinstalled\n")
                with mock.patch.object(runtime, "Path", InertRootOwnedPath), \
                        mock.patch.object(C, "read_regular", side_effect=[data, content]), \
                        mock.patch.object(C.R, "trusted_tool", return_value="/usr/bin/dpkg-query"):
                    if admitted:
                        result = runtime.host_inputs(run, hashlib.sha256(data).hexdigest(), uapi_paths=[allowed])
                        self.assertTrue(result["actual_bytes_match_pinned_packages"])
                    else:
                        with self.assertRaises(C.PackageFailure):
                            runtime.host_inputs(run, hashlib.sha256(data).hexdigest(), uapi_paths=[allowed])

    def test_runtime_make_commands_keep_bounds_and_preserve_fixed_failures(self):
        # Mock commands only; this fixture never executes a compiler or make.
        runtime = load("appimage_runtime_build")
        diagnostics = load("appimage_diagnostics")
        from appimage_process import CommandFailure
        original_read = C.read_regular
        tools = ("gcc-13", "make", "ar", "ranlib", "strip", "readelf", "patch",
                 "meson", "ninja", "autoreconf", "pkg-config")
        for failed_stage, reason in (("runtime-musl-build", "output"),
                                     ("runtime-musl-install", "exit"),
                                     ("runtime-musl-install", "output"),
                                     ("runtime-malformed-build", "exit"),
                                     ("runtime-malformed-test", "exit")):
            with self.subTest(stage=failed_stage, reason=reason), tempfile.TemporaryDirectory() as folder:
                root = Path(folder).resolve()
                recipe = root / "recipe"
                recipe.mkdir(mode=0o700)
                binaries = root / "tools"
                binaries.mkdir(mode=0o700)
                for name in tools:
                    (binaries / name).write_bytes(b"inert tool identity fixture")
                host = {"files": {str(binaries / name): {} for name in tools}}
                spec = runtime.manifest()
                spec["archives"] = {}
                spec["kernel_headers"] = {}
                for key in ("musl", "squashfuse", "zlib", "runtime"):
                    spec[key + "_patched_files"] = {}
                progress = diagnostics.Progress(root)
                progress.wrapper("build")
                calls = []
                failure = CommandFailure(reason)

                def read(path, bound):
                    if Path(path).name == "runtime-build-host.json":
                        return json.dumps(host).encode()
                    return original_read(path, bound)

                def command(stage, argv, *, cwd, env, timeout, output_limit):
                    calls.append((stage, argv, cwd, env, timeout, output_limit))
                    progress.command(stage)
                    if stage == failed_stage:
                        progress.failed(failure)
                        raise failure
                    work = recipe / "runtime-source-build"
                    if stage == "runtime-musl-install":
                        for name in ("bin", "include", "lib"):
                            (work / "prefix" / name).mkdir(mode=0o700)
                        (work / "prefix/bin/musl-gcc").write_bytes(b"inert compiler fixture")
                    elif stage == "runtime-zstd-build":
                        library = work / "source/zstd/lib"
                        library.mkdir(mode=0o700, parents=True)
                        for name in ("libzstd.a", "zstd.h", "zstd_errors.h"):
                            (library / name).write_bytes(b"inert source fixture")
                    elif stage == "runtime-malformed-build":
                        sq = work / "source/squashfuse"
                        sq.mkdir(mode=0o700)
                        (sq / "malformedtest").write_bytes(static_runtime())
                    return b"fixed command fixture"

                runner = mock.Mock(progress=progress)
                runner.run.side_effect = command
                with mock.patch.object(C, "require_build_platform"), \
                        mock.patch.object(C, "read_regular", side_effect=read), \
                        mock.patch.object(C.R, "trusted_tool", side_effect=lambda name: str(binaries / name)), \
                        mock.patch.object(runtime, "manifest", return_value=spec), \
                        mock.patch.object(runtime, "host_inputs", return_value={}), \
                        mock.patch.object(runtime, "download", side_effect=AssertionError("no fixture downloads")), \
                        self.assertRaises(CommandFailure) as error:
                    runtime.prepare(runner, recipe, {"LD_PRELOAD": "/private/foreign.so"})
                self.assertIs(error.exception, failure)
                musl = [call for call in calls if call[0].startswith("runtime-musl-")]
                expected = [("runtime-musl-build", [str(binaries / "make"), "-s", "-j2"], 300)]
                if failed_stage != "runtime-musl-build":
                    expected.append(("runtime-musl-install", [str(binaries / "make"), "-s", "install"], 120))
                self.assertEqual(len(musl), len(expected))
                for (label, argv, cwd, env, timeout, bound), (wanted, arguments, budget) in zip(musl, expected):
                    self.assertEqual((label, argv, cwd), (wanted, arguments, recipe / "runtime-source-build/source/musl"))
                    self.assertGreater(timeout, 0)
                    self.assertLessEqual(timeout, budget)
                    self.assertEqual(bound, 1024 * 1024)
                    self.assertEqual(env["PATH"], "/usr/bin:/bin")
                    self.assertNotIn("LD_PRELOAD", env)
                malformed = [call for call in calls if call[0].startswith("runtime-malformed-")]
                if failed_stage.startswith("runtime-malformed-"):
                    self.assertEqual(len(malformed), 2 if failed_stage == "runtime-malformed-test" else 1)
                    stage, argv, cwd, env, timeout, bound = malformed[0]
                    self.assertEqual(stage, "runtime-malformed-build")
                    self.assertEqual(argv, [str(binaries / "make"), "malformedtest", "LDFLAGS=" + env["LDFLAGS"] + " -all-static"])
                    self.assertTrue(env["LDFLAGS"].startswith("-static -L"))
                    self.assertNotIn("-all-static", env["LDFLAGS"])
                    for stage, argv, cwd, env, timeout, bound in malformed:
                        is_test = stage == "runtime-malformed-test"
                        self.assertEqual(cwd, recipe / "runtime-source-build/source/squashfuse")
                        self.assertGreater(timeout, 0)
                        self.assertLessEqual(timeout, 60 if is_test else 120)
                        self.assertEqual(bound, 65536 if is_test else 1024 * 1024)
                        if is_test:
                            self.assertEqual(argv, [str(cwd / "malformedtest")])
                self.assertEqual(calls[-1][0], failed_stage)
                self.assertTrue(diagnostics.valid_record(progress.value))
                self.assertEqual(progress.value["failure"]["command_stage"], failed_stage)
                self.assertEqual(progress.value["failure"]["reason"], "command_" + reason)

    def test_regression_elf_accepts_static_exec_and_pie_but_rejects_loader_or_needed(self):
        runtime = load("appimage_runtime_build")
        for kind in (2, 3):
            data = bytearray(static_runtime())
            struct.pack_into("<H", data, 16, kind)
            runtime.require_static_regression(bytes(data))
        dynamic = bytearray(static_runtime())
        struct.pack_into("<IIQQQQQQ", dynamic, 64 + 2 * 56, 2, 4, 1408, 0, 0, 32, 32, 8)
        struct.pack_into("<qQqQ", dynamic, 1408, 21, 0, 0, 0)
        runtime.require_static_regression(bytes(dynamic))
        needed = bytearray(dynamic)
        struct.pack_into("<qQ", needed, 1408, 1, 0)
        interpreter = bytearray(static_runtime())
        struct.pack_into("<IIQQQQQQ", interpreter, 64 + 2 * 56, 3, 4, 1408, 0, 0, 16, 16, 1)
        for data in (needed, interpreter):
            with self.subTest(segment=data[64 + 2 * 56:68 + 2 * 56]), self.assertRaises(C.PackageFailure):
                runtime.require_static_regression(bytes(data))

    def test_regression_elf_rejects_malformed_header_and_dynamic_bounds(self):
        runtime = load("appimage_runtime_build")
        invalid = [b"", static_runtime()[:1023], bytearray(static_runtime())]
        for offset, format, value in ((16, "H", 1), (18, "H", 183), (20, "I", 0),
                                      (32, "Q", 63), (32, "Q", 2048), (52, "H", 63),
                                      (54, "H", 55), (56, "H", 0), (56, "H", 129)):
            data = bytearray(static_runtime())
            struct.pack_into("<" + format, data, offset, value)
            invalid.append(bytes(data))
        for size, offset, tag in ((16, 1408, 21), (17, 1408, 0), (32, 2040, 0), (65552, 1408, 0)):
            data = bytearray(static_runtime())
            struct.pack_into("<IIQQQQQQ", data, 64 + 2 * 56, 2, 4, offset, 0, 0, size, size, 8)
            struct.pack_into("<qQ", data, 1408, tag, 0)
            invalid.append(bytes(data))
        for index, data in enumerate(invalid):
            with self.subTest(case=index), self.assertRaises(C.PackageFailure):
                runtime.require_static_regression(data)

    def test_source_rejection_precedes_any_command(self):
        callback = mock.Mock(side_effect=AssertionError("native work forbidden"))
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(C, "require_build_platform"), self.assertRaises(C.PackageFailure):
            C.prepare_client(Path(directory).resolve(), {}, b"", callback, variant="appimage")
        callback.assert_not_called()

    def test_only_unprivileged_ubuntu_2404_x64_build_platform(self):
        with mock.patch.object(C.sys, "platform", "linux"), mock.patch.object(C.platform, "machine", return_value="x86_64"), \
                mock.patch.object(C.os, "getuid", return_value=1001), mock.patch.object(C.os, "geteuid", return_value=1001):
            C.require_build_platform('ID=ubuntu\nVERSION_ID="24.04"\n')
            for release in ('ID=debian\nVERSION_ID="24.04"\n', 'ID=ubuntu\nVERSION_ID="26.04"\n'):
                with self.assertRaises(C.PackageFailure):
                    C.require_build_platform(release)
            with mock.patch.object(C.platform, "machine", return_value="aarch64"), self.assertRaises(C.PackageFailure):
                C.require_build_platform('ID=ubuntu\nVERSION_ID="24.04"\n')


class ConfigurationTests(unittest.TestCase):
    def test_appimage_only_mapping_does_not_mutate_deb(self):
        base = {"name": "morpheus-app", "deb": {"depends": ["libpipewire-0.3-0"]}}
        result = C.appimage_config(base, Path("/private/stage"))
        self.assertNotIn("appimage", base)
        self.assertEqual(result["deb"], base["deb"])
        self.assertEqual(result["appimage"]["excluded-libs"], ["libpipewire-0.3.so*"])
        self.assertEqual(result["appimage"]["files"]["/private/stage/" + C.SONAME], C.DESTINATION)
        self.assertEqual(sum(path.endswith(C.SONAME) for path in result["appimage"]["files"]), 1)
        self.assertTrue(any(name.endswith("exported-node-startup.patch") for name in result["appimage"]["files"]))

    def test_distinct_native_soname_requires_actual_elf_identity(self):
        native = elf(soname=C.R.NATIVE_SONAME)
        self.assertEqual(C.R.elf_identity(native, library=True, expected_soname=C.R.NATIVE_SONAME),
                         ("61" * 20, ("libc.so.6",)))
        with self.assertRaises(C.R.ProbeFailure):
            C.R.elf_identity(native, library=True)
        with self.assertRaises(C.R.ProbeFailure):
            C.R.elf_identity(elf(), library=True, expected_soname=C.R.NATIVE_SONAME)
        with self.assertRaises(C.R.ProbeFailure):
            C.R.elf_identity(native, library=True, expected_soname="unreviewed.so")

    def test_native_receipt_binds_source_bytes_and_cannot_relabel_stock_elf(self):
        data = elf(soname=C.R.NATIVE_SONAME)
        metadata = {"schema": 1, "source_client_sha256": C.sha256(data), "build_id": "61" * 20,
                    "needed": ["libc.so.6"], "source_version": C.R.VERSION,
                    "upstream_commit": C.R.COMMIT, "upstream_patch_sha256": C.R.UPSTREAM_PATCH_SHA256,
                    "patched_source_sha256": C.R.PATCHED_SOURCE_SHA256,
                    "source_archives": {k: v[1] for k, v in C.R.ARCHIVES.items()},
                    "variant": "native", "soname": C.R.NATIVE_SONAME,
                    "native_meson_sha256": C.NATIVE_MESON_PATCHED_SHA256}
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory).resolve()
            (stage / C.R.NATIVE_SONAME).write_bytes(data)
            receipt = C.build_receipt(stage, metadata, variant="native")
            artifact = json.loads((stage / "artifact.json").read_bytes())
            self.assertEqual(artifact["sha256"], C.sha256(data))
            self.assertEqual(artifact["library_path"], str(stage / C.R.NATIVE_SONAME))
            self.assertIn("variant=native\n", receipt.read_text())
            self.assertEqual(artifact["variant"], "native")
        for change in ("stock", "source", "digest"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                stage = Path(directory).resolve()
                (stage / C.R.NATIVE_SONAME).write_bytes(elf() if change == "stock" else data)
                altered = dict(metadata)
                if change == "source":
                    altered["patched_source_sha256"] = "0" * 64
                if change == "digest":
                    altered["source_client_sha256"] = "0" * 64
                with self.assertRaises((C.PackageFailure, C.R.ProbeFailure)):
                    C.build_receipt(stage, altered, variant="native")
                self.assertFalse((stage / "build-client.txt").exists())
                self.assertFalse((stage / "artifact.json").exists())

    def test_ambiguous_existing_appimage_config_rejected(self):
        with self.assertRaises(C.PackageFailure):
            C.appimage_config({"appimage": {}}, Path("/private/stage"))

    def test_original_strict_elf_reader_unchanged(self):
        C.R.elf_identity(elf(), library=True)
        with self.assertRaises(C.R.ProbeFailure):
            C.R.elf_identity(elf(runpath="$ORIGIN"), library=True)

    def test_only_single_local_runpath_is_allowed_for_deployed_client(self):
        C.R.elf_identity(elf(runpath="$ORIGIN"), library=True, allow_origin_runpath=True)
        for path in ("", ":$ORIGIN", "$ORIGIN:", "$ORIGIN/..", "/usr/lib", "$ORIGIN:$ORIGIN"):
            with self.subTest(path=path), self.assertRaises(C.R.ProbeFailure):
                C.R.elf_identity(elf(runpath=path), library=True, allow_origin_runpath=True)
        with self.assertRaises(C.R.ProbeFailure):
            C.R.elf_identity(elf(runpath="$ORIGIN", legacy=True), library=True, allow_origin_runpath=True)

    def test_deployed_sha_measured_after_transform_and_duplicate_refused(self):
        original = elf()
        metadata = {"build_id": "61" * 20, "needed": ["libc.so.6"], "source_client_sha256": C.sha256(original)}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            target = root / C.DESTINATION
            target.parent.mkdir(parents=True)
            target.write_bytes(elf(runpath="$ORIGIN"))
            target.chmod(0o500)
            result = C.verify_deployed_client(root, metadata)
            self.assertEqual(result["sha256"], C.sha256(target.read_bytes()))
            self.assertNotEqual(result["sha256"], result["source_client_sha256"])
            duplicate = root / "libpipewire-0.3.so.0.1005.0"
            duplicate.symlink_to(target)
            with self.assertRaises(C.PackageFailure):
                C.verify_deployed_client(root, metadata)

    def test_deployed_wrong_identity_and_world_writable_rejected(self):
        metadata = {"build_id": "61" * 20, "needed": ["libc.so.6"], "source_client_sha256": "a" * 64}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            path = root / C.DESTINATION
            path.parent.mkdir(parents=True)
            path.write_bytes(elf(marker=b"b"))
            with self.assertRaises(C.PackageFailure):
                C.verify_deployed_client(root, metadata)
            path.write_bytes(elf())
            path.chmod(0o666)
            with self.assertRaises(C.PackageFailure):
                C.verify_deployed_client(root, metadata)


class PreparationContractTests(unittest.TestCase):
    def test_single_patched_target_and_source_notices_with_mocked_commands(self):
        calls = []
        licenses = {"LICENSE": b"fixture license", "COPYING": b"fixture copyright"}
        patch = b"fixture upstream patch"
        archives = {key: key.encode() for key in C.R.ARCHIVES}

        def extraction(data, destination, **options):
            if options["prefix"] == "pipewire-1.0.5":
                for name, content in licenses.items():
                    (destination / name).write_bytes(content)

        def command(stage, argv, *, cwd, env, timeout, output_limit):
            calls.append((stage, argv))
            self.assertGreater(timeout, 0)
            self.assertLessEqual(timeout, 600)
            self.assertNotIn("LD_LIBRARY_PATH", env)
            self.assertEqual(env["SOURCE_DATE_EPOCH"], "1727740800")
            if stage == "configure":
                self.assertTrue(patched.called)
                build = Path(argv[2])
                build.mkdir()
                (build / "config.h").write_bytes(b"configuration")
                (build / "build.ninja").write_bytes(b"recipe")
            if stage == "client-build":
                build = Path(argv[2])
                target = build / C.R.TARGET
                target.parent.mkdir(parents=True)
                target.write_bytes(elf())
                objects = build / (C.R.TARGET + ".p")
                objects.mkdir()
                for name in C.R.SOURCES:
                    (objects / (name + ".c.o")).write_bytes(name.encode())
            return b"fixed mock output"

        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(C, "require_build_platform"), \
                mock.patch.object(C, "LICENSES", {name: C.sha256(data) for name, data in licenses.items()}), \
                mock.patch.object(C.R, "UPSTREAM_PATCH_SHA256", C.sha256(patch)), \
                mock.patch.object(C.R, "trusted_tool", side_effect=lambda name: "/usr/bin/" + name), \
                mock.patch.object(C.R, "verified_archive", side_effect=lambda data, key: data), \
                mock.patch.object(C.R, "extract_archive", side_effect=extraction), \
                mock.patch.object(C.R, "patch_series", return_value=[]), \
                mock.patch.object(C.R, "apply_upstream_patch") as patched, \
                mock.patch.dict(C.os.environ, {"LD_LIBRARY_PATH": "/untrusted"}):
            stage, metadata = C.prepare_client(Path(directory).resolve(), archives, patch, command, variant="appimage")
            self.assertEqual(metadata["source_client_sha256"], C.sha256(elf()))
            self.assertEqual(len(metadata["objects"]), 33)
            self.assertEqual((stage / "exported-node-startup.patch").read_bytes(), patch)
            self.assertEqual((stage / "COPYING").read_bytes(), licenses["COPYING"])
            self.assertEqual(json.loads((stage / "SOURCE.json").read_bytes()), metadata)
            builds = [argv for label, argv in calls if label == "client-build"]
            self.assertEqual(len(builds), 1)
            self.assertEqual(builds[0][-2:], ["-j2", C.R.TARGET])

    def test_child_exception_is_static_and_no_subsequent_command_runs(self):
        calls = mock.Mock(side_effect=RuntimeError("private child output"))
        patch = b"fixture upstream patch"
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(C, "require_build_platform"), \
                mock.patch.object(C.R, "UPSTREAM_PATCH_SHA256", C.sha256(patch)), \
                mock.patch.object(C.R, "trusted_tool", side_effect=lambda name: "/usr/bin/" + name), \
                mock.patch.object(C.R, "verified_archive", side_effect=lambda data, key: data):
            with self.assertRaises(C.PackageFailure) as error:
                C.prepare_client(Path(directory).resolve(), {key: b"fixture" for key in C.R.ARCHIVES}, patch, calls, variant="appimage")
            self.assertEqual(str(error.exception), "AppImage client preparation failed: command")
            self.assertEqual(calls.call_count, 1)

    def test_exhausted_total_deadline_starts_no_command(self):
        calls = mock.Mock(side_effect=AssertionError("must not start"))
        patch = b"fixture upstream patch"
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(C, "require_build_platform"), \
                mock.patch.object(C.R, "UPSTREAM_PATCH_SHA256", C.sha256(patch)), \
                mock.patch.object(C.R, "trusted_tool", side_effect=lambda name: "/usr/bin/" + name), \
                mock.patch.object(C.R, "verified_archive", side_effect=lambda data, key: data), \
                mock.patch.object(C.time, "monotonic", side_effect=[0, 601]):
            with self.assertRaises(C.PackageFailure) as error:
                C.prepare_client(Path(directory).resolve(), {key: b"fixture" for key in C.R.ARCHIVES}, patch, calls, variant="appimage")
            self.assertEqual(error.exception.reason, "deadline")
            calls.assert_not_called()


class BindingTests(unittest.TestCase):
    COLLISIONS = frozenset(f"pw_fixture_{number}" for number in range(33))

    def table(self, *rows):
        entries = ["0: 00000000 0 NOTYPE LOCAL DEFAULT UND", *rows]
        return (f"Symbol table '.dynsym' contains {len(entries)} entries:\n"
                "   Num:    Value          Size Type    Bind   Vis      Ndx Name\n" + "\n".join(entries) + "\n").encode()

    def test_actual_imports_and_not_imported_are_distinct(self):
        result = B.imported_pipewire_symbols(self.table("1: 00000000 0 FUNC GLOBAL DEFAULT UND pw_fixture_1"), self.COLLISIONS)
        self.assertEqual(result["imports"], {"pw_fixture_1"})
        self.assertEqual(len(result["not_imported_collisions"]), 32)

    def test_no_definitions_missing_duplicate_or_truncated_inventory(self):
        good = self.table("1: 00000000 0 FUNC GLOBAL DEFAULT UND pw_fixture_1")
        for value in (good.replace(b"UND pw_", b"12 pw_"), good.replace(b"GLOBAL DEFAULT UND pw_", b"WEAK DEFAULT UND pw_"),
                      good.replace(b"2 entries", b"3 entries"), good + good.splitlines()[-1] + b"\n", good.rsplit(b"\n", 2)[0]):
            with self.subTest(value=value[:30]), self.assertRaises(B.BindingFailure):
                B.imported_pipewire_symbols(value, self.COLLISIONS)

    def line(self, symbol="pw_fixture_1", *, provider="/app/usr/lib/libpipewire-0.3.so.0", origin="/app/usr/bin/morpheus-app", namespace="0"):
        return f"  123: binding file {origin} [0] to {provider} [{namespace}]: normal symbol `{symbol}'\n".encode()

    def check(self, value, **extra):
        return B.verify_binding_trace(value, pid=123, executable="/app/usr/bin/morpheus-app",
                                      library="/app/usr/lib/libpipewire-0.3.so.0", imports={"pw_fixture_1"},
                                      complete=extra.get("complete", True), verified_library_aliases=extra.get("aliases", ()))

    def test_exact_app_binding_returns_only_counts(self):
        self.assertEqual(self.check(self.line()), {"app_pipewire_imports": 1, "app_pipewire_bindings_verified": 1, "unexpected_providers": 0})

    def test_library_self_binding_cannot_prove_app_binding(self):
        with self.assertRaises(B.BindingFailure):
            self.check(self.line(origin="/app/usr/lib/libpipewire-0.3.so.0"))

    def test_missing_wrong_provider_namespace_or_conflict_is_fatal(self):
        for value in (b"", self.line(provider="/usr/lib/libpipewire-0.3.so.0"), self.line(namespace="1"),
                      self.line() + self.line(provider="/tmp/other.so"), self.line("pw_unexpected")):
            with self.subTest(value=value[:20]), self.assertRaises(B.BindingFailure):
                self.check(value)

    def test_incomplete_and_malformed_trace_cannot_pass(self):
        for value in (self.line() + b"123: binding file malformed\n", self.line() + b"\xff"):
            with self.assertRaises(B.BindingFailure):
                self.check(value)
        with self.assertRaises(B.BindingFailure):
            self.check(self.line(), complete=False)

    def test_only_exact_preverified_aliases_can_supplement_canonical_provider(self):
        alias = "/app/usr/bin/../lib/libpipewire-0.3.so.0"
        with self.assertRaises(B.BindingFailure):
            self.check(self.line(provider=alias))
        self.assertEqual(self.check(self.line(provider=alias), aliases=[alias])["app_pipewire_bindings_verified"], 1)
        with self.assertRaises(B.BindingFailure):
            self.check(self.line(provider=alias + ".other"), aliases=[alias])

    def test_live_alias_collection_is_only_actual_pid_app_and_complete_lines(self):
        alias = "/app/usr/bin/../lib/libpipewire-0.3.so.0"
        trace = self.line(provider=alias) + self.line(provider="/other", origin="/other") + b"123: binding file unfinished"
        self.assertEqual(B.provider_spellings(trace, pid=123, executable="/app/usr/bin/morpheus-app"), {alias})
        with self.assertRaises(B.BindingFailure):
            B.provider_spellings(trace + b"\n", pid=123, executable="/app/usr/bin/morpheus-app")


if __name__ == "__main__":
    unittest.main()
