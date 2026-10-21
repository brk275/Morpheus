//! Bounded, memory-only preparation for room-avatar uploads. No chooser,
//! account store, filesystem, Slint object, or network operation is created here.
//! The existing PNG/baseline-JPEG decoder strips metadata before decoding.

use super::{Pixels, PreviewError, decode};
use image::{
    ExtendedColorType, ImageEncoder,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use morpheus_matrix::{
    AccountSelection, AvatarPng, MAX_IMAGE_PREVIEW_BYTES, MAX_ROOM_AVATAR_UPLOAD_BYTES,
    RoomSettingsCancellation,
};
use std::{
    io::{self, Write},
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};
use zeroize::Zeroizing;

pub(crate) const MAX_PREPARED_AVATAR_SIDE: u32 = 512;
static PREPARATION_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Which form holds the shared worker (an `AvatarConsumer` as u8, 0 none),
/// only so a waiting form can say who has it. Never used for admission.
static PREPARATION_OWNER: AtomicU8 = AtomicU8::new(0);

/// The forms that share the one chooser and preparation worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum AvatarConsumer {
    RoomSettings = 1,
    AccountProfile = 2,
}
impl AvatarConsumer {
    pub(crate) fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::RoomSettings),
            2 => Some(Self::AccountProfile),
            _ => None,
        }
    }
}

/// The form holding the preparation worker now, if it said.
pub(crate) fn preparation_owner() -> Option<AvatarConsumer> {
    AvatarConsumer::from_u8(PREPARATION_OWNER.load(Ordering::Acquire))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AvatarPrepareError {
    Busy,
    Cancelled,
    TooLarge,
    Unsupported,
    Invalid,
    Unavailable,
}
impl AvatarPrepareError {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Busy => "The previous image is still being prepared. Try again shortly.",
            Self::Cancelled => "This room settings form is no longer active.",
            Self::TooLarge => {
                "Choose an image at most 8 MiB, 4096 pixels per side, and 4 megapixels."
            }
            Self::Unsupported => {
                "Choose a static 8-bit PNG or baseline JPEG. Animated and progressive images are not supported yet."
            }
            Self::Invalid => "This image could not be prepared safely.",
            Self::Unavailable => "Image preparation is unavailable. Try again shortly.",
        }
    }
}
impl From<PreviewError> for AvatarPrepareError {
    fn from(error: PreviewError) -> Self {
        match error {
            PreviewError::TooLarge => Self::TooLarge,
            PreviewError::Unsupported => Self::Unsupported,
            PreviewError::Invalid => Self::Invalid,
            PreviewError::Busy => Self::Busy,
            PreviewError::Unavailable => Self::Unavailable,
        }
    }
}

/// No Debug/Serialize/Clone: encoded data and preview pixels stay in memory.
/// The parent must recheck the captured form/account before publishing a Slint
/// Image, and create that image only in its UI-thread completion closure.
/// Zeroizing covers our owned buffers, not library scratch or GPU allocations.
pub(crate) struct PreparedAvatar {
    png: Zeroizing<Vec<u8>>,
    pixels: Pixels,
}
impl PreparedAvatar {
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.pixels.width, self.pixels.height)
    }
    pub(crate) fn rgba(&self) -> &[u8] {
        self.pixels.rgba.as_slice()
    }
    pub(crate) fn encoded_len(&self) -> usize {
        self.png.len()
    }
    /// Consumes the staged image only on explicit Save. Call from the parent's
    /// background save task: core envelope validation includes bounded CRC work.
    /// After transfer, the SDK/core owns the PNG; this helper cannot promise
    /// zeroization of their upload buffers. No image decoding/encoding occurs here.
    /// A validated copy of the staged PNG for one upload attempt, keeping the
    /// staged image so a failed attempt can be retried without choosing the
    /// file again. Like `into_upload`, call from a background task.
    pub(crate) fn upload_copy(&self) -> Result<AvatarPng, AvatarPrepareError> {
        AvatarPng::new(self.png.to_vec(), self.pixels.width, self.pixels.height)
            .map_err(|_| AvatarPrepareError::Invalid)
    }
    pub(crate) fn into_upload(mut self) -> Result<AvatarPng, AvatarPrepareError> {
        let bytes = std::mem::take(&mut *self.png);
        AvatarPng::new(bytes, self.pixels.width, self.pixels.height)
            .map_err(|_| AvatarPrepareError::Invalid)
    }
}

