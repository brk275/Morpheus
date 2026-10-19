//! Memory-only PNG and baseline/progressive JPEG decoding. Framing/dimension
//! guards run before decoder construction; metadata is discarded without
//! decompression. Still decoding deliberately excludes animation, arithmetic
//! coding, ICC/EXIF and WebP; `animation` plays GIF, WebP and APNG through `png_animation_input`.
//! The one EXIF value read is a JPEG's orientation, taken from the first Exif
//! segment before that segment is discarded, so camera photos show upright.
use super::{Pixels, PreviewError};
use image::{ColorType, ImageDecoder, Limits, metadata::Orientation};
use std::io::Cursor;
use zeroize::Zeroizing;

pub(super) const MAX_SIDE: u32 = 4096;
pub(super) const MAX_PIXELS: usize = 4 * 1024 * 1024;
const MAX_DECODER_ALLOC: u64 = 32 * 1024 * 1024;
/// Frames one animation pass may contain or decode.
pub(super) const MAX_ANIMATION_FRAMES: usize = 1000;
/// Scans one progressive JPEG may contain; its decoder gets the same cap.
const MAX_JPEG_SCANS: usize = 64;

pub(super) fn size(width: u32, height: u32) -> Result<usize, PreviewError> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or(PreviewError::TooLarge)?;
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(PreviewError::TooLarge);
    }
    Ok(pixels * 4)
}

