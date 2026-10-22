import os
import io
import tarfile
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from scripts import package_revision as revision
from scripts import package_scope


class RevisionIdentityTests(unittest.TestCase):
    def test_update_export_allows_only_exact_two_version_fields_and_full_file_map(self):
        originals = {"crates/morpheus-app/Cargo.toml": b'[package]\nname = "morpheus-app"\nversion = "0.1.0"\n\n[dependencies]\nother = "0.1.0"\n',
                     "Cargo.lock": b'[[package]]\nname = "morpheus-app"\nversion = "0.1.0"\n\n[[package]]\nname = "other"\nversion = "0.1.0"\n',
                     "scripts/unchanged.py": b"reviewed source\n"}
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as stream:
            for name, content in originals.items():
                member = tarfile.TarInfo(name)
                member.size, member.mode = len(content), 0o644
                stream.addfile(member, io.BytesIO(content))
        projected = dict(originals)
        projected["crates/morpheus-app/Cargo.toml"] = originals["crates/morpheus-app/Cargo.toml"].replace(b'version = "0.1.0"', b'version = "0.1.0-updateqa.1"', 1)
        projected["Cargo.lock"] = originals["Cargo.lock"].replace(b'version = "0.1.0"', b'version = "0.1.0-updateqa.1"', 1)
        for mutation in (None, "extra", "missing", "other-bytes", "other-dependency", "executable"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                for name, content in projected.items():
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(content)
                    path.chmod(0o600)
                if mutation == "extra": (root / "unreviewed").write_text("different")
                if mutation == "missing": (root / "scripts/unchanged.py").unlink()
                if mutation == "other-bytes": (root / "scripts/unchanged.py").write_text("different")
                if mutation == "other-dependency":
                    path = root / "Cargo.lock"
                    path.write_bytes(path.read_bytes().replace(b'name = "other"\nversion = "0.1.0"', b'name = "other"\nversion = "0.2.0"'))
                if mutation == "executable": (root / "scripts/unchanged.py").chmod(0o700)
                if mutation is None:
                    result = revision.verify_versioned_export(root, archive.getvalue(), "0.1.0-updateqa.1")
                    self.assertEqual(result["variant"], "updateqa-version-only")
                    self.assertEqual(result["files"], 3)
                    self.assertEqual(result["allowed_paths"], ["Cargo.lock", "crates/morpheus-app/Cargo.toml"])
                else:
                    with self.assertRaises(revision.SourceRevisionError):
                        revision.verify_versioned_export(root, archive.getvalue(), "0.1.0-updateqa.1")

        for invalid_name, kind in (("../outside", tarfile.REGTYPE), ("linked", tarfile.SYMTYPE),
                                   ("scripts/unchanged.py", tarfile.REGTYPE)):
            with self.subTest(archive_member=invalid_name), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                for name, content in projected.items():
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(content)
                    path.chmod(0o600)
                archive = io.BytesIO()
                with tarfile.open(fileobj=archive, mode="w") as stream:
                    for name, content in originals.items():
                        member = tarfile.TarInfo(name)
                        member.size, member.mode = len(content), 0o644
                        stream.addfile(member, io.BytesIO(content))
                    invalid = tarfile.TarInfo(invalid_name)
                    invalid.type, invalid.linkname = kind, "outside"
                    stream.addfile(invalid)
                with self.assertRaises(revision.SourceRevisionError):
                    revision.verify_versioned_export(root, archive.getvalue(), "0.1.0-updateqa.1")

    def test_default_and_explicit_sources_keep_workflow_identity_separate(self):
        workflow, source = "a" * 40, "b" * 40
        self.assertEqual(revision.select_revision("", workflow), workflow)
        self.assertEqual(revision.select_revision(source, workflow), source)
        env = {"GITHUB_SHA": workflow, "MORPHEUS_PACKAGE_SOURCE_REVISION": source}
        self.assertEqual(revision.expected_revision(env), source)
        self.assertEqual(env["GITHUB_SHA"], workflow)

    def test_nonimmutable_refs_and_output_injection_fail_before_git(self):
        invalid = [None, False, "main", "refs/heads/main", "a" * 7, "0" * 40,
                   "A" * 40, "a" * 40 + "\ncache_write=true", "b" * 40 + "\n",
                   "$(printf private)", "a" * 41]
        for value in invalid:
            with self.subTest(value=value), patch.object(revision.subprocess, "run") as run:
                with self.assertRaises(revision.SourceRevisionError):
                    revision.select_revision(value, "a" * 40)
                with self.assertRaises(revision.SourceRevisionError):
                    revision.verify_checkout(Path("unused"), value)
                run.assert_not_called()
        for workflow in invalid:
            with self.subTest(workflow=workflow), self.assertRaises(revision.SourceRevisionError):
                revision.select_revision("b" * 40, workflow)

    def test_checkout_requires_exact_head_and_clean_tracked_tree(self):
        source = "b" * 40
        replies = [subprocess.CompletedProcess([], 0, (source + "\n").encode(), b""),
                   subprocess.CompletedProcess([], 0, b"", b"")]
        with patch.object(revision.subprocess, "run", side_effect=replies) as run:
            self.assertEqual(revision.verify_checkout(Path("source"), source), source)
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args_list[0].args[0], ["git", "rev-parse", "--verify", "HEAD^{commit}"])
        self.assertIn("--no-ext-diff", run.call_args.args[0])
        self.assertIn("--no-textconv", run.call_args.args[0])
        self.assertTrue(all(call.kwargs["timeout"] == 20 and not call.kwargs.get("shell", False)
                            for call in run.call_args_list))

    def test_windows_line_ending_is_canonicalized_without_accepting_extra_lines(self):
        with patch.object(revision.subprocess, "run", side_effect=[
                subprocess.CompletedProcess([], 0, b"b" * 40 + b"\r\n", b""),
                subprocess.CompletedProcess([], 0, b"", b"")]):
            self.assertEqual(revision.verify_checkout(Path("source"), "b" * 40), "b" * 40)
        for ending in (b"\r", b"\r\n\r\n", b" \n"):
            with self.subTest(ending=ending), patch.object(revision.subprocess, "run", return_value=
                    subprocess.CompletedProcess([], 0, b"b" * 40 + ending, b"")):
                with self.assertRaises(revision.SourceRevisionError):
                    revision.verify_checkout(Path("source"), "b" * 40)

    def test_head_failure_cannot_reach_dirty_check_or_reflect_git_output(self):
        failures = [subprocess.CompletedProcess([], 0, b"a" * 40 + b"\n", b""),
                    subprocess.CompletedProcess([], 1, b"private-output", b"private-error"),
                    subprocess.CompletedProcess([], 0, b"b" * 40 + b"\nextra", b""),
                    OSError("private-path"),
                    subprocess.TimeoutExpired("private-command", 20, output=b"private-output")]
        for result in failures:
            with self.subTest(kind=type(result).__name__):
                effect = {"side_effect": result} if isinstance(result, BaseException) else {"return_value": result}
                with patch.object(revision.subprocess, "run", **effect) as run:
                    with self.assertRaises(revision.SourceRevisionError) as error:
                        revision.verify_checkout(Path("source"), "b" * 40)
                self.assertEqual(run.call_count, 1)
                self.assertNotIn("private", str(error.exception))

    def test_dirty_tree_is_not_the_claimed_commit(self):
        for result in [subprocess.CompletedProcess([], 1, b"", b""),
                       subprocess.CompletedProcess([], 0, b"private-output", b""),
                       subprocess.CompletedProcess([], 0, b"", b"private-error")]:
            with patch.object(revision.subprocess, "run", side_effect=[
                    subprocess.CompletedProcess([], 0, b"b" * 40 + b"\n", b""), result]):
                with self.assertRaises(revision.SourceRevisionError) as error:
                    revision.verify_checkout(Path("source"), "b" * 40)
            self.assertEqual(error.exception.reason, "dirty")
            self.assertNotIn("private", str(error.exception))