/// Clears the owner mark before the permit is released (declared after the
/// permit, so it drops first).
struct OwnerMark<'a>(&'a AtomicU8);
impl Drop for OwnerMark<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

struct WorkerPermit<'a>(&'a AtomicBool);
impl<'a> WorkerPermit<'a> {
    fn acquire(gate: &'a AtomicBool) -> Option<Self> {
        gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(gate))
    }
}
impl Drop for WorkerPermit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The only production preparation entry point. One dedicated worker may run
/// process-wide, independently of the attachment-preview worker. It never uses
/// Tokio's blocking pool or delays runtime shutdown. Dropping this future cannot
/// recall an entered codec call: that worker retains its permit until it exits.
/// Cancel the form token on Close/navigation; its result is checked between
/// CPU stages and again on receipt. Account revocation also wakes this await.
pub(crate) async fn prepare_avatar(
    bytes: Zeroizing<Vec<u8>>,
    selection: AccountSelection,
    cancellation: RoomSettingsCancellation,
) -> Result<PreparedAvatar, AvatarPrepareError> {
    prepare_avatar_for(AvatarConsumer::RoomSettings, bytes, selection, cancellation).await
}

/// `prepare_avatar` for a named form, so another form waiting on the shared
/// worker can say which one holds it.
pub(crate) async fn prepare_avatar_for(
    consumer: AvatarConsumer,
    bytes: Zeroizing<Vec<u8>>,
    selection: AccountSelection,
    cancellation: RoomSettingsCancellation,
) -> Result<PreparedAvatar, AvatarPrepareError> {
    let active = || selection.is_current() && !cancellation.is_cancelled();
    require_active(active())?;
    if bytes.len() > MAX_IMAGE_PREVIEW_BYTES {
        return Err(AvatarPrepareError::TooLarge);
    }
    let permit = WorkerPermit::acquire(&PREPARATION_ACTIVE).ok_or(AvatarPrepareError::Busy)?;
    PREPARATION_OWNER.store(consumer as u8, Ordering::Release);
    let worker_selection = selection.clone();
    let worker_cancel = cancellation.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("morpheus-avatar-preparation".into())
        .spawn(move || {
            let _permit = permit;
            let _owner = OwnerMark(&PREPARATION_OWNER);
            let result = prepare_image(bytes, || {
                worker_selection.is_current() && !worker_cancel.is_cancelled()
            });
            // On receiver cancellation, the returned image is dropped here and its
            // owned buffers are zeroized; the permit is released only afterward.
            let _ = sender.send(result);
        })
        .map_err(|_| {
            PREPARATION_OWNER.store(0, Ordering::Release);
            AvatarPrepareError::Unavailable
        })?;
    let result = tokio::select! {
        biased;
        _=selection.revoked()=>return Err(AvatarPrepareError::Cancelled),
        result=receiver=>result.map_err(|_|AvatarPrepareError::Unavailable)?,
    };
    require_active(active())?;
    result
}
fn require_active(active: bool) -> Result<(), AvatarPrepareError> {
    if active {
        Ok(())
    } else {
        Err(AvatarPrepareError::Cancelled)
    }
}
fn prepare_image(
    bytes: Zeroizing<Vec<u8>>,
    active: impl Fn() -> bool,
) -> Result<PreparedAvatar, AvatarPrepareError> {
    require_active(active())?;
    let pixels = decode::decode(bytes)?;
    require_active(active())?;
    let pixels = downsize(pixels)?;
    require_active(active())?;
    let png = encode_rgba(&pixels, MAX_ROOM_AVATAR_UPLOAD_BYTES)?;
    require_active(active())?;
    Ok(PreparedAvatar { png, pixels })
}

