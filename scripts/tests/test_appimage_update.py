import json
import os
import struct
import tempfile
import unittest
from pathlib import Path

from scripts.prepare_appimage_update import MAGIC, prepare_appimage, validate_metadata


METADATA = {
    "protocol": 1, "product": "io.github.brk275.morpheus",
    "version": "0.2.0", "arch": "x86_64", "persistence": "a" * 64,
}


def image_bytes():
    image = bytearray(64)
    image[:6] = b"\x7fELF\x02\x01"
    image[8:11] = b"AI\x02"
    image[18:20] = b"\x3e\x00"
    return bytes(image) + b"squashfs fixture bytes"


class AppImageMetadataTests(unittest.TestCase):
    def test_metadata_rejects_wrong_product_architecture_or_unbounded_content(self):
        for key, value in [("product", "other"), ("arch", "aarch64"), ("persistence", "a" * 65), ("version", "v" * 5000)]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                validate_metadata(dict(METADATA, **{key: value}))

    @unittest.skipUnless(os.name == "posix", "Linux packaging requires POSIX file/directory fsync")
    def test_footer_is_bounded_repeatable_and_preserves_original_image(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "Morpheus.AppImage"
            original = image_bytes()
            path.write_bytes(original)
            path.chmod(0o755)
            prepare_appimage(path, METADATA)
            once = path.read_bytes()
            prepare_appimage(path, METADATA)
            self.assertEqual(path.read_bytes(), once)
            self.assertEqual(path.stat().st_mode & 0o777, 0o755)
            self.assertTrue(once.startswith(original))
            self.assertTrue(once.endswith(MAGIC))
            end = len(once) - len(MAGIC) - 4
            size = struct.unpack("<I", once[end:end + 4])[0]
            self.assertLessEqual(size, 4096)
            self.assertEqual(json.loads(once[end - size:end]), METADATA)

    @unittest.skipUnless(os.name == "posix", "Linux packaging test")
    def test_existing_official_signature_prevents_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "Morpheus.AppImage"
            original = image_bytes()
            path.write_bytes(original)
            path.with_name(path.name + ".sig").write_text("synthetic existing signature")
            with self.assertRaises(ValueError):
                prepare_appimage(path, METADATA)
            self.assertEqual(path.read_bytes(), original)

    @unittest.skipUnless(os.name == "posix", "Linux packaging test")
    def test_symlink_and_malformed_footer_preserve_original_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "Morpheus.AppImage"
            path.write_bytes(image_bytes())
            link = Path(temporary) / "link.AppImage"
            link.symlink_to(path)
            with self.assertRaises(ValueError):
                prepare_appimage(link, METADATA)
            broken = image_bytes() + struct.pack("<I", 2**32 - 1) + MAGIC
            path.write_bytes(broken)
            with self.assertRaises(ValueError):
                prepare_appimage(path, METADATA)
            self.assertEqual(path.read_bytes(), broken)


if __name__ == "__main__":
    unittest.main()
