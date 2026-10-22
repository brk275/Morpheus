//! Memory-only PNG and baseline/progressive JPEG decoding. Framing/dimension
//! guards run before decoder construction; metadata is discarded without
//! decompression. Still decoding deliberately excludes animation, arithmetic
//! coding, ICC/EXIF, GIF and WebP; the viewer's `animation` module decodes
//! GIF and WebP, and APNG through `png_animation_input`. The one EXIF value
//! read is the orientation of a JPEG's first Exif segment or a still PNG's
//! first eXIf chunk, before it is discarded, so camera photos show upright.
use super::{Pixels, PreviewError};
use image::{ColorType, ImageDecoder, Limits, metadata::Orientation};
use std::io::Cursor;
use zeroize::Zeroizing;

pub(super) const MAX_SIDE: u32 = 4096;
pub(super) const MAX_PIXELS: usize = 4 * 1024 * 1024;
const MAX_DECODER_ALLOC: u64 = 32 * 1024 * 1024;
/// Frames one animation pass may contain or decode.
pub(super) const MAX_ANIMATION_FRAMES: usize = 1000;
/// Chunks an animated PNG may contain (a still PNG may have 4096).
const MAX_ANIMATION_CHUNKS: usize = 16 * 1024;
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
    png_input_with(bytes, false).map(|(input, _)| input)
}

