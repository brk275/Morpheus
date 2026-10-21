//! Structural checks for GIF and WebP previews before any image data is
//! decoded. Each walks the whole container once, bounds what it holds against
//! the preview limits (the logical screen or canvas, every frame's place and
//! size, the frame count, block and chunk counts, exact lengths), and rebuilds
//! an input of only the structure the pinned decoder needs. Every RIFF chunk
//! and frame subchunk length is checked against the admitted input, and every
//! embedded VP8/VP8L bitstream's own width and height must equal its frame's
//! before a decoder sizes any buffer from them. Comments, plain text,
//! application extensions, ICCP, EXIF, XMP and unknown chunks are left out
//! unread, so no decoder ever holds them and no metadata read can allocate
//! from a declared length. A frame count over `decode::MAX_ANIMATION_FRAMES`
//! is refused here, so playback never mistakes the limit for the animation's
//! natural end.
use super::{
    PreviewError,
    decode::{MAX_ANIMATION_FRAMES, size},
};
use image::metadata::LoopCount;
use std::num::NonZeroU32;
use zeroize::Zeroizing;

/// The most extension and image blocks a GIF may hold.
const MAX_GIF_BLOCKS: usize = 4096;
/// The most top-level chunks a WebP file may hold besides its frames, and
/// the most unknown chunks after one frame's bitstream.
const MAX_WEBP_CHUNKS: usize = 64;
const WEBP_ANIMATION: u8 = 0x02;
const WEBP_ALPHA: u8 = 0x10;

/// How many times an animation plays through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Loops {
    Endless,
    Times(NonZeroU32),
}

impl From<Loops> for LoopCount {
    fn from(loops: Loops) -> Self {
        match loops {
            Loops::Endless => LoopCount::Infinite,
            Loops::Times(times) => LoopCount::Finite(times),
        }
    }
}

/// A checked GIF: the rebuilt stream, its frame count and how often it plays.
#[derive(Debug)]
pub(super) struct Gif {
    pub(super) bytes: Zeroizing<Vec<u8>>,
    pub(super) frames: usize,
    /// Once without a NETSCAPE2.0/ANIMEXTS1.0 loop setting, count + 1 times
    /// for a count, endlessly for 0. The decoder never sees the setting.
    pub(super) loops: Loops,
}

/// A checked WebP: the rebuilt file and its frame count (1 for a still).
#[derive(Debug)]
pub(super) struct WebP {
    pub(super) bytes: Zeroizing<Vec<u8>>,
    pub(super) frames: usize,
}

/// The position after a chain of GIF data sub-blocks starting at `position`
/// (each a length byte and that many bytes, ended by a zero length).
fn sub_blocks(bytes: &[u8], mut position: usize) -> Result<usize, PreviewError> {
    loop {
        let length = *bytes.get(position).ok_or(PreviewError::Invalid)? as usize;
        position += 1;
        if length == 0 {
            return Ok(position);
        }
        position = position
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or(PreviewError::Invalid)?;
    }
}

/// The GIF colour table that `flags` (a screen or image descriptor's)
/// announces: the position after it.
fn color_table(bytes: &[u8], position: usize, flags: u8) -> Result<usize, PreviewError> {
    if flags & 0x80 == 0 {
        return Ok(position);
    }
    position
        .checked_add(3usize << ((flags & 7) + 1))
        .filter(|end| *end <= bytes.len())
        .ok_or(PreviewError::Invalid)
}

