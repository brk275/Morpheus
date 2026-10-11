//! Memory-only PNG/baseline-JPEG decoding. Framing/dimension guards run before
//! decoder construction; metadata is discarded without decompression. This
//! deliberately excludes animation, progressive JPEG, ICC/EXIF and WebP.
use super::{Pixels, PreviewError};
use image::{ColorType, ImageDecoder, Limits};
use std::io::Cursor;
use zeroize::Zeroizing;

pub(super) const MAX_SIDE: u32 = 4096;
pub(super) const MAX_PIXELS: usize = 4 * 1024 * 1024;
const MAX_DECODER_ALLOC: u64 = 32 * 1024 * 1024;

fn size(width: u32, height: u32) -> Result<usize, PreviewError> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or(PreviewError::TooLarge)?;
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(PreviewError::TooLarge);
    }
    Ok(pixels * 4)
}

fn png_input(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, PreviewError> {
    let mut position = 8usize;
    let mut output = Zeroizing::new(Vec::new());
    let mut chunks = 0;
    let mut ended = false;
    let mut palette = false;
    let mut transparency = false;
    // Validate IHDR before even allocating the bounded sanitized input.
    if bytes.get(8..16) != Some(&b"\0\0\0\rIHDR"[..]) || bytes.len() < 33 {
        return Err(PreviewError::Invalid);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    size(width, height)?;
    if bytes[24] > 8 {
        return Err(PreviewError::Unsupported);
    }
    output.extend_from_slice(&bytes[..8]);
    while position < bytes.len() {
        chunks += 1;
        if chunks > 4096 || ended {
            return Err(PreviewError::Invalid);
        }
        let header = bytes
            .get(position..position + 8)
            .ok_or(PreviewError::Invalid)?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let end = position
            .checked_add(12)
            .and_then(|p| p.checked_add(length))
            .ok_or(PreviewError::Invalid)?;
        let chunk = bytes.get(position..end).ok_or(PreviewError::Invalid)?;
        let kind = &header[4..8];
        if matches!(kind, b"acTL" | b"fcTL" | b"fdAT") {
            return Err(PreviewError::Unsupported);
        }
        if kind == b"IHDR" && chunks != 1 {
            return Err(PreviewError::Invalid);
        }
        if kind == b"PLTE" {
            if palette || length > 768 {
                return Err(PreviewError::Invalid);
            }
            palette = true;
        }
        if kind == b"tRNS" {
            if transparency || length > 256 {
                return Err(PreviewError::Invalid);
            }
            transparency = true;
        }
        match kind {
            b"IHDR" | b"PLTE" | b"IDAT" | b"IEND" | b"tRNS" => output.extend_from_slice(chunk),
            _ if kind[0].is_ascii_lowercase() => {} // Ancillary metadata is never decompressed.
            _ => return Err(PreviewError::Unsupported),
        }
        ended = kind == b"IEND";
        if ended && length != 0 {
            return Err(PreviewError::Invalid);
        }
        position = end;
    }
    if !ended {
        return Err(PreviewError::Invalid);
    }
    Ok(output)
}

fn jpeg_input(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, PreviewError> {
    let mut output = Zeroizing::new(vec![0xff, 0xd8]);
    let mut position = 2usize;
    let mut components = None;
    for _ in 0..1024 {
        let start = position;
        if bytes.get(position) != Some(&0xff) {
            return Err(PreviewError::Invalid);
        }
        while bytes.get(position) == Some(&0xff) {
            position += 1;
        }
        let marker = *bytes.get(position).ok_or(PreviewError::Invalid)?;
        position += 1;
        let length = bytes
            .get(position..position + 2)
            .ok_or(PreviewError::Invalid)?;
        let length = u16::from_be_bytes(length.try_into().unwrap()) as usize;
        if length < 2 {
            return Err(PreviewError::Invalid);
        }
        let end = position.checked_add(length).ok_or(PreviewError::Invalid)?;
        let payload = bytes.get(position + 2..end).ok_or(PreviewError::Invalid)?;
        match marker {
            0xc0 => {
                if components.is_some() || payload.len() < 6 || payload[0] != 8 {
                    return Err(PreviewError::Unsupported);
                }
                let count = payload[5] as usize;
                if !matches!(count, 1 | 3) || payload.len() != 6 + 3 * count {
                    return Err(PreviewError::Unsupported);
                }
                size(
                    u16::from_be_bytes(payload[3..5].try_into().unwrap()) as u32,
                    u16::from_be_bytes(payload[1..3].try_into().unwrap()) as u32,
                )?;
                for component in payload[6..].chunks_exact(3) {
                    if !(1..=2).contains(&(component[1] >> 4))
                        || !(1..=2).contains(&(component[1] & 15))
                    {
                        return Err(PreviewError::Unsupported);
                    }
                }
                components = Some(count);
            }
            0xda => {
                let count = components.ok_or(PreviewError::Invalid)?;
                if payload.len() != 1 + 2 * count + 3
                    || payload[0] as usize != count
                    || payload[payload.len() - 3..] != [0, 63, 0]
                {
                    return Err(PreviewError::Unsupported);
                }
                output.extend_from_slice(&bytes[start..end]);
                let mut cursor = end;
                while cursor < bytes.len() {
                    if bytes[cursor] != 0xff {
                        cursor += 1;
                        continue;
                    }
                    cursor += 1;
                    while bytes.get(cursor) == Some(&0xff) {
                        cursor += 1;
                    }
                    match bytes.get(cursor) {
                        Some(0x00 | 0xd0..=0xd7) => cursor += 1,
                        Some(0xd9) if cursor + 1 == bytes.len() => {
                            output.extend_from_slice(&bytes[end..]);
                            return Ok(output);
                        }
                        _ => return Err(PreviewError::Unsupported), // No later scans/headers/metadata.
                    }
                }
                return Err(PreviewError::Invalid);
            }
            0xdb | 0xc4 | 0xdd => {} // Quantization, Huffman and restart tables.
            0xe0 | 0xee if payload.len() <= 32 => {} // Small JFIF/Adobe colour hints.
            0xe0..=0xef | 0xfe => {
                position = end;
                continue;
            } // Discard metadata before decoder parsing.
            _ => return Err(PreviewError::Unsupported),
        }
        output.extend_from_slice(&bytes[start..end]);
        position = end;
    }
    Err(PreviewError::Invalid)
}

pub(super) fn decode(bytes: Zeroizing<Vec<u8>>) -> Result<Pixels, PreviewError> {
    if bytes.len() > morpheus_matrix::MAX_IMAGE_PREVIEW_BYTES {
        return Err(PreviewError::TooLarge);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let input = png_input(&bytes)?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_SIDE);
        limits.max_image_height = Some(MAX_SIDE);
        limits.max_alloc = Some(MAX_DECODER_ALLOC);
        let decoder =
            image::codecs::png::PngDecoder::with_limits(Cursor::new(input.as_slice()), limits)
                .map_err(|_| PreviewError::Invalid)?;
        let (width, height) = decoder.dimensions();
        let length = size(width, height)?;
        let color = decoder.color_type();
        let channels = match color {
            ColorType::L8 => 1,
            ColorType::La8 => 2,
            ColorType::Rgb8 => 3,
            ColorType::Rgba8 => 4,
            _ => return Err(PreviewError::Unsupported),
        };
        let mut raw = Zeroizing::new(vec![0; length / 4 * channels]);
        if decoder.total_bytes() != raw.len() as u64 {
            return Err(PreviewError::Invalid);
        }
        decoder
            .read_image(&mut raw)
            .map_err(|_| PreviewError::Invalid)?;
        let mut rgba = Zeroizing::new(Vec::with_capacity(length));
        for pixel in raw.chunks_exact(channels) {
            match channels {
                1 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
                2 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]),
                3 => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
                _ => rgba.extend_from_slice(pixel),
            }
        }
        Ok(Pixels {
            width,
            height,
            rgba,
        })
    } else if bytes.starts_with(b"\xff\xd8") {
        let input = jpeg_input(&bytes)?;
        let options = zune_core::options::DecoderOptions::default()
            .set_max_width(MAX_SIDE as usize)
            .set_max_height(MAX_SIDE as usize)
            .set_strict_mode(true)
            .jpeg_set_max_scans(1)
            .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGBA);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
            zune_core::bytestream::ZCursor::new(input.as_slice()),
            options,
        );
        decoder
            .decode_headers()
            .map_err(|_| PreviewError::Invalid)?;
        let (width, height) = decoder.dimensions().ok_or(PreviewError::Invalid)?;
        let length = size(width as u32, height as u32)?;
        if decoder.output_buffer_size() != Some(length) {
            return Err(PreviewError::Invalid);
        }
        let mut rgba = Zeroizing::new(vec![0; length]);
        decoder
            .decode_into(&mut rgba)
            .map_err(|_| PreviewError::Invalid)?;
        Ok(Pixels {
            width: width as u32,
            height: height as u32,
            rgba,
        })
    } else {
        Err(PreviewError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    #[test]
    fn valid_png_pixels_decode_but_metadata_is_not_parsed() {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[12, 34, 56, 78], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        // Invalid compressed text metadata is discarded; its decoder is never entered.
        let mut metadata = vec![0, 0, 0, 4];
        metadata.extend_from_slice(b"zTXt");
        metadata.extend_from_slice(&[255; 8]);
        bytes.splice(33..33, metadata);
        let pixels = decode(Zeroizing::new(bytes)).unwrap();
        assert_eq!(&*pixels.rgba, &[12, 34, 56, 78]);
    }
    #[test]
    fn baseline_jpeg_decodes_color_and_grayscale_with_bounded_rgba_output() {
        for (data, color) in [
            (&[230, 40, 20][..], image::ExtendedColorType::Rgb8),
            (&[128][..], image::ExtendedColorType::L8),
        ] {
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
                .encode(data, 1, 1, color)
                .unwrap();
            let pixels = decode(Zeroizing::new(bytes)).unwrap();
            assert_eq!((pixels.width, pixels.height, pixels.rgba.len()), (1, 1, 4));
            assert_eq!(pixels.rgba[3], 255);
            if data.len() == 1 {
                assert!(
                    pixels.rgba[..3]
                        .iter()
                        .all(|&channel| channel.abs_diff(128) <= 3)
                );
            } else {
                assert!(pixels.rgba[0] > 200 && pixels.rgba[1] < 60 && pixels.rgba[2] < 40);
            }
        }
    }
    #[test]
    fn animation_and_oversized_palette_are_rejected_before_png_decoder() {
        let mut original = Vec::new();
        image::codecs::png::PngEncoder::new(&mut original)
            .write_image(&[1, 2, 3, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        for (kind, length) in [(b"acTL", 8usize), (b"PLTE", 769), (b"tRNS", 257)] {
            let mut bytes = original.clone();
            let mut chunk = (length as u32).to_be_bytes().to_vec();
            chunk.extend_from_slice(kind);
            chunk.resize(length + 12, 0);
            bytes.splice(33..33, chunk);
            assert!(png_input(&bytes).is_err());
        }
    }
    #[test]
    fn dimensions_reject_before_decoder_or_output_allocation() {
        assert!(size(4096, 1024).is_ok());
        for (w, h) in [(0, 1), (4097, 1), (4096, 4096), (u32::MAX, u32::MAX)] {
            assert_eq!(size(w, h), Err(PreviewError::TooLarge));
        }
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 9]);
        assert!(matches!(
            decode(Zeroizing::new(bytes)),
            Err(PreviewError::TooLarge)
        ));
    }
    #[test]
    fn unsupported_and_truncated_inputs_never_reach_pixel_display() {
        for bytes in [
            b"<svg/>".to_vec(),
            b"GIF89a".to_vec(),
            b"RIFFwebp".to_vec(),
            b"\xff\xd8\xff\xc2\0\x02".to_vec(),
        ] {
            assert!(decode(Zeroizing::new(bytes)).is_err());
        }
    }
    #[test]
    fn jpeg_metadata_is_removed_and_later_header_injection_rejected() {
        let mut source = vec![0xff, 0xd8, 0xff, 0xe1, 0, 6, 1, 2, 3, 4];
        source.extend_from_slice(&[0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
        source.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0xff, 0xd9]);
        let bounded = jpeg_input(&source).unwrap();
        assert!(!bounded.windows(4).any(|w| w == [1, 2, 3, 4]));
        source.truncate(source.len() - 2);
        source.extend_from_slice(&[0xff, 0xe1, 0, 6, 1, 2, 3, 4, 0xff, 0xd9]);
        assert!(jpeg_input(&source).is_err());
    }
}
