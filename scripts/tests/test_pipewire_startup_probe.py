"""Pure filesystem/format and mocked-command checks; never run native tools."""

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("pipewire_startup_probe", Path(__file__).resolve().parents[1] / "pipewire_startup_probe.py")
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)


def tar_bytes(entries):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for name, value, kind in entries:
            item = tarfile.TarInfo(name)
            item.type = kind
            item.size = len(value) if kind == tarfile.REGTYPE else 0
            item.linkname = "/outside"
            archive.addfile(item, io.BytesIO(value) if kind == tarfile.REGTYPE else None)
    return output.getvalue()


def synthetic_elf(marker=b"a", *, needed="libc.so.6", soname=M.SONAME, runpath=False):
    data = bytearray(1024)
    data[:16] = b"\x7fELF\x02\x01\x01" + bytes(9)
    struct.pack_into("<HHIQQQIHHHHHH", data, 16, 3, 62, 1, 0, 64, 0, 0, 64, 56, 3, 0, 0, 0)
    note = struct.pack("<III", 4, 20, 3) + b"GNU\0" + marker * 20
    strings = b"\0" + soname.encode() + b"\0" + needed.encode() + b"\0"
    needed_offset = len(soname) + 2
    tags = [(5, 0x1000 + 512), (10, len(strings)), (14, 1), (1, needed_offset)]
    if runpath:
        tags.append((29, 1))
    tags.append((0, 0))
    segments = [(1, 5, 0, 0x1000, 0, len(data), len(data), 4096),
                (4, 4, 256, 0, 0, len(note), len(note), 4),
                (2, 4, 320, 0, 0, len(tags) * 16, len(tags) * 16, 8)]
    for index, segment in enumerate(segments):
        struct.pack_into("<IIQQQQQQ", data, 64 + index * 56, *segment)
    data[256:256 + len(note)] = note
    data[512:512 + len(strings)] = strings
    for index, tag in enumerate(tags):
        struct.pack_into("<qQ", data, 320 + index * 16, *tag)
    return bytes(data)


def package_output():
    return "".join(f"{name.removesuffix(':amd64')}\t{M.VERSION}\tpipewire\t{M.VERSION}\t"
                   f"{'amd64' if name.endswith(':amd64') else 'all'}\tinstalled\n" for name in M.PACKAGES).encode()


class EligibilityTests(unittest.TestCase):
    def test_requires_explicit_disposable_linux_environment(self):
        good = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Linux",
                "MORPHEUS_WAYLAND_STARTUP_PROBE": "1"}
        with mock.patch.object(M.sys, "platform", "linux"), mock.patch.object(M.platform, "machine", return_value="x86_64"), \
                mock.patch.object(M.os, "getuid", return_value=1001), mock.patch.object(M.os, "geteuid", return_value=1001):
            M.check_eligibility(good, 'ID=ubuntu\nVERSION_ID="24.04"\n')
            for key in good:
                environment = {**good, key: "wrong"}
                with self.subTest(key=key), self.assertRaisesRegex(M.ProbeFailure, "ineligible"):
                    M.check_eligibility(environment, 'ID=ubuntu\nVERSION_ID="24.04"\n')
            for release in ('ID=debian\nVERSION_ID="24.04"\n', 'ID=ubuntu\nVERSION_ID="26.04"\n'):
                with self.assertRaises(M.ProbeFailure):
                    M.check_eligibility(good, release)
            with mock.patch.object(M.os, "geteuid", return_value=0), self.assertRaises(M.ProbeFailure):
                M.check_eligibility(good, 'ID=ubuntu\nVERSION_ID="24.04"\n')

    def test_private_root_excludes_checkout_symlink_and_open_permissions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            self.assertEqual(M.private_root(root), root)
            root.chmod(0o755)
            with self.assertRaisesRegex(M.ProbeFailure, "private_root"):
                M.private_root(root)
            root.chmod(0o700)
            alias = root / "alias"
            alias.symlink_to(root, target_is_directory=True)
            with self.assertRaisesRegex(M.ProbeFailure, "private_root"):
                M.private_root(alias)
            with mock.patch.object(M, "REPO", root), self.assertRaisesRegex(M.ProbeFailure, "private_root"):
                M.private_root(root)

    def test_exact_binary_source_architecture_and_installed_package_identity(self):
        good = package_output()
        M.validate_packages(good)
        for bad in (good + good.splitlines(keepends=True)[0], good.replace(b"1ubuntu3.3", b"1ubuntu3.2", 1),
                    good.replace(b"pipewire\t", b"other\t", 1), good.replace(b"amd64", b"arm64", 1),
                    good.replace(b"installed", b"unpacked", 1), good.replace(b"pipewire-bin\t", b"pipewire-bin:amd64\t"), b"\xff"):
            with self.subTest(bad=bad[:20]), self.assertRaisesRegex(M.ProbeFailure, "package_identity"):
                M.validate_packages(bad)

    def test_environment_clears_loader_and_compiler_injection_without_changing_home(self):
        original = {"HOME": "/existing", "LD_PRELOAD": "private", "LD_LIBRARY_PATH": "private", "DYLD_INSERT_LIBRARIES": "private",
                    "CFLAGS": "hostile", "CC": "hostile", "PYTHONPATH": "hostile", "BASH_ENV": "hostile",
                    "PKG_CONFIG_PATH": "hostile", "PIPEWIRE_MODULE_DIR": "hostile", "MESON_ARGS": "hostile"}
        cleaned = M.clean_environment(original)
        self.assertEqual(cleaned["HOME"], original["HOME"])
        self.assertEqual(cleaned["CC"], "/usr/bin/cc")
        self.assertFalse(any("hostile" == value or "private" == value for value in cleaned.values()))
        self.assertEqual(original["LD_PRELOAD"], "private")