/// Check every block of a GIF and rebuild it from its header, palettes,
/// graphic controls and image data only. Refused: truncated input or bytes
/// after the trailer, no frame, more than MAX_GIF_BLOCKS blocks or
/// MAX_ANIMATION_FRAMES frames, a frame that is empty or reaches outside the
/// logical screen, a frame with no colour table, an LZW code size outside
/// 2-8, an image without data, a reserved disposal method, two graphic
/// controls for one image, a malformed loop setting.
pub(super) fn gif(bytes: &[u8]) -> Result<Gif, PreviewError> {
    if bytes.len() < 14 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Err(PreviewError::Invalid);
    }
    let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    size(width, height)?;
    let global = bytes[10] & 0x80 != 0;
    let mut position = color_table(bytes, 13, bytes[10])?;
    // Never longer than the input, itself at most 8 MiB.
    let mut output = Zeroizing::new(Vec::with_capacity(bytes.len()));
    output.extend_from_slice(&bytes[..position]);
    let mut frames = 0usize;
    // The graphic control extension waiting for its image: its byte range.
    let mut control: Option<(usize, usize)> = None;
    let mut loops = Loops::Times(NonZeroU32::MIN);
    let mut looping = false;
    let mut blocks = 0usize;
    loop {
        blocks += 1;
        if blocks > MAX_GIF_BLOCKS {
            return Err(PreviewError::TooLarge);
        }
        match *bytes.get(position).ok_or(PreviewError::Invalid)? {
            0x3b => {
                if frames == 0 || position + 1 != bytes.len() {
                    return Err(PreviewError::Invalid);
                }
                output.push(0x3b);
                return Ok(Gif {
                    bytes: output,
                    frames,
                    loops,
                });
            }
            0x21 => match *bytes.get(position + 1).ok_or(PreviewError::Invalid)? {
                // Graphic control: a 4-byte block and its terminator.
                0xf9 => {
                    let block = bytes
                        .get(position..position + 8)
                        .ok_or(PreviewError::Invalid)?;
                    if block[2] != 4
                        || block[7] != 0
                        || control.is_some()
                        || (block[3] >> 2) & 7 > 3
                    {
                        return Err(PreviewError::Invalid);
                    }
                    control = Some((position, position + 8));
                    position += 8;
                }
                // Application: an 11-byte identity, then its data, left out.
                0xff => {
                    let identity = bytes
                        .get(position + 2..position + 14)
                        .ok_or(PreviewError::Invalid)?;
                    if identity[0] != 11 {
                        return Err(PreviewError::Invalid);
                    }
                    let end = sub_blocks(bytes, position + 14)?;
                    if &identity[1..] == b"NETSCAPE2.0" || &identity[1..] == b"ANIMEXTS1.0" {
                        let data = &bytes[position + 14..end];
                        if looping || data.len() != 5 || data[0] != 3 || data[1] != 1 {
                            return Err(PreviewError::Invalid);
                        }
                        looping = true;
                        loops = match u16::from_le_bytes([data[2], data[3]]) {
                            0 => Loops::Endless,
                            count => Loops::Times(
                                NonZeroU32::new(u32::from(count) + 1).unwrap_or(NonZeroU32::MIN),
                            ),
                        };
                    }
                    position = end;
                }
                // Plain text is not drawn: it is left out with any graphic
                // control meant for it.
                0x01 => {
                    position = sub_blocks(bytes, position + 2)?;
                    control = None;
                }
                // Comments and unknown extensions: left out unread.
                _ => position = sub_blocks(bytes, position + 2)?,
            },
            0x2c => {
                let descriptor = bytes
                    .get(position..position + 10)
                    .ok_or(PreviewError::Invalid)?;
                let field =
                    |at: usize| u32::from(u16::from_le_bytes([descriptor[at], descriptor[at + 1]]));
                let (left, top, frame_width, frame_height) =
                    (field(1), field(3), field(5), field(7));
                if frame_width == 0
                    || frame_height == 0
                    || left + frame_width > width
                    || top + frame_height > height
                {
                    return Err(PreviewError::Invalid);
                }
                let flags = descriptor[9];
                if flags & 0x80 == 0 && !global {
                    return Err(PreviewError::Invalid);
                }
                let data = color_table(bytes, position + 10, flags)?;
                let code_size = *bytes.get(data).ok_or(PreviewError::Invalid)?;
                if !(2..=8).contains(&code_size) || bytes.get(data + 1).is_none_or(|n| *n == 0) {
                    return Err(PreviewError::Invalid);
                }
                let end = sub_blocks(bytes, data + 1)?;
                frames += 1;
                if frames > MAX_ANIMATION_FRAMES {
                    return Err(PreviewError::TooLarge);
                }
                if let Some((start, end)) = control {
                    output.extend_from_slice(&bytes[start..end]);
                }
                output.extend_from_slice(&bytes[position..end]);
                control = None;
                position = end;
            }
            _ => return Err(PreviewError::Invalid),
        }
    }
}

/// Dimensions of a lossy WebP key frame (RFC 6386 9.1): a shown key frame of
/// version 0-3, its start code and 14-bit sizes, and a first partition that
/// fits in the chunk after its 10-byte header. The pinned decoder sizes its
/// padded planes from these inner sizes before comparing them with the frame
/// and reads the first partition into a buffer of the declared size, so both
/// are checked here first. Its later token partitions are sized from bytes
/// inside that compressed first partition and are not checked here.
fn lossy(payload: &[u8]) -> Result<(u32, u32), PreviewError> {
    let header = payload.get(..10).ok_or(PreviewError::Invalid)?;
    let tag = u32::from_le_bytes([header[0], header[1], header[2], 0]);
    if tag & 1 != 0 || (tag >> 1) & 7 > 3 || (tag >> 4) & 1 == 0 {
        return Err(PreviewError::Invalid);
    }
    let first_partition = (tag >> 5) as usize;
    if first_partition > payload.len() - 10 || header[3..6] != [0x9d, 0x01, 0x2a] {
        return Err(PreviewError::Invalid);
    }
    let width = u32::from(u16::from_le_bytes([header[6], header[7]]) & 0x3fff);
    let height = u32::from(u16::from_le_bytes([header[8], header[9]]) & 0x3fff);
    size(width, height)?;
    Ok((width, height))
}

