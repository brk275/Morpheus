"""Bounded source/archive checks with mocked Git; no child, hooks or downloads."""

import gzip
import io
import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "packaging/linux"))
import corresponding_source as S
import source_bundle_qa as Q
from scripts import package as P


def crate(members):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for name, data, link in members:
            info = tarfile.TarInfo(name)
            if link:
                info.type, info.linkname = tarfile.SYMTYPE, link
            else:
                info.size = len(data)
            archive.addfile(info, None if link else io.BytesIO(data))
    return output.getvalue()


class SourceTests(unittest.TestCase):
    def test_git_uses_only_bounded_authenticated_raw_objects(self):
        blob = b"export me even when .gitattributes says export-ignore\n"
        entries = {"README": {"mode": "100644", "oid": S.object_id("blob", blob), "bytes": len(blob)}}
        tree = S.tree_identity(entries)
        commit = ("tree " + tree + "\nauthor Fixture <fixture@example.org> 0 +0000\n\nfixture\n").encode()
        revision = S.object_id("commit", commit)
        listing = ("100644 blob " + entries["README"]["oid"] + " " + str(len(blob)) + "\tREADME\0").encode()
        batch = (entries["README"]["oid"] + " blob " + str(len(blob)) + "\n").encode() + blob + b"\n"
        runner = SimpleNamespace(run=mock.Mock(side_effect=[commit, listing, batch]))
        with mock.patch.object(S, "safe_git", return_value=["/usr/bin/git"]):
            payload, identity, files = S.git_export(runner, Path("/mirror"), revision,
                                                    "/usr/bin/git", {"GIT_CONFIG_COUNT": "1", "SSH_AUTH_SOCK": "/secret"}, S.Budget())
        self.assertEqual(identity["commit"], revision)
        self.assertEqual(identity["tree"], tree)
        self.assertFalse(identity["git_attributes_applied"])
        self.assertEqual(files, {"README": blob})
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
            self.assertEqual(archive.extractfile("README").read(), blob)
        for call in runner.run.call_args_list:
            self.assertEqual(call.kwargs["timeout"], 60)
            self.assertEqual(call.kwargs["stderr_limit"], 65536)
            self.assertNotIn("GIT_CONFIG_COUNT", call.kwargs["env"])
            self.assertNotIn("SSH_AUTH_SOCK", call.kwargs["env"])
            self.assertEqual(call.kwargs["env"]["GIT_NO_LAZY_FETCH"], "1")
            self.assertNotIn("archive", call.args[1])
        self.assertEqual(runner.run.call_args.kwargs["stdin_bytes"], (entries["README"]["oid"] + "\n").encode())
        for tampered in (b"different commit", listing.replace(b"README", b"omitted"), batch.replace(blob, b"x" * len(blob))):
            sequence = [commit, listing, batch]
            sequence[0 if tampered == b"different commit" else 1 if b"omitted" in tampered else 2] = tampered
            with mock.patch.object(S, "safe_git", return_value=["/usr/bin/git"]), self.assertRaises(S.SourceFailure):
                S.git_export(SimpleNamespace(run=mock.Mock(side_effect=sequence)), Path("/mirror"), revision,
                             "/usr/bin/git", {}, S.Budget())

    def test_git_environment_and_configuration_cannot_select_hooks_or_transports(self):
        result = S.clean_git_environment({"HOME": "/fixture", "GIT_DIR": "/other", "GIT_CONFIG_SYSTEM": "/evil",
                                          "GIT_SSH_COMMAND": "evil", "SSH_ASKPASS": "evil"})
        self.assertEqual(result["HOME"], "/fixture")
        self.assertNotIn("GIT_DIR", result)
        self.assertNotIn("GIT_SSH_COMMAND", result)
        self.assertNotIn("SSH_ASKPASS", result)
        self.assertEqual(result["GIT_CONFIG_NOSYSTEM"], "1")
        self.assertEqual(result["GIT_CONFIG_GLOBAL"], "/dev/null")
        self.assertEqual(result["GIT_NO_REPLACE_OBJECTS"], "1")
        self.assertEqual(result["GIT_ALLOW_PROTOCOL"], "")
        with mock.patch.object(Path, "resolve", side_effect=lambda strict: Path("/mirror")), \
                mock.patch.object(Path, "stat", return_value=SimpleNamespace(st_mode=0o40700, st_uid=os.getuid())):
            command = S.safe_git("/usr/bin/git", "/mirror")
        for setting in ("core.hooksPath=/dev/null", "core.fsmonitor=false", "diff.external=", "core.attributesFile=/dev/null"):
            self.assertIn(setting, command)

    def test_registry_identity_traversal_links_and_expansion_limits(self):
        manifest = b'[package]\nname="fixture"\nversion="1.0.0"\nlicense="MIT"\n'
        good = [("fixture-1.0.0/Cargo.toml", manifest, None)]
        for members, maximum in ((good, S.MAX_GIT_CONTENT),
                                 (good + [("fixture-1.0.0/../../escape", b"x", None)], S.MAX_GIT_CONTENT),
                                 (good + [("fixture-1.0.0/link", b"", "../../escape")], S.MAX_GIT_CONTENT),
                                 (good, 512)):
            payload = crate(members)
            package = {"name": "fixture", "version": "1.0.0", "checksum": S.digest(payload)}
            with self.subTest(members=members, maximum=maximum), mock.patch.object(S, "MAX_GIT_CONTENT", maximum):
                if members == good and maximum == S.MAX_GIT_CONTENT and maximum > 512:
                    self.assertEqual(S.registry_contents(payload, package, S.Budget())["Cargo.toml"], manifest)
                else:
                    with self.assertRaises(S.SourceFailure):
                        S.registry_contents(payload, package, S.Budget())
        with self.assertRaises(S.SourceFailure):
            S.registry_contents(crate(good), {"name": "fixture", "version": "1.0.0", "checksum": "0" * 64}, S.Budget())

    def test_git_links_are_preserved_only_when_the_resolved_graph_exists_inside(self):
        entries = {"LICENSE": {"mode": "100644"}, "docs/link": {"mode": "120000"}}
        S.check_links(entries, {"LICENSE": b"license", "docs/link": b"../LICENSE"})
        for target in (b"../../escape", b"/absolute", b"../missing", b"link", b"../LICENSE/../LICENSE"):
            with self.subTest(target=target), self.assertRaises(S.SourceFailure):
                S.check_links(entries, {"LICENSE": b"license", "docs/link": target})

    def test_catalog_preserves_inherited_license_source_and_published_archive_identity(self):
        files = {"Cargo.toml": b'[workspace]\nmembers=["member"]\n[workspace.package]\nversion="1.0.0"\nlicense-file="LICENSE"\n',
                 "member/Cargo.toml": b'[package]\nname="fixture"\nversion.workspace=true\nlicense-file.workspace=true\n',
                 "LICENSE": b"license text"}
        record = S.package_catalog(files)[("fixture", "1.0.0")]
        self.assertEqual(record["license_text_candidates"]["LICENSE"], S.digest(b"license text"))
        self.assertFalse(record["license_obligations_verified"])
        published = {"Cargo.toml": b'[package]\nname="fixture"\nversion="1.0.0"\n[dev-dependencies.unpublished]\npath="../unpublished"\n'}
        self.assertIn(("fixture", "1.0.0"), S.package_catalog(published, published=True))
        with self.assertRaises(S.SourceFailure):
            S.package_catalog(published)

    def test_missing_git_input_and_unknown_lock_sources_fail_closed(self):
        source = "git+https://github.com/fixture/source?rev=" + "a" * 40 + "#" + "a" * 40
        plan = json.dumps({"schema": 1, "registry_cache": "/cache", "git_mirrors": {}}).encode()
        with mock.patch.object(S, "read_owned", return_value=plan), self.assertRaises(S.SourceFailure):
            S.input_plan(Path("/plan"), [{"source": source}])
        for raw in (b'version=4\n[[package]]\nname="fixture"\nversion="1.0.0"\nsource="registry+https://other.invalid"\n',
                    b'version=4\n[[package]]\nname="fixture"\nversion="1.0.0"\nsource="registry+https://github.com/rust-lang/crates.io-index"\n'):
            with self.assertRaises(S.SourceFailure):
                S.lock_packages({"Cargo.lock": raw})

    def test_atomic_publication_is_private_nonreplacing_and_removable_by_exact_identity(self):
        payload = b"source evidence"
        receipt = {"bundle_sha256": S.digest(payload), "distribution_ready": False}
        with tempfile.TemporaryDirectory() as directory:
            identity = S.publish(Path(directory), payload, receipt)
            target = Path(directory) / S.BUNDLE_NAME
            self.assertEqual(target.read_bytes(), payload)
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(S.SourceFailure):
                S.publish(Path(directory), payload, receipt)
            self.assertEqual(target.read_bytes(), payload)
            with self.assertRaises(S.SourceFailure):
                S.remove_published(Path(directory), (identity[0], identity[1] + 1, identity[2]))
            S.remove_published(Path(directory), identity)
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_source_route_is_explicit_and_retains_unsigned_and_scanner_guards(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(P.os, "execv") as execute:
            P.linux_appimage("appimage", "/reviewed/scanner", source_bundle=(Path("/plan"), Path("/out")))
        args = execute.call_args.args[1]
        self.assertEqual(args[-4:], ["--source-bundle-inputs", "/plan", "--source-bundle-output", "/out"])
        self.assertNotIn("--qa-output", args)
        for scanner, environment in ((None, {}), ("/reviewed/scanner", {"CARGO_PACKAGER_SIGN_PRIVATE_KEY": "fixture"})):
            with mock.patch.dict(os.environ, environment, clear=True), mock.patch.object(P.os, "execv") as execute, self.assertRaises(ValueError):
                P.linux_appimage("appimage", scanner, source_bundle=(Path("/plan"), Path("/out")))
            execute.assert_not_called()

    def test_acquisition_rejects_unreviewed_manifest_origins_before_fetch(self):
        lock = 'version=4\n' + ''.join('[[package]]\nname="fixture' + str(index) + '"\nversion="1.0.0"\nsource="' + source + '"\n'
                                             for index, source in enumerate(sorted(Q.PINS)))
        root = b'[package]\nname="fixture"\nversion="1.0.0"\n'
        files = {"Cargo.toml": root, "Cargo.lock": lock.encode()}
        self.assertEqual(len(Q.source_inputs(files)), 3)
        for extra in (b'[dependencies.bad]\ngit="https://unreviewed.invalid/repository"\nrev="abc"\n',
                      b'[dependencies.bad]\nversion="1"\nregistry="unreviewed"\n'):
            with self.assertRaises(S.SourceFailure):
                Q.source_inputs({**files, "Cargo.toml": root + extra})

    def test_acquisition_environment_uses_private_cache_and_no_ambient_credentials(self):
        environment = Q.environment_for_acquisition({"HOME": "/old-home", "GITHUB_ACTOR": "brk275",
            "GITHUB_TRIGGERING_ACTOR": "brk275", "SSH_AUTH_SOCK": "/socket", "CARGO_REGISTRY_TOKEN": "fixture",
            "HTTP_PROXY": "http://unreviewed.invalid"}, Path("/private/owned"), "/toolchain/bin/cargo", "/toolchain/bin/rustc")
        self.assertEqual(environment["HOME"], "/private/owned/home")
        self.assertEqual(environment["CARGO_HOME"], "/private/owned/cargo")
        self.assertEqual(environment["RUSTC"], "/toolchain/bin/rustc")
        self.assertEqual(environment["CARGO_NET_GIT_FETCH_WITH_CLI"], "false")
        self.assertEqual(environment["GIT_ALLOW_PROTOCOL"], "https")
        for name in ("SSH_AUTH_SOCK", "CARGO_REGISTRY_TOKEN", "HTTP_PROXY"):
            self.assertNotIn(name, environment)

    def test_real_outer_receipt_parser_accepts_only_its_fixed_containment_line_and_json(self):
        self.assertEqual(Q.package_receipt(b'MORPHEUS_ELEMENT_CONTAINMENT passed\n{"schema":1}\n'), {"schema": 1})
        for data in (b'{}\n', b'private data\n{}\n', b'MORPHEUS_ELEMENT_CONTAINMENT passed\n{}\nprivate tail\n'):
            with self.assertRaises(S.SourceFailure):
                Q.package_receipt(data)

    def test_finalizer_removes_owned_inputs_even_if_preparation_or_package_receipt_is_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "owned"
            root.mkdir(mode=0o700)
            (root / "canonical-temp").mkdir(mode=0o700)
            (root / "logs").mkdir(mode=0o700)
            with mock.patch.object(Q, "private_root", return_value=root), \
                    mock.patch.object(Q.P, "scan_private_logs"), mock.patch.object(Q.P, "owned_tool", return_value="/scanner"), \
                    self.assertRaises(S.SourceFailure):
                Q.finish(root, "/scanner", {})
            self.assertFalse(root.exists())


if __name__ == "__main__":
    unittest.main()