class PackageSourceScopeTests(unittest.TestCase):
    def environment(self, output, source=""):
        return {"GITHUB_OUTPUT": str(output), "GITHUB_SHA": "a" * 40,
                "PACKAGE_SOURCE_REVISION": source, "PACKAGE_PLATFORM": "linux",
                "LINUX_FORMATS": "appimage", "APPIMAGE_UPDATES": "false",
                "APPIMAGE_CAPTURE": "false"}

    def test_only_actual_main_workflow_source_can_write_shared_cache(self):
        for requested, expected, cache in [("", "a" * 40, "true"),
                                           ("a" * 40, "a" * 40, "true"),
                                           ("b" * 40, "b" * 40, "false")]:
            with self.subTest(requested=requested), tempfile.TemporaryDirectory() as folder:
                output = Path(folder) / "output"
                with patch.dict(os.environ, self.environment(output, requested), clear=True):
                    package_scope.main()
                fields = dict(line.split("=", 1) for line in output.read_text().splitlines())
                self.assertEqual(fields["source_revision"], expected)
                self.assertEqual(fields["cache_write"], cache)
                self.assertNotIn("macos", fields["matrix"])
                self.assertNotIn("windows", fields["matrix"])

    def test_invalid_source_emits_no_partial_runner_or_cache_outputs(self):
        with tempfile.TemporaryDirectory() as folder:
            output = Path(folder) / "output"
            with patch.dict(os.environ, self.environment(output, "main"), clear=True):
                with self.assertRaises(revision.SourceRevisionError):
                    package_scope.main()
            self.assertFalse(output.exists())

    def test_workflow_keeps_main_guard_and_binds_cache_source_before_build(self):
        root = Path(__file__).resolve().parents[2]
        workflow = (root / ".github/workflows/package-qa.yml").read_text()
        self.assertIn("github.ref == 'refs/heads/main'", workflow)
        self.assertIn("needs.scope.outputs.cache_write == 'true'", workflow)
        self.assertIn("key: package-inputs-${{ hashFiles('Cargo.toml', 'Cargo.lock') }}", workflow)
        self.assertIn("ref: ${{ needs.scope.outputs.source_revision }}", workflow)
        self.assertIn("native-${{ needs.scope.outputs.source_revision }}-${{ matrix.os }}", workflow)
        self.assertLess(workflow.index("Scan selected source before any package worker executes it"),
                        workflow.index("Verify exact reviewed source checkout"))
        self.assertLess(workflow.index("Verify exact reviewed source checkout"),
                        workflow.index("Install pinned Rust toolchain"))
        # Actual scanner suppression resistance is covered by HistoryScannerTests.
        self.assertNotIn("pull_request_target", workflow)
        self.assertNotIn("upload-artifact", workflow)