/// Sanitized APNG input for the animation player and its frame count: the
/// still framing plus the animation control and frame chunks, with one acTL
/// before any image data declaring at most `MAX_ANIMATION_FRAMES` frames,
/// exactly that many fcTL frames, each a rectangle inside the image with
/// valid dispose and blend operations, and a frame before the image data only
/// when it is the whole image.
pub(super) fn png_animation_input(
    bytes: &[u8],
) -> Result<(Zeroizing<Vec<u8>>, usize), PreviewError> {
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

fn png_input_with(
    bytes: &[u8],
    animation: bool,
) -> Result<(Zeroizing<Vec<u8>>, usize), PreviewError> {
    let mut position = 8usize;
    let mut output = Zeroizing::new(Vec::new());
    let mut chunks = 0;
    let mut ended = false;
    let mut palette = false;
    let mut transparency = false;
    let mut control = false;
    let mut declared = 0usize;
    let mut frames = 0usize;
    let mut image_data = false;
    // Validate IHDR before even allocating the bounded sanitized input.
    if bytes.get(8..16) != Some(&b"\0\0\0\rIHDR"[..]) || bytes.len() < 33 {
        return Err(PreviewError::Invalid);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    size(width, height)?;
    // Still images may use 16-bit samples, which are narrowed after decoding;
    // animation frames stay 8-bit.
    if bytes[24] > 16 || (animation && bytes[24] > 8) {
        return Err(PreviewError::Unsupported);
    }
    output.extend_from_slice(&bytes[..8]);
    // An animation may split each of its up to 1000 frames over several fdAT
    // chunks. The walk stays linear and the input is at most 8 MiB.
    let max_chunks = if animation {
        MAX_ANIMATION_CHUNKS
    } else {
        4096
    };
    while position < bytes.len() {
        chunks += 1;
        if chunks > max_chunks || ended {
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
            declared = u32::from_be_bytes(chunk[8..12].try_into().unwrap()) as usize;
            if declared == 0 || declared > MAX_ANIMATION_FRAMES {
                return Err(PreviewError::TooLarge);
            }
            control = true;
        }
        if kind == b"fcTL" {
            frames += 1;
            if !control || length != 26 || frames > declared {
                return Err(PreviewError::Invalid);
            }
            // Width, height, x and y offsets, then dispose and blend.
            let field = |at: usize| u32::from_be_bytes(chunk[at..at + 4].try_into().unwrap());
            let (frame_width, frame_height) = (field(12), field(16));
            let (left, top) = (field(20), field(24));
            let inside = |offset: u32, extent: u32, limit: u32| {
                extent > 0 && offset.checked_add(extent).is_some_and(|end| end <= limit)
            };
            // A frame before the image data is the default image itself.
            let whole = (left, top, frame_width, frame_height) == (0, 0, width, height);
            if !inside(left, frame_width, width)
                || !inside(top, frame_height, height)
                || chunk[32] > 2
                || chunk[33] > 1
                || (!image_data && (frames > 1 || !whole))
            {
                return Err(PreviewError::Invalid);
            }
        }
        // Frame data follows the image data and a frame control, and starts
        // with its sequence number.
        if kind == b"fdAT" && (!control || frames == 0 || !image_data || length < 4) {
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
    // Every declared frame is present: the count the player admits is the
    // count the decoder can produce.
    if !ended || (animation && (!control || frames != declared)) {
        return Err(PreviewError::Invalid);
    }
    Ok((output, frames))
}

/// The sanitized decoder input alone (see `jpeg_frame`).
fn jpeg_input(bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, PreviewError> {
    jpeg_frame(bytes).map(|(input, _)| input)
}

/// The sanitized decoder input and the orientation of the first Exif segment
/// (upright when absent or unreadable). Baseline frames have exactly one
/// scan. Progressive (SOF2, Huffman) frames may have up to `MAX_JPEG_SCANS`,
/// with only Huffman and restart tables between them. Nothing else may follow
/// the first scan.
fn jpeg_frame(bytes: &[u8]) -> Result<(Zeroizing<Vec<u8>>, Orientation), PreviewError> {
    let mut output = Zeroizing::new(vec![0xff, 0xd8]);
    let mut position = 2usize;
    // Component count, and whether the frame is progressive.
    let mut frame = None;
    // The frame's component identifiers, which every scan must name.
    let mut ids = Vec::new();
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
                ids = payload[6..].chunks_exact(3).map(|c| c[0]).collect();
                frame = Some((count, marker == 0xc2));
            }
            0xda => {
                let (count, progressive) = frame.ok_or(PreviewError::Invalid)?;
                scan_header(payload, count, progressive, &ids)?;
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
/// within the 8-bit range; each named component is one of the frame's
/// (`ids`), named once, with table selectors 0-3.
fn scan_header(
    payload: &[u8],
    components: usize,
    progressive: bool,
    ids: &[u8],
) -> Result<(), PreviewError> {
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
    let selectors = &payload[1..1 + 2 * count];
    for (index, selector) in selectors.chunks_exact(2).enumerate() {
        let repeated = selectors[..2 * index]
            .chunks_exact(2)
            .any(|earlier| earlier[0] == selector[0]);
        if !ids.contains(&selector[0]) || repeated || selector[1] >> 4 > 3 || selector[1] & 15 > 3 {
            return Err(PreviewError::Unsupported);
        }
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

/// The orientation in a still PNG's first eXIf chunk (raw TIFF data), or
/// upright. Only chunk headers and that one chunk are read, at most 4096
/// chunks; the chunk itself never reaches the decoder.
fn png_orientation(bytes: &[u8]) -> Orientation {
    let mut position = 8usize;
    for _ in 0..4096 {
        let Some(header) = bytes.get(position..position + 8) else {
            break;
        };
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let Some(end) = position
            .checked_add(8)
            .and_then(|start| start.checked_add(length))
        else {
            break;
        };
        match &header[4..8] {
            b"eXIf" => {
                return bytes
                    .get(position + 8..end)
                    .and_then(Orientation::from_exif_chunk)
                    .unwrap_or(Orientation::NoTransforms);
            }
            b"IEND" => break,
            _ => {}
        }
        position = end + 4;
    }
    Orientation::NoTransforms
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
        let (channels, wide) = match color {
            ColorType::L8 => (1, false),
            ColorType::La8 => (2, false),
            ColorType::Rgb8 => (3, false),
            ColorType::Rgba8 => (4, false),
            ColorType::L16 => (1, true),
            ColorType::La16 => (2, true),
            ColorType::Rgb16 => (3, true),
            ColorType::Rgba16 => (4, true),
            _ => return Err(PreviewError::Unsupported),
        };
        let sample_bytes = if wide { 2 } else { 1 };
        let mut raw = Zeroizing::new(vec![0; length / 4 * channels * sample_bytes]);
        if decoder.total_bytes() != raw.len() as u64 {
            return Err(PreviewError::Invalid);
        }
        decoder
            .read_image(&mut raw)
            .map_err(|_| PreviewError::Invalid)?;
        let rgba = if wide {
            narrow_to_rgba(raw, channels)
        } else {
            let mut rgba = Zeroizing::new(Vec::with_capacity(length));
            for pixel in raw.chunks_exact(channels) {
                match channels {
                    1 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
                    2 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]),
                    3 => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
                    _ => rgba.extend_from_slice(pixel),
                }
            }
            rgba
        };
        let pixels = Pixels {
            width,
            height,
            rgba,
        };
        Ok(orient(pixels, png_orientation(&bytes)))
    } else if bytes.starts_with(b"\xff\xd8") {
        let (input, orientation) = jpeg_frame(&bytes)?;
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

/// One 16-bit sample, rounded to 8 bits. The PNG decoder writes 16-bit
/// samples in native byte order.
fn narrow(sample: [u8; 2]) -> u8 {
    ((u32::from(u16::from_ne_bytes(sample)) * 255 + 32_767) / 65_535) as u8
}

/// 16-bit gray, gray-alpha, RGB or RGBA samples to 8-bit RGBA. Formats of at
/// least four bytes per pixel are compacted in place (each pixel is read
/// before its smaller output overwrites anything still unread), so the peak
/// stays at the decoded buffer; gray expands into a new buffer.
fn narrow_to_rgba(mut raw: Zeroizing<Vec<u8>>, channels: usize) -> Zeroizing<Vec<u8>> {
    let stride = channels * 2;
    let pixels = raw.len() / stride;
    let sample = |raw: &[u8], index: usize| narrow([raw[index], raw[index + 1]]);
    if channels == 1 {
        let mut rgba = Zeroizing::new(Vec::with_capacity(pixels * 4));
        for pixel in raw.chunks_exact(2) {
            let gray = narrow([pixel[0], pixel[1]]);
            rgba.extend_from_slice(&[gray, gray, gray, 255]);
        }
        return rgba;
    }
    for index in 0..pixels {
        let at = index * stride;
        let out = match channels {
            2 => {
                let gray = sample(&raw, at);
                [gray, gray, gray, sample(&raw, at + 2)]
            }
            3 => [
                sample(&raw, at),
                sample(&raw, at + 2),
                sample(&raw, at + 4),
                255,
            ],
            _ => [
                sample(&raw, at),
                sample(&raw, at + 2),
                sample(&raw, at + 4),
                sample(&raw, at + 6),
            ],
        };
        raw[index * 4..index * 4 + 4].copy_from_slice(&out);
    }
    // Truncation keeps the capacity, which Zeroizing still clears on drop.
    raw.truncate(pixels * 4);
    raw
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
    fn sixteen_bit_png_is_narrowed_with_rounding() {
        let encode = |samples: &[u16], color| {
            // The encoder takes native-endian samples and writes big-endian.
            let bytes: Vec<u8> = samples.iter().flat_map(|v| v.to_ne_bytes()).collect();
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&bytes, 1, 1, color)
                .unwrap();
            decode(Zeroizing::new(png)).unwrap()
        };
        let rgba = encode(&[65_535, 0, 32_896, 257], image::ExtendedColorType::Rgba16);
        assert_eq!(&*rgba.rgba, &[255, 0, 128, 1]);
        let rgb = encode(&[0, 65_535, 128], image::ExtendedColorType::Rgb16);
        assert_eq!(&*rgb.rgba, &[0, 255, 0, 255]);
        let gray = encode(&[32_896], image::ExtendedColorType::L16);
        assert_eq!(&*gray.rgba, &[128, 128, 128, 255]);
        let gray_alpha = encode(&[65_535, 0], image::ExtendedColorType::La16);
        assert_eq!(&*gray_alpha.rgba, &[255, 255, 255, 0]);
    }
    #[test]
    fn in_place_narrowing_keeps_every_pixel() {
        // Two RGBA16 pixels: the second must survive the first's write.
        let samples: [u16; 8] = [1, 2, 3, 4, 65_535, 65_535, 0, 65_535];
        let raw: Vec<u8> = samples.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let rgba = narrow_to_rgba(Zeroizing::new(raw), 4);
        assert_eq!(&*rgba, &[0, 0, 0, 0, 255, 255, 0, 255]);
        let samples: [u16; 6] = [0, 65_535, 0, 65_535, 0, 65_535];
        let raw: Vec<u8> = samples.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let rgba = narrow_to_rgba(Zeroizing::new(raw), 3);
        assert_eq!(&*rgba, &[0, 255, 0, 255, 255, 0, 255, 255]);
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
            .write_image(
                &[1, 2, 3, 255, 4, 5, 6, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        // Structure only: the sanitizer never decompresses, so the frame
        // data and checksums need not be real.
        let chunk = |kind: &[u8; 4], payload: &[u8]| {
            let mut chunk = (payload.len() as u32).to_be_bytes().to_vec();
            chunk.extend_from_slice(kind);
            chunk.extend_from_slice(payload);
            chunk.extend_from_slice(&[0; 4]);
            chunk
        };
        let control = |frames: u32| {
            let mut payload = frames.to_be_bytes().to_vec();
            payload.extend_from_slice(&0u32.to_be_bytes());
            chunk(b"acTL", &payload)
        };
        // Sequence; width, height, x and y; a 1/10 s delay; dispose; blend.
        let frame = |sequence: u32, rectangle: [u32; 4], dispose: u8, blend: u8| {
            let mut payload = sequence.to_be_bytes().to_vec();
            for value in rectangle {
                payload.extend_from_slice(&value.to_be_bytes());
            }
            payload.extend_from_slice(&[0, 1, 0, 10, dispose, blend]);
            chunk(b"fcTL", &payload)
        };
        let data = |sequence: u32| {
            let mut payload = sequence.to_be_bytes().to_vec();
            payload.push(0);
            chunk(b"fdAT", &payload)
        };
        // acTL and `first` before the image data; `later` before IEND.
        let build = |declared: u32, first: Vec<u8>, later: Vec<u8>| {
            let mut bytes = original.clone();
            let end = bytes.len() - 12;
            bytes.splice(end..end, later);
            bytes.splice(33..33, [control(declared), first].concat());
            bytes
        };
        let later = |rectangle: [u32; 4], dispose: u8, blend: u8| {
            [frame(1, rectangle, dispose, blend), data(2)].concat()
        };
        let whole = frame(0, [2, 1, 0, 0], 0, 0);
        let animated = build(2, whole.clone(), later([1, 1, 1, 0], 0, 0));
        assert!(png_animated(&animated) && !png_animated(&original));
        assert!(png_input(&animated).is_err());
        let (kept, frames) = png_animation_input(&animated).unwrap();
        assert!(kept.windows(4).any(|window| window == b"acTL"));
        assert_eq!(frames, 2);
        // Every frame is admitted before decoding: the declared and present
        // counts agree, each rectangle lies inside the image with valid
        // operations, a frame before the image data is the whole image, and
        // frame data follows the image data.
        for (label, bytes) in [
            (
                "missing frame",
                build(3, whole.clone(), later([1, 1, 1, 0], 0, 0)),
            ),
            (
                "extra frame",
                build(1, whole.clone(), later([1, 1, 1, 0], 0, 0)),
            ),
            (
                "outside",
                build(2, whole.clone(), later([2, 1, 1, 0], 0, 0)),
            ),
            ("empty", build(2, whole.clone(), later([0, 1, 0, 0], 0, 0))),
            (
                "wrapping offset",
                build(2, whole.clone(), later([1, 1, u32::MAX, 0], 0, 0)),
            ),
            (
                "dispose",
                build(2, whole.clone(), later([1, 1, 1, 0], 3, 0)),
            ),
            ("blend", build(2, whole.clone(), later([1, 1, 1, 0], 0, 2))),
            (
                "partial default",
                build(2, frame(0, [1, 1, 0, 0], 0, 0), later([1, 1, 1, 0], 0, 0)),
            ),
            (
                "early data",
                build(
                    2,
                    [whole.clone(), data(1)].concat(),
                    later([1, 1, 1, 0], 0, 0),
                ),
            ),
        ] {
            assert!(
                matches!(png_animation_input(&bytes), Err(PreviewError::Invalid)),
                "{label}"
            );
        }
        let mut too_many = original.clone();
        too_many.splice(33..33, control(MAX_ANIMATION_FRAMES as u32 + 1));
        assert!(matches!(
            png_animation_input(&too_many),
            Err(PreviewError::TooLarge)
        ));
        let mut twice = animated.clone();
        twice.splice(33..33, control(2));
        assert!(png_animation_input(&twice).is_err());
        // Many small ancillary chunks: over the still ceiling, under the
        // animation one.
        let filler: Vec<u8> = (0..5000)
            .flat_map(|_| [0, 0, 0, 0, b't', b'E', b'X', b't', 0, 0, 0, 0])
            .collect();
        let mut crowded = animated.clone();
        crowded.splice(33..33, filler.iter().copied());
        assert!(png_animation_input(&crowded).is_ok());
        let mut crowded_still = original.clone();
        crowded_still.splice(33..33, filler);
        assert!(png_input(&crowded_still).is_err());
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
        assert_eq!(
            (tall.width, tall.height, values(&tall)),
            (1, 3, vec![1, 2, 3])
        );
        let tall = orient(pixels(3, 1, &[1, 2, 3]), Orientation::Rotate270);
        assert_eq!(
            (tall.width, tall.height, values(&tall)),
            (1, 3, vec![3, 2, 1])
        );
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
        let (bounded, orientation) = jpeg_frame(&source).unwrap();
        assert_eq!(orientation, Orientation::Rotate90);
        assert!(!bounded.windows(4).any(|w| w == b"Exif"));
        // An unknown value leaves the image as stored, and a segment that is
        // not Exif is never read for one.
        let mut source = vec![0xff, 0xd8];
        source.extend_from_slice(&exif(9));
        source.extend_from_slice(&frame);
        assert_eq!(jpeg_frame(&source).unwrap().1, Orientation::NoTransforms);
        let mut source = vec![0xff, 0xd8, 0xff, 0xe1, 0, 6, 1, 2, 3, 4];
        source.extend_from_slice(&frame);
        assert_eq!(jpeg_frame(&source).unwrap().1, Orientation::NoTransforms);
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
        let bounded = jpeg_input(&source).unwrap();
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
        for tail in [
            [1, 0, 0],
            [0, 5, 0],
            [2, 64, 0],
            [1, 63, 0xe0],
            [1, 63, 0x0e],
        ] {
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
    fn progressive_scans_name_only_their_frames_components_once() {
        // A one-component frame (id 1): a scan naming id 2, or a table
        // selector past 3, is refused before the decoder.
        let mut unknown = jpeg(0xc2, &[([0, 0, 0], &[1u8][..])], &[]);
        let scan = unknown.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
        unknown[scan + 5] = 2;
        assert!(matches!(
            jpeg_input(&unknown),
            Err(PreviewError::Unsupported)
        ));
        let mut table = jpeg(0xc2, &[([0, 0, 0], &[1u8][..])], &[]);
        table[scan + 6] = 0x40;
        assert!(matches!(jpeg_input(&table), Err(PreviewError::Unsupported)));
        // A DC scan of three components names each once.
        let mut colour = vec![0xff, 0xd8, 0xff, 0xc2, 0, 17, 8, 0, 1, 0, 1, 3];
        colour.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        let scan = |second: u8| {
            let mut bytes = colour.clone();
            bytes.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0, second, 0, 3, 0]);
            bytes.extend_from_slice(&[0, 0, 0, 1, 0xff, 0xd9]);
            bytes
        };
        assert!(jpeg_input(&scan(2)).is_ok());
        assert!(matches!(
            jpeg_input(&scan(1)),
            Err(PreviewError::Unsupported)
        ));
    }
    #[test]
    fn png_exif_orientation_is_read_and_the_chunk_still_discarded() {
        let mut original = Vec::new();
        image::codecs::png::PngEncoder::new(&mut original)
            .write_image(
                &[1, 0, 0, 255, 2, 0, 0, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        // Big-endian TIFF, IFD0 with one orientation entry (6: quarter turn).
        let mut data = b"MM\0*\0\0\0\x08\0\x01".to_vec();
        data.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"eXIf");
        chunk.extend_from_slice(&data);
        chunk.extend_from_slice(&[0; 4]); // The CRC is not checked for discarded chunks.
        let mut bytes = original.clone();
        bytes.splice(33..33, chunk);
        assert_eq!(png_orientation(&bytes), Orientation::Rotate90);
        assert_eq!(png_orientation(&original), Orientation::NoTransforms);
        assert!(!png_input(&bytes).unwrap().windows(4).any(|w| w == b"eXIf"));
        let pixels = decode(Zeroizing::new(bytes)).unwrap();
        assert_eq!((pixels.width, pixels.height), (1, 2));
        assert_eq!(values(&pixels), [1, 2]);
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
