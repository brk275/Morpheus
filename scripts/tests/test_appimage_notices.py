"""Inventory checks with mocked ELF tooling; no download.

Fixtures reproduce GNU readelf --wide --dynamic --notes output. The single
exception to mocking is SystemReadelfTests, which runs only on a fresh
GitHub-hosted Ubuntu 24.04 runner and checks the genuine installed readelf
against the parser on a trusted system ELF.
"""

import os
from pathlib import Path
import platform
import re
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "packaging/linux"))
import appimage_notices as N


def elf_bytes(marker=b"x"):
    data = bytearray(64)
    data[:7] = b"\x7fELF\x02\x01\x01"
    data[16:18] = (3).to_bytes(2, "little")
    data[18:20] = (62).to_bytes(2, "little")
    return bytes(data) + marker


def record(name=None, needed=(), marker="a", content=None):
    return {"build_id": marker * 40, "needed": sorted(needed), "soname": name,
            "search": [], "sha256": N.digest(content or elf_bytes(marker.encode()))}


LABELS = ("inventory-elf", "inventory-elf-client", "inventory-elf-dependency", "inventory-elf-deployed")
NOTE_HEADER = "  Owner                Data size \tDescription"


def build_id_line(build_id, size=None, owner="GNU"):
    """One note line exactly as GNU readelf --wide --notes prints it."""
    size = len(build_id) // 2 if size is None else size
    return (f"  {owner:<20} 0x{size:08x}\tNT_GNU_BUILD_ID (unique build ID bitstring)"
            f"\t    Build ID: {build_id}")


def wide_report(build_ids=("a" * 40,), needed=("libc.so.6",), soname=None, runpath=None,
                build_lines=None):
    """A GNU readelf --wide --dynamic --notes report for one x86-64 object."""
    lines = ["", "Dynamic section at offset 0x2d8c0 contains 27 entries:",
             "  Tag        Type                         Name/Value"]
    lines += [f" 0x0000000000000001 (NEEDED)             Shared library: [{name}]" for name in needed]
    if soname is not None:
        lines.append(f" 0x000000000000000e (SONAME)             Library soname: [{soname}]")
    if runpath is not None:
        lines.append(f" 0x000000000000001d (RUNPATH)            Library runpath: [{runpath}]")
    lines += [" 0x000000000000000c (INIT)               0x4000",
              " 0x0000000000000000 (NULL)               0x0",
              "", "Displaying notes found in: .note.gnu.property", NOTE_HEADER,
              "  GNU                  0x00000020\tNT_GNU_PROPERTY_TYPE_0\t      Properties: x86 feature: IBT, SHSTK",
              "\tx86 ISA needed: x86-64-baseline"]
    for line in (build_lines if build_lines is not None else [build_id_line(i) for i in build_ids]):
        lines += ["", "Displaying notes found in: .note.gnu.build-id", NOTE_HEADER, line]
    lines += ["", "Displaying notes found in: .note.ABI-tag", NOTE_HEADER,
              "  GNU                  0x00000010\tNT_GNU_ABI_TAG (ABI version tag)\t    OS: Linux, ABI: 3.2.0"]
    return "\n".join(lines) + "\n"


