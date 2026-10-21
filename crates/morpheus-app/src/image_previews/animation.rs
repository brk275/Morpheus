//! Bounded, sequential playback of animated GIF, WebP and PNG previews with
//! the pinned `image` codecs. It runs on the preview's single dedicated worker,
//! which already owns the only decode permit. Frames are decoded strictly in
//! order, one at a time, and only the newest picture is handed to the window.
//! Nothing is collected ahead of playback; looping and Replay decode the same
//! in-memory input again from its start.
//!
//! Every input is first checked and rebuilt (`prepare`): GIF and WebP by
//! framing.rs (frames placed inside the canvas, counted, metadata left out),
//! APNG by the PNG chunk sanitizer. A frame count over the limit is refused
//! there, before decoding, and a decoder that still finds more frames stops
//! with an error: the limit is never taken for the animation's natural end.
//!
//! Bounds, decided from checked headers and frame counts before any frame
//! after the first is decoded: the existing 8 MiB preview input, a canvas of
//! at most 4096 pixels per side and 2 megapixels for animation, at most 1000
//! frames and MAX_PASS_PIXELS canvas pixels per pass (a larger or longer
//! animation shows its first frame), delays no shorter than 20 ms, and at
//! most ten minutes of continuous playback per start, however long a frame's
//! own delay. Pausing, closing or any newer view stops the worker between
//! frames; one frame already being decoded finishes first. These are a finite
//! complexity policy, not a hard memory total or a realtime guarantee.
//!
//! Whether playback starts by itself is decided when it would start, from
//! the window's current autoplay setting and reduced motion (`Motion`), and
//! commands travel through a one-slot `Mailbox` whose newest command always
//! is retained. A reduced-motion pause replaces older commands and takes
//! effect at the next wait; one frame already being decoded can finish first.