/// Dimensions of a lossless WebP image: its signature, 14-bit sizes and
/// version 0. Huffman-table storage inside the stream depends on the
/// decoder's own limit on meta-Huffman groups; this check does not bound it.
fn lossless(payload: &[u8]) -> Result<(u32, u32), PreviewError> {
    let header = payload.get(..5).ok_or(PreviewError::Invalid)?;
    let bits = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    if header[0] != 0x2f || bits >> 29 != 0 {
        return Err(PreviewError::Invalid);
    }
    let (width, height) = (1 + (bits & 0x3fff), 1 + ((bits >> 14) & 0x3fff));
    size(width, height)?;
    Ok((width, height))
}

/// An alpha plane's header byte: compression none or lossless, filtering
/// any, pre-processing 0-1, reserved bits clear.
fn alpha_header(payload: &[u8]) -> Result<u8, PreviewError> {
    let method = *payload.first().ok_or(PreviewError::Invalid)?;
    if method & 3 > 1 || (method >> 4) & 3 > 1 || method >> 6 != 0 {
        return Err(PreviewError::Invalid);
    }
    Ok(method)
}

fn push_chunk(output: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    output.extend_from_slice(kind);
    // At most the 8 MiB input: always within u32.
    output.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    output.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        output.push(0);
    }
}

/// One RIFF chunk at `position` within `limit`: its kind, payload range and
/// the position after its padding.
fn riff_chunk(
    bytes: &[u8],
    position: usize,
    limit: usize,
) -> Result<([u8; 4], std::ops::Range<usize>, usize), PreviewError> {
    let header = bytes
        .get(position..position + 8)
        .filter(|_| position + 8 <= limit)
        .ok_or(PreviewError::Invalid)?;
    let kind: [u8; 4] = header[..4].try_into().unwrap();
    let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let start = position + 8;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= limit)
        .ok_or(PreviewError::Invalid)?;
    // A chunk of odd length is padded to an even one.
    let next = end
        .checked_add(length % 2)
        .filter(|next| *next <= limit)
        .ok_or(PreviewError::Invalid)?;
    Ok((kind, start..end, next))
}

/// An image bitstream (with an optional alpha plane before a lossy one) of
/// exactly `dimensions`: refused when its own header disagrees.
fn bitstream(
    kind: &[u8; 4],
    payload: &[u8],
    dimensions: (u32, u32),
    alpha: Option<&[u8]>,
) -> Result<(), PreviewError> {
    let found = match kind {
        b"VP8 " => lossy(payload)?,
        b"VP8L" if alpha.is_none() => lossless(payload)?,
        _ => return Err(PreviewError::Invalid),
    };
    if found != dimensions {
        return Err(PreviewError::Invalid);
    }
    if let Some(alpha) = alpha
        && alpha_header(alpha)? & 3 == 0
        && alpha.len() - 1 != size(dimensions.0, dimensions.1)? / 4
    {
        // Uncompressed alpha is exactly one byte per pixel after its header.
        return Err(PreviewError::Invalid);
    }
    Ok(())
}

