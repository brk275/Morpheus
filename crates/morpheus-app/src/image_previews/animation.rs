//! Bounded, sequential playback of animated GIF and WebP previews with the
//! pinned `image` codecs. It runs on the preview's single dedicated worker,
//! which already owns the only decode permit. Frames are decoded strictly in
//! order, one at a time, and only the newest picture is handed to the window.
//! Nothing is collected ahead of playback; looping and Replay decode the same
//! in-memory plaintext again from its start.
//!
//! Bounds: the existing 8 MiB preview input, a canvas of at most 4096 pixels
//! per side and 2 megapixels for animation (larger ones show their first
//! frame), the codec allocation budget below, at most 1000 frames per pass,
//! delays no shorter than 20 ms, and at most ten minutes of playback per
//! start. Pausing, closing or any newer view stops the worker's work.

use std::{
    io::Cursor,
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

use image::{
    AnimationDecoder, ColorType, Frames, ImageDecoder, ImageError, Limits,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
    metadata::LoopCount,
};
use zeroize::Zeroizing;

use super::{Pixels, PreviewError, decode};

/// Largest canvas that plays; larger animations show their first frame.
pub(super) const MAX_ANIMATED_PIXELS: usize = 2 * 1024 * 1024;
/// The codec's own buffers for one pass, including the frame it returns.
const MAX_CODEC_ALLOC: u64 = 64 * 1024 * 1024;
/// Frames decoded in one pass; any further frames are not shown.
const MAX_FRAMES: usize = 1000;
/// Delays shorter than this play at the customary 100 ms instead.
const MIN_DELAY: Duration = Duration::from_millis(20);
const SHORT_DELAY: Duration = Duration::from_millis(100);
/// Continuous playback per Play or autoplay before it pauses itself.
const MAX_PLAY: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Format {
    Gif,
    WebP,
}

impl Format {
    pub(super) fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.get(..4) == Some(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some(Self::WebP)
        } else {
            None
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Gif => "GIF",
            Self::WebP => "WebP",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Control {
    Play,
    Pause,
    Replay,
}

/// The playback state shown with each picture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Playback {
    pub(super) format: Format,
    pub(super) playing: bool,
    /// Every planned loop has played; Play or Replay starts it again.
    pub(super) ended: bool,
    pub(super) note: Option<&'static str>,
}

/// One composited canvas picture and how long it stays.
pub(super) struct Picture {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) rgba: Zeroizing<Vec<u8>>,
    pub(super) delay: Duration,
}

impl Picture {
    pub(super) fn into_pixels(self) -> Pixels {
        Pixels {
            width: self.width,
            height: self.height,
            rgba: self.rgba,
        }
    }
}

/// What opening the plaintext found.
pub(super) enum Opened<'a> {
    Still(Pixels),
    Animated(Pass<'a>),
}

/// One pass over an animation's frames, decoding each only when asked.
pub(super) struct Pass<'a> {
    format: Format,
    frames: Frames<'a>,
    loops: LoopCount,
    width: u32,
    height: u32,
    decoded: usize,
}

fn codec_error(error: ImageError) -> PreviewError {
    match error {
        ImageError::Limits(_) => PreviewError::TooLarge,
        ImageError::Unsupported(_) => PreviewError::Unsupported,
        _ => PreviewError::Invalid,
    }
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(decode::MAX_SIDE);
    limits.max_image_height = Some(decode::MAX_SIDE);
    limits.max_alloc = Some(MAX_CODEC_ALLOC);
    limits
}

/// GIF and WebP state their canvas before any frame data; refuse an
/// oversized one before a decoder exists.
fn header_dimensions(format: Format, bytes: &[u8]) -> Option<(u32, u32)> {
    let le16 = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])))
    };
    let le24 = |at: usize| {
        bytes
            .get(at..at + 3)
            .map(|b| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
    };
    match format {
        Format::Gif => Some((le16(6)?, le16(8)?)),
        // An extended WebP's VP8X chunk; simple files are checked after
        // their header is parsed instead.
        Format::WebP if bytes.get(12..16) == Some(b"VP8X") => Some((le24(24)? + 1, le24(27)? + 1)),
        Format::WebP => None,
    }
}