use std::{
    io::Cursor,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use image::{
    AnimationDecoder, ColorType, Frames, ImageDecoder, ImageError, Limits,
    codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
    metadata::LoopCount,
};
use zeroize::Zeroizing;

use super::{Pixels, PreviewError, decode, framing};

/// Largest canvas that plays; larger animations show their first frame.
pub(super) const MAX_ANIMATED_PIXELS: usize = 2 * 1024 * 1024;
/// Canvas pixels one pass may decode and composite: the frame count times
/// the canvas (every frame is drawn onto the whole canvas). For example 256
/// frames at 2 megapixels, or 1000 frames at half a megapixel. A longer
/// animation at its size shows its first frame instead.
const MAX_PASS_PIXELS: u64 = 512 * 1024 * 1024;
/// Passed as `image::Limits` to the GIF and PNG codecs, which count some of
/// their own allocations against it. It is not a total for the preview, and
/// the pinned WebP codec takes no such budget at all: framing.rs bounds what
/// reaches it instead (see docs/IMAGE_PREVIEWS.md for the peak reasoning).
const MAX_CODEC_ALLOC: u64 = 64 * 1024 * 1024;
/// Frames decoded in one pass; framing refuses any animation with more.
const MAX_FRAMES: usize = decode::MAX_ANIMATION_FRAMES;
/// Delays shorter than this play at the customary 100 ms instead.
const MIN_DELAY: Duration = Duration::from_millis(20);
const SHORT_DELAY: Duration = Duration::from_millis(100);
/// Continuous playback per Play or autoplay before it pauses itself.
const MAX_PLAY: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Format {
    Gif,
    WebP,
    /// Only a PNG that declares an animation; still PNGs keep their path.
    Apng,
}

impl Format {
    pub(super) fn sniff(bytes: &[u8]) -> Option<Self> {
        if decode::png_animated(bytes) {
            Some(Self::Apng)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
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
            Self::Apng => "PNG",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Control {
    Play,
    Pause,
    Replay,
}

/// The newest playback command for one animation, between the viewer and
/// the player. A newer command replaces one the player has not taken yet,
/// so the latest intent (a forced pause included) always arrives and none
/// can be lost in a full queue.
#[derive(Default)]
struct Mailbox {
    slot: Mutex<Slot>,
    ready: Condvar,
}

#[derive(Default)]
struct Slot {
    command: Option<Control>,
    closed: bool,
}

/// The viewer's end of one animation's commands. Dropping it (closing, or
/// any newer view) stops the player at its next wait, at once.
pub(super) struct Remote(Arc<Mailbox>);

/// The player's end.
pub(super) struct Commands(Arc<Mailbox>);

/// The viewer let go of the animation.
#[derive(Debug)]
pub(super) struct Closed;

pub(super) fn commands() -> (Remote, Commands) {
    let mailbox = Arc::new(Mailbox::default());
    (Remote(mailbox.clone()), Commands(mailbox))
}

impl Remote {
    /// Keep the latest command without channel backpressure or queue failure.
    pub(super) fn send(&self, control: Control) {
        let mut slot = self.0.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.command = Some(control);
        self.0.ready.notify_one();
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let mut slot = self.0.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.closed = true;
        self.0.ready.notify_one();
    }
}

impl Commands {
    /// The newest command, waiting for one or, with a timeout, at most that
    /// long (`Ok(None)`). Once the viewer has let go, `Err(Closed)` even if a
    /// command is waiting.
    pub(super) fn wait(&self, timeout: Option<Duration>) -> Result<Option<Control>, Closed> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut slot = self.0.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if slot.closed {
                return Err(Closed);
            }
            if let Some(command) = slot.command.take() {
                return Ok(Some(command));
            }
            slot = match deadline {
                None => self.0.ready.wait(slot).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    let (slot, _) = self
                        .0
                        .ready
                        .wait_timeout(slot, left)
                        .unwrap_or_else(|e| e.into_inner());
                    slot
                }
            };
        }
    }
}

/// The window's motion preferences, mirrored on the UI thread whenever they
/// change. The player reads them at the moment playback would start by
/// itself, never from a copy taken when the image was opened.
#[derive(Default)]
pub(super) struct Motion {
    autoplay: AtomicBool,
    reduced: AtomicBool,
}

impl Motion {
    /// Mirror the current autoplay setting and reduced motion. True when
    /// reduced motion has just turned on: a playing animation must pause.
    pub(super) fn set(&self, autoplay: bool, reduced: bool) -> bool {
        self.autoplay.store(autoplay, Ordering::SeqCst);
        !self.reduced.swap(reduced, Ordering::SeqCst) && reduced
    }

    /// Whether playback starts by itself now: autoplay is on and reduced
    /// motion is off. Explicit Play and Replay never ask.
    fn starts(&self) -> bool {
        self.autoplay.load(Ordering::SeqCst) && !self.reduced.load(Ordering::SeqCst)
    }
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

/// A sniffed input, checked and rebuilt, held for the whole playback so
/// looping and Replay decode the same bytes again; a GIF's loop count, read
/// from the setting the rebuilt stream leaves out; and how many frames one
/// pass may decode.
pub(super) struct Prepared {
    bytes: Zeroizing<Vec<u8>>,
    loops: Option<LoopCount>,
    frames: usize,
}

/// Check and rebuild `bytes` as `format` before any decoder sees them.
pub(super) fn prepare(format: Format, bytes: &[u8]) -> Result<Prepared, PreviewError> {
    if bytes.len() > morpheus_matrix::MAX_IMAGE_PREVIEW_BYTES {
        return Err(PreviewError::TooLarge);
    }
    Ok(match format {
        // Framing counted every frame; a pass decodes no more than that.
        Format::Gif => {
            let gif = framing::gif(bytes)?;
            Prepared {
                bytes: gif.bytes,
                loops: Some(gif.loops.into()),
                frames: gif.frames,
            }
        }
        Format::WebP => {
            let webp = framing::webp(bytes)?;
            Prepared {
                bytes: webp.bytes,
                loops: None,
                frames: webp.frames,
            }
        }
        // The same chunk framing as still PNGs plus the bounded animation
        // chunks, each frame's rectangle checked and counted.
        Format::Apng => {
            let (bytes, frames) = decode::png_animation_input(bytes)?;
            Prepared {
                bytes,
                loops: None,
                frames,
            }
        }
    })
}

/// What opening the input found.
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
    /// Frames this pass may decode: the count framing found, at most
    /// `MAX_FRAMES`.
    limit: usize,
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
        Format::WebP | Format::Apng => None,
    }
}