/// Check one animation frame (ANMF) inside `canvas` and rebuild it from its
/// header, optional alpha plane and bitstream only. Every subchunk must fit
/// the frame: an alpha plane only directly before a lossy bitstream, exactly
/// one bitstream of the frame's own size, then at most MAX_WEBP_CHUNKS
/// unknown chunks, which are left out unread.
fn webp_frame(
    output: &mut Vec<u8>,
    bytes: &[u8],
    frame: std::ops::Range<usize>,
    canvas: (u32, u32),
) -> Result<(), PreviewError> {
    let header = bytes
        .get(frame.start..frame.start + 16)
        .filter(|_| frame.start + 16 <= frame.end)
        .ok_or(PreviewError::Invalid)?;
    let u24 = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], 0]);
    let (left, top) = (2 * u24(0), 2 * u24(3));
    let (width, height) = (1 + u24(6), 1 + u24(9));
    // Reserved flag bits clear; the frame lies wholly within the canvas.
    if header[15] >> 2 != 0 || left + width > canvas.0 || top + height > canvas.1 {
        return Err(PreviewError::Invalid);
    }
    let mut position = frame.start + 16;
    let (mut first, mut range, mut next) = riff_chunk(bytes, position, frame.end)?;
    let mut alpha = None;
    if &first == b"ALPH" {
        alpha = Some(range.clone());
        position = next;
        (first, range, next) = riff_chunk(bytes, position, frame.end)?;
    }
    bitstream(
        &first,
        &bytes[range.clone()],
        (width, height),
        alpha.clone().map(|a| &bytes[a]),
    )?;
    for _ in 0..MAX_WEBP_CHUNKS {
        if next == frame.end {
            break;
        }
        let (kind, _, after) = riff_chunk(bytes, next, frame.end)?;
        if matches!(
            &kind,
            b"ALPH" | b"VP8 " | b"VP8L" | b"ANMF" | b"ANIM" | b"VP8X"
        ) {
            return Err(PreviewError::Invalid);
        }
        next = after;
    }
    if next != frame.end {
        return Err(PreviewError::Invalid);
    }
    let mut data = Vec::with_capacity(frame.len());
    data.extend_from_slice(header);
    if let Some(alpha) = alpha {
        push_chunk(&mut data, b"ALPH", &bytes[alpha]);
    }
    push_chunk(&mut data, &first, &bytes[range]);
    push_chunk(output, b"ANMF", &data);
    // The rebuilt frame never outlives its copy in `output`.
    zeroize::Zeroize::zeroize(&mut data);
    Ok(())
}

