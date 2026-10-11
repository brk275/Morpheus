import hashlib
import json
import struct
import tempfile
import unittest
import zlib
from pathlib import Path

from scripts import encode_preview_pngs as preview


class PreviewEncodingTests(unittest.TestCase):
    def pixels(self):
        # Includes whitespace, '#', zero, and full-intensity channel values.
        sample = b"\x00\x20\xff\x0a\x23\x0d"
        return (sample * (preview.PIXEL_BYTES // len(sample) + 1))[:preview.PIXEL_BYTES]

    def test_png_roundtrip_preserves_every_rgb_byte_and_has_no_extra_metadata(self):
        pixels = self.pixels()
        self.assertEqual(preview.parse_ppm(preview.HEADER + pixels), pixels)
        data = preview.encode_png(pixels)
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        offset, blocks = 8, []
        while offset < len(data):
            size = struct.unpack(">I", data[offset:offset + 4])[0]
            kind = data[offset + 4:offset + 8]
            body = data[offset + 8:offset + 8 + size]
            crc = struct.unpack(">I", data[offset + 8 + size:offset + 12 + size])[0]
            self.assertEqual(crc, zlib.crc32(kind + body))
            blocks.append((kind, body))
            offset += size + 12
        self.assertEqual(offset, len(data))
        self.assertEqual([kind for kind, _ in blocks], [b"IHDR", b"IDAT", b"IEND"])
        self.assertEqual(struct.unpack(">IIBBBBB", blocks[0][1]), (1440, 900, 8, 2, 0, 0, 0))
        rows = zlib.decompress(blocks[1][1])
        stride = preview.WIDTH * 3
        self.assertEqual(len(rows), (stride + 1) * preview.HEIGHT)
        self.assertTrue(all(rows[index] == 0 for index in range(0, len(rows), stride + 1)))
        decoded = b"".join(rows[index + 1:index + stride + 1] for index in range(0, len(rows), stride + 1))
        self.assertEqual(decoded, pixels)

    def test_noncanonical_ppm_wrong_dimensions_truncation_and_trailing_bytes_fail(self):
        canonical = preview.HEADER + self.pixels()
        for data in (canonical[:-1], canonical + b"\x00", canonical.replace(b"1440", b"1441", 1),
                     canonical.replace(b"255\n", b"65535\n", 1), canonical.replace(b"P6\n", b"P3\n", 1)):
            with self.subTest(header=data[:24]), self.assertRaises(ValueError):
                preview.parse_ppm(data)

    def fixture(self, root):
        raw = root / "raw"
        raw.mkdir()
        for name in preview.NAMES:
            (raw / (name + ".ppm")).write_bytes(preview.HEADER + self.pixels())
        binary = root / "native-fixture-binary"
        binary.write_bytes(b"fixture binary bytes only")
        return raw, binary

    def test_complete_output_is_exact_allowlist_with_path_free_hash_provenance(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder).resolve()
            raw, binary = self.fixture(root)
            out = root / "png"
            facts = preview.convert(raw, out, "a" * 40, binary, "123", "1")
            self.assertEqual({p.name for p in out.iterdir()}, {name + ".png" for name in preview.NAMES} | {"provenance.json"})
            self.assertEqual(json.loads((out / "provenance.json").read_text()), facts)
            self.assertNotIn(str(root), json.dumps(facts))
            self.assertEqual(facts["binary_sha256"], hashlib.sha256(binary.read_bytes()).hexdigest())
            for name in preview.NAMES:
                self.assertEqual(facts["images"][name + ".png"]["png_sha256"], hashlib.sha256((out / (name + ".png")).read_bytes()).hexdigest())
                self.assertEqual(facts["images"][name + ".png"]["rgb_sha256"], hashlib.sha256(self.pixels()).hexdigest())
            self.assertTrue(all(path.stat().st_mode & 0o777 == 0o600 for path in out.iterdir()))

    def test_existing_output_extra_raw_file_and_symlink_are_rejected(self):
        for issue in ("existing", "extra", "symlink", "directory"):
            with self.subTest(issue=issue), tempfile.TemporaryDirectory() as folder:
                root = Path(folder).resolve()
                raw, binary = self.fixture(root)
                out = root / "png"
                if issue == "existing":
                    out.mkdir()
                elif issue == "extra":
                    (raw / "private.log").write_text("not an upload input")
                else:
                    target = raw / "sign-in.ppm"
                    target.unlink()
                    if issue == "symlink":
                        target.symlink_to(raw / "desktop-preview.ppm")
                    else:
                        target.mkdir()
                with self.assertRaises((OSError, ValueError)):
                    preview.convert(raw, out, "a" * 40, binary, "123", "1")
                self.assertEqual(out.exists(), issue == "existing")

    def test_invalid_provenance_inputs_fail_before_output_creation(self):
        for sha, run_id, attempt in (("bad", "1", "1"), ("a" * 40, "private-name", "1"),
                                      ("a" * 40, "1", "0")):
            with self.subTest(sha=sha, run_id=run_id, attempt=attempt), tempfile.TemporaryDirectory() as folder:
                root = Path(folder).resolve()
                raw, binary = self.fixture(root)
                with self.assertRaises(ValueError):
                    preview.convert(raw, root / "png", sha, binary, run_id, attempt)
                self.assertFalse((root / "png").exists())
