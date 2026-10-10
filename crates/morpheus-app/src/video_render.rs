//! One native remote video tile for the active MatrixRTC call.
//!
//! Slint only receives owned RGB buffers. The RTC task owns the decoded I420
//! stream and samples its newest frame at most fifteen times per second. A
//! screen share takes precedence over a camera when both are available.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use futures_util::{StreamExt, stream::BoxStream};
use morpheus_rtc::{
    MediaStreamKind, ParticipantInfo, RtcService, RtcStreamKind, VideoFrame, VideoRotation,
};
use slint::{Rgb8Pixel, SharedPixelBuffer};
use tokio::{sync::watch, time::MissedTickBehavior};

const MAX_EDGE: u32 = 1920;
const MAX_PIXELS: usize = 1920 * 1080;
const FRAME_INTERVAL: Duration = Duration::from_millis(67);

#[derive(Clone)]
pub enum VideoUpdate {
    Clear {
        token: u64,
    },
    Frame {
        token: u64,
        source: String,
        pixels: SharedPixelBuffer<Rgb8Pixel>,
    },
}

impl VideoUpdate {
    pub fn token(&self) -> u64 {
        match self {
            Self::Clear { token } | Self::Frame { token, .. } => *token,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RosterUpdate {
    pub token: u64,
    pub members: Vec<ParticipantInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Source {
    member_id: String,
    user_id: String,
    kind: MediaStreamKind,
}

impl Source {
    fn label(&self) -> String {
        let medium = match self.kind {
            MediaStreamKind::ScreenShare => "Screen",
            MediaStreamKind::Camera => "Camera",
            _ => unreachable!("only video sources are selected"),
        };
        format!("{medium} · {}", self.user_id)
    }
}

/// Run beside the audio future. Dropping this future drops its stream, pending
/// frame, and converter result. The controller checks the call token again
/// before any queued Slint update is applied.
pub async fn run_remote_video(
    rtc: &RtcService,
    room_id: &str,
    token: u64,
    updates: watch::Sender<VideoUpdate>,
    roster_updates: watch::Sender<RosterUpdate>,
    active_token: &AtomicU64,
) {
    let Some(call) = rtc.call(room_id) else {
        return;
    };
    let mut roster = call.subscribe_participants();
    let mut refresh = tokio::time::interval(Duration::from_secs(2));
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut presentation = tokio::time::interval(FRAME_INTERVAL);
    presentation.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut selected: Option<Source> = None;
    let mut frames: Option<BoxStream<'static, VideoFrame>> = None;
    let mut newest: Option<VideoFrame> = None;
    let select_track = |participants: &[ParticipantInfo], current: &Option<Source>| {
        let mut candidates = Vec::new();
        for participant in participants.iter().filter(|member| !member.is_local) {
            for (stream_kind, kind) in [
                (RtcStreamKind::ScreenShare, MediaStreamKind::ScreenShare),
                (RtcStreamKind::Camera, MediaStreamKind::Camera),
            ] {
                if participant
                    .streams
                    .iter()
                    .any(|stream| stream.kind == stream_kind && !stream.muted)
                {
                    candidates.push(Source {
                        member_id: participant.member_id.clone(),
                        user_id: participant.user_id.clone(),
                        kind,
                    });
                }
            }
        }
        candidates.sort_by(|a, b| {
            let priority = |kind| {
                if kind == MediaStreamKind::ScreenShare {
                    0
                } else {
                    1
                }
            };
            priority(a.kind)
                .cmp(&priority(b.kind))
                .then_with(|| a.member_id.cmp(&b.member_id))
        });
        candidates.into_iter().find_map(|source| {
            if current.as_ref() == Some(&source) {
                return Some((source, None));
            }
            call.remote_track(&source.member_id, source.kind)
                .and_then(|track| track.video_frames())
                .map(|frames| (source, Some(frames)))
        })
    };

    loop {
        tokio::select! {
            _ = refresh.tick() => {
                let participants = rtc.participants(room_id).unwrap_or_default();
                publish_roster(&roster_updates, token, participants.clone(), active_token);
                let next = select_track(&participants, &selected);
                change_source(next, &mut selected, &mut frames, &mut newest, token, &updates, active_token);
            }
            changed = roster.changed() => {
                if changed.is_err() { break; }
                let participants = rtc.participants(room_id).unwrap_or_default();
                publish_roster(&roster_updates, token, participants.clone(), active_token);
                let next = select_track(&participants, &selected);
                change_source(next, &mut selected, &mut frames, &mut newest, token, &updates, active_token);
            }
            frame = async { frames.as_mut().expect("guarded stream").next().await }, if frames.is_some() => {
                match frame {
                    Some(frame) => newest = Some(frame), // overwrite, never queue decoded frames
                    None => {
                        selected = None;
                        frames = None;
                        newest = None;
                        if active_token.load(Ordering::SeqCst) == token {
                            updates.send_replace(VideoUpdate::Clear { token });
                        }
                    }
                }
            }
            _ = presentation.tick() => {
                if let (Some(source), Some(frame)) = (selected.as_ref(), newest.take()) {
                    let source = source.label();
                    // RGB conversion can be expensive at 1080p. Never block
                    // the Tokio worker that receives MatrixRTC media.
                    if let Ok(Ok(pixels)) = tokio::task::spawn_blocking(move || rgb_buffer(&frame)).await {
                        if active_token.load(Ordering::SeqCst) == token {
                            updates.send_replace(VideoUpdate::Frame { token, source, pixels });
                        }
                    }
                }
            }
        }
    }
    if active_token.load(Ordering::SeqCst) == token {
        updates.send_replace(VideoUpdate::Clear { token });
    }
}

fn publish_roster(
    updates: &watch::Sender<RosterUpdate>,
    token: u64,
    members: Vec<ParticipantInfo>,
    active_token: &AtomicU64,
) {
    if active_token.load(Ordering::SeqCst) != token {
        return;
    }
    updates.send_if_modified(|current| {
        if current.token == token && current.members == members {
            false
        } else {
            *current = RosterUpdate { token, members };
            true
        }
    });
}

fn change_source(
    next: Option<(Source, Option<BoxStream<'static, VideoFrame>>)>,
    selected: &mut Option<Source>,
    frames: &mut Option<BoxStream<'static, VideoFrame>>,
    newest: &mut Option<VideoFrame>,
    token: u64,
    updates: &watch::Sender<VideoUpdate>,
    active_token: &AtomicU64,
) {
    let new_source = next.as_ref().map(|(source, _)| source);
    if selected.as_ref() == new_source {
        return;
    }
    *selected = next.as_ref().map(|(source, _)| source.clone());
    *frames = next.and_then(|(_, frames)| frames);
    *newest = None;
    if active_token.load(Ordering::SeqCst) == token {
        updates.send_replace(VideoUpdate::Clear { token });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameError {
    InvalidDimensions,
    InvalidPlane,
}

/// Convert BT.601 limited-range I420 to RGB, applying clockwise rotation.
fn rgb_buffer(frame: &VideoFrame) -> Result<SharedPixelBuffer<Rgb8Pixel>, FrameError> {
    let source = &frame.buffer;
    let width = usize::try_from(source.width).map_err(|_| FrameError::InvalidDimensions)?;
    let height = usize::try_from(source.height).map_err(|_| FrameError::InvalidDimensions)?;
    let pixels = width
        .checked_mul(height)
        .ok_or(FrameError::InvalidDimensions)?;
    if width == 0
        || height == 0
        || source.width > MAX_EDGE
        || source.height > MAX_EDGE
        || pixels > MAX_PIXELS
    {
        return Err(FrameError::InvalidDimensions);
    }
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let stride_y = checked_plane(&source.data_y, source.stride_y, width, height)?;
    let stride_u = checked_plane(&source.data_u, source.stride_u, chroma_width, chroma_height)?;
    let stride_v = checked_plane(&source.data_v, source.stride_v, chroma_width, chroma_height)?;
    let (output_width, output_height) = match frame.rotation {
        VideoRotation::Deg0 | VideoRotation::Deg180 => (source.width, source.height),
        VideoRotation::Deg90 | VideoRotation::Deg270 => (source.height, source.width),
    };
    let mut output = SharedPixelBuffer::<Rgb8Pixel>::new(output_width, output_height);
    let output_bytes = output.make_mut_bytes();
    let output_width = output_width as usize;
    for y in 0..height {
        for x in 0..width {
            let luma = source.data_y[y * stride_y + x];
            let chroma_u = source.data_u[(y / 2) * stride_u + x / 2];
            let chroma_v = source.data_v[(y / 2) * stride_v + x / 2];
            let rgb = yuv_to_rgb(luma, chroma_u, chroma_v);
            let (display_x, display_y) = match frame.rotation {
                VideoRotation::Deg0 => (x, y),
                VideoRotation::Deg90 => (height - 1 - y, x),
                VideoRotation::Deg180 => (width - 1 - x, height - 1 - y),
                VideoRotation::Deg270 => (y, width - 1 - x),
            };
            let offset = (display_y * output_width + display_x) * 3;
            output_bytes[offset..offset + 3].copy_from_slice(&rgb);
        }
    }
    Ok(output)
}

fn checked_plane(
    data: &[u8],
    stride: u32,
    visible_width: usize,
    rows: usize,
) -> Result<usize, FrameError> {
    let stride = usize::try_from(stride).map_err(|_| FrameError::InvalidPlane)?;
    let length = stride.checked_mul(rows).ok_or(FrameError::InvalidPlane)?;
    if stride < visible_width || data.len() < length {
        return Err(FrameError::InvalidPlane);
    }
    Ok(stride)
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = i32::from(y) - 16;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let channel = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
    [
        channel(298 * c + 409 * e),
        channel(298 * c - 100 * d - 208 * e),
        channel(298 * c + 516 * d),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_rtc::I420Buffer;

    fn padded_frame(rotation: VideoRotation) -> VideoFrame {
        VideoFrame {
            buffer: I420Buffer {
                width: 2,
                height: 3,
                data_y: vec![16, 235, 0, 81, 145, 0, 41, 106, 0],
                stride_y: 3,
                data_u: vec![128, 0, 128, 0],
                stride_u: 2,
                data_v: vec![128, 0, 128, 0],
                stride_v: 2,
            },
            rotation,
            timestamp_us: 0,
        }
    }

    #[test]
    fn respects_padding_and_limited_range() {
        let image = rgb_buffer(&padded_frame(VideoRotation::Deg0)).unwrap();
        assert_eq!((image.width(), image.height()), (2, 3));
        assert_eq!(
            &image.as_bytes()[..9],
            &[0, 0, 0, 255, 255, 255, 76, 76, 76]
        );
    }

    #[test]
    fn applies_clockwise_quarter_turn() {
        let image = rgb_buffer(&padded_frame(VideoRotation::Deg90)).unwrap();
        assert_eq!((image.width(), image.height()), (3, 2));
        assert_eq!(&image.as_bytes()[..3], &[29, 29, 29]);
    }

    #[test]
    fn rejects_short_planes_and_excessive_dimensions() {
        let mut frame = padded_frame(VideoRotation::Deg0);
        frame.buffer.data_u.pop();
        assert_eq!(rgb_buffer(&frame).unwrap_err(), FrameError::InvalidPlane);
        frame.buffer.width = MAX_EDGE + 1;
        assert_eq!(
            rgb_buffer(&frame).unwrap_err(),
            FrameError::InvalidDimensions
        );
    }
}