class ParseTests(unittest.TestCase):
    def test_wide_report_yields_exact_identity_needed_soname_and_search(self):
        self.assertEqual(N.parse_elf_report(wide_report()), {
            "build_id": "a" * 40, "needed": ["libc.so.6"], "soname": None, "search": [],
        })
        library = wide_report(build_ids=("0123456789abcdef",), needed=("libm.so.6", "libc.so.6"),
                              soname="libsample.so.1", runpath="$ORIGIN")
        self.assertEqual(N.parse_elf_report(library), {
            "build_id": "0123456789abcdef", "needed": ["libc.so.6", "libm.so.6"],
            "soname": "libsample.so.1", "search": ["$ORIGIN"],
        })

    def test_note_separators_are_matched_structurally(self):
        line = build_id_line("c" * 40)
        # readelf pads the owner to 20 columns, then prints " 0x<size>".
        unpadded = line.replace("GNU" + " " * 17 + " 0x", "GNU\t0x")
        self.assertNotEqual(unpadded, line)
        for variant in (line.replace("\t", " "), line.replace("\t", "\t\t"), unpadded, line + " \t"):
            with self.subTest(variant=variant):
                report = wide_report(build_lines=[variant])
                self.assertEqual(N.parse_elf_report(report)["build_id"], "c" * 40)

    def test_build_id_bounds_are_inclusive_at_four_and_sixty_four_bytes(self):
        for build_id in ("ab" * 4, "cd" * 64):
            with self.subTest(length=len(build_id)):
                self.assertEqual(N.parse_build_id(wide_report(build_ids=(build_id,))), build_id)

    def test_build_id_must_be_one_well_formed_gnu_note_in_the_wide_layout(self):
        good = build_id_line("a" * 40)
        own_line = (f"  GNU                  0x00000014\tNT_GNU_BUILD_ID (unique build ID bitstring)\n"
                    f"    Build ID: {'a' * 40}")
        cases = {
            "missing": wide_report(build_ids=()),
            "duplicate": wide_report(build_ids=("a" * 40, "b" * 40)),
            "duplicate_identical": wide_report(build_ids=("a" * 40, "a" * 40)),
            "non_wide_layout": wide_report(build_lines=[own_line]),
            "size_too_large": wide_report(build_lines=[build_id_line("a" * 32, size=20)]),
            "size_too_small": wide_report(build_lines=[build_id_line("a" * 40, size=8)]),
            "odd_length": wide_report(build_lines=[build_id_line("a" * 41, size=20)]),
            "too_short": wide_report(build_ids=("ab" * 3,)),
            "too_long": wide_report(build_ids=("ab" * 65,)),
            "uppercase": wide_report(build_ids=("AB" * 20,)),
            "non_hex": wide_report(build_lines=[build_id_line("a" * 38 + "zz", size=20)]),
            "trailing_text": wide_report(build_lines=[good + " extra"]),
            "other_owner": wide_report(build_lines=[build_id_line("a" * 40, owner="FDO")]),
            "other_type": wide_report(build_lines=[good.replace("NT_GNU_BUILD_ID", "NT_GNU_HWCAP")]),
            "stray_text_line": wide_report() + "    Build ID: " + "b" * 40 + "\n",
            "stray_in_other_note": wide_report().replace("OS: Linux", "Build ID: Linux"),
            "stray_type_without_id": wide_report() + build_id_line("b" * 40).split("\t    ")[0] + "\n",
        }
        for case, report in cases.items():
            with self.subTest(case=case), self.assertRaises(N.InventoryFailure) as caught:
                N.parse_elf_report(report)
            self.assertEqual(caught.exception.reason, "elf_build_id")

    def test_dynamic_checks_still_reject_with_their_own_reasons(self):
        cases = [
            (wide_report(needed=("../private.so",)), "elf_needed"),
            (wide_report(needed=("libc.so.6", "libc.so.6")), "elf_needed"),
            (wide_report(needed=tuple(f"lib{n}.so" for n in range(129))), "elf_needed"),
            (wide_report(soname="liba.so").replace(
                " 0x000000000000000c (INIT)",
                " 0x000000000000000e (SONAME)             Library soname: [liba.so]\n"
                " 0x000000000000000c (INIT)"), "elf_soname"),
            (wide_report(soname="../private.so"), "elf_soname"),
            (wide_report(runpath="$ORIGIN").replace(
                " 0x000000000000000c (INIT)",
                " 0x000000000000000f (RPATH)              Library rpath: [$ORIGIN]\n"
                " 0x000000000000000c (INIT)"), "elf_search"),
        ]
        for report, reason in cases:
            with self.subTest(reason=reason), self.assertRaises(N.InventoryFailure) as caught:
                N.parse_elf_report(report)
            self.assertEqual(caught.exception.reason, reason)
            self.assertNotIn("private", str(caught.exception))

    def test_build_id_is_checked_before_dynamic_entries(self):
        with self.assertRaises(N.InventoryFailure) as caught:
            N.parse_elf_report(wide_report(build_ids=(), needed=("../private.so",)))
        self.assertEqual(caught.exception.reason, "elf_build_id")

    def test_elf_role_labels_are_closed_and_never_execute_inspected_objects(self):
        runner = mock.Mock()
        runner.run.return_value = wide_report().encode()
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            path = root / "object"
            path.write_bytes(elf_bytes())
            with mock.patch.object(N, "root_owned", return_value=Path("/usr/bin/readelf")):
                for label in LABELS:
                    result = N.elf_record(runner, path, root, {}, label)
                    self.assertEqual(result["build_id"], "a" * 40)
                    self.assertEqual(runner.run.call_args.args[0], label)
                    self.assertEqual(runner.run.call_args.args[1],
                                     ["/usr/bin/readelf", "--wide", "--dynamic", "--notes", str(path)])
                runner.reset_mock()
                for label in ("private-object-name", [], None):
                    with self.assertRaises(N.InventoryFailure):
                        N.elf_record(runner, path, root, {}, label)
                runner.run.assert_not_called()

    def test_package_source_and_version_must_be_exact_bounded_fields(self):
        source = "libsample1:amd64\t1.2-3ubuntu4\tsample\t1.2-3ubuntu4\n"
        self.assertEqual(N.parse_package_record(source, "libsample1:amd64")["source_package"], "sample")
        for bad in (source * 2, source.replace("\tsample\t", "\t../sample\t"),
                    source.replace("\tsample\t", "\t\t"), source.replace("1.2-3ubuntu4", "bad value")):
            with self.subTest(bad=bad), self.assertRaises(N.InventoryFailure):
                N.parse_package_record(bad, "libsample1:amd64")
        with self.assertRaises(N.InventoryFailure):
            N.parse_package_record(source, "another-package")

    def test_wrong_architecture_rejected_before_any_tool_is_called(self):
        runner = mock.Mock()
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "file"
            data = bytearray(elf_bytes()); data[18:20] = (183).to_bytes(2, "little")
            path.write_bytes(data)
            with self.assertRaises(N.InventoryFailure):
                N.elf_record(runner, path, Path(folder), {})
        runner.run.assert_not_called()


