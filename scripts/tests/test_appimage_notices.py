"""Pure inventory checks: mocked ELF tooling; no native command or download."""

import os
from pathlib import Path
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


class ParseTests(unittest.TestCase):
    def test_exact_elf_identity_and_needed_without_target_execution(self):
        text = ("  Build ID: " + "a" * 40 + "\n"
                " 0 (NEEDED) Shared library: [libc.so.6]\n"
                " 0 (SONAME) Library soname: [libsample.so.1]\n"
                " 0 (RUNPATH) Library runpath: [$ORIGIN]\n")
        self.assertEqual(N.parse_elf_report(text), {
            "build_id": "a" * 40, "needed": ["libc.so.6"],
            "soname": "libsample.so.1", "search": ["$ORIGIN"],
        })
        for bad in (text + "Build ID: " + "b" * 40 + "\n",
                    text + "0 (NEEDED) Shared library: [libc.so.6]\n",
                    text.replace("libc.so.6", "../other.so"),
                    text.replace("a" * 40, "bad")):
            with self.subTest(bad=bad), self.assertRaises(N.InventoryFailure):
                N.parse_elf_report(bad)

    def test_elf_failures_identify_the_rejected_check_without_report_contents(self):
        good = "Build ID: " + "a" * 40 + "\n"
        cases = [
            ("", "elf_build_id"), (good + good, "elf_build_id"),
            ("Build ID: abc\n", "elf_build_id"),
            (good + "0 (NEEDED) Shared library: [../private.so]\n", "elf_needed"),
            (good + "0 (NEEDED) Shared library: [libc.so.6]\n" * 2, "elf_needed"),
            (good + "0 (SONAME) Library soname: [liba.so]\n" * 2, "elf_soname"),
            (good + "0 (SONAME) Library soname: [../private.so]\n", "elf_soname"),
            (good + "0 (RPATH) Library rpath: [$ORIGIN]\n0 (RUNPATH) Library runpath: [$ORIGIN]\n", "elf_search"),
        ]
        for report, reason in cases:
            with self.subTest(reason=reason), self.assertRaises(N.InventoryFailure) as caught:
                N.parse_elf_report(report)
            self.assertEqual(caught.exception.reason, reason)
            self.assertNotIn("private", str(caught.exception))

    def test_elf_role_labels_are_closed_and_never_execute_inspected_objects(self):
        runner = mock.Mock()
        runner.run.return_value = b"Build ID: " + b"a" * 40 + b"\n"
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            path = root / "object"
            path.write_bytes(elf_bytes())
            with mock.patch.object(N, "root_owned", return_value=Path("/usr/bin/readelf")):
                for label in ("inventory-elf", "inventory-elf-client", "inventory-elf-dependency", "inventory-elf-deployed"):
                    N.elf_record(runner, path, root, {}, label)
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
            "launcher_sha256": N.digest(launcher.read_bytes()), "runtime_pin": {"sha256": "d" * 64}}
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

    def test_notice_destination_collision_cannot_overwrite_existing_mapping(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            destination = N.NOTICE_ROOT + "/copyright"
            config = {"appimage": {"files": {"/different/source": destination}}}
            with self.assertRaises(N.InventoryFailure):
                N.add_notice(root, config, {}, "copyright", b"new content")
            self.assertEqual(config["appimage"]["files"], {"/different/source": destination})
            self.assertEqual(list(root.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
