"""Pure/private-draft tests: never invoke native tools, services or downloads."""

import importlib.util
import json
from pathlib import Path
import struct
import sys
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


def elf(*, runpath=None, legacy=False, marker=b"a", needed="libc.so.6"):
    data = bytearray(2048)
    data[:16] = b"\x7fELF\x02\x01\x01" + bytes(9)
    struct.pack_into("<HHIQQQIHHHHHH", data, 16, 3, 62, 1, 0, 64, 0, 0, 64, 56, 3, 0, 0, 0)
    note = struct.pack("<III", 4, 20, 3) + b"GNU\0" + marker * 20
    strings = b"\0" + C.SONAME.encode() + b"\0" + needed.encode() + b"\0"
    tags = [(5, 0x1000 + 1024), (14, 1), (1, len(C.SONAME) + 2)]
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


class ToolTests(unittest.TestCase):
    def test_official_pins_have_exact_identities(self):
        pins = C.pinned_tools()
        self.assertEqual({p["asset_id"] for p in pins}, {602438599, 602435573, 596078161})
        self.assertEqual(len({p["sha256"] for p in pins}), 3)

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
                C.seed_tools(root, payloads)
            self.assertEqual(list(root.iterdir()), [])

    def test_new_cache_and_only_header_derivation_allowed(self):
        pins, payloads = self.fake_pins()
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(C, "pinned_tools", return_value=pins):
            root = Path(directory).resolve()
            environment = C.seed_tools(root, payloads)
            self.assertEqual(set(environment), {"XDG_CACHE_HOME", "LDAI_RUNTIME_FILE"})
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads)

            path = root / "xdg-cache/.cargo-packager/AppImage/linuxdeploy-x86_64.AppImage"
            original = payloads[path.name]
            path.write_bytes(original[:8] + bytes(3) + original[11:])
            self.assertEqual(len(C.verify_tools_after(root, payloads)), 4)
            path.write_bytes(path.read_bytes() + b"unexpected")
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads)

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
                    C.seed_tools(root, payloads)
                self.assertEqual(list(Path(elsewhere).iterdir()), [])
                self.assertEqual(list(cache.iterdir()), [])

    def test_after_check_rejects_symlink_ancestor_and_writable_tool(self):
        pins, payloads = self.fake_pins()
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(C, "pinned_tools", return_value=pins):
            root = Path(directory).resolve()
            C.seed_tools(root, payloads)
            cache = root / "xdg-cache"
            actual = root / "moved-cache"
            cache.rename(actual)
            cache.symlink_to(actual, target_is_directory=True)
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads)
            cache.unlink()
            actual.rename(cache)
            path = cache / ".cargo-packager/AppImage/linuxdeploy-x86_64.AppImage"
            original = payloads[path.name]
            path.write_bytes(original[:8] + bytes(3) + original[11:])
            path.chmod(0o777)
            with self.assertRaises(C.PackageFailure):
                C.verify_tools_after(root, payloads)

    def test_source_rejection_precedes_any_command(self):
        callback = mock.Mock(side_effect=AssertionError("native work forbidden"))
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(C, "require_build_platform"), self.assertRaises(C.PackageFailure):
            C.prepare_client(Path(directory).resolve(), {}, b"", callback)
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
            stage, metadata = C.prepare_client(Path(directory).resolve(), archives, patch, command)
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
                C.prepare_client(Path(directory).resolve(), {key: b"fixture" for key in C.R.ARCHIVES}, patch, calls)
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
                C.prepare_client(Path(directory).resolve(), {key: b"fixture" for key in C.R.ARCHIVES}, patch, calls)
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