class PathTests(unittest.TestCase):
    def test_internal_file_symlink_is_allowed_and_external_symlink_rejected(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as outside:
            root = Path(folder).resolve()
            target = root / "actual"; target.write_bytes(b"test"); target.chmod(0o600)
            link = root / "alias"; link.symlink_to("actual")
            self.assertEqual(list(N.walk_regular_files(root)), [target])
            link.unlink()
            external = Path(outside).resolve() / "file"; external.write_bytes(b"outside")
            link.symlink_to(external)
            with self.assertRaises(N.InventoryFailure):
                list(N.walk_regular_files(root))

    def test_writable_hardlinked_or_symlinked_directory_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); path = root / "file"; path.write_bytes(b"a")
            path.chmod(0o666)
            with self.assertRaises(N.InventoryFailure): list(N.walk_regular_files(root))
            path.chmod(0o600); os.link(path, root / "hardlink")
            with self.assertRaises(N.InventoryFailure): list(N.walk_regular_files(root))
            (root / "hardlink").unlink(); (root / "actual-dir").mkdir(mode=0o700)
            (root / "dir-link").symlink_to("actual-dir", target_is_directory=True)
            with self.assertRaises(N.InventoryFailure): list(N.walk_regular_files(root))

    def test_runpath_must_resolve_inside_appdir(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            (root / "usr/bin").mkdir(parents=True); (root / "usr/lib").mkdir()
            path = root / "usr/bin/morpheus-app"
            N.validate_search({"search": ["$ORIGIN/../lib"]}, path, root)
            for entry in ("", "/usr/lib", "$ORIGIN/../../..", "$ORIGIN/$LIB", "./lib"):
                with self.subTest(entry=entry), self.assertRaises(N.InventoryFailure):
                    N.validate_search({"search": [entry]}, path, root)

    def test_library_resolution_does_not_guess_between_different_providers(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); dirs = (root / "a", root / "b")
            for directory in dirs:
                directory.mkdir(); (directory / "libx.so").write_bytes(b"ELF placeholder")
            with mock.patch.object(N, "SYSTEM_DIRS", dirs), mock.patch.object(N, "root_owned", side_effect=lambda p: p.resolve()):
                with self.assertRaises(N.InventoryFailure): N.resolve_library("libx.so")
                (dirs[1] / "libx.so").unlink()
                (dirs[1] / "libx.so").symlink_to(dirs[0] / "libx.so")
                self.assertEqual(N.resolve_library("libx.so"), dirs[0] / "libx.so")
                with self.assertRaises(N.InventoryFailure): N.resolve_library("missing.so")
                with self.assertRaises(N.InventoryFailure): N.resolve_library("../libx.so")


class DeploymentTests(unittest.TestCase):
    def fixture(self, root):
        main = root / "usr/bin/morpheus-app"
        provider = root / N.client.DESTINATION
        notice = root / N.NOTICE_ROOT / "sample.copyright"
        for path in (main, provider, notice): path.parent.mkdir(parents=True, exist_ok=True)
        main.write_bytes(elf_bytes(b"main")); main.chmod(0o700)
        provider.write_bytes(elf_bytes(b"pipewire")); provider.chmod(0o600)
        notice.write_bytes(b"Exact copyright and license notice"); notice.chmod(0o600)
        launcher = root / "AppRun"; launcher.write_bytes(b"#!/bin/sh\nexec app\n"); launcher.chmod(0o700)
        records = {
            main: record(needed=[N.client.SONAME], content=main.read_bytes()),
            provider: record(N.client.SONAME, ["libc.so.6"], marker="b", content=provider.read_bytes()),
        }
        prepared = {"components": {
            "morpheus-app": {**records[main], "kind": "project"},
            N.client.SONAME: {**records[provider], "kind": "pipewire", "source_version": "1.0.5-test"},
            "libc.so.6": {**record("libc.so.6", marker="c"), "kind": "system-package"},
        }, "notices": {notice.relative_to(root).as_posix(): N.digest(notice.read_bytes())},
            "launcher_sha256": N.digest(launcher.read_bytes()), "runtime_pin": {"sha256": "d" * 64},
            "runtime_sources": {}, "runtime_source_patches": {}}
        shipped = {"relative_path": N.client.DESTINATION, "sha256": records[provider]["sha256"]}
        return records, prepared, shipped

    def test_verified_qa_inventory_still_cannot_authorize_distribution(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); records, prepared, shipped = self.fixture(root)
            with mock.patch.object(N, "elf_record", side_effect=lambda runner, p, *_: records[p]):
                result = N.verify(mock.Mock(), root, prepared, shipped, {})
            self.assertFalse(result["distribution_ready"])
            self.assertFalse(result["identity_is_reproducible_build_proof"])
            self.assertEqual(result["external_dependencies"], ["libc.so.6"])
            self.assertEqual(len(result["unresolved_release_inputs"]), 3)
            self.assertNotIn(str(root), str(result))

    def test_unknown_or_changed_elf_cannot_enter_inventory(self):
        for mutation in ("unknown", "build_id", "needed", "missing_provider"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as folder:
                root = Path(folder).resolve(); records, prepared, shipped = self.fixture(root)
                main = root / "usr/bin/morpheus-app"
                if mutation == "unknown":
                    unknown = root / "usr/lib/libunrecorded.so"
                    unknown.write_bytes(elf_bytes(b"unknown")); records[unknown] = record("libunrecorded.so")
                elif mutation == "missing_provider":
                    (root / N.client.DESTINATION).unlink()
                else:
                    records[main] = {**records[main], mutation: "b" * 40 if mutation == "build_id" else ["other.so"]}
                with mock.patch.object(N, "elf_record", side_effect=lambda runner, p, *_: records[p]), self.assertRaises(N.InventoryFailure):
                    N.verify(mock.Mock(), root, prepared, shipped, {})

    def test_notice_or_launcher_mutation_fails(self):
        for name in ("AppRun", N.NOTICE_ROOT + "/sample.copyright"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as folder:
                root = Path(folder).resolve(); records, prepared, shipped = self.fixture(root)
                (root / name).write_bytes(b"modified")
                with mock.patch.object(N, "elf_record", side_effect=lambda runner, p, *_: records[p]), self.assertRaises(N.InventoryFailure):
                    N.verify(mock.Mock(), root, prepared, shipped, {})

    def test_deployed_label_parses_wide_reports_through_the_real_record_path(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); records, prepared, shipped = self.fixture(root)
            main = root / "usr/bin/morpheus-app"
            provider = root / N.client.DESTINATION
            reports = {
                main: wide_report(build_ids=("a" * 40,), needed=(N.client.SONAME,)),
                provider: wide_report(build_ids=("b" * 40,), needed=("libc.so.6",),
                                      soname=N.client.SONAME, runpath="$ORIGIN"),
            }
            runner = mock.Mock()
            runner.run.side_effect = lambda label, argv, **_: reports[Path(argv[-1])].encode()
            with mock.patch.object(N, "root_owned", return_value=Path("/usr/bin/readelf")):
                result = N.verify(runner, root, prepared, shipped, {})
            self.assertEqual({call.args[0] for call in runner.run.call_args_list}, {"inventory-elf-deployed"})
            self.assertEqual({item["build_id"] for item in result["deployed"]}, {"a" * 40, "b" * 40})
            reports[main] = wide_report(build_ids=("a" * 40, "a" * 40), needed=(N.client.SONAME,))
            with (mock.patch.object(N, "root_owned", return_value=Path("/usr/bin/readelf")),
                  self.assertRaises(N.InventoryFailure) as caught):
                N.verify(runner, root, prepared, shipped, {})
            self.assertEqual(caught.exception.reason, "elf_build_id")

    def test_recorded_post_patchelf_sha_is_distinct_from_source_hash(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve(); records, prepared, shipped = self.fixture(root)
            main = root / "usr/bin/morpheus-app"
            records[main] = {**records[main], "sha256": "f" * 64}
            with mock.patch.object(N, "elf_record", side_effect=lambda runner, p, *_: records[p]):
                result = N.verify(mock.Mock(), root, prepared, shipped, {})
            item = next(item for item in result["deployed"] if item["kind"] == "project")
            self.assertNotEqual(item["source_sha256"], item["deployed_sha256"])
            self.assertFalse(result["identity_is_reproducible_build_proof"])


class PreparationTests(unittest.TestCase):
    def test_absent_usrmerge_alias_is_expected_but_owner_metadata_is_exact(self):
        runner = mock.Mock()
        runner.run.side_effect = [
            b"", b"libsample1:amd64: /lib/x86_64-linux-gnu/libsample.so.1\n",
            b"libsample1:amd64\t1.2-3\tsample\t1.2-3\n",
        ]
        with (mock.patch.object(N, "root_owned", side_effect=Path),
              mock.patch.object(N, "regular", return_value=b"Exact copyright")):
            package, copyright_text, common = N.package_record(
                runner, Path("/usr/lib/x86_64-linux-gnu/libsample.so.1"), Path("/private"), {})
        self.assertEqual(package["source_package"], "sample")
        self.assertEqual(copyright_text, b"Exact copyright")
        self.assertEqual(common, {})
        self.assertEqual([call.kwargs["expected_status"] for call in runner.run.call_args_list],
                         [(0, 1), (0, 1), (0,)])

    def test_ownership_failures_never_fall_back_to_another_alias(self):
        from appimage_process import CommandFailure
        for reason in ("cleanup", "deadline", "pipe", "output", "exit"):
            runner = mock.Mock()
            failure = CommandFailure(reason)
            runner.run.side_effect = failure
            with (self.subTest(reason=reason),
                  mock.patch.object(N, "root_owned", side_effect=Path),
                  self.assertRaises(CommandFailure) as caught):
                N.package_record(runner, Path("/usr/lib/x86_64-linux-gnu/libsample.so.1"),
                                 Path("/private"), {})
            self.assertIs(caught.exception, failure)
            self.assertEqual(runner.run.call_count, 1)

    def test_graph_is_bounded_deduplicated_and_notices_are_immutable_copies(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as checkout:
            root = Path(folder).resolve(); project = Path(checkout).resolve()
            (project / "LICENSE").write_bytes(b"Morpheus original code license")
            stage = root / "stage"; stage.mkdir()
            (stage / "COPYING").write_bytes(b"PipeWire license")
            binary = root / "binary"
            pipewire = stage / N.client.SONAME
            system = root / "system-lib"
            infos = {
                binary: record(needed=[N.client.SONAME, "libsample.so.1"]),
                pipewire: record(N.client.SONAME, ["libsample.so.1"], marker="b"),
                system: record("libsample.so.1", ["libsample.so.1"], marker="c"),
            }
            source = {"source_client_sha256": infos[pipewire]["sha256"],
                      "build_id": infos[pipewire]["build_id"], "source_version": "1.0.5-test"}
            config = {"appimage": {"files": {str(stage / "COPYING"): N.client.NOTICE_DIR + "/COPYING"}}}
            package = {"package": "libsample1:amd64", "version": "1.2-3", "source_package": "sample",
                       "source_version": "1.2-3"}
            with (mock.patch.object(N.client, "ROOT", project),
                  mock.patch.object(N, "elf_record", side_effect=lambda runner, path, *_: infos[path]),
                  mock.patch.object(N, "resolve_library", return_value=system) as resolver,
                  mock.patch.object(N, "package_record", return_value=(package, b"Exact package copyright", {"MIT": b"Common license text"}))):
                prepared = N.prepare(mock.Mock(), root, binary, stage, source, config, {})
            self.assertEqual(set(prepared["components"]), {"morpheus-app", N.client.SONAME, "libsample.so.1"})
            resolver.assert_called_once_with("libsample.so.1")
            self.assertEqual(len(prepared["notices"]), 4)
            for path, destination in config["appimage"]["files"].items():
                self.assertEqual(N.digest(Path(path).read_bytes()), prepared["notices"][destination])
            self.assertNotIn(str(root), str(prepared))

    def test_main_client_and_dependency_labels_parse_wide_reports_through_prepare(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as checkout:
            root = Path(folder).resolve(); project = Path(checkout).resolve()
            (project / "LICENSE").write_bytes(b"Morpheus original code license")
            stage = root / "stage"; stage.mkdir()
            binary = root / "binary"; binary.write_bytes(elf_bytes(b"main"))
            pipewire = stage / N.client.SONAME; pipewire.write_bytes(elf_bytes(b"pipewire"))
            system = root / "system-lib"; system.write_bytes(elf_bytes(b"system"))
            reports = {
                binary: wide_report(build_ids=("a" * 40,), needed=(N.client.SONAME, "libsample.so.1")),
                pipewire: wide_report(build_ids=("b" * 40,), needed=("libsample.so.1",),
                                      soname=N.client.SONAME),
                system: wide_report(build_ids=("0123456789abcdef",), needed=(),
                                    soname="libsample.so.1"),
            }
            runner = mock.Mock()
            runner.run.side_effect = lambda label, argv, **_: reports[Path(argv[-1])].encode()
            source = {"source_client_sha256": N.digest(pipewire.read_bytes()), "build_id": "b" * 40,
                      "source_version": "1.0.5-test"}
            package = {"package": "libsample1:amd64", "version": "1.2-3", "source_package": "sample",
                       "source_version": "1.2-3"}
            with (mock.patch.object(N.client, "ROOT", project),
                  mock.patch.object(N, "root_owned", return_value=Path("/usr/bin/readelf")),
                  mock.patch.object(N, "resolve_library", return_value=system),
                  mock.patch.object(N, "package_record", return_value=(package, b"Exact package copyright", {}))):
                prepared = N.prepare(runner, root, binary, stage, source, {"appimage": {"files": {}}}, {},
                                     runtime_metadata={"schema": 1, "fresh_private_prefix": True,
                                         "malformedtest_returned_zero": True,
                                         "upstream_runtime_binary_downloaded": False,
                                         "runtime": {"sha256": "c" * 64}, "sources": {}, "patches": {}})
            labels = [call.args[0] for call in runner.run.call_args_list]
            self.assertEqual(labels[0], "inventory-elf")
            self.assertEqual(sorted(labels), ["inventory-elf", "inventory-elf-client", "inventory-elf-dependency"])
            components = prepared["components"]
            self.assertEqual(components["morpheus-app"]["build_id"], "a" * 40)
            self.assertEqual(components[N.client.SONAME]["build_id"], "b" * 40)
            self.assertEqual(components["libsample.so.1"]["build_id"], "0123456789abcdef")

    def test_notice_destination_collision_cannot_overwrite_existing_mapping(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            destination = N.NOTICE_ROOT + "/copyright"
            config = {"appimage": {"files": {"/different/source": destination}}}
            with self.assertRaises(N.InventoryFailure):
                N.add_notice(root, config, {}, "copyright", b"new content")
            self.assertEqual(config["appimage"]["files"], {"/different/source": destination})
            self.assertEqual(list(root.iterdir()), [])


def fresh_hosted_ubuntu():
    """A standard GitHub-hosted Ubuntu 24.04 x86-64 runner, where the
    installed GNU readelf is the same distribution build the packaging runs."""
    expected = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                "RUNNER_OS": "Linux", "ImageOS": "ubuntu24"}
    if (sys.platform != "linux" or platform.machine() != "x86_64"
            or any(os.environ.get(name) != value for name, value in expected.items())):
        return False
    try:
        release = Path("/etc/os-release").read_text()
    except OSError:
        return False
    fields = dict(re.findall(r'^([A-Z_]+)="?([^"\n]+)"?$', release, re.MULTILINE))
    return fields.get("ID") == "ubuntu" and fields.get("VERSION_ID") == "24.04"


class SystemRunner:
    """Run exactly the inventory's argv within its bounds: no shell, no stdin."""

    def __init__(self):
        self.calls = []

    def run(self, label, argv, *, cwd, env, timeout, output_limit, expected_status):
        result = subprocess.run(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=timeout, check=False)
        self.calls.append((label, list(argv), result.stdout))
        if result.returncode not in expected_status or len(result.stdout) > output_limit:
            raise AssertionError("installed readelf did not complete within the inventory bounds")
        return result.stdout


@unittest.skipUnless(fresh_hosted_ubuntu(), "genuine readelf output is checked only on a fresh "
                     "GitHub-hosted Ubuntu 24.04 runner")
class SystemReadelfTests(unittest.TestCase):
    def test_installed_readelf_wide_report_of_a_trusted_system_elf_parses(self):
        # A trusted distribution ELF: the producer/parser boundary is checked
        # on genuine output instead of a fixture copied from the parser.
        target = Path("/usr/bin/true")
        data = target.read_bytes()
        runner = SystemRunner()
        with tempfile.TemporaryDirectory() as folder:
            result = N.elf_record(runner, target, Path(folder), N.client.R.clean_environment({}))
        self.assertEqual(len(runner.calls), 1)
        label, argv, output = runner.calls[0]
        self.assertEqual(label, "inventory-elf")
        self.assertEqual(argv[1:], ["--wide", "--dynamic", "--notes", str(target)])
        self.assertEqual(result["sha256"], N.digest(data))
        self.assertIn("libc.so.6", result["needed"])
        self.assertIsNone(result["soname"])
        self.assertEqual(result["search"], [])
        # The genuine wide layout puts the description on the note's own line,
        # which is the shape the fixtures above reproduce.
        report = output.decode("utf-8")
        lines = [line for line in report.splitlines() if "Build ID:" in line]
        self.assertEqual(len(lines), 1)
        self.assertIn("NT_GNU_BUILD_ID", lines[0])
        # Independent check of the parsed value against the ELF bytes: the
        # exact GNU build-ID note record occurs once in the file.
        build_id = bytes.fromhex(result["build_id"])
        note = struct.pack("<III", 4, len(build_id), 3) + b"GNU\0" + build_id
        self.assertEqual(data.count(note), 1)


if __name__ == "__main__":
    unittest.main()