impl<'a> Pass<'a> {
    /// Open a `prepare`d input.
    pub(super) fn open(format: Format, prepared: &'a Prepared) -> Result<Opened<'a>, PreviewError> {
        let bytes = prepared.bytes.as_slice();
        if let Some((width, height)) = header_dimensions(format, bytes) {
            decode::size(width, height)?;
        }
        let (frames, loops, width, height) = match format {
            Format::Gif => {
                let mut decoder = GifDecoder::new(Cursor::new(bytes)).map_err(codec_error)?;
                let (width, height) = decoder.dimensions();
                decode::size(width, height)?;
                decoder.set_limits(limits()).map_err(codec_error)?;
                // The rebuilt stream carries no loop setting: the count read
                // before it was rebuilt applies.
                let loops = prepared.loops.unwrap_or(decoder.loop_count());
                (decoder.into_frames(), loops, width, height)
            }
            Format::Apng => {
                let decoder =
                    PngDecoder::with_limits(Cursor::new(bytes), limits()).map_err(codec_error)?;
                let (width, height) = decoder.dimensions();
                decode::size(width, height)?;
                if !decoder.is_apng().map_err(codec_error)? {
                    return Err(PreviewError::Invalid);
                }
                let decoder = decoder.apng().map_err(codec_error)?;
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
            limit: prepared.frames.min(MAX_FRAMES),
        }))
    }

    pub(super) fn format(&self) -> Format {
        self.format
    }

    /// Why this animation shows only its first frame instead of playing: a
    /// canvas over 2 megapixels, or more frames than one pass may decode at
    /// that canvas size. Decided from the checked header and frame count,
    /// before any frame after the first is decoded.
    pub(super) fn still_reason(&self) -> Option<&'static str> {
        let canvas = u64::from(self.width) * u64::from(self.height);
        if canvas > MAX_ANIMATED_PIXELS as u64 {
            Some("Shown still: animations over 2 megapixels don't play.")
        } else if canvas.saturating_mul(self.limit as u64) > MAX_PASS_PIXELS {
            Some("Shown still: this animation has too many frames at its size to play.")
        } else {
            None
        }
    }

    /// Decode the next frame, or `None` at the end: only once every frame the
    /// checks counted has been decoded. A decoder that stops sooner or finds
    /// more is an error, so a cut-short pass is never shown as complete.
    pub(super) fn next_frame(&mut self) -> Result<Option<Picture>, PreviewError> {
        let Some(frame) = self.frames.next() else {
            return if self.decoded == self.limit {
                Ok(None)
            } else {
                Err(PreviewError::Invalid)
            };
        };
        let frame = frame.map_err(codec_error)?;
        let delay = frame_delay(frame.delay());
        let dimensions = frame.buffer().dimensions();
        // Owned and zeroized on drop before any check below can refuse it.
        let rgba = Zeroizing::new(frame.into_buffer().into_raw());
        // Framing admitted at most MAX_FRAMES frames and counted them; a
        // decoder finding more is refused rather than taken as the end.
        if self.decoded >= self.limit {
            return Err(if self.decoded >= MAX_FRAMES {
                PreviewError::TooLarge
            } else {
                PreviewError::Invalid
            });
        }
        self.decoded += 1;
        if dimensions != (self.width, self.height) {
            return Err(PreviewError::Invalid);
        }
        Ok(Some(Picture {
            width: self.width,
            height: self.height,
            rgba,
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
    if delay < MIN_DELAY {
        SHORT_DELAY
    } else {
        delay
    }
}

/// Why playback stopped (or never started) after a later frame failed: over
/// the frame or codec memory limit, or undecodable.
pub(super) fn failure_note(error: PreviewError) -> &'static str {
    if error == PreviewError::TooLarge {
        "Playback stopped: this animation exceeds the preview's frame or memory limit."
    } else {
        "Playback stopped: a later frame couldn't be decoded."
    }
}

/// Play an animation whose first two frames are already decoded. `show`
/// publishes each picture with its state; it must only store the newest one
/// and queue at most one window update. Returns when the viewer lets go of
/// `commands` or `current` turns false; a frame that can't be read ends
/// playback on the picture already shown.
#[allow(clippy::too_many_arguments)]
pub(super) fn play<'a>(
    prepared: &'a Prepared,
    pass: Pass<'a>,
    first: Picture,
    second: Picture,
    commands: &Commands,
    motion: &Motion,
    current: &dyn Fn() -> bool,
    show: &dyn Fn(&Picture, Playback),
) {
    run(
        prepared, pass, first, second, commands, motion, current, show, MAX_PLAY,
    );
}

/// `play` with a continuous-playback budget of `max_play`.
#[allow(clippy::too_many_arguments)]
fn run<'a>(
    prepared: &'a Prepared,
    mut pass: Pass<'a>,
    first: Picture,
    second: Picture,
    commands: &Commands,
    motion: &Motion,
    current: &dyn Fn() -> bool,
    show: &dyn Fn(&Picture, Playback),
    max_play: Duration,
) {
    let format = pass.format();
    // Decided now, as playback would start, from the window's current
    // preferences; a reduced-motion change since then arrives as a Pause.
    let mut state = Playback {
        format,
        playing: motion.starts(),
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
        // Playing, wait for the next frame but never past the rest of the
        // continuous-playback budget, however long the frame's own delay;
        // paused, wait for a command.
        let timeout = state.playing.then(|| {
            let left = max_play.saturating_sub(started.elapsed());
            due.saturating_duration_since(Instant::now()).min(left)
        });
        let Ok(command) = commands.wait(timeout) else {
            return;
        };
        match command {
            Some(Control::Pause) => {
                state.playing = false;
                state.note = None;
                show(&shown, state);
                continue;
            }
            Some(Control::Play) if !state.ended => {
                state.playing = true;
                state.note = None;
                started = Instant::now();
                due = Instant::now();
                show(&shown, state);
                continue;
            }
            Some(Control::Play | Control::Replay) => {
                // Start again from the first frame.
                match restart(format, prepared) {
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
                    Err(error) => stop(&shown, &mut state, error, show),
                }
                continue;
            }
            None => {}
        }
        if !current() {
            return;
        }
        // A spent budget pauses before any further decode or restart.
        if started.elapsed() >= max_play {
            state.playing = false;
            state.note = Some("Paused after ten minutes of playback.");
            show(&shown, state);
            continue;
        }
        if Instant::now() < due {
            continue;
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
                match restart(format, prepared) {
                    Ok((next, picture)) => {
                        pass = next;
                        shown = picture;
                        due = Instant::now() + shown.delay;
                        show(&shown, state);
                    }
                    Err(error) => {
                        stop(&shown, &mut state, error, show);
                        continue;
                    }
                }
            }
            Err(error) => {
                stop(&shown, &mut state, error, show);
                continue;
            }
        }
    }
}

