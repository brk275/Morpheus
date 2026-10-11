"""Pure filesystem/map checks; never opens another process or executes ELF."""
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "packaging/linux"))
sys.path.insert(0, str(ROOT / "scripts"))
# The production module imports the existing ownership helper; this private
# source-only fixture does not need its platform/native dependencies.
with mock.patch.dict(sys.modules, {"package_smoke": mock.Mock()}):
    import appimage_runtime as M


class MapTests(unittest.TestCase):
    app = M.FileIdentity("/private/run/usr/bin/morpheus-app", 8, 1, 12)
    library = M.FileIdentity("/private/run/usr/lib/libpipewire-0.3.so.0", 8, 1, 13)

    def maps(self, provider=None, *, inode=13, permission="r-xp"):
        return ("1000-2000 r-xp 000000 08:01 12 " + self.app.path + "\n"
                + f"3000-4000 {permission} 000000 08:01 {inode} "
                + (provider or self.library.path) + "\n").encode()

    def test_exact_executable_and_provider_inode_are_required(self):
        facts = M.validate_map_inventory(self.maps(), self.app, self.library)
        self.assertTrue(facts["app_local_pipewire_mapped"])
        for contents in (self.maps(inode=14), self.maps(permission="rwxp"),
                         self.maps(permission="r--p"), self.maps().splitlines()[1]):
            with self.subTest(contents=contents), self.assertRaises(M.IdentityFailure):
                M.validate_map_inventory(contents, self.app, self.library)

    def test_system_or_second_provider_fails_even_with_right_bytes_elsewhere(self):
        system = self.maps(provider="/usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0.3.0")
        for contents in (system, self.maps() + system.splitlines()[1] + b"\n"):
            with self.assertRaises(M.IdentityFailure):
                M.validate_map_inventory(contents, self.app, self.library)

    def test_malformed_deleted_and_oversized_inventory_fails(self):
        for contents in (b"", b"private server text", self.maps().replace(b".so.0\n", b".so.0 (deleted)\n"),
                         b"x" * (1024 * 1024 + 1), b"\xff"):
            with self.assertRaises(M.IdentityFailure):
                M.validate_map_inventory(contents, self.app, self.library)

    def test_opened_file_stays_open_and_symlink_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.write_bytes(b"fixture")
            source.chmod(0o600)
            fd, info, data = M.read_opened(source, 32)
            self.assertEqual(data, b"fixture")
            self.assertEqual(os.fstat(fd).st_ino, info.st_ino)
            os.close(fd)
            link = root / "link"
            link.symlink_to(source)
            with self.assertRaises(OSError):
                M.read_opened(link, 32)
            with self.assertRaises(M.IdentityFailure):
                M.read_opened(source, 1)

    def test_metadata_comparison_ignores_access_time_only(self):
        from types import SimpleNamespace
        facts = dict(st_dev=1, st_ino=2, st_mode=3, st_uid=4, st_size=5, st_mtime_ns=6, st_ctime_ns=7)
        self.assertEqual(M.file_facts(SimpleNamespace(**facts, st_atime_ns=1)),
                         M.file_facts(SimpleNamespace(**facts, st_atime_ns=2)))
        self.assertNotEqual(M.file_facts(SimpleNamespace(**facts)),
                            M.file_facts(SimpleNamespace(**{**facts, "st_mtime_ns": 8})))


class LiveIdentityAdapterTests(unittest.TestCase):
    def scenario(self, *, wrong_bytes=False, wrong_map=False, alias=False):
        from types import SimpleNamespace
        import contextlib
        with tempfile.TemporaryDirectory() as directory, contextlib.ExitStack() as patches:
            root = Path(directory).resolve()
            expected, actual = root / "expected", root / "tmp/extracted"
            for base in (expected, actual):
                (base / "usr/bin").mkdir(parents=True)
                (base / "usr/lib").mkdir(parents=True)
                for name, contents in (("usr/bin/morpheus-app", b"ordinary release executable"),
                                       ("usr/lib/libpipewire-0.3.so.0", b"exact shipped library"),
                                       ("AppRun", b"original source launcher")):
                    (base / name).write_bytes(contents)
                    (base / name).chmod(0o755)
            if wrong_bytes:
                (actual / "usr/lib/libpipewire-0.3.so.0").write_bytes(b"different provider")
            exe, lib = actual / "usr/bin/morpheus-app", actual / "usr/lib/libpipewire-0.3.so.0"
            exe_info, lib_info = exe.stat(), lib.stat()
            maps = (f"1000-2000 r-xp 000000 {os.major(exe_info.st_dev):x}:{os.minor(exe_info.st_dev):x} {exe_info.st_ino} {exe}\n"
                    f"3000-4000 r-xp 000000 {os.major(lib_info.st_dev):x}:{os.minor(lib_info.st_dev):x} {lib_info.st_ino + int(wrong_map)} {lib}\n").encode()
            real_stat, real_open = M.os.stat, open
            sentinel = os.open(exe, os.O_RDONLY)
            patches.enter_context(mock.patch.object(M.smoke, "owned_linux_pid", return_value=True))
            patches.enter_context(mock.patch.object(M.smoke, "linux_process_identity", return_value=("S", 42, 42, 99)))
            patches.enter_context(mock.patch.object(M.os, "pidfd_open", return_value=sentinel, create=True))
            patches.enter_context(mock.patch.object(M.os, "readlink", return_value=str(exe)))
            patches.enter_context(mock.patch.object(M.os, "stat", side_effect=lambda path, *a, **k: exe_info if str(path) == "/proc/123/exe" else real_stat(path, *a, **k)))
            patches.enter_context(mock.patch.object(M.select, "select", return_value=([], [], [])))
            patches.enter_context(mock.patch("builtins.open", side_effect=lambda path, *a, **k: __import__("io").BytesIO(maps) if str(path) == "/proc/123/maps" else real_open(path, *a, **k)))
            try:
                identity = M.RuntimeIdentity(123, SimpleNamespace(pid=42), "nonce", root / "tmp", expected)
                self.assertTrue(identity.check()["app_local_pipewire_mapped"])
                if alias:
                    spelling = str(actual / "usr/bin/../lib/libpipewire-0.3.so.0")
                    trace = f"123: binding file {exe} [0] to {spelling} [0]: normal symbol `pw_init'\n".encode()
                    identity.attest_aliases(trace)
                    self.assertEqual(identity.verified_library_aliases, {spelling})
                    foreign = root / "other.so"
                    foreign.write_bytes(lib.read_bytes())
                    with self.assertRaises(M.IdentityFailure):
                        identity.attest_aliases(trace.replace(spelling.encode(), str(foreign).encode()))
                identity.close()
            finally:
                with self.assertRaises(OSError):
                    os.fstat(sentinel)

    def test_live_process_checks_anchored_opened_bytes_and_maps(self):
        self.scenario()

    def test_loader_alias_requires_same_live_held_inode_and_hash(self):
        self.scenario(alias=True)

    def test_changed_library_or_forged_inode_closes_all_ownership_handles(self):
        for arguments in ({"wrong_bytes": True}, {"wrong_map": True}):
            with self.subTest(arguments=arguments), self.assertRaises(M.IdentityFailure):
                self.scenario(**arguments)


if __name__ == "__main__":
    unittest.main()