class ArchivePatchTests(unittest.TestCase):
    def test_archive_digest_and_size_are_checked(self):
        with self.assertRaisesRegex(M.ProbeFailure, "archive_identity"):
            M.verified_archive(b"not pinned", "original")
        with mock.patch.object(M, "ARCHIVES", {"original": ("x", M.digest(b"ok"), 1)}), self.assertRaises(M.ProbeFailure):
            M.verified_archive(b"ok", "original")

    def test_archive_member_budget_fails_before_any_extraction(self):
        payload = tar_bytes([(f"prefix/file{number}", b"x", tarfile.REGTYPE) for number in range(1601)])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(M.ProbeFailure, "archive_bound"):
                M.extract_archive(payload, root, prefix="prefix", strip_prefix=True)
            self.assertEqual(list(root.iterdir()), [])

    def test_archive_rejects_paths_links_devices_duplicates_before_writing(self):
        invalid = ["/prefix/file", "prefix/../escape", "prefix//file", "prefix/./file", "different/file", "prefix/\\escape"]
        cases = [[(path, b"x", tarfile.REGTYPE)] for path in invalid]
        cases += [[("prefix/file", b"", kind)] for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.CHRTYPE, tarfile.FIFOTYPE)]
        cases += [[("prefix/file", b"x", tarfile.REGTYPE)] * 2,
                  [("prefix/file", b"x", tarfile.REGTYPE), ("prefix/file/child", b"y", tarfile.REGTYPE)]]
        for entries in cases:
            with self.subTest(entries=entries), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                with self.assertRaises(M.ProbeFailure):
                    M.extract_archive(tar_bytes(entries), root, prefix="prefix", strip_prefix=True)
                self.assertEqual(list(root.iterdir()), [])

    def test_archive_excludes_existing_symlink_and_keeps_modes_private(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = tar_bytes([("prefix/dir/file", b"contents", tarfile.REGTYPE)])
            M.extract_archive(payload, root, prefix="prefix", strip_prefix=True)
            self.assertEqual((root / "dir/file").read_bytes(), b"contents")
            self.assertEqual((root / "dir/file").stat().st_mode & 0o777, 0o600)
            with self.assertRaises(M.ProbeFailure):
                M.extract_archive(payload, root, prefix="prefix", strip_prefix=True)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "dir").symlink_to(root, target_is_directory=True)
            payload = tar_bytes([("prefix/dir", b"", tarfile.DIRTYPE), ("prefix/dir/file", b"x", tarfile.REGTYPE)])
            with self.assertRaises(M.ProbeFailure):
                M.extract_archive(payload, root, prefix="prefix", strip_prefix=True)
            self.assertFalse((root / "file").exists())
            # Tar archives need not include explicit directory entries.
            payload = tar_bytes([("prefix/dir/file", b"x", tarfile.REGTYPE)])
            with self.assertRaises(M.ProbeFailure):
                M.extract_archive(payload, root, prefix="prefix", strip_prefix=True)
            self.assertFalse((root / "file").exists())

    def test_series_rejects_options_duplicates_and_unsafe_patch_targets(self):
        for name, patch in [("fix.patch --evil", b"--- a/x\n+++ b/x\n"),
                            ("../fix.patch", b"--- a/x\n+++ b/x\n"),
                            ("fix.patch", b"--- a/../../escape\n+++ b/../../escape\n"),
                            ("fix.patch", b"--- /absolute\n+++ /absolute\n")]:
            with self.subTest(name=name, patch=patch), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                patches = root / "debian/patches"
                patches.mkdir(parents=True)
                series = name.encode() + b"\n"
                (patches / "series").write_bytes(series)
                (patches / "fix.patch").write_bytes(patch)
                with mock.patch.object(M, "SERIES_SHA256", M.digest(series)), self.assertRaises(M.ProbeFailure):
                    M.patch_series(root)

    def test_patch_is_exact_once_and_hash_guards_both_sides(self):
        original = b"before\n" + M.ORIGINAL + b"\nafter\n"
        replaced = original.replace(M.ORIGINAL, M.REPLACEMENT)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / M.SOURCE_FILE
            source.parent.mkdir(parents=True)
            source.write_bytes(original)
            with mock.patch.object(M, "DISTRO_SOURCE_SHA256", M.digest(original)), mock.patch.object(M, "PATCHED_SOURCE_SHA256", M.digest(replaced)):
                M.apply_upstream_patch(root)
                self.assertEqual(source.read_bytes(), replaced)
                with self.assertRaisesRegex(M.ProbeFailure, "source_identity"):
                    M.apply_upstream_patch(root)
            source.write_bytes(original)
            with mock.patch.object(M, "DISTRO_SOURCE_SHA256", M.digest(original)), self.assertRaisesRegex(M.ProbeFailure, "source_identity"):
                M.apply_upstream_patch(root)
            self.assertEqual(source.read_bytes(), original)
            duplicate = original + M.ORIGINAL
            source.write_bytes(duplicate)
            with mock.patch.object(M, "DISTRO_SOURCE_SHA256", M.digest(duplicate)), self.assertRaisesRegex(M.ProbeFailure, "patch_anchor"):
                M.apply_upstream_patch(root)