/// End playback on the picture already shown after a frame or restart
/// failed, saying why; Play or Replay tries again from the first frame.
fn stop(
    shown: &Picture,
    state: &mut Playback,
    error: PreviewError,
    show: &dyn Fn(&Picture, Playback),
) {
    state.playing = false;
    state.ended = true;
    state.note = Some(failure_note(error));
    show(shown, *state);
}

/// A fresh pass positioned after its first frame.
fn restart(format: Format, prepared: &Prepared) -> Result<(Pass<'_>, Picture), PreviewError> {
    let Opened::Animated(mut pass) = Pass::open(format, prepared)? else {
        return Err(PreviewError::Invalid);
    };
    let first = pass.next_frame()?.ok_or(PreviewError::Invalid)?;
    Ok((pass, first))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{
        Delay, Frame, ImageEncoder, RgbaImage,
        codecs::gif::{GifEncoder, Repeat},
    };
    use std::cell::RefCell;

    fn gif(frames: &[[u8; 4]], repeat: Repeat) -> Vec<u8> {
        gif_with(frames, repeat, 50)
    }

    fn gif_with(frames: &[[u8; 4]], repeat: Repeat, delay_ms: u32) -> Vec<u8> {
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
                        Delay::from_numer_denom_ms(delay_ms, 1),
                    ))
                    .unwrap();
            }
        }
        bytes
    }

    /// Run the real player over a looping red-then-blue GIF whose frames
    /// last `delay_ms`, with `motion` and the commands the viewer `sent`
    /// before it started. After each shown picture `react` may act as the
    /// viewer; when it returns true the viewer lets go (also after 20
    /// pictures). Returns each shown picture's blue byte and state.
    fn watch(
        delay_ms: u32,
        motion: &Motion,
        sent: &[Control],
        max_play: Duration,
        react: &dyn Fn(u8, Playback, &Remote) -> bool,
    ) -> Vec<(u8, Playback)> {
        let bytes = gif_with(
            &[[255, 0, 0, 255], [0, 0, 255, 255]],
            Repeat::Infinite,
            delay_ms,
        );
        let prepared = prepare(Format::Gif, &bytes).unwrap();
        let mut pass = animated(&prepared);
        let first = pass.next_frame().unwrap().unwrap();
        let second = pass.next_frame().unwrap().unwrap();
        let (remote, commands) = commands();
        for control in sent {
            remote.send(*control);
        }
        let remote = RefCell::new(Some(remote));
        let shown = RefCell::new(Vec::new());
        let show = |picture: &Picture, state: Playback| {
            shown.borrow_mut().push((picture.rgba[2], state));
            let mut remote = remote.borrow_mut();
            let done = shown.borrow().len() >= 20
                || remote
                    .as_ref()
                    .is_some_and(|remote| react(picture.rgba[2], state, remote));
            if done {
                remote.take();
            }
        };
        run(
            &prepared,
            pass,
            first,
            second,
            &commands,
            motion,
            &|| true,
            &show,
            max_play,
        );
        shown.into_inner()
    }

    fn animated(prepared: &Prepared) -> Pass<'_> {
        match Pass::open(Format::Gif, prepared).unwrap() {
            Opened::Animated(pass) => pass,
            Opened::Still(_) => panic!("expected an animation"),
        }
    }

    #[test]
    fn gif_frames_decode_in_order_with_their_delays() {
        let bytes = gif(&[[255, 0, 0, 255], [0, 0, 255, 255]], Repeat::Infinite);
        assert_eq!(Format::sniff(&bytes), Some(Format::Gif));
        let prepared = prepare(Format::Gif, &bytes).unwrap();
        let mut pass = animated(&prepared);
        assert_eq!(pass.still_reason(), None);
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
    fn a_gif_without_a_loop_setting_plays_once_and_a_count_plays_count_plus_one() {
        let once = gif(&[[1, 2, 3, 255], [4, 5, 6, 255]], Repeat::Finite(0));
        let prepared = prepare(Format::Gif, &once).unwrap();
        let pass = animated(&prepared);
        assert!(pass.repeats_after(0) && !pass.repeats_after(1));
        let twice = gif(&[[1, 2, 3, 255], [4, 5, 6, 255]], Repeat::Finite(1));
        let prepared = prepare(Format::Gif, &twice).unwrap();
        let pass = animated(&prepared);
        assert!(pass.repeats_after(1) && !pass.repeats_after(2));
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
            prepare(Format::Gif, &bytes),
            Err(PreviewError::TooLarge)
        ));
        assert_eq!(Format::sniff(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(Format::sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::WebP));
        assert!(prepare(Format::WebP, b"RIFF\0\0\0\0WEBPVP8 ").is_err());
    }

    #[test]
    fn large_or_long_animations_show_their_first_frame_still() {
        // Decided from the canvas and frame count alone: no frame is decoded.
        let pass = |width: u32, height: u32, limit: usize| Pass {
            format: Format::WebP,
            frames: Frames::new(Box::new(std::iter::empty())),
            loops: LoopCount::Infinite,
            width,
            height,
            decoded: 0,
            limit,
        };
        // Exactly 2 megapixels and 256 frames: MAX_PASS_PIXELS, still played.
        assert_eq!(pass(2048, 1024, 256).still_reason(), None);
        assert_eq!(pass(724, 724, MAX_FRAMES).still_reason(), None);
        assert_eq!(
            pass(2048, 1024, 257).still_reason(),
            Some("Shown still: this animation has too many frames at its size to play.")
        );
        assert_eq!(
            pass(2049, 1024, 2).still_reason(),
            Some("Shown still: animations over 2 megapixels don't play.")
        );
    }

    #[test]
    fn a_decoder_past_the_frame_limit_stops_with_an_error_not_an_end() {
        // A decoder that keeps yielding frames: the pass refuses the one past
        // the limit instead of reporting the end of the animation.
        let endless = std::iter::repeat_with(|| Ok(Frame::new(RgbaImage::new(1, 1))));
        let mut pass = Pass {
            format: Format::Gif,
            frames: Frames::new(Box::new(endless)),
            loops: LoopCount::Infinite,
            width: 1,
            height: 1,
            decoded: 0,
            limit: MAX_FRAMES,
        };
        for _ in 0..MAX_FRAMES {
            assert!(pass.next_frame().unwrap().is_some());
        }
        assert!(matches!(pass.next_frame(), Err(PreviewError::TooLarge)));
        // A decoder finding more frames than framing counted is refused too.
        let mut counted = Pass {
            format: Format::Gif,
            frames: Frames::new(Box::new(std::iter::repeat_with(|| {
                Ok(Frame::new(RgbaImage::new(1, 1)))
            }))),
            loops: LoopCount::Infinite,
            width: 1,
            height: 1,
            decoded: 0,
            limit: 2,
        };
        assert!(counted.next_frame().unwrap().is_some());
        assert!(counted.next_frame().unwrap().is_some());
        assert!(matches!(counted.next_frame(), Err(PreviewError::Invalid)));
        // And one that ends before every counted frame is not an end either.
        let mut early = Pass {
            format: Format::Gif,
            frames: Frames::new(Box::new(std::iter::once(Ok(Frame::new(RgbaImage::new(
                1, 1,
            )))))),
            loops: LoopCount::Infinite,
            width: 1,
            height: 1,
            decoded: 0,
            limit: 2,
        };
        assert!(early.next_frame().unwrap().is_some());
        assert!(matches!(early.next_frame(), Err(PreviewError::Invalid)));
        assert_eq!(
            failure_note(PreviewError::TooLarge),
            "Playback stopped: this animation exceeds the preview's frame or memory limit."
        );
    }

    #[test]
    fn an_animated_webp_plays_through_its_checked_frames() {
        // Two 2 × 1 lossless frames side by side on a 4 × 1 canvas.
        let mut still = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut still)
            .write_image(
                &[255, 0, 0, 255, 0, 0, 255, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let chunk = |bytes: &mut Vec<u8>, kind: &[u8], payload: &[u8]| {
            bytes.extend_from_slice(kind);
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                bytes.push(0);
            }
        };
        let mut chunks = Vec::new();
        chunk(&mut chunks, b"VP8X", &[0x12, 0, 0, 0, 3, 0, 0, 0, 0, 0]);
        chunk(&mut chunks, b"ANIM", &[0; 6]);
        for left in [0u32, 1] {
            let mut frame = Vec::new();
            for value in [left, 0, 1, 0, 100] {
                frame.extend_from_slice(&value.to_le_bytes()[..3]);
            }
            frame.push(0);
            frame.extend_from_slice(&still[12..]);
            chunk(&mut chunks, b"ANMF", &frame);
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(chunks.len() as u32 + 4).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(&chunks);
        assert_eq!(Format::sniff(&bytes), Some(Format::WebP));
        let prepared = prepare(Format::WebP, &bytes).unwrap();
        let Opened::Animated(mut pass) = Pass::open(Format::WebP, &prepared).unwrap() else {
            panic!("expected an animation");
        };
        for _ in 0..2 {
            let picture = pass.next_frame().unwrap().unwrap();
            assert_eq!((picture.width, picture.height), (4, 1));
            assert_eq!(picture.delay, Duration::from_millis(100));
        }
        assert!(pass.next_frame().unwrap().is_none());
    }

    #[test]
    fn paused_playback_waits_and_a_dropped_viewer_stops_the_worker() {
        let bytes = gif(&[[255, 0, 0, 255], [0, 0, 255, 255]], Repeat::Infinite);
        let prepared = prepare(Format::Gif, &bytes).unwrap();
        let mut pass = animated(&prepared);
        let first = pass.next_frame().unwrap().unwrap();
        let second = pass.next_frame().unwrap().unwrap();
        let (remote, commands) = commands();
        let shown = std::cell::RefCell::new(Vec::new());
        remote.send(Control::Play);
        remote.send(Control::Pause);
        drop(remote);
        play(
            &prepared,
            pass,
            first,
            second,
            &commands,
            &Motion::default(),
            &|| true,
            &|picture, state| shown.borrow_mut().push((picture.rgba[2], state.playing)),
        );
        let shown = shown.into_inner();
        // Paused on the first frame (autoplay is off), then Play and Pause,
        // then the viewer let go: the worker returned without decoding on
        // its own, even with a command still waiting.
        assert_eq!(shown.first(), Some(&(0, false)));
        assert_eq!(shown.last().map(|(_, playing)| *playing), Some(false));
        assert!(shown.len() <= 4);
    }

    #[test]
    fn autoplay_is_decided_when_playback_starts_and_explicit_play_still_works() {
        let motion = Motion::default();
        // Autoplay was on when the image was opened, and reduced motion
        // turned on while it loaded: the player, starting later, holds.
        assert!(!motion.set(true, false));
        assert!(motion.set(true, true));
        assert!(!motion.set(true, true), "only the turn to reduced pauses");
        let held = watch(50, &motion, &[], MAX_PLAY, &|_, state, _| !state.playing);
        assert!(held.iter().all(|(_, state)| !state.playing), "{held:?}");
        // The user's own Play still plays under reduced motion.
        let played = watch(
            50,
            &motion,
            &[Control::Play],
            MAX_PLAY,
            &|blue, state, _| state.playing && blue == 255,
        );
        assert!(!played[0].1.playing);
        assert!(
            played
                .iter()
                .any(|(blue, state)| *blue == 255 && state.playing)
        );
        // Autoplay with motion allowed starts by itself.
        assert!(!motion.set(true, false));
        let started = watch(50, &motion, &[], MAX_PLAY, &|_, state, _| state.playing);
        assert!(started[0].1.playing);
    }

    #[test]
    fn a_forced_pause_is_never_lost_behind_other_commands_or_a_playing_frame() {
        let motion = Motion::default();
        motion.set(true, false);
        // More commands than the old four-slot queue held, then the pause the
        // controller sends when reduced motion turns on: the newest wins.
        let mut burst = vec![Control::Replay; 16];
        burst.push(Control::Pause);
        let paused = watch(50, &motion, &burst, MAX_PLAY, &|_, state, _| !state.playing);
        assert_eq!(paused.len(), 2, "{paused:?}");
        assert!(paused[0].1.playing && !paused[1].1.playing);
        // Reduced motion turning on while frames play: the pause sent then is
        // taken before any later frame.
        let reduced = watch(50, &motion, &[], MAX_PLAY, &|_, state, remote| {
            if state.playing && motion.set(true, true) {
                remote.send(Control::Pause);
            }
            !state.playing
        });
        assert!(reduced[0].1.playing, "{reduced:?}");
        assert!(!reduced.last().unwrap().1.playing);
        assert!(reduced.iter().all(|(blue, _)| *blue == 0));
    }

    #[test]
    fn long_frame_delays_still_pause_when_the_play_budget_runs_out() {
        let motion = Motion::default();
        motion.set(true, false);
        let budget = Duration::from_millis(200);
        // Frames of 30 s each, far longer than the whole budget.
        let started = Instant::now();
        let shown = watch(30_000, &motion, &[], budget, &|_, state, _| !state.playing);
        let waited = started.elapsed();
        assert!(
            waited >= budget && waited < Duration::from_secs(10),
            "{waited:?}"
        );
        // Paused on the first frame: the predecoded second picture was not shown.
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(shown.iter().all(|(blue, _)| *blue == 0));
        assert_eq!(
            shown[1].1.note,
            Some("Paused after ten minutes of playback.")
        );
        // An explicit Play afterwards starts a fresh budget and plays on.
        let resumed = watch(30_000, &motion, &[], budget, &|blue, state, remote| {
            if state.note.is_some() {
                remote.send(Control::Play);
            }
            blue == 255
        });
        assert!(
            resumed
                .iter()
                .any(|(blue, state)| *blue == 255 && state.playing)
        );
    }

    #[test]
    fn closing_during_a_long_frame_wakes_the_player_at_once() {
        let bytes = gif_with(
            &[[255, 0, 0, 255], [0, 0, 255, 255]],
            Repeat::Infinite,
            30_000,
        );
        let prepared = prepare(Format::Gif, &bytes).unwrap();
        let mut pass = animated(&prepared);
        let first = pass.next_frame().unwrap().unwrap();
        let second = pass.next_frame().unwrap().unwrap();
        let (remote, commands) = commands();
        let motion = Motion::default();
        motion.set(true, false);
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(remote);
        });
        let started = Instant::now();
        play(
            &prepared,
            pass,
            first,
            second,
            &commands,
            &motion,
            &|| true,
            &|_, _| {},
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        closer.join().unwrap();
    }
}
