//! Literal lossless streams exercise the public decoder used by previews.

use std::{error::Error, io::Cursor};

use image::{ImageDecoder, ImageError, codecs::webp::WebPDecoder};

/// Write the least-significant-bit-first fields of a VP8L stream. These fixtures
/// contain only single-symbol prefix codes; no encoder or decoder is executed
/// while preparing them, and the expected pixels are independent constants.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Bits {
    fn field(&mut self, value: u32, width: usize) {
        assert!(width <= 32);
        for bit in 0..width {
            if self.len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            self.bytes[self.len / 8] |= (((value >> bit) & 1) as u8) << (self.len % 8);
            self.len += 1;
        }
    }

    fn single_symbol(&mut self, symbol: u8) {
        self.field(1, 1); // Simple prefix code.
        self.field(0, 1); // One symbol.
        let wide = symbol > 1;
        self.field(u32::from(wide), 1);
        self.field(u32::from(symbol), if wide { 8 } else { 1 });
    }
}

fn lossless_fixture(meta_label: Option<u16>, groups: usize) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.field(0x2f, 8); // VP8L signature.
    bits.field(3, 14); // Four pixels wide.
    bits.field(3, 14); // Four pixels high.
    bits.field(1, 1); // Alpha is present.
    bits.field(0, 3); // Version zero.
    bits.field(0, 1); // No transforms.
    bits.field(0, 1); // No color cache.
    bits.field(u32::from(meta_label.is_some()), 1);
    if let Some(label) = meta_label {
        bits.field(0, 3); // One entropy-image pixel for the entire 4x4 image.
        bits.field(0, 1); // Auxiliary stream has no color cache or meta image.
        // Tree order is green, red, blue, alpha, distance. A meta label is
        // encoded in the red and green components of this auxiliary pixel.
        for symbol in [label as u8, (label >> 8) as u8, 0, 255, 0] {
            bits.single_symbol(symbol);
        }
    }
    for _ in 0..groups {
        for symbol in [17, 34, 51, 255, 0] {
            bits.single_symbol(symbol);
        }
    }

    let payload = bits.bytes;
    let mut body = b"WEBPVP8L".to_vec();
    body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    body.extend_from_slice(&payload);
    if !payload.len().is_multiple_of(2) {
        body.push(0);
    }
    let mut riff = b"RIFF".to_vec();
    riff.extend_from_slice(&(body.len() as u32).to_le_bytes());
    riff.extend_from_slice(&body);
    riff
}

fn decode_fixture(bytes: &[u8]) -> image::ImageResult<Vec<u8>> {
    let decoder = WebPDecoder::new(Cursor::new(bytes))?;
    assert_eq!(decoder.dimensions(), (4, 4));
    assert_eq!(decoder.total_bytes(), 64);
    let mut pixels = vec![0; 64];
    decoder.read_image(&mut pixels)?;
    Ok(pixels)
}

#[test]
fn lossless_meta_huffman_budget_rejects_before_missing_group_data() {
    // Both streams end after their small, valid entropy image. The decoder must
    // report the complexity rejection before trying to read any outer trees;
    // an ordinary truncated-stream error would not establish that ordering.
    for label in [128, u16::MAX] {
        let bytes = lossless_fixture(Some(label), 0);
        assert!(bytes.len() < 64);
        match decode_fixture(&bytes).unwrap_err() {
            ImageError::Decoding(error) => {
                assert_eq!(error.source().unwrap().to_string(), "Memory limit exceeded");
            }
            error => panic!("unexpected rejection: {error}"),
        }
    }
}

#[test]
fn lossless_meta_huffman_budget_keeps_the_last_admitted_group() {
    let pixels = decode_fixture(&lossless_fixture(Some(127), 128)).unwrap();
    assert_eq!(pixels, [34, 17, 51, 255].repeat(16));
}

#[test]
fn lossless_without_meta_huffman_image_remains_supported() {
    let pixels = decode_fixture(&lossless_fixture(None, 1)).unwrap();
    assert_eq!(pixels, [34, 17, 51, 255].repeat(16));
}