fn encode_rgba(pixels: &Pixels, maximum: usize) -> Result<Zeroizing<Vec<u8>>, AvatarPrepareError> {
    let mut output = BoundedPng::new(maximum);
    // An explicit RGBA8 encoder with no ICC/EXIF setters emits only structural
    // IHDR/IDAT/IEND chunks. Canonical here means this metadata-free shape, not
    // byte identity across future encoder/library versions.
    PngEncoder::new_with_quality(&mut output, CompressionType::Fast, FilterType::NoFilter)
        .write_image(
            pixels.rgba.as_slice(),
            pixels.width,
            pixels.height,
            ExtendedColorType::Rgba8,
        )
        .map_err(|_| {
            if output.full {
                AvatarPrepareError::TooLarge
            } else {
                AvatarPrepareError::Invalid
            }
        })?;
    // The pinned PNG writer finalizes IEND on Drop. An output-cap error there
    // cannot be returned by write_image, so the writer's latch is checked too.
    if output.full {
        return Err(AvatarPrepareError::TooLarge);
    }
    Ok(output.bytes)
}

struct BoundedPng {
    bytes: Zeroizing<Vec<u8>>,
    maximum: usize,
    full: bool,
}
impl BoundedPng {
    fn new(maximum: usize) -> Self {
        // Capacity is fixed up front: writes never reallocate a sensitive buffer
        // or grow beyond the upload bound. Production maximum is exactly 4 MiB.
        Self {
            bytes: Zeroizing::new(Vec::with_capacity(maximum)),
            maximum,
            full: false,
        }
    }
}
impl Write for BoundedPng {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(end) = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|end| *end <= self.maximum)
        else {
            self.full = true;
            return Err(io::Error::other("avatar output limit"));
        };
        debug_assert!(end <= self.bytes.capacity());
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn target_dimensions(width: u32, height: u32) -> Result<(u32, u32), AvatarPrepareError> {
    // Recheck the decoder contract before indexing or allocating resampled data.
    if width == 0
        || height == 0
        || width > decode::MAX_SIDE
        || height > decode::MAX_SIDE
        || u64::from(width) * u64::from(height) > decode::MAX_PIXELS as u64
    {
        return Err(AvatarPrepareError::TooLarge);
    }
    let long = width.max(height);
    if long <= MAX_PREPARED_AVATAR_SIDE {
        return Ok((width, height));
    }
    let scaled = |side: u32| {
        ((u64::from(side) * u64::from(MAX_PREPARED_AVATAR_SIDE) + u64::from(long) / 2)
            / u64::from(long))
        .max(1) as u32
    };
    Ok((scaled(width), scaled(height)))
}
fn downsize(source: Pixels) -> Result<Pixels, AvatarPrepareError> {
    let (width, height) = target_dimensions(source.width, source.height)?;
    if source.rgba.len() != source.width as usize * source.height as usize * 4 {
        return Err(AvatarPrepareError::Invalid);
    }
    if (width, height) == (source.width, source.height) {
        return Ok(source);
    }
    let mut rgba = Zeroizing::new(vec![0; width as usize * height as usize * 4]);
    // Integer area resampling with alpha premultiplication avoids colour bleed
    // from transparent pixels. No unzeroized ImageBuffer/float scratch copies.
    // Coordinates share exact integer scales. Bounds above keep all products
    // (including 255*255*area) well within u64, and work proportional to input.
    let sw = u64::from(source.width);
    let sh = u64::from(source.height);
    let dw = u64::from(width);
    let dh = u64::from(height);
    let area = sw * sh;
    for y in 0..dh {
        let top = y * sh;
        let bottom = (y + 1) * sh;
        for x in 0..dw {
            let left = x * sw;
            let right = (x + 1) * sw;
            let mut alpha = 0u64;
            let mut colour = [0u64; 3];
            for sy in top / dh..bottom.div_ceil(dh) {
                let wy = bottom.min((sy + 1) * dh) - top.max(sy * dh);
                for sx in left / dw..right.div_ceil(dw) {
                    let wx = right.min((sx + 1) * dw) - left.max(sx * dw);
                    let offset = (sy * sw + sx) as usize * 4;
                    let pixel = &source.rgba[offset..offset + 4];
                    let weighted_alpha = u64::from(pixel[3]) * wx * wy;
                    alpha += weighted_alpha;
                    for channel in 0..3 {
                        colour[channel] += u64::from(pixel[channel]) * weighted_alpha;
                    }
                }
            }
            let offset = (y * dw + x) as usize * 4;
            let output_alpha = ((alpha + area / 2) / area) as u8;
            rgba[offset + 3] = output_alpha;
            if output_alpha != 0 {
                for channel in 0..3 {
                    rgba[offset + channel] = ((colour[channel] + alpha / 2) / alpha) as u8;
                }
            }
        }
    }
    Ok(Pixels {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(rgba, width, height, ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }
    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        // Metadata CRC is deliberately irrelevant: the shared decoder discards
        // ancillary chunks before parsing/decompression; output CRCs are encoded.
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }
    fn chunks(bytes: &[u8]) -> Vec<[u8; 4]> {
        let mut result = Vec::new();
        let mut offset = 8;
        while offset < bytes.len() {
            let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            result.push(bytes[offset + 4..offset + 8].try_into().unwrap());
            offset += size + 12;
        }
        assert_eq!(offset, bytes.len());
        result
    }
    fn prepare(bytes: Vec<u8>) -> PreparedAvatar {
        prepare_image(Zeroizing::new(bytes), || true).unwrap()
    }

    #[test]
    fn png_metadata_is_removed_and_small_rgba_pixels_are_preserved_exactly() {
        let rgba = [12, 34, 56, 78, 250, 100, 3, 0, 1, 2, 3, 255, 8, 9, 10, 128];
        let mut original = png(2, 2, &rgba);
        let mut metadata = chunk(b"tEXt", b"author\0private fixture");
        metadata.extend_from_slice(&chunk(b"iCCP", b"unparsed compressed profile"));
        metadata.extend_from_slice(&chunk(b"eXIf", b"private fixture location"));
        original.splice(33..33, metadata);
        let prepared = prepare(original);
        assert_eq!(prepared.dimensions(), (2, 2));
        assert_eq!(prepared.rgba(), &rgba);
        assert!(
            chunks(&prepared.png)
                .iter()
                .all(|kind| matches!(kind, b"IHDR" | b"IDAT" | b"IEND"))
        );
        assert!(!prepared.png.windows(7).any(|bytes| bytes == b"private"));
        let decoded = decode::decode(prepared.png.clone()).unwrap();
        assert_eq!(decoded.rgba.as_slice(), &rgba);
        assert!(prepared.into_upload().is_ok());
    }

    #[test]
    fn baseline_jpeg_metadata_is_not_forwarded_and_output_is_rgba_png() {
        let mut input = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut input, 100)
            .encode(&[220, 30, 20], 1, 1, ExtendedColorType::Rgb8)
            .unwrap();
        let marker = b"Exif\0\0private fixture";
        let mut exif = vec![0xff, 0xe1];
        exif.extend_from_slice(&((marker.len() + 2) as u16).to_be_bytes());
        exif.extend_from_slice(marker);
        input.splice(2..2, exif);
        let prepared = prepare(input);
        assert_eq!(prepared.dimensions(), (1, 1));
        assert_eq!(prepared.rgba()[3], 255);
        assert!(prepared.rgba()[0] > 195 && prepared.rgba()[1] < 55 && prepared.rgba()[2] < 45);
        assert_eq!(&prepared.png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(
            chunks(&prepared.png)
                .iter()
                .all(|kind| matches!(kind, b"IHDR" | b"IDAT" | b"IEND"))
        );
        assert!(prepared.into_upload().is_ok());
    }

    #[test]
    fn aspect_ratio_rounds_at_most_one_pixel_and_never_upscales() {
        for ((w, h), (expected_w, expected_h)) in [
            ((20, 10), (20, 10)),
            ((512, 511), (512, 511)),
            ((1024, 512), (512, 256)),
            ((600, 900), (341, 512)),
            ((1, 4096), (1, 512)),
            ((4096, 1), (512, 1)),
        ] {
            assert_eq!(target_dimensions(w, h).unwrap(), (expected_w, expected_h));
        }
        let rgba = vec![90; 1024 * 512 * 4];
        let prepared = prepare(png(1024, 512, &rgba));
        assert_eq!(prepared.dimensions(), (512, 256));
        assert_eq!(prepared.rgba().len(), 512 * 256 * 4);
        assert!(
            prepared
                .rgba()
                .chunks_exact(4)
                .all(|pixel| pixel == [90, 90, 90, 90])
        );
        assert!(prepared.encoded_len() <= MAX_ROOM_AVATAR_UPLOAD_BYTES);
        assert!(prepared.into_upload().is_ok());
    }

    #[test]
    fn downsize_averages_alpha_without_bleeding_invisible_colours() {
        let mut rgba = Zeroizing::new(Vec::with_capacity(1024 * 4));
        for _ in 0..512 {
            rgba.extend_from_slice(&[255, 0, 0, 255, 0, 0, 255, 0]);
        }
        let pixels = downsize(Pixels {
            width: 1024,
            height: 1,
            rgba,
        })
        .unwrap();
        assert_eq!((pixels.width, pixels.height), (512, 1));
        assert!(
            pixels
                .rgba
                .chunks_exact(4)
                .all(|pixel| pixel == [255, 0, 0, 128])
        );
    }

    #[test]
    fn invalid_oversized_animated_and_progressive_inputs_are_rejected() {
        for input in [
            b"not an image".to_vec(),
            b"\xff\xd8\xff\xc2\0\x02".to_vec(),
            vec![0; MAX_IMAGE_PREVIEW_BYTES + 1],
        ] {
            assert!(prepare_image(Zeroizing::new(input), || true).is_err());
        }
        let mut input = png(1, 1, &[1, 2, 3, 255]);
        input.splice(33..33, chunk(b"acTL", &[0; 8]));
        assert!(matches!(
            prepare_image(Zeroizing::new(input), || true),
            Err(AvatarPrepareError::Unsupported)
        ));
        let mut input = png(1, 1, &[1, 2, 3, 255]);
        input[16..20].copy_from_slice(&4097u32.to_be_bytes());
        assert!(matches!(
            prepare_image(Zeroizing::new(input), || true),
            Err(AvatarPrepareError::TooLarge)
        ));
        assert!(
            downsize(Pixels {
                width: 2,
                height: 1,
                rgba: Zeroizing::new(vec![0; 4])
            })
            .is_err()
        );
    }

    #[test]
    fn output_bound_is_enforced_before_growth_and_cancelled_stages_cannot_publish() {
        let mut writer = BoundedPng::new(4);
        let initial_capacity = writer.bytes.capacity();
        writer.write_all(&[1, 2, 3, 4]).unwrap();
        assert!(writer.write_all(&[5]).is_err());
        assert!(writer.full);
        assert_eq!(writer.bytes.as_slice(), &[1, 2, 3, 4]);
        assert_eq!(writer.bytes.capacity(), initial_capacity);
        let pixels = Pixels {
            width: 1,
            height: 1,
            rgba: Zeroizing::new(vec![1, 2, 3, 255]),
        };
        let complete = encode_rgba(&pixels, MAX_ROOM_AVATAR_UPLOAD_BYTES).unwrap();
        // The final IEND cannot fit: even a finalizer-only write failure is fatal.
        assert!(matches!(
            encode_rgba(&pixels, complete.len() - 1),
            Err(AvatarPrepareError::TooLarge)
        ));
        let checks = std::cell::Cell::new(0);
        let result = prepare_image(Zeroizing::new(png(1, 1, &[1, 2, 3, 255])), || {
            let value = checks.get();
            checks.set(value + 1);
            value == 0
        });
        assert!(matches!(result, Err(AvatarPrepareError::Cancelled)));
        assert_eq!(checks.get(), 2);
        let gate = AtomicBool::new(false);
        let permit = WorkerPermit::acquire(&gate).unwrap();
        assert!(WorkerPermit::acquire(&gate).is_none());
        drop(permit);
        assert!(WorkerPermit::acquire(&gate).is_some());
    }
}