/// Check a WebP file and rebuild it from its image data only. A still is a
/// simple file, or an extended one with an alpha plane before a lossy image;
/// an animation keeps its extended header (only the animation and alpha
/// flags), its ANIM chunk and every frame (at most MAX_ANIMATION_FRAMES).
/// The RIFF size must match the input exactly.
pub(super) fn webp(bytes: &[u8]) -> Result<WebP, PreviewError> {
    if bytes.len() < 20 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(PreviewError::Invalid);
    }
    let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if riff.checked_add(8) != Some(bytes.len()) {
        return Err(PreviewError::Invalid);
    }
    let mut output = Zeroizing::new(Vec::with_capacity(bytes.len() + 18));
    output.extend_from_slice(b"RIFF\0\0\0\0WEBP");
    let mut position = 12usize;
    let mut chunks = 0usize;
    // The extended header's flags and canvas, if the file has one.
    let mut extended: Option<(u8, (u32, u32))> = None;
    let mut alpha = None;
    let mut image = None;
    let mut animation = None;
    let mut frames = 0usize;
    while position < bytes.len() {
        let (kind, range, next) = riff_chunk(bytes, position, bytes.len())?;
        if &kind != b"ANMF" {
            chunks += 1;
            if chunks > MAX_WEBP_CHUNKS {
                return Err(PreviewError::Invalid);
            }
        }
        let payload = &bytes[range.clone()];
        let first = position == 12;
        // An alpha plane pairs only with the lossy image right after it.
        if alpha.is_some() && image.is_none() && &kind != b"VP8 " {
            return Err(PreviewError::Invalid);
        }
        match &kind {
            b"VP8X" => {
                if !first || payload.len() != 10 {
                    return Err(PreviewError::Invalid);
                }
                let side = |at: usize| {
                    1 + u32::from_le_bytes([payload[at], payload[at + 1], payload[at + 2], 0])
                };
                let canvas = (side(4), side(7));
                size(canvas.0, canvas.1)?;
                extended = Some((payload[0], canvas));
            }
            b"ANIM" => {
                let animated = extended.is_some_and(|(flags, _)| flags & WEBP_ANIMATION != 0);
                if !animated || animation.is_some() || frames > 0 || payload.len() != 6 {
                    return Err(PreviewError::Invalid);
                }
                animation = Some(range.clone());
            }
            b"ANMF" => {
                let (Some((_, canvas)), Some(_)) = (extended, &animation) else {
                    return Err(PreviewError::Invalid);
                };
                frames += 1;
                if frames > MAX_ANIMATION_FRAMES {
                    return Err(PreviewError::TooLarge);
                }
                if frames == 1 {
                    let (flags, (width, height)) = extended.unwrap();
                    let mut header = [0u8; 10];
                    header[0] = flags & (WEBP_ANIMATION | WEBP_ALPHA);
                    header[4..7].copy_from_slice(&(width - 1).to_le_bytes()[..3]);
                    header[7..10].copy_from_slice(&(height - 1).to_le_bytes()[..3]);
                    push_chunk(&mut output, b"VP8X", &header);
                    push_chunk(&mut output, b"ANIM", &bytes[animation.clone().unwrap()]);
                }
                webp_frame(&mut output, bytes, range.clone(), canvas)?;
            }
            b"VP8 " | b"VP8L" => {
                // One still image: first in a simple file, after VP8X otherwise,
                // never in an animation.
                let animated = extended.is_some_and(|(flags, _)| flags & WEBP_ANIMATION != 0);
                if image.is_some() || animated || (extended.is_none() && !first) {
                    return Err(PreviewError::Invalid);
                }
                image = Some((kind, range.clone()));
            }
            // An alpha plane precedes its lossy still image, once.
            b"ALPH" => {
                if extended.is_none() || alpha.is_some() || image.is_some() {
                    return Err(PreviewError::Invalid);
                }
                alpha_header(payload)?;
                alpha = Some(range.clone());
            }
            // ICCP, EXIF, XMP and unknown chunks are left out unread.
            _ => {}
        }
        position = next;
    }
    let animated = extended.is_some_and(|(flags, _)| flags & WEBP_ANIMATION != 0);
    if animated {
        if frames == 0 || image.is_some() || alpha.is_some() {
            return Err(PreviewError::Invalid);
        }
    } else {
        let (kind, data) = image.ok_or(PreviewError::Invalid)?;
        let dimensions = match &kind {
            b"VP8 " => lossy(&bytes[data.clone()])?,
            _ => lossless(&bytes[data.clone()])?,
        };
        if extended.is_some_and(|(_, canvas)| canvas != dimensions) {
            return Err(PreviewError::Invalid);
        }
        bitstream(
            &kind,
            &bytes[data.clone()],
            dimensions,
            alpha.clone().map(|a| &bytes[a]),
        )?;
        if let Some(alpha) = alpha {
            let mut header = [0u8; 10];
            header[0] = WEBP_ALPHA;
            header[4..7].copy_from_slice(&(dimensions.0 - 1).to_le_bytes()[..3]);
            header[7..10].copy_from_slice(&(dimensions.1 - 1).to_le_bytes()[..3]);
            push_chunk(&mut output, b"VP8X", &header);
            push_chunk(&mut output, b"ALPH", &bytes[alpha]);
        }
        push_chunk(&mut output, &kind, &bytes[data]);
        frames = 1;
    }
    let riff = (output.len() - 8) as u32;
    output[4..8].copy_from_slice(&riff.to_le_bytes());
    Ok(WebP {
        bytes: output,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    /// The 2-colour palette and LZW data of one pixel of index 0 (minimum
    /// code size 2: clear, 0, end), as the smallest GIFs carry it.
    const PIXEL: [u8; 5] = [2, 2, 0x44, 0x01, 0];

    fn header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        // A global table of two colours: black and white.
        bytes.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
        bytes
    }

    fn looping(bytes: &mut Vec<u8>, count: u16) {
        bytes.extend_from_slice(&[0x21, 0xff, 11]);
        bytes.extend_from_slice(b"NETSCAPE2.0");
        bytes.extend_from_slice(&[3, 1]);
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.push(0);
    }

    /// One frame at `at` of `dimensions`, its graphic control first. Its LZW
    /// data is the single pixel above: larger frames exercise framing only.
    fn frame(bytes: &mut Vec<u8>, at: (u16, u16), dimensions: (u16, u16)) {
        bytes.extend_from_slice(&[0x21, 0xf9, 4, 0, 5, 0, 0, 0, 0x2c]);
        for value in [at.0, at.1, dimensions.0, dimensions.1] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.push(0);
        bytes.extend_from_slice(&PIXEL);
    }

    fn two_frames() -> Vec<u8> {
        let mut bytes = header(1, 1);
        looping(&mut bytes, 0);
        frame(&mut bytes, (0, 0), (1, 1));
        frame(&mut bytes, (0, 0), (1, 1));
        bytes.push(0x3b);
        bytes
    }

    #[test]
    fn gif_metadata_never_reaches_the_decoder_and_loops_are_read_first() {
        let mut bytes = header(1, 1);
        looping(&mut bytes, 2);
        bytes.extend_from_slice(&[0x21, 0xfe, 4]);
        bytes.extend_from_slice(b"note");
        bytes.push(0);
        for identity in [b"XMP DataXMP", b"ICCRGBG1012"] {
            bytes.extend_from_slice(&[0x21, 0xff, 11]);
            bytes.extend_from_slice(identity);
            bytes.push(6);
            bytes.extend_from_slice(b"secret");
            bytes.push(0);
        }
        frame(&mut bytes, (0, 0), (1, 1));
        bytes.push(0x3b);
        let checked = gif(&bytes).unwrap();
        assert_eq!(checked.frames, 1);
        assert_eq!(checked.loops, Loops::Times(NonZeroU32::new(3).unwrap()));
        for removed in [&b"note"[..], b"secret", b"NETSCAPE", b"XMP", b"ICCRGB"] {
            assert!(!checked.bytes.windows(removed.len()).any(|w| w == removed));
        }
        assert_eq!(gif(&two_frames()).unwrap().loops, Loops::Endless);
        let mut once = header(1, 1);
        frame(&mut once, (0, 0), (1, 1));
        once.push(0x3b);
        assert_eq!(gif(&once).unwrap().loops, Loops::Times(NonZeroU32::MIN));
    }

    #[test]
    fn gif_framing_counts_and_bounds_are_refused_before_decoding() {
        let complete = two_frames();
        for end in [13, 20, complete.len() - 1] {
            assert!(gif(&complete[..end]).is_err(), "cut at {end}");
        }
        let mut padded = complete.clone();
        padded.push(0);
        assert_eq!(gif(&padded).unwrap_err(), PreviewError::Invalid);
        for (at, dimensions) in [((1, 0), (1, 1)), ((0, 0), (2, 1)), ((0, 0), (0, 1))] {
            let mut bytes = header(1, 1);
            frame(&mut bytes, at, dimensions);
            bytes.push(0x3b);
            assert_eq!(gif(&bytes).unwrap_err(), PreviewError::Invalid);
        }
        for (width, height) in [(4097, 1), (4096, 4096), (0, 1)] {
            let mut bytes = header(width, height);
            frame(&mut bytes, (0, 0), (1, 1));
            bytes.push(0x3b);
            assert_eq!(gif(&bytes).unwrap_err(), PreviewError::TooLarge);
        }
        let mut reserved = two_frames();
        let control = reserved.windows(2).position(|w| w == [0x21, 0xf9]).unwrap();
        reserved[control + 3] = 4 << 2;
        assert_eq!(gif(&reserved).unwrap_err(), PreviewError::Invalid);
        // One frame over the limit is refused while counting, never cut short.
        let mut many = header(1, 1);
        for _ in 0..=MAX_ANIMATION_FRAMES {
            frame(&mut many, (0, 0), (1, 1));
        }
        many.push(0x3b);
        assert_eq!(gif(&many).unwrap_err(), PreviewError::TooLarge);
        // Without its trailer and last 23-byte frame: exactly the limit.
        many.truncate(many.len() - 1 - 23);
        many.push(0x3b);
        assert_eq!(gif(&many).unwrap().frames, MAX_ANIMATION_FRAMES);
    }

    /// A lossless 2 × 1 WebP from the pinned encoder: one opaque red and one
    /// half-transparent blue pixel. Its VP8L chunk starts at byte 12.
    fn lossless_file() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
            .write_image(
                &[255, 0, 0, 255, 0, 0, 255, 128],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        assert_eq!(&bytes[12..16], b"VP8L");
        bytes
    }

    fn riff(chunks: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(chunks.len() as u32 + 4).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(chunks);
        bytes
    }

    fn extended(flags: u8, width: u32, height: u32) -> Vec<u8> {
        let mut payload = vec![flags, 0, 0, 0];
        payload.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        payload.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        let mut bytes = Vec::new();
        push_chunk(&mut bytes, b"VP8X", &payload);
        bytes
    }

    /// An ANMF frame at (left, top) whose bitstream is the 2 × 1 VP8L chunk.
    fn animation_frame(bytes: &mut Vec<u8>, left: u32, top: u32) {
        let mut payload = Vec::new();
        for value in [left / 2, top / 2, 1, 0, 100] {
            payload.extend_from_slice(&value.to_le_bytes()[..3]);
        }
        payload.push(0);
        payload.extend_from_slice(&lossless_file()[12..]);
        push_chunk(bytes, b"ANMF", &payload);
    }

    #[test]
    fn webp_metadata_is_left_out_and_stills_decode_from_the_rebuilt_file() {
        let mut chunks = extended(0x20 | 0x08 | 0x04, 2, 1);
        push_chunk(&mut chunks, b"ICCP", b"profile!");
        push_chunk(&mut chunks, b"EXIF", b"camera!!");
        chunks.extend_from_slice(&lossless_file()[12..]);
        push_chunk(&mut chunks, b"XMP ", b"<x:xmp/>");
        let checked = webp(&riff(&chunks)).unwrap();
        assert_eq!(checked.frames, 1);
        for removed in [&b"profile!"[..], b"camera!!", b"<x:xmp/>", b"EXIF", b"VP8X"] {
            assert!(!checked.bytes.windows(removed.len()).any(|w| w == removed));
        }
        assert_eq!(
            webp(&lossless_file()).unwrap().bytes.as_slice(),
            lossless_file()
        );
    }

    #[test]
    fn webp_animation_frames_are_counted_placed_and_stripped() {
        let mut chunks = extended(WEBP_ANIMATION | 0x08, 4, 1);
        push_chunk(&mut chunks, b"ANIM", &[0, 0, 0, 0, 0, 0]);
        animation_frame(&mut chunks, 0, 0);
        animation_frame(&mut chunks, 2, 0);
        push_chunk(&mut chunks, b"EXIF", b"camera!!");
        let checked = webp(&riff(&chunks)).unwrap();
        assert_eq!(checked.frames, 2);
        assert!(!checked.bytes.windows(8).any(|w| w == b"camera!!"));
        // The rebuilt header keeps only the animation flag.
        assert_eq!(checked.bytes[20], WEBP_ANIMATION);
        // A frame reaching outside the canvas, a frame before ANIM, or none.
        let mut outside = extended(WEBP_ANIMATION, 4, 1);
        push_chunk(&mut outside, b"ANIM", &[0; 6]);
        animation_frame(&mut outside, 4, 0);
        assert_eq!(webp(&riff(&outside)).unwrap_err(), PreviewError::Invalid);
        let mut early = extended(WEBP_ANIMATION, 4, 1);
        animation_frame(&mut early, 0, 0);
        assert_eq!(webp(&riff(&early)).unwrap_err(), PreviewError::Invalid);
        let mut empty = extended(WEBP_ANIMATION, 4, 1);
        push_chunk(&mut empty, b"ANIM", &[0; 6]);
        assert_eq!(webp(&riff(&empty)).unwrap_err(), PreviewError::Invalid);
        // More frames than the limit are refused while counting.
        let mut many = extended(WEBP_ANIMATION, 4, 1);
        push_chunk(&mut many, b"ANIM", &[0; 6]);
        for _ in 0..=MAX_ANIMATION_FRAMES {
            animation_frame(&mut many, 0, 0);
        }
        assert_eq!(webp(&riff(&many)).unwrap_err(), PreviewError::TooLarge);
    }

    #[test]
    fn webp_containers_with_unsafe_lengths_or_sizes_are_refused() {
        let file = lossless_file();
        assert!(webp(&file[..file.len() - 1]).is_err());
        let mut padded = file.clone();
        padded.extend_from_slice(&[0, 0]);
        assert!(webp(&padded).is_err());
        let mut overlong = file.clone();
        overlong[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(webp(&overlong).is_err());
        for (width, height, error) in [
            (4097, 1, PreviewError::TooLarge),
            (4096, 4096, PreviewError::TooLarge),
            (3, 1, PreviewError::Invalid),
        ] {
            let mut chunks = extended(0, width, height);
            chunks.extend_from_slice(&file[12..]);
            assert_eq!(
                webp(&riff(&chunks)).unwrap_err(),
                error,
                "{width} × {height}"
            );
        }
        let mut not_key = Vec::new();
        push_chunk(
            &mut not_key,
            b"VP8 ",
            &[1, 0, 0, 0x9d, 0x01, 0x2a, 2, 0, 1, 0],
        );
        assert!(webp(&riff(&not_key)).is_err());
    }

    /// A shown lossy key frame header of `width` × `height` with a first
    /// partition of `first` zero bytes: structure only, never decoded.
    fn lossy_payload(width: u16, height: u16, first: u32) -> Vec<u8> {
        let mut payload = (0x10 | (first << 5)).to_le_bytes()[..3].to_vec();
        payload.extend_from_slice(&[0x9d, 0x01, 0x2a]);
        payload.extend_from_slice(&width.to_le_bytes());
        payload.extend_from_slice(&height.to_le_bytes());
        payload.resize(10 + first as usize, 0);
        payload
    }

    /// An animated 2 × 1 file of one 2 × 1 frame whose data is `subchunks`.
    fn one_frame(subchunks: &[u8]) -> Vec<u8> {
        let mut chunks = extended(WEBP_ANIMATION | WEBP_ALPHA, 2, 1);
        push_chunk(&mut chunks, b"ANIM", &[0; 6]);
        let mut payload = vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 100, 0, 0, 0];
        payload.extend_from_slice(subchunks);
        push_chunk(&mut chunks, b"ANMF", &payload);
        riff(&chunks)
    }

    #[test]
    fn embedded_bitstream_sizes_are_checked_before_any_decoder_sizes_buffers() {
        let mut lossy = Vec::new();
        push_chunk(&mut lossy, b"VP8 ", &lossy_payload(2, 1, 1));
        assert_eq!(webp(&one_frame(&lossy)).unwrap().frames, 1);
        assert_eq!(webp(&one_frame(&lossless_file()[12..])).unwrap().frames, 1);
        // Inner sizes other than the frame's, up to the 14-bit maximum the
        // pinned decoder would size its padded planes from.
        for (width, height, error) in [
            (3, 1, PreviewError::Invalid),
            (1, 1, PreviewError::Invalid),
            (0x3fff, 0x3fff, PreviewError::TooLarge),
        ] {
            let mut other = Vec::new();
            push_chunk(&mut other, b"VP8 ", &lossy_payload(width, height, 1));
            assert_eq!(
                webp(&one_frame(&other)).unwrap_err(),
                error,
                "{width} × {height}"
            );
        }
        let mut lossless = lossless_file();
        // 3 × 1 in the VP8L header (width - 1 in its low 14 bits).
        lossless[21] = 2;
        assert_eq!(
            webp(&one_frame(&lossless[12..])).unwrap_err(),
            PreviewError::Invalid
        );
        // A first partition declared longer than its chunk.
        let mut payload = lossy_payload(2, 1, 1);
        payload.truncate(10);
        let mut short = Vec::new();
        push_chunk(&mut short, b"VP8 ", &payload);
        assert_eq!(webp(&one_frame(&short)).unwrap_err(), PreviewError::Invalid);
        let mut still = Vec::new();
        push_chunk(&mut still, b"VP8 ", &payload);
        assert_eq!(webp(&riff(&still)).unwrap_err(), PreviewError::Invalid);
    }

    #[test]
    fn alpha_pairs_only_with_its_lossy_image_and_every_subchunk_fits() {
        let mut lossy = Vec::new();
        push_chunk(&mut lossy, b"VP8 ", &lossy_payload(2, 1, 1));
        let alpha = |payload: &[u8]| {
            let mut chunk = Vec::new();
            push_chunk(&mut chunk, b"ALPH", payload);
            chunk
        };
        let paired = [alpha(&[0, 255, 255]), lossy.clone()].concat();
        assert_eq!(webp(&one_frame(&paired)).unwrap().frames, 1);
        for (label, subchunks) in [
            ("alpha alone", alpha(&[0, 255, 255])),
            (
                "alpha before lossless",
                [alpha(&[0, 255, 255]), lossless_file()[12..].to_vec()].concat(),
            ),
            (
                "short uncompressed alpha",
                [alpha(&[0, 255]), lossy.clone()].concat(),
            ),
            (
                "reserved alpha bits",
                [alpha(&[0xc0, 255, 255]), lossy.clone()].concat(),
            ),
            ("second bitstream", [lossy.clone(), lossy.clone()].concat()),
            (
                "trailing alpha",
                [lossy.clone(), alpha(&[0, 255, 255])].concat(),
            ),
            // A trailing chunk claiming more bytes than the frame holds.
            (
                "overlong trailer",
                [lossy.clone(), b"XYZW\xe8\x03\0\0".to_vec()].concat(),
            ),
        ] {
            assert_eq!(
                webp(&one_frame(&subchunks)).unwrap_err(),
                PreviewError::Invalid,
                "{label}"
            );
        }
        // A well-formed unknown trailer is left out unread.
        let mut trailer = lossy.clone();
        push_chunk(&mut trailer, b"XYZW", b"junk");
        let checked = webp(&one_frame(&trailer)).unwrap();
        assert!(!checked.bytes.windows(4).any(|w| w == b"junk"));
        // At the top level the alpha plane must come right before its image.
        let mut chunks = extended(WEBP_ALPHA, 2, 1);
        chunks.extend_from_slice(&alpha(&[0, 255, 255]));
        let mut adjacent = chunks.clone();
        adjacent.extend_from_slice(&lossy);
        assert_eq!(webp(&riff(&adjacent)).unwrap().frames, 1);
        push_chunk(&mut chunks, b"ICCP", b"profile!");
        chunks.extend_from_slice(&lossy);
        assert_eq!(webp(&riff(&chunks)).unwrap_err(), PreviewError::Invalid);
    }
}