fn png_input(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, PreviewError> {
    png_input_with(bytes, false)
}

/// Sanitized APNG input for the animation player: the still framing plus the
/// animation control and frame chunks, with one acTL before any image data
/// and at most `MAX_ANIMATION_FRAMES` frames.
pub(super) fn png_animation_input(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, PreviewError> {
    if bytes.len() > morpheus_matrix::MAX_IMAGE_PREVIEW_BYTES {
        return Err(PreviewError::TooLarge);
    }
    png_input_with(bytes, true)
}

/// Whether a PNG declares an animation (acTL before its first IDAT). Only
/// chunk headers are read, at most 4096 of them.
pub(super) fn png_animated(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let mut position = 8usize;
    for _ in 0..4096 {
        let Some(header) = bytes.get(position..position + 8) else {
            return false;
        };
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        match &header[4..8] {
            b"acTL" => return true,
            b"IDAT" | b"IEND" => return false,
            _ => {}
        }
        let Some(next) = position
            .checked_add(12)
            .and_then(|position| position.checked_add(length))
        else {
            return false;
        };
        position = next;
    }
    false
}

fn png_input_with(bytes: &[u8], animation: bool) -> Result<Zeroizing<Vec<u8>>, PreviewError> {
    let mut position = 8usize;
    let mut output = Zeroizing::new(Vec::new());
    let mut chunks = 0;
    let mut ended = false;
    let mut palette = false;
    let mut transparency = false;
    let mut control = false;
    let mut frames = 0usize;
    let mut image_data = false;
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
        if matches!(kind, b"acTL" | b"fcTL" | b"fdAT") && !animation {
            return Err(PreviewError::Unsupported);
        }
        if kind == b"acTL" {
            if control || image_data || length != 8 {
                return Err(PreviewError::Invalid);
            }
            let declared = u32::from_be_bytes(chunk[8..12].try_into().unwrap()) as usize;
            if declared == 0 || declared > MAX_ANIMATION_FRAMES {
                return Err(PreviewError::TooLarge);
            }
            control = true;
        }
        if kind == b"fcTL" {
            frames += 1;
            if !control || length != 26 || frames > MAX_ANIMATION_FRAMES {
                return Err(PreviewError::Invalid);
            }
        }
        if kind == b"fdAT" && !control {
            return Err(PreviewError::Invalid);
        }
        image_data |= kind == b"IDAT";
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
            b"acTL" | b"fcTL" | b"fdAT" if animation => output.extend_from_slice(chunk),
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

/// The sanitized decoder input and the orientation of the first Exif segment
/// (upright when absent or unreadable). Baseline frames have exactly one
/// scan. Progressive (SOF2, Huffman) frames may have up to `MAX_JPEG_SCANS`,
/// with only Huffman and restart tables between them. Nothing else may follow
/// the first scan.
fn jpeg_input(bytes: &[u8]) -> Result<(Zeroizing<Vec<u8>>, Orientation), PreviewError> {
    let mut output = Zeroizing::new(vec![0xff, 0xd8]);
    let mut position = 2usize;
    // Component count, and whether the frame is progressive.
    let mut frame = None;
    let mut orientation = None;
    let mut scans = 0usize;
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
        if scans > 0 && !matches!(marker, 0xc4 | 0xdd | 0xda) {
            return Err(PreviewError::Unsupported); // No later headers or metadata.
        }
        match marker {
            0xc0 | 0xc2 => {
                if frame.is_some() || payload.len() < 6 || payload[0] != 8 {
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
                frame = Some((count, marker == 0xc2));
            }
            0xda => {
                let (count, progressive) = frame.ok_or(PreviewError::Invalid)?;
                scan_header(payload, count, progressive)?;
                scans += 1;
                let limit = if progressive { MAX_JPEG_SCANS } else { 1 };
                if scans > limit {
                    return Err(PreviewError::TooLarge);
                }
                output.extend_from_slice(&bytes[start..end]);
                let (next, following, at) = entropy_end(bytes, end)?;
                if following == 0xd9 && at + 1 == bytes.len() {
                    output.extend_from_slice(&bytes[end..]);
                    let orientation = orientation.unwrap_or(Orientation::NoTransforms);
                    return Ok((output, orientation));
                }
                // A baseline scan ends the image; a progressive one may be
                // followed only by tables and its next scan.
                if !progressive || !matches!(following, 0xc4 | 0xdd | 0xda) {
                    return Err(PreviewError::Unsupported);
                }
                output.extend_from_slice(&bytes[end..next]);
                position = next;
                continue;
            }
            0xdb | 0xc4 | 0xdd => {} // Quantization, Huffman and restart tables.
            0xe0 | 0xee if payload.len() <= 32 => {} // Small JFIF/Adobe colour hints.
            // Only the first Exif segment's orientation tag is read (a bounded
            // IFD0 scan of at most one 64 KiB segment); the segment itself is
            // discarded like any other metadata.
            0xe1 if orientation.is_none() && payload.starts_with(b"Exif\0\0") => {
                orientation = Some(
                    Orientation::from_exif_chunk(&payload[6..])
                        .unwrap_or(Orientation::NoTransforms),
                );
                position = end;
                continue;
            }
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

/// A scan header. Baseline: one interleaved scan of every component over the
/// full spectrum. Progressive: a DC scan (spectrum 0..0) of up to every
/// component, or an AC scan (1..=63) of exactly one, with approximation bits
/// within the 8-bit range.
fn scan_header(payload: &[u8], components: usize, progressive: bool) -> Result<(), PreviewError> {
    let count = *payload.first().ok_or(PreviewError::Invalid)? as usize;
    if payload.len() != 1 + 2 * count + 3 {
        return Err(PreviewError::Unsupported);
    }
    let tail = &payload[payload.len() - 3..];
    let (first, last, approximation) = (tail[0], tail[1], tail[2]);
    if !progressive {
        return if count == components && tail == [0, 63, 0] {
            Ok(())
        } else {
            Err(PreviewError::Unsupported)
        };
    }
    let dc = first == 0;
    if count == 0
        || count > components
        || first > last
        || last > 63
        || dc != (last == 0)
        || (!dc && count != 1)
        || approximation >> 4 > 13
        || approximation & 15 > 13
    {
        return Err(PreviewError::Unsupported);
    }
    Ok(())
}

/// The end of entropy-coded data starting at `from`: the index of the 0xff
/// that begins the next real marker, that marker, and the marker's index.
/// Byte stuffing and restart markers belong to the data.
fn entropy_end(bytes: &[u8], from: usize) -> Result<(usize, u8, usize), PreviewError> {
    let mut cursor = from;
    while cursor < bytes.len() {
        if bytes[cursor] != 0xff {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while bytes.get(cursor) == Some(&0xff) {
            cursor += 1;
        }
        match bytes.get(cursor) {
            Some(0x00 | 0xd0..=0xd7) => cursor += 1,
            Some(&marker) => return Ok((start, marker, cursor)),
            None => break,
        }
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
        let (input, orientation) = jpeg_input(&bytes)?;
        let options = zune_core::options::DecoderOptions::default()
            .set_max_width(MAX_SIDE as usize)
            .set_max_height(MAX_SIDE as usize)
            .set_strict_mode(true)
            .jpeg_set_max_scans(MAX_JPEG_SCANS)
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
        let pixels = Pixels {
            width: width as u32,
            height: height as u32,
            rgba,
        };
        Ok(orient(pixels, orientation))
    } else {
        Err(PreviewError::Unsupported)
    }
}

/// Turn decoded RGBA rows upright for an Exif orientation. The copy goes to a
/// fresh zeroizing buffer of the same size, so the pixel bounds still hold;
/// the quarter turns swap width and height.
fn orient(pixels: Pixels, orientation: Orientation) -> Pixels {
    if orientation == Orientation::NoTransforms {
        return pixels;
    }
    let (w, h) = (pixels.width as usize, pixels.height as usize);
    let swap = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    let out_w = if swap { h } else { w };
    let mut rgba = Zeroizing::new(vec![0; pixels.rgba.len()]);
    for (index, pixel) in pixels.rgba.chunks_exact(4).enumerate() {
        let (x, y) = (index % w, index / w);
        let (dx, dy) = match orientation {
            Orientation::NoTransforms => (x, y),
            Orientation::FlipHorizontal => (w - 1 - x, y),
            Orientation::Rotate180 => (w - 1 - x, h - 1 - y),
            Orientation::FlipVertical => (x, h - 1 - y),
            // Clockwise quarter turn: the left column becomes the top row.
            Orientation::Rotate90 => (h - 1 - y, x),
            Orientation::Rotate270 => (y, w - 1 - x),
            // A quarter turn, then a horizontal flip: transpose and transverse.
            Orientation::Rotate90FlipH => (y, x),
            Orientation::Rotate270FlipH => (h - 1 - y, w - 1 - x),
        };
        let target = (dy * out_w + dx) * 4;
        rgba[target..target + 4].copy_from_slice(pixel);
    }
    let (width, height) = if swap {
        (pixels.height, pixels.width)
    } else {
        (pixels.width, pixels.height)
    };
    Pixels {
        width,
        height,
        rgba,
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
    fn animation_chunks_pass_only_the_bounded_animation_sanitizer() {
        let mut original = Vec::new();
        image::codecs::png::PngEncoder::new(&mut original)
            .write_image(&[1, 2, 3, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let control = |frames: u32| {
            let mut chunk = 8u32.to_be_bytes().to_vec();
            chunk.extend_from_slice(b"acTL");
            chunk.extend_from_slice(&frames.to_be_bytes());
            chunk.extend_from_slice(&0u32.to_be_bytes());
            chunk.extend_from_slice(&[0; 4]);
            chunk
        };
        let mut animated = original.clone();
        animated.splice(33..33, control(2));
        assert!(png_animated(&animated) && !png_animated(&original));
        assert!(png_input(&animated).is_err());
        let kept = png_animation_input(&animated).unwrap();
        assert!(kept.windows(4).any(|window| window == b"acTL"));
        let mut too_many = original.clone();
        too_many.splice(33..33, control(MAX_ANIMATION_FRAMES as u32 + 1));
        assert!(matches!(
            png_animation_input(&too_many),
            Err(PreviewError::TooLarge)
        ));
        let mut twice = animated.clone();
        twice.splice(33..33, control(2));
        assert!(png_animation_input(&twice).is_err());
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
    fn pixels(width: u32, height: u32, values: &[u8]) -> Pixels {
        Pixels {
            width,
            height,
            rgba: Zeroizing::new(values.iter().flat_map(|&v| [v, v, v, 255]).collect()),
        }
    }
    fn values(pixels: &Pixels) -> Vec<u8> {
        pixels.rgba.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }
    #[test]
    fn every_exif_orientation_turns_pixels_upright() {
        // a b / c d, stored row by row.
        for (orientation, expected) in [
            (Orientation::NoTransforms, [1, 2, 3, 4]),
            (Orientation::FlipHorizontal, [2, 1, 4, 3]),
            (Orientation::Rotate180, [4, 3, 2, 1]),
            (Orientation::FlipVertical, [3, 4, 1, 2]),
            (Orientation::Rotate90, [3, 1, 4, 2]),
            (Orientation::Rotate270, [2, 4, 1, 3]),
            (Orientation::Rotate90FlipH, [1, 3, 2, 4]),
            (Orientation::Rotate270FlipH, [4, 2, 3, 1]),
        ] {
            let turned = orient(pixels(2, 2, &[1, 2, 3, 4]), orientation);
            assert_eq!(values(&turned), expected, "{orientation:?}");
        }
        // Quarter turns swap the dimensions of a wide image.
        let tall = orient(pixels(3, 1, &[1, 2, 3]), Orientation::Rotate90);
        assert_eq!((tall.width, tall.height, values(&tall)), (1, 3, vec![1, 2, 3]));
        let tall = orient(pixels(3, 1, &[1, 2, 3]), Orientation::Rotate270);
        assert_eq!((tall.width, tall.height, values(&tall)), (1, 3, vec![3, 2, 1]));
    }
    #[test]
    fn only_the_first_exif_orientation_is_read_and_metadata_still_removed() {
        let exif = |value: u8| {
            // "Exif\0\0", big-endian TIFF header, IFD0 at 8 with one SHORT
            // orientation entry, no next IFD.
            let mut payload = b"Exif\0\0MM\0*\0\0\0\x08\0\x01".to_vec();
            payload.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, value, 0, 0]);
            payload.extend_from_slice(&[0, 0, 0, 0]);
            let mut segment = vec![0xff, 0xe1];
            segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            segment.extend_from_slice(&payload);
            segment
        };
        let frame = [
            &[0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0][..],
            &[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0xff, 0xd9][..],
        ]
        .concat();
        let mut source = vec![0xff, 0xd8];
        source.extend_from_slice(&exif(6));
        source.extend_from_slice(&exif(3));
        source.extend_from_slice(&frame);
        let (bounded, orientation) = jpeg_input(&source).unwrap();
        assert_eq!(orientation, Orientation::Rotate90);
        assert!(!bounded.windows(4).any(|w| w == b"Exif"));
        // An unknown value leaves the image as stored.
        let mut source = vec![0xff, 0xd8];
        source.extend_from_slice(&exif(9));
        source.extend_from_slice(&frame);
        assert_eq!(jpeg_input(&source).unwrap().1, Orientation::NoTransforms);
    }
    // SOI, a one-component 1x1 frame (`frame` is 0xc0 or 0xc2), then each
    // scan as (scan header tail, entropy bytes), each preceded by `between`.
    fn jpeg(frame: u8, scans: &[([u8; 3], &[u8])], between: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, frame, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0];
        for (tail, data) in scans {
            bytes.extend_from_slice(between);
            bytes.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0]);
            bytes.extend_from_slice(tail);
            bytes.extend_from_slice(data);
        }
        bytes.extend_from_slice(&[0xff, 0xd9]);
        bytes
    }
    const HUFFMAN: &[u8] = &[0xff, 0xc4, 0, 3, 0];
    #[test]
    fn progressive_scans_pass_with_tables_between_them() {
        let dc = ([0, 0, 0], &[0x12, 0xff, 0x00, 0x34][..]);
        let ac = ([1, 63, 0], &[0x56, 0xff, 0xd0, 0x78][..]);
        let refine = ([1, 63, 0x10], &[0x9a][..]);
        let source = jpeg(0xc2, &[dc, ac, refine], HUFFMAN);
        let (bounded, _) = jpeg_input(&source).unwrap();
        // Every scan, its stuffed and restart bytes and every table are kept.
        assert_eq!(bounded.as_slice(), source.as_slice());
        // A baseline frame still has exactly one full-spectrum scan.
        assert!(jpeg_input(&jpeg(0xc0, &[([0, 63, 0], &[1u8][..])], &[])).is_ok());
        assert!(jpeg_input(&jpeg(0xc0, &[([0, 63, 0], &[1u8][..]); 2], &[])).is_err());
        assert!(jpeg_input(&jpeg(0xc0, &[([0, 0, 0], &[1u8][..])], &[])).is_err());
    }
    #[test]
    fn progressive_framing_rejects_metadata_bad_scans_and_too_many_scans() {
        let dc = ([0, 0, 0], &[1u8][..]);
        // Metadata after the first scan never reaches the decoder.
        let comment = [0xff, 0xfe, 0, 3, b'x'];
        let late = jpeg(0xc2, &[dc, ([1, 63, 0], &[2u8][..])], &comment);
        assert!(matches!(jpeg_input(&late), Err(PreviewError::Unsupported)));
        for tail in [[1, 0, 0], [0, 5, 0], [2, 64, 0], [1, 63, 0xe0], [1, 63, 0x0e]] {
            let bad = jpeg(0xc2, &[(tail, &[1u8][..])], &[]);
            assert!(jpeg_input(&bad).is_err(), "{tail:?}");
        }
        let many = vec![dc; MAX_JPEG_SCANS + 1];
        assert!(matches!(
            jpeg_input(&jpeg(0xc2, &many, HUFFMAN)),
            Err(PreviewError::TooLarge)
        ));
        assert!(jpeg_input(&jpeg(0xc2, &many[..MAX_JPEG_SCANS], HUFFMAN)).is_ok());
        // An AC scan covers exactly one component.
        let mut colour = vec![0xff, 0xd8, 0xff, 0xc2, 0, 17, 8, 0, 1, 0, 1, 3];
        colour.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        colour.extend_from_slice(&[0xff, 0xda, 0, 10, 2, 1, 0, 2, 0, 1, 63, 0, 1]);
        colour.extend_from_slice(&[0xff, 0xd9]);
        assert!(jpeg_input(&colour).is_err());
    }
    #[test]
    fn jpeg_metadata_is_removed_and_later_header_injection_rejected() {
        let mut source = vec![0xff, 0xd8, 0xff, 0xe1, 0, 6, 1, 2, 3, 4];
        source.extend_from_slice(&[0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
        source.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0xff, 0xd9]);
        let (bounded, orientation) = jpeg_input(&source).unwrap();
        assert!(!bounded.windows(4).any(|w| w == [1, 2, 3, 4]));
        assert_eq!(orientation, Orientation::NoTransforms);
        source.truncate(source.len() - 2);
        source.extend_from_slice(&[0xff, 0xe1, 0, 6, 1, 2, 3, 4, 0xff, 0xd9]);
        assert!(jpeg_input(&source).is_err());
    }
}