class ProvenanceTests(unittest.TestCase):
    def test_elf_reads_actual_note_and_dynamic_identity(self):
        self.assertEqual(M.elf_identity(synthetic_elf(), library=True), ("61" * 20, ("libc.so.6",)))
        self.assertEqual(M.elf_identity(synthetic_elf(b"b"), library=False), ("62" * 20, ()))

    def test_elf_rejects_wrong_machine_bounds_note_soname_runpath_and_dependencies(self):
        malformed = []
        for offset, value in [(18, b"\xb7\0"), (4, b"\x01"), (5, b"\x02"), (56, b"\xff\xff"), (256, b"\xff\xff\xff\xff")]:
            data = bytearray(synthetic_elf())
            data[offset:offset + len(value)] = value
            malformed.append(bytes(data))
        malformed += [synthetic_elf()[:600], synthetic_elf(needed="libunreviewed.so"),
                      synthetic_elf(soname="other.so"), synthetic_elf(runpath=True)]
        for data in malformed:
            with self.subTest(digest=M.digest(data)), self.assertRaisesRegex(M.ProbeFailure, "elf_identity"):
                M.elf_identity(data, library=True)

    def test_object_pair_requires_only_impl_node_to_change(self):
        baseline = {"impl-node.c.o": "a", "stream.c.o": "b"}
        M.verify_object_pair(baseline, {**baseline, "impl-node.c.o": "c"})
        for changed in (baseline, {**baseline, "stream.c.o": "c"}, {"impl-node.c.o": "c"},
                        {**baseline, "impl-node.c.o": "c", "extra.c.o": "x"}):
            with self.assertRaisesRegex(M.ProbeFailure, "object_identity"):
                M.verify_object_pair(baseline, changed)

    def test_artifact_reader_rejects_symlink_and_size_overflow(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "regular"
            path.write_bytes(b"1234")
            with self.assertRaisesRegex(M.ProbeFailure, "artifact_identity"):
                M.read_regular(path, 3, "paired_provenance")
            alias = root / "alias"
            alias.symlink_to(path)
            with self.assertRaises(OSError):
                M.read_regular(alias, 10, "paired_provenance")

    def test_artifact_metadata_exact_private_schema(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            build = root / "build"
            (build / M.TARGET).parent.mkdir(parents=True)
            (build / M.TARGET).write_bytes(synthetic_elf())
            metadata, metadata_path, _ = M.artifact(build, root, "control")
            self.assertEqual(set(metadata), {"schema", "variant", "library_path", "sha256", "build_id"})
            self.assertEqual(json.loads(metadata_path.read_text()), metadata)
            path = Path(metadata["library_path"])
            self.assertEqual(path.parent, metadata_path.parent)
            self.assertEqual(path.stat().st_mode & 0o777, 0o500)
            self.assertEqual(metadata_path.stat().st_mode & 0o777, 0o600)
            self.assertEqual((path.parent / M.SONAME).readlink(), Path(M.LIBRARY))
            self.assertEqual(set(p.name for p in path.parent.iterdir()), {M.LIBRARY, M.SONAME, "artifact.json"})

    def test_loader_accepts_only_installed_root_owned_exact_merged_usr_identity(self):
        loader = Path("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2")
        metadata = SimpleNamespace(st_mode=stat.S_IFREG | 0o755, st_uid=0)
        with mock.patch.object(Path, "lstat", return_value=metadata), mock.patch.object(Path, "resolve", return_value=loader), \
                mock.patch.object(M, "read_regular", return_value=synthetic_elf()):
            for alias in (str(loader), "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"):
                run = mock.Mock(return_value=(alias + "\n").encode())
                path, _, build_id = M.loader_identity(run, "/usr/bin/dpkg-query")
                self.assertEqual(path, loader)
                self.assertEqual(build_id, "61" * 20)
            for listing in (b"/tmp/ld-linux-x86-64.so.2\n", b"", (str(loader) + "\n") .encode() * 2):
                with self.assertRaisesRegex(M.ProbeFailure, "loader_identity"):
                    M.loader_identity(mock.Mock(return_value=listing), "/usr/bin/dpkg-query")
            metadata.st_mode = stat.S_IFREG | 0o777
            with self.assertRaisesRegex(M.ProbeFailure, "loader_identity"):
                M.loader_identity(mock.Mock(), "/usr/bin/dpkg-query")


class PreparationTests(unittest.TestCase):
    @contextlib.contextmanager
    def fixture(self):
        original = b"start\n" + M.ORIGINAL + b"\nend\n"
        patched = original.replace(M.ORIGINAL, M.REPLACEMENT)
        series = b"fixture.patch\n"
        payloads = {"dsc": b"pinned dsc",
            "original": tar_bytes([("pipewire-1.0.5/" + M.SOURCE_FILE, original, tarfile.REGTYPE)]),
            "debian": tar_bytes([("debian/patches/series", series, tarfile.REGTYPE),
                                  ("debian/patches/fixture.patch", b"--- a/file\n+++ b/file\n", tarfile.REGTYPE)])}
        archives = {key: (M.ARCHIVES[key][0], M.digest(value), len(value) + 1) for key, value in payloads.items()}
        with tempfile.TemporaryDirectory() as directory, contextlib.ExitStack() as stack:
            root = Path(directory).resolve()
            stack.enter_context(mock.patch.object(M, "check_eligibility"))
            stack.enter_context(mock.patch.object(M, "trusted_tool", side_effect=lambda name: "/usr/bin/" + name))
            stack.enter_context(mock.patch.object(M, "loader_identity", return_value=(Path("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"), synthetic_elf(), "61" * 20)))
            stack.enter_context(mock.patch.object(M, "ARCHIVES", archives))
            stack.enter_context(mock.patch.object(M, "SERIES_SHA256", M.digest(series)))
            stack.enter_context(mock.patch.object(M, "DISTRO_SOURCE_SHA256", M.digest(original)))
            stack.enter_context(mock.patch.object(M, "PATCHED_SOURCE_SHA256", M.digest(patched)))
            calls = []
            def run(stage, argv, **options):
                calls.append((stage, argv, options))
                if stage == "packages":
                    return package_output()
                if stage == "download":
                    return next(payloads[key] for key in payloads if argv[-1].endswith(archives[key][0]))
                build = root / "pipewire-client-probe/build"
                if stage == "configure":
                    build.mkdir()
                    (build / "config.h").write_bytes(b"identical config")
                    (build / "build.ninja").write_bytes(b"identical recipe")
                if stage in ("control_build", "patched_build"):
                    (build / M.TARGET).parent.mkdir(parents=True, exist_ok=True)
                    (build / M.TARGET).write_bytes(synthetic_elf(b"a" if stage == "control_build" else b"b"))
                    objects = build / (M.TARGET + ".p")
                    objects.mkdir(exist_ok=True)
                    for name in M.SOURCES:
                        value = (name + (" patched" if name == "impl-node" and stage == "patched_build" else "")).encode()
                        (objects / (name + ".c.o")).write_bytes(value)
                return b""
            yield root, run, calls

    def test_mocked_pair_runs_all_patches_then_same_target_and_preserves_provenance(self):
        with self.fixture() as (root, run, calls), mock.patch.dict(os.environ, {"LD_PRELOAD": "secret", "CFLAGS": "secret"}):
            pair = M.prepare_clients(root, run)
            self.assertEqual(pair.control["variant"], "control")
            self.assertEqual(pair.patched["variant"], "patched")
            self.assertNotEqual(pair.control["sha256"], pair.patched["sha256"])
            self.assertEqual(pair.facts(), {"eligible": True, "prepared": True, "stage": "complete"})
            stages = [stage for stage, _, _ in calls]
            self.assertLess(stages.index("distro_patch"), stages.index("configure"))
            builds = [(argv, options) for stage, argv, options in calls if stage.endswith("_build")]
            self.assertEqual(builds[0][0], builds[1][0])
            self.assertEqual(builds[0][1]["env"], builds[1][1]["env"])
            self.assertEqual(builds[0][0][-1], M.TARGET)
            for _, argv, options in calls:
                self.assertTrue(argv[0].startswith("/usr/bin/"))
                self.assertNotIn("install", argv)
                self.assertNotIn("ldconfig", argv)
                self.assertNotIn("secret", options["env"].values())
                self.assertLessEqual(options["timeout"], 300)
                self.assertGreater(options["output_limit"], 0)
            self.assertEqual(pair.provenance["control_objects"]["stream.c.o"], pair.provenance["patched_objects"]["stream.c.o"])

    def test_command_errors_are_fixed_and_do_not_expose_argv_or_payload(self):
        for error, reason in [(subprocess.CalledProcessError(7, ["sensitive-argv"], output=b"sensitive-body"), "command"),
                              (subprocess.TimeoutExpired(["sensitive-argv"], 1), "deadline")]:
            with self.fixture() as (root, _, _):
                with self.assertRaises(M.ProbeFailure) as caught:
                    M.prepare_clients(root, mock.Mock(side_effect=error))
                self.assertEqual(caught.exception.facts()["failure"], reason)
                self.assertNotIn("sensitive", str(caught.exception))
                self.assertNotIn("sensitive", json.dumps(caught.exception.facts()))
                self.assertFalse((root / "pipewire-client-probe").exists())

    def test_callback_output_bound_and_ineligibility_fail_before_download(self):
        with self.fixture() as (root, _, _):
            with self.assertRaisesRegex(M.ProbeFailure, "output_bound"):
                M.prepare_clients(root, mock.Mock(return_value=b"x" * (64 * 1024 + 1)))
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(M, "check_eligibility", side_effect=M.ProbeFailure("eligibility", "ineligible")):
            run = mock.Mock()
            with self.assertRaisesRegex(M.ProbeFailure, "ineligible"):
                M.prepare_clients(Path(directory), run)
            run.assert_not_called()

    def test_total_deadline_prevents_next_command(self):
        with self.fixture() as (root, _, _), mock.patch.object(M.time, "monotonic", side_effect=[1, 902]):
            callback = mock.Mock()
            with self.assertRaisesRegex(M.ProbeFailure, "deadline"):
                M.prepare_clients(root, callback)
            callback.assert_not_called()

    def test_configuration_or_unrelated_object_change_rejects_pair(self):
        for change in ("config.h", "object"):
            with self.subTest(change=change), self.fixture() as (root, run, _):
                def changed(stage, argv, **options):
                    output = run(stage, argv, **options)
                    if stage == "patched_build":
                        build = root / "pipewire-client-probe/build"
                        target = build / "config.h" if change == "config.h" else build / (M.TARGET + ".p") / "stream.c.o"
                        target.write_bytes(b"unexpected difference")
                    return output
                with self.assertRaisesRegex(M.ProbeFailure, "configuration_changed|object_identity"):
                    M.prepare_clients(root, changed)

    def test_failure_facts_reject_arbitrary_fields_and_boolean_status(self):
        failure = M.ProbeFailure("secret-stage", "secret-reason", True)
        self.assertEqual(failure.facts(), {"eligible": True, "prepared": False, "stage": "eligibility", "failure": "unexpected"})
        self.assertNotIn("status", M.ProbeFailure("configure", "command", 9999).facts())


if __name__ == "__main__":
    unittest.main()
