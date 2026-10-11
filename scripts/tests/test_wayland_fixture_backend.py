import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("wayland_fixture_backend", Path(__file__).resolve().parents[1] / "wayland_fixture_backend.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def archive_entry(name, content=b"fixture source", kind=tarfile.REGTYPE):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        entry = tarfile.TarInfo(name)
        entry.type = kind
        if kind == tarfile.REGTYPE:
            entry.size = len(content)
            archive.addfile(entry, io.BytesIO(content))
        else:
            entry.linkname = "/outside-fixture"
            archive.addfile(entry)
    return buffer.getvalue()


class WaylandBackendTests(unittest.TestCase):
    def test_wrong_archive_hash_creates_no_source_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "source"
            with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                MODULE.extract_verified_archive(b"not trusted", output)
            self.assertFalse(output.exists())

    def test_archive_rejects_links_and_path_traversal_even_with_matching_hash(self):
        prefix = f"xdg-desktop-portal-wlr-{MODULE.COMMIT}"
        for name, kind in [
            (f"{prefix}/../../escape", tarfile.REGTYPE),
            (f"/{prefix}/absolute", tarfile.REGTYPE),
            (f"{prefix}/link", tarfile.SYMTYPE),
            (f"{prefix}/hardlink", tarfile.LNKTYPE),
            ("unexpected-root/file", tarfile.REGTYPE),
        ]:
            data = archive_entry(name, kind=kind)
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "source"
                with mock.patch.object(MODULE, "ARCHIVE_SHA256", hashlib.sha256(data).hexdigest()):
                    with self.assertRaisesRegex(RuntimeError, "unsafe entry"):
                        MODULE.extract_verified_archive(data, output)
                self.assertFalse(output.exists())

    def test_verified_regular_file_extracts_inside_private_root(self):
        data = archive_entry(f"xdg-desktop-portal-wlr-{MODULE.COMMIT}/src/example.c")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "source"
            with mock.patch.object(MODULE, "ARCHIVE_SHA256", hashlib.sha256(data).hexdigest()):
                MODULE.extract_verified_archive(data, output)
            self.assertEqual((output / "src/example.c").read_bytes(), b"fixture source")
            self.assertEqual(output.stat().st_mode & 0o777, 0o700)
            self.assertEqual((output / "src/example.c").stat().st_mode & 0o777, 0o600)

    def test_shm_adaptation_only_changes_exact_check_and_preserves_format_guard(self):
        content = "before\n" + MODULE.ORIGINAL_CHECK + "\n\t\treturn -1;\n\t}\nafter\n"
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "screencast.c"
            source.write_text(content)
            with mock.patch.object(MODULE, "SOURCE_SHA256", hashlib.sha256(content.encode()).hexdigest()):
                MODULE.adapt_shm_check(source)
            self.assertEqual(source.read_text(), content.replace(MODULE.ORIGINAL_CHECK, MODULE.SHM_CHECK))
            self.assertIn("[WL_SHM].format == DRM_FORMAT_INVALID", source.read_text())
            self.assertIn("return -1;", source.read_text())
            with self.assertRaisesRegex(RuntimeError, "source checksum mismatch"):
                MODULE.adapt_shm_check(source)

    def test_adaptation_rejects_duplicate_check(self):
        content = MODULE.ORIGINAL_CHECK * 2
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "screencast.c"
            source.write_text(content)
            with mock.patch.object(MODULE, "SOURCE_SHA256", hashlib.sha256(content.encode()).hexdigest()):
                with self.assertRaisesRegex(RuntimeError, "did not match exactly"):
                    MODULE.adapt_shm_check(source)
            self.assertEqual(source.read_text(), content)

    def test_producer_diagnostics_preserve_one_queue_call_and_only_fixed_values(self):
        content = (MODULE.PRODUCER_INCLUDE + '\n' + MODULE.PRODUCER_BEGIN + '\n\tbefore();\n' + MODULE.PRODUCER_QUEUE
                   + '\n\tafter();\n}\n')
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'pipewire_screencast.c'
            source.write_text(content)
            with mock.patch.object(MODULE, 'PRODUCER_SOURCE_SHA256', hashlib.sha256(content.encode()).hexdigest()):
                MODULE.add_producer_diagnostics(source)
            result = source.read_text()
            self.assertEqual(result.count('pw_stream_queue_buffer(cast->stream, pw_buf)'), 1)
            self.assertEqual(result.count('pw_stream_is_driving(cast->stream)'), 1)
            self.assertIn('diagnostic_samples < 16', result)
            self.assertIn('const int queue_result = pw_stream_queue_buffer', result)
            self.assertIn('buffer_corrupt ? "corrupt" : "valid"', result)
            self.assertIn('queue_result < 0 ? "rejected" : "accepted"', result)
            self.assertIn('clock_gettime(CLOCK_MONOTONIC_RAW, &now)', result)
            self.assertIn('now.tv_sec & 0x1FFFFFFF) % 100000', result)
            self.assertLess(result.index('morpheus_fixture_order("queue-begin")'), result.index('const int queue_result'))
            self.assertGreater(result.index('morpheus_fixture_order(queue_result'), result.index('const int queue_result'))
            self.assertNotIn('%d', result)
            self.assertNotIn('%p', result)
            self.assertEqual(result.replace(MODULE.PRODUCER_INCLUDE + '\n#include <time.h>', MODULE.PRODUCER_INCLUDE)
                             .replace(MODULE.PRODUCER_CLOCK_HELPER, '')
                             .replace(MODULE.PRODUCER_DIAGNOSTIC_BEGIN, MODULE.PRODUCER_BEGIN)
                             .replace(MODULE.PRODUCER_DIAGNOSTIC_QUEUE, MODULE.PRODUCER_QUEUE), content)
            with self.assertRaisesRegex(RuntimeError, 'checksum mismatch'):
                MODULE.add_producer_diagnostics(source)

    def test_producer_diagnostics_reject_wrong_hash_missing_and_duplicate_anchors(self):
        valid = MODULE.PRODUCER_INCLUDE + '\n' + MODULE.PRODUCER_BEGIN + '\n' + MODULE.PRODUCER_QUEUE
        for content in ('untrusted source', valid.replace(MODULE.PRODUCER_QUEUE, ''),
                        valid + MODULE.PRODUCER_QUEUE, valid + MODULE.PRODUCER_BEGIN,
                        valid.replace(MODULE.PRODUCER_INCLUDE, ''), valid + MODULE.PRODUCER_INCLUDE):
            with self.subTest(content=content), tempfile.TemporaryDirectory() as directory:
                source = Path(directory) / 'pipewire_screencast.c'
                source.write_text(content)
                expected = ('checksum mismatch' if content == 'untrusted source' else 'anchors did not match exactly')
                digest = MODULE.PRODUCER_SOURCE_SHA256 if content == 'untrusted source' else hashlib.sha256(content.encode()).hexdigest()
                with mock.patch.object(MODULE, 'PRODUCER_SOURCE_SHA256', digest):
                    with self.assertRaisesRegex(RuntimeError, expected):
                        MODULE.add_producer_diagnostics(source)
                self.assertEqual(source.read_text(), content)
