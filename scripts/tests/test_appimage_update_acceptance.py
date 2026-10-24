import contextlib
import io
import json
import os
from pathlib import Path
import struct
import sys
import tempfile
import tarfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import appimage_update_acceptance as target


def image(path, version="0.1.0-updateqa.2"):
    header = bytearray(64)
    header[:6] = b"\x7fELF\x02\x01"
    header[8:11] = b"AI\x02"
    header[18:20] = b"\x3e\x00"
    struct.pack_into("<H", header, 54, 56)
    metadata = {"protocol": 2, "product": "io.github.brk275.morpheus", "version": version,
                "arch": "x86_64", "persistence": "a" * 64}
    footer = json.dumps(metadata).encode()
    path.write_bytes(header + b"squashfs-fixture-bytes" + footer + struct.pack("<I", len(footer)) + target.FOOTER)
    return metadata


class AppImageAcceptanceTests(unittest.TestCase):
    def test_published_identity_is_bounded_exact_and_distinguishes_negative_payload(self):
        artifacts = [{"version": f"0.1.0-updateqa.{i}", "sha256": str(i) * 64, "persistence": "a" * 64} for i in (1, 2)]
        record = target.identity_record("b" * 40, artifacts, "c" * 64, "d" * 64)
        self.assertEqual(record["source_revision"], "b" * 40)
        self.assertEqual(record["genuine_new"], artifacts[1])
        self.assertEqual(record["signed_invalid_elf_fixture_sha256"], "c" * 64)
        artifacts[0]["private_path"] = "private-value"
        with self.assertRaises(RuntimeError):
            target.identity_record("b" * 40, artifacts, "c" * 64, "d" * 64)

    def test_hosted_guard_requires_explicit_free_linux_hosted_context(self):
        env = {"GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux", "RUNNER_ARCH": "X64",
               "RUNNER_ENVIRONMENT": "github-hosted", "MORPHEUS_FREE_HOSTED_UPDATE_QA": "1"}
        with mock.patch.dict(os.environ, env, clear=True), mock.patch.object(target.sys, "platform", "linux"), \
                mock.patch.object(target.platform, "machine", return_value="x86_64"), \
                mock.patch.object(target.os, "getuid", return_value=1001, create=True), \
                mock.patch.object(target.os, "geteuid", return_value=1001, create=True):
            target.hosted_guard(True)
            with self.assertRaises(RuntimeError):
                target.hosted_guard(False)
            for key in env:
                with self.subTest(missing=key), mock.patch.dict(os.environ, {key: ""}), self.assertRaises(RuntimeError):
                    target.hosted_guard(True)

    def test_export_rejects_wrong_source_before_archiving_without_reflecting_output(self):
        commands = mock.Mock()
        commands.run.return_value = "b" * 40
        with mock.patch.dict(os.environ, {"GITHUB_SHA": "a" * 40,
                                          "MORPHEUS_PACKAGE_SOURCE_REVISION": "c" * 40}), \
                self.assertRaises(target.QAError) as error:
            target.export_source(commands, Path("source"))
        self.assertEqual(error.exception.facts(), {"reason": "artifact_identity", "operation": "source_revision"})
        self.assertEqual(commands.run.call_count, 1)
        self.assertEqual(commands.run.call_args.args[0], ["git", "rev-parse", "HEAD"])

    def test_export_uses_selected_commit_without_changing_workflow_identity(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            commands = mock.Mock()
            commands.root = root
            def command(args, **_kwargs):
                if args == ["git", "rev-parse", "HEAD"]:
                    return "b" * 40
                self.assertEqual(args[:3], ["git", "archive", "--format=tar"])
                self.assertEqual(args[-1], "b" * 40)
                archive = Path(args[3].removeprefix("--output="))
                with tarfile.open(archive, "w") as output:
                    output.addfile(tarfile.TarInfo("marker"))
                return ""
            commands.run.side_effect = command
            with mock.patch.dict(os.environ, {"GITHUB_SHA": "a" * 40,
                                              "MORPHEUS_PACKAGE_SOURCE_REVISION": "b" * 40}):
                destination, exported_revision = target.export_source(commands, root)
                self.assertEqual(exported_revision, "b" * 40)
                self.assertEqual(os.environ["GITHUB_SHA"], "a" * 40)
            self.assertTrue((destination / "marker").is_file())
            self.assertFalse((root / "source.tar").exists())
            self.assertEqual(commands.run.call_count, 2)

    def test_only_explicit_app_versions_change(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            manifest = root / "crates/morpheus-app/Cargo.toml"
            manifest.parent.mkdir(parents=True)
            original_manifest = '[package]\nname = "morpheus-app"\nversion = "0.1.0"\n\n[dependencies]\nexample = "0.1.0"\n'
            original_lock = 'version = 4\n\n[[package]]\nname = "morpheus-app"\nversion = "0.1.0"\n\n[[package]]\nname = "example"\nversion = "0.1.0"\n'
            manifest.write_text(original_manifest)
            (root / "Cargo.lock").write_text(original_lock)
            self.assertEqual(target.app_versions(root), ("0.1.0-updateqa.1", "0.1.0-updateqa.2"))
            for version in target.app_versions(root):
                target.replace_app_version(root, version)
                self.assertEqual(manifest.read_text(), original_manifest.replace('version = "0.1.0"', f'version = "{version}"'))
                self.assertEqual((root / "Cargo.lock").read_text(), original_lock.replace('name = "morpheus-app"\nversion = "0.1.0"', f'name = "morpheus-app"\nversion = "{version}"'))

    def test_missing_or_duplicate_app_lock_entry_is_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "crates/morpheus-app").mkdir(parents=True)
            (root / "crates/morpheus-app/Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
            for lock in ('version = 4\n', '[[package]]\nname = "morpheus-app"\nversion = "0.1.0"\n' * 2):
                (root / "Cargo.lock").write_text(lock)
                with self.assertRaises(RuntimeError):
                    target.replace_app_version(root, "0.1.0-updateqa.1")

    def test_update_build_uses_canonical_owned_builder_without_signer_environment(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source = root / "source"
            (source / "crates/morpheus-app").mkdir(parents=True)
            (source / "crates/morpheus-app/Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
            (source / "Cargo.lock").write_text('[[package]]\nname = "morpheus-app"\nversion = "0.1.0"\n')
            commands = mock.Mock()
            commands.root, commands.reviewed_source, commands.reviewed_revision = root, root / "reviewed", "b" * 40
            commands.gitleaks = "/reviewed/scanner"
            commands.env = {"PATH": "/usr/bin:/bin", "GITHUB_ACTIONS": "true", "RUNNER_OS": "Linux",
                            "MORPHEUS_FREE_HOSTED_UPDATE_QA": "1", "CARGO_PACKAGER_SIGN_PRIVATE_KEY": "must-not-cross",
                            "RUSTFLAGS": "must-not-cross", "GITHUB_TOKEN": "must-not-cross"}
            metadata = {"protocol": 2, "product": "io.github.brk275.morpheus", "version": "0.1.0-updateqa.1",
                        "arch": "x86_64", "persistence": "a" * 64}
            def namespace(args, directory, env):
                self.assertEqual(args[:3], ["/usr/bin/python3", "-I", "-B"])
                self.assertEqual(args[3], str(source / "packaging/linux/package_appimage.py"))
                self.assertEqual(args[4], "--update-build-plan")
                plan = json.loads(Path(args[5]).read_text())
                self.assertEqual(plan["reviewed_checkout"], str(root / "reviewed"))
                self.assertEqual(plan["revision"], "b" * 40)
                self.assertNotIn("must-not-cross", env.values())
                destination = root / "artifacts/0.1.0-updateqa.1/packages"
                destination.mkdir(parents=True)
                image(destination / "candidate.AppImage", "0.1.0-updateqa.1")
                return ["/owned/namespace"]
            commands.run.side_effect = lambda args, **kwargs: json.dumps(metadata) if args[-1] == "--morpheus-update-metadata" else ""
            helper = {"validate_metadata": mock.Mock(), "prepare_appimage": mock.Mock()}
            with mock.patch.object(target, "namespace_command", side_effect=namespace), \
                    mock.patch.object(target.runpy, "run_path", return_value=helper):
                artifact, observed = target.build_image(commands, source, root / "target", "0.1.0-updateqa.1", "https://127.0.0.1:12345/feed.json", "public-data")
            self.assertEqual(observed, metadata)
            self.assertTrue(artifact.is_file())
            self.assertEqual(commands.run.call_args_list[0].kwargs["seconds"], 3300)
            self.assertEqual(commands.run.call_args_list[0].kwargs["operation"], "package_build")
            helper["validate_metadata"].assert_called_once_with(metadata)
            helper["prepare_appimage"].assert_called_once_with(artifact, metadata)

    def test_signed_failure_fixture_preserves_genuine_package_and_footer(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source, bad = root / "new.AppImage", root / "invalid.AppImage"
            expected = image(source)
            original = source.read_bytes()
            target.invalid_elf_copy(source, bad)
            self.assertEqual(source.read_bytes(), original)
            self.assertEqual(target.image_metadata(bad), expected)
            self.assertEqual(bad.read_bytes(), original[:54] + b"\0\0" + original[56:])

    def test_footer_and_symlink_rejection(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "candidate.AppImage"
            image(path)
            if os.name != "nt":
                link = Path(folder) / "link"
                link.symlink_to(path)
                with self.assertRaises(RuntimeError):
                    target.image_metadata(link)
            data = path.read_bytes()
            for value in (data[:-1], data[:-len(target.FOOTER)-4] + struct.pack("<I", 5000) + target.FOOTER,
                          b"invalid" + data[7:]):
                path.write_bytes(value)
                with self.assertRaises((RuntimeError, ValueError)):
                    target.image_metadata(path)

    def test_legacy_or_non_integer_protocol_cannot_enter_installed_acceptance(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "candidate.AppImage"
            metadata = image(path)
            original = path.read_bytes()
            tail_size = len(json.dumps(metadata).encode()) + 4 + len(target.FOOTER)
            payload = original[:-tail_size]
            for protocol in [1, True, 2.0, 3]:
                footer = json.dumps(dict(metadata, protocol=protocol)).encode()
                path.write_bytes(payload + footer + struct.pack("<I", len(footer)) + target.FOOTER)
                with self.subTest(protocol=protocol), self.assertRaises(RuntimeError):
                    target.image_metadata(path)

    def test_signer_receives_private_key_path_in_env_never_argv(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            artifact = root / "candidate.AppImage"
            artifact.write_bytes(b"fixture")
            artifact.with_name(artifact.name + ".sig").write_text("cHVibGljLXNpZ25hdHVyZQ==")
            commands = mock.Mock()
            key = root / "private-key"
            self.assertEqual(target.sign(commands, artifact, key), "cHVibGljLXNpZ25hdHVyZQ==")
            args, kwargs = commands.run.call_args
            self.assertEqual(args[0], ["cargo", "packager", "signer", "sign", str(artifact)])
            self.assertNotIn(str(key), args[0])
            self.assertEqual(kwargs["env"]["CARGO_PACKAGER_SIGN_PRIVATE_KEY"], str(key))

    def test_certificate_cleanup_never_removes_an_unrelated_certificate(self):
        with tempfile.TemporaryDirectory() as folder:
            commands = target.Commands(Path(folder))
            with mock.patch.object(target, "CA_DIRECTORY", Path(folder)):
                cert = target.Certificate(commands)
            (cert.directory / "ca.crt").write_text("owned")
            cert.trusted.write_text("unrelated")
            cert.installed = True
            with mock.patch.object(commands, "run") as run, self.assertRaises(RuntimeError):
                cert.close()
            run.assert_not_called()

    def test_command_failure_never_uses_called_process_error_or_prints_argv(self):
        with tempfile.TemporaryDirectory() as folder:
            commands = target.Commands(Path(folder))
            process = mock.Mock(returncode=None)
            visible = io.StringIO()
            with mock.patch.object(target.subprocess, "Popen", return_value=process), \
                    mock.patch.object(target, "wait_owned", return_value=1), \
                    mock.patch.object(target, "stop_owned") as stop, contextlib.redirect_stdout(visible), \
                    self.assertRaisesRegex(RuntimeError, "^command_failed$") as failure:
                commands.run(["command", "private-value"])
            self.assertEqual(visible.getvalue(), "")
            self.assertEqual(failure.exception.facts(), {"reason": "command_failed", "operation": "command", "exit_status": 1})
            stop.assert_called_once_with(process)

    def test_child_diagnostics_cannot_publish_raw_values_or_unbounded_status(self):
        output = '\n'.join((
            'opaque-private-error',
            'MORPHEUS_APPIMAGE_QA_FAILURE {"reason":"ui_deadline","selector":"restart","exit_status":1}',
            'MORPHEUS_APPIMAGE_QA_FAILURE {"reason":"private-value","selector":"secret","operation":"https://private","exit_status":999,"stage":"/private/path"}',
        ))
        self.assertEqual(target.child_failure(output).facts(), {"reason": "acceptance"})
        self.assertIsNone(target.child_failure('opaque-private-error'))
        self.assertEqual(target.selector_code('Available version: private-value'), 'candidate_version')
        self.assertEqual(target.selector_code('private-value'), 'unknown_selector')

    def test_command_timeout_is_fixed_and_still_cleans_owned_process(self):
        with tempfile.TemporaryDirectory() as folder:
            commands = target.Commands(Path(folder))
            process = mock.Mock(returncode=None)
            with mock.patch.object(target.subprocess, "Popen", return_value=process), \
                    mock.patch.object(target, "wait_owned", side_effect=target.subprocess.TimeoutExpired(['private-value'], 1)), \
                    mock.patch.object(target, "stop_owned") as stop, self.assertRaises(target.QAError) as failure:
                commands.run(['cargo', 'packager', '--release'])
            self.assertEqual(failure.exception.facts(), {"reason": "command_deadline", "operation": "package_build"})
            stop.assert_called_once_with(process)

    def test_ui_wait_needs_a_new_process_after_restart(self):
        ui = target.NativeUI.__new__(target.NativeUI)
        node = mock.Mock()
        with mock.patch.object(ui, "find", side_effect=[(node, 10), (node, 11)]), mock.patch.object(target.time, "sleep"):
            self.assertEqual(ui.wait("button", previous_pid=10), (node, 11))

    def test_ui_click_requires_enabled_real_accessibility_action(self):
        ui = target.NativeUI.__new__(target.NativeUI)
        node = mock.Mock()
        node.queryAction.return_value.nActions = 1
        node.queryAction.return_value.doAction.return_value = True
        with mock.patch.object(ui, "wait", return_value=(node, 11)) as wait:
            self.assertEqual(ui.click("Restart to update"), 11)
        wait.assert_called_once_with("Restart to update", enabled=True)
        node.queryAction.return_value.doAction.assert_called_once_with(0)


if __name__ == "__main__":
    unittest.main()