@unittest.skipUnless(os.name == "posix" and Path("/bin/bash").is_file(), "Bash fixture requires a POSIX host")
class RuntimeHostDownloadTests(unittest.TestCase):
    def test_only_interrupted_transfers_retry_and_partial_files_are_removed(self):
        root = Path(__file__).resolve().parents[2]
        workflow = (root / ".github/workflows/package-qa.yml").read_text()
        start = workflow.index("          download_pinned_runtime_deb() {\n")
        end = workflow.index("          {\n", start)
        function = "\n".join(line[10:] for line in workflow[start:end].splitlines()) + "\n"
        self.assertEqual(function.count("/usr/bin/curl"), 1)
        self.assertEqual(function.count("/bin/sleep"), 1)
        function = function.replace("/usr/bin/curl", "fixture_curl").replace("/bin/sleep", "fixture_sleep")
        # Run the actual workflow function with only transport and delay replaced.
        # The stub refuses a retry unless its previous partial output was removed.
        fixture = r'''set -euo pipefail
calls=0
fixture_curl() {
  calls=$((calls + 1))
  printf '%s' "$calls" > calls
  local destination
  for destination do :; done
  [[ ! -e "$destination" ]] || return 90
  printf 'attempt-%s' "$calls" > "$destination"
  return "${codes[calls - 1]}"
}
fixture_sleep() { [[ "$1" == 2 ]]; }
'''
        cases = [([0], 0, 1), ([56, 0], 0, 2), ([18, 28, 0], 0, 3),
                 ([52, 55, 56], 56, 3), ([22, 0], 22, 1),
                 ([60, 0], 60, 1), ([23, 0], 23, 1)]
        for codes, status, calls in cases:
            with self.subTest(codes=codes), tempfile.TemporaryDirectory() as folder:
                command = (fixture + "codes=(" + " ".join(map(str, codes)) + ")\n" + function
                           + 'download_pinned_runtime_deb output.deb 100 https://example.invalid/pinned.deb\n')
                result = subprocess.run(["/bin/bash", "--noprofile", "--norc", "-c", command],
                                        cwd=folder, env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                                        capture_output=True, timeout=10)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual((Path(folder) / "calls").read_text(), str(calls))
                self.assertEqual((Path(folder) / "output.deb").read_text(), f"attempt-{calls}")


if __name__ == "__main__":
    unittest.main()