impl<'a> Pass<'a> {
    pub(super) fn open(format: Format, bytes: &'a [u8]) -> Result<Opened<'a>, PreviewError> {
        if bytes.len() > morpheus_matrix::MAX_IMAGE_PREVIEW_BYTES {
            return Err(PreviewError::TooLarge);
        }
        if let Some((width, height)) = header_dimensions(format, bytes) {
            decode::size(width, height)?;
        }
        let (frames, loops, width, height) = match format {
            Format::Gif => {
                let mut decoder = GifDecoder::new(Cursor::new(bytes)).map_err(codec_error)?;
                let (width, height) = decoder.dimensions();
                decode::size(width, height)?;
                decoder.set_limits(limits()).map_err(codec_error)?;
                let loops = decoder.loop_count();
                (decoder.into_frames(), loops, width, height)
            }
            Format::WebP => {
                let mut decoder = WebPDecoder::new(Cursor::new(bytes)).map_err(codec_error)?;
                let (width, height) = decoder.dimensions();
                decode::size(width, height)?;
                decoder.set_limits(limits()).map_err(codec_error)?;
                if !decoder.has_animation() {
                    return still_webp(decoder, width, height).map(Opened::Still);
                }
                let loops = decoder.loop_count();
                (decoder.into_frames(), loops, width, height)
            }
        };
        Ok(Opened::Animated(Pass {
            format,
            frames,
            loops,
            width,
            height,
            decoded: 0,
        }))
    }

    pub(super) fn format(&self) -> Format {
        self.format
    }

    /// Whether this canvas is small enough to play.
    pub(super) fn playable(&self) -> bool {
        decode::size(self.width, self.height).is_ok_and(|bytes| bytes / 4 <= MAX_ANIMATED_PIXELS)
    }

    /// Decode the next frame, or `None` after the last one this pass shows.
    pub(super) fn next_frame(&mut self) -> Result<Option<Picture>, PreviewError> {
        if self.decoded >= MAX_FRAMES {
            return Ok(None);
        }
        let Some(frame) = self.frames.next() else {
            return Ok(None);
        };
        let frame = frame.map_err(codec_error)?;
        self.decoded += 1;
        let delay = frame_delay(frame.delay());
        let buffer = frame.into_buffer();
        if buffer.dimensions() != (self.width, self.height) {
            return Err(PreviewError::Invalid);
        }
        Ok(Some(Picture {
            width: self.width,
            height: self.height,
            rgba: Zeroizing::new(buffer.into_raw()),
            delay,
        }))
    }

    /// Whether the animation plays again after `completed` full passes.
    fn repeats_after(&self, completed: u32) -> bool {
        match self.loops {
            LoopCount::Infinite => true,
            LoopCount::Finite(count) => completed < count.get(),
        }
    }
}

fn still_webp(
    decoder: WebPDecoder<Cursor<&[u8]>>,
    width: u32,
    height: u32,
) -> Result<Pixels, PreviewError> {
    let length = decode::size(width, height)?;
    let channels = match decoder.color_type() {
        ColorType::Rgb8 => 3,
        ColorType::Rgba8 => 4,
        _ => return Err(PreviewError::Unsupported),
    };
    let mut raw = Zeroizing::new(vec![0; length / 4 * channels]);
    if decoder.total_bytes() != raw.len() as u64 {
        return Err(PreviewError::Invalid);
    }
    decoder.read_image(&mut raw).map_err(codec_error)?;
    let rgba = if channels == 4 {
        raw
    } else {
        let mut rgba = Zeroizing::new(Vec::with_capacity(length));
        for pixel in raw.chunks_exact(3) {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
        rgba
    };
    Ok(Pixels {
        width,
        height,
        rgba,
    })
}

fn frame_delay(delay: image::Delay) -> Duration {
    let (numerator, denominator) = delay.numer_denom_ms();
    let millis = u64::from(numerator) / u64::from(denominator.max(1));
    let delay = Duration::from_millis(millis);
    if delay < MIN_DELAY { SHORT_DELAY } else { delay }
}

/// Play an animation whose first two frames are already decoded. `show`
/// publishes each picture with its state; it must only store the newest one
/// and queue at most one window update. Returns when the viewer drops the
/// command sender, `current` turns false, or the plaintext can't be read.
pub(super) fn play<'a>(
    bytes: &'a [u8],
    mut pass: Pass<'a>,
    first: Picture,
    second: Picture,
    commands: &Receiver<Control>,
    autoplay: bool,
    current: &dyn Fn() -> bool,
    show: &dyn Fn(&Picture, Playback),
) {
    let format = pass.format();
    let mut state = Playback {
        format,
        playing: autoplay,
        ended: false,
        note: None,
    };
    let mut started = Instant::now();
    let mut completed = 0_u32;
    let mut shown = first;
    let mut pending = Some(second);
    show(&shown, state);
    let mut due = Instant::now() + shown.delay;
    loop {
        if !current() {
            return;
        }
        let command = if state.playing {
            commands.recv_timeout(due.saturating_duration_since(Instant::now()))
        } else {
            commands.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match command {
            Ok(Control::Pause) => {
                state.playing = false;
                state.note = None;
                show(&shown, state);
                continue;
            }
            Ok(Control::Play) if !state.ended => {
                state.playing = true;
                state.note = None;
                started = Instant::now();
                due = Instant::now();
                show(&shown, state);
                continue;
            }
            Ok(Control::Play | Control::Replay) => {
                // Start again from the first frame.
                match restart(format, bytes) {
                    Ok((next, picture)) => {
                        pass = next;
                        pending = None;
                        completed = 0;
                        shown = picture;
                        state = Playback {
                            format,
                            playing: true,
                            ended: false,
                            note: None,
                        };
                        started = Instant::now();
                        due = Instant::now() + shown.delay;
                        show(&shown, state);
                    }
                    Err(_) => return,
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !current() {
            return;
        }
        let next = match pending.take() {
            Some(picture) => Ok(Some(picture)),
            None => pass.next_frame(),
        };
        match next {
            Ok(Some(picture)) => {
                shown = picture;
                due = Instant::now() + shown.delay;
                show(&shown, state);
            }
            Ok(None) => {
                completed = completed.saturating_add(1);
                if !pass.repeats_after(completed) {
                    state.playing = false;
                    state.ended = true;
                    show(&shown, state);
                    continue;
                }
                match restart(format, bytes) {
                    Ok((next, picture)) => {
                        pass = next;
                        shown = picture;
                        due = Instant::now() + shown.delay;
                        show(&shown, state);
                    }
                    Err(_) => return,
                }
            }
            Err(_) => {
                state.playing = false;
                state.ended = true;
                state.note = Some("A later frame couldn't be decoded; playback stopped.");
                show(&shown, state);
                continue;
            }
        }
        if state.playing && started.elapsed() >= MAX_PLAY {
            state.playing = false;
            state.note = Some("Paused after ten minutes of playback.");
            show(&shown, state);
        }
    }
}

/// A fresh pass positioned after its first frame.
fn restart(format: Format, bytes: &[u8]) -> Result<(Pass<'_>, Picture), PreviewError> {
    let Opened::Animated(mut pass) = Pass::open(format, bytes)? else {
        return Err(PreviewError::Invalid);
    };
    let first = pass.next_frame()?.ok_or(PreviewError::Invalid)?;
    Ok((pass, first))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{
        Delay, Frame, RgbaImage,
        codecs::gif::{GifEncoder, Repeat},
    };
    use std::sync::mpsc::sync_channel;

    fn gif(frames: &[[u8; 4]], repeat: Repeat) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            encoder.set_repeat(repeat).unwrap();
            for color in frames {
                let image = RgbaImage::from_pixel(2, 2, image::Rgba(*color));
                encoder
                    .encode_frame(Frame::from_parts(
                        image,
                        0,
                        0,
                        Delay::from_numer_denom_ms(50, 1),
                    ))
                    .unwrap();
            }
        }
        bytes
    }

    fn animated(bytes: &[u8]) -> Pass<'_> {
        match Pass::open(Format::Gif, bytes).unwrap() {
            Opened::Animated(pass) => pass,
            Opened::Still(_) => panic!("expected an animation"),
        }
    }

    #[test]
    fn gif_frames_decode_in_order_with_their_delays() {
        let bytes = gif(&[[255, 0, 0, 255], [0, 0, 255, 255]], Repeat::Infinite);
        assert_eq!(Format::sniff(&bytes), Some(Format::Gif));
        let mut pass = animated(&bytes);
        assert!(pass.playable());
        let first = pass.next_frame().unwrap().unwrap();
        assert_eq!((first.width, first.height), (2, 2));
        assert_eq!(&first.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(first.delay, Duration::from_millis(50));
        let second = pass.next_frame().unwrap().unwrap();
        assert_eq!(&second.rgba[..4], &[0, 0, 255, 255]);
        assert!(pass.next_frame().unwrap().is_none());
        assert!(pass.repeats_after(1000));
    }

    #[test]
    fn short_delays_play_at_the_customary_rate() {
        assert_eq!(frame_delay(Delay::from_numer_denom_ms(0, 1)), SHORT_DELAY);
        assert_eq!(frame_delay(Delay::from_numer_denom_ms(10, 1)), SHORT_DELAY);
        assert_eq!(
            frame_delay(Delay::from_numer_denom_ms(70, 1)),
            Duration::from_millis(70)
        );
    }

    #[test]
    fn oversized_and_foreign_inputs_never_reach_a_decoder() {
        let mut bytes = gif(&[[1, 2, 3, 255]], Repeat::Finite(0));
        bytes[6..8].copy_from_slice(&5000_u16.to_le_bytes());
        assert!(matches!(
            Pass::open(Format::Gif, &bytes),
            Err(PreviewError::TooLarge)
        ));
        assert_eq!(Format::sniff(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(Format::sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::WebP));
        assert!(Pass::open(Format::WebP, b"RIFF\0\0\0\0WEBPVP8 ").is_err());
    }

    #[test]
    fn paused_playback_waits_and_a_dropped_viewer_stops_the_worker() {
        let bytes = gif(&[[255, 0, 0, 255], [0, 0, 255, 255]], Repeat::Infinite);
        let mut pass = animated(&bytes);
        let first = pass.next_frame().unwrap().unwrap();
        let second = pass.next_frame().unwrap().unwrap();
        let (sender, receiver) = sync_channel(4);
        let shown = std::cell::RefCell::new(Vec::new());
        sender.send(Control::Play).unwrap();
        sender.send(Control::Pause).unwrap();
        drop(sender);
        play(
            &bytes,
            pass,
            first,
            second,
            &receiver,
            false,
            &|| true,
            &|picture, state| shown.borrow_mut().push((picture.rgba[2], state.playing)),
        );
        let shown = shown.into_inner();
        // Paused on the first frame, then Play and Pause, then the sender
        // closed: the worker returned without decoding on its own.
        assert_eq!(shown.first(), Some(&(0, false)));
        assert_eq!(shown.last().map(|(_, playing)| *playing), Some(false));
        assert!(shown.len() <= 4);
    }
}
