use std::sync::Arc;

use matrix_rtc_transport::{
    AudioFrame, AudioSourceConfig, I420Buffer, LocalTrackHandle, MediaStreamKind, PublishOptions,
    VideoFrame, VideoSourceConfig,
};

use crate::{CallInfo, RtcService, native::CallLease};

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("there is no active call in this room")]
    NotJoined,
    #[error("the requested media dimensions or audio format are invalid")]
    InvalidFormat,
    #[error("the frame does not match the publication format")]
    FrameFormatMismatch,
    #[error("the I420 video frame has invalid plane strides or lengths")]
    InvalidVideoPlanes,
    #[error("the publication does not accept this media kind")]
    WrongMediaKind,
    #[error("the call ended")]
    CallEnded,
    #[error("the control belongs to a different account or call join")]
    WrongCall,
    #[error("the call audio loop is not running")]
    ControlNotRunning,
    #[error("the call audio loop is already running")]
    ControlAlreadyRunning,
    #[error(
        "the selected microphone is unavailable or its device ID is ambiguous; choose another microphone"
    )]
    MicrophoneUnavailable,
    #[error(
        "the selected speaker is unavailable or its device ID is ambiguous; choose another speaker"
    )]
    SpeakerUnavailable,
    #[error("native device error: {0}")]
    Device(String),
    #[error(transparent)]
    Call(#[from] matrix_rtc_call_sdk::LiveKitCallError),
    #[error(transparent)]
    Transport(#[from] matrix_rtc_transport::TransportError),
}

/// A local media track whose format is checked before entering libwebrtc.
pub struct Publication {
    track: Arc<dyn LocalTrackHandle>,
    audio: Option<AudioSourceConfig>,
    video: Option<VideoSourceConfig>,
    lease: CallLease,
}

impl Publication {
    pub fn call_info(&self) -> &CallInfo {
        &self.lease.info
    }

    pub fn account_user_id(&self) -> &str {
        &self.lease.account_user_id
    }

    pub fn account_device_id(&self) -> &str {
        &self.lease.account_device_id
    }

    pub fn is_active(&self) -> bool {
        self.lease.is_active()
    }

    pub fn kind(&self) -> MediaStreamKind {
        self.track.kind()
    }

    pub fn set_muted(&self, muted: bool) -> Result<(), MediaError> {
        if !self.is_active() {
            return Err(MediaError::CallEnded);
        }
        self.track.set_muted(muted)?;
        Ok(())
    }

    pub async fn unpublish(&self) -> Result<(), MediaError> {
        if !self.is_active() {
            return Err(MediaError::CallEnded);
        }
        self.track.unpublish().await?;
        Ok(())
    }

    /// Push one captured PCM chunk. The upstream transport applies backpressure.
    pub async fn push_audio(&self, frame: AudioFrame) -> Result<(), MediaError> {
        if !self.is_active() {
            return Err(MediaError::CallEnded);
        }
        let format = self.audio.ok_or(MediaError::WrongMediaKind)?;
        let expected = frame
            .samples_per_channel
            .checked_mul(frame.num_channels)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(MediaError::FrameFormatMismatch)?;
        if frame.sample_rate != format.sample_rate
            || frame.num_channels != format.num_channels
            || frame.samples_per_channel == 0
            || frame.data.len() != expected
        {
            return Err(MediaError::FrameFormatMismatch);
        }
        self.track.capture_audio(frame).await?;
        Ok(())
    }

    /// Push one captured I420 frame. The upstream video path keeps the latest
    /// frame, so callers should pace camera/screen capture themselves.
    pub fn push_video(&self, frame: VideoFrame) -> Result<(), MediaError> {
        if !self.is_active() {
            return Err(MediaError::CallEnded);
        }
        let format = self.video.ok_or(MediaError::WrongMediaKind)?;
        if frame.buffer.width != format.width || frame.buffer.height != format.height {
            return Err(MediaError::FrameFormatMismatch);
        }
        validate_i420(&frame.buffer)?;
        self.track.capture_video(frame)?;
        Ok(())
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        // A cancelled capture task can drop its publication without awaiting
        // unpublish. Silence it immediately; callers should still unpublish
        // explicitly to remove the stream from peers' rosters.
        if self.is_active() {
            let _ = self.track.set_muted(true);
        }
    }
}

impl RtcService {
    /// Publish a microphone as muted initially unless the caller explicitly
    /// opts into an open microphone. The format must match the captured frames.
    pub async fn publish_microphone(
        &self,
        room_id: &str,
        sample_rate: u32,
        channels: u32,
        muted: bool,
    ) -> Result<Publication, MediaError> {
        if sample_rate == 0 || !(1..=2).contains(&channels) {
            return Err(MediaError::InvalidFormat);
        }
        let mut options = PublishOptions::microphone();
        options.audio = Some(AudioSourceConfig {
            sample_rate,
            num_channels: channels,
        });
        options.muted = muted;
        self.publish(room_id, options).await
    }

    pub async fn publish_camera(
        &self,
        room_id: &str,
        width: u32,
        height: u32,
        muted: bool,
    ) -> Result<Publication, MediaError> {
        let format = video_format(width, height)?;
        let mut options = PublishOptions::camera(format);
        options.muted = muted;
        self.publish(room_id, options).await
    }

    pub async fn publish_screen(
        &self,
        room_id: &str,
        width: u32,
        height: u32,
        muted: bool,
    ) -> Result<Publication, MediaError> {
        let format = video_format(width, height)?;
        let mut options = PublishOptions::screen_share(format);
        options.muted = muted;
        self.publish(room_id, options).await
    }

    async fn publish(
        &self,
        room_id: &str,
        options: PublishOptions,
    ) -> Result<Publication, MediaError> {
        let call = self.call(room_id).ok_or(MediaError::NotJoined)?;
        let lease = self
            .call_lease(room_id)
            .ok_or(MediaError::NotJoined)?
            .clone();
        let audio = options.audio;
        let video = options.video;
        let track = call.publish(options).await?;
        Ok(Publication {
            track,
            audio,
            video,
            lease,
        })
    }
}

fn video_format(width: u32, height: u32) -> Result<VideoSourceConfig, MediaError> {
    if width == 0 || height == 0 || width > 7680 || height > 4320 {
        return Err(MediaError::InvalidFormat);
    }
    Ok(VideoSourceConfig { width, height })
}

fn validate_i420(buffer: &I420Buffer) -> Result<(), MediaError> {
    let width = usize::try_from(buffer.width).map_err(|_| MediaError::InvalidVideoPlanes)?;
    let height = usize::try_from(buffer.height).map_err(|_| MediaError::InvalidVideoPlanes)?;
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    for (stride, rows, columns, data) in [
        (buffer.stride_y, height, width, &buffer.data_y),
        (buffer.stride_u, chroma_height, chroma_width, &buffer.data_u),
        (buffer.stride_v, chroma_height, chroma_width, &buffer.data_v),
    ] {
        let stride = usize::try_from(stride).map_err(|_| MediaError::InvalidVideoPlanes)?;
        let length = stride
            .checked_mul(rows)
            .ok_or(MediaError::InvalidVideoPlanes)?;
        if stride < columns || data.len() < length {
            return Err(MediaError::InvalidVideoPlanes);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FakeVideoTrack(AtomicUsize);

    impl LocalTrackHandle for FakeVideoTrack {
        fn kind(&self) -> MediaStreamKind {
            MediaStreamKind::Camera
        }

        fn set_muted(&self, _: bool) -> Result<(), matrix_rtc_transport::TransportError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn capture_video(&self, _: VideoFrame) -> Result<(), matrix_rtc_transport::TransportError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn tiny_frame() -> VideoFrame {
        VideoFrame {
            buffer: I420Buffer {
                width: 2,
                height: 2,
                data_y: vec![16; 4],
                stride_y: 2,
                data_u: vec![128],
                stride_u: 1,
                data_v: vec![128],
                stride_v: 1,
            },
            rotation: matrix_rtc_transport::VideoRotation::Deg0,
            timestamp_us: 0,
        }
    }

    #[test]
    fn rejects_short_chroma_plane_before_libwebrtc() {
        let buffer = I420Buffer {
            width: 3,
            height: 3,
            data_y: vec![0; 9],
            stride_y: 3,
            data_u: vec![0; 2],
            stride_u: 2,
            data_v: vec![0; 4],
            stride_v: 2,
        };
        assert!(matches!(
            validate_i420(&buffer),
            Err(MediaError::InvalidVideoPlanes)
        ));
    }

    #[test]
    fn old_publication_cannot_capture_or_unmute_after_leave_and_rejoin() {
        let info = CallInfo {
            room_id: "!room:example.test".into(),
            slot_id: "m.call#ROOM".into(),
            membership_id: "old-member".into(),
            local_identity: "old-focus-identity".into(),
        };
        let old_lease = CallLease::new(info.clone(), "@alice:example.test".into(), "DEVICE".into());
        let old_track = Arc::new(FakeVideoTrack(AtomicUsize::new(0)));
        let old = Publication {
            track: old_track.clone(),
            audio: None,
            video: Some(VideoSourceConfig {
                width: 2,
                height: 2,
            }),
            lease: old_lease.clone(),
        };
        old.push_video(tiny_frame()).unwrap();
        assert_eq!(old_track.0.load(Ordering::SeqCst), 1);

        old_lease.revoke();
        let new_lease = CallLease::new(
            CallInfo {
                membership_id: "new-member".into(),
                local_identity: "new-focus-identity".into(),
                ..info
            },
            "@alice:example.test".into(),
            "DEVICE".into(),
        );
        let new_track = Arc::new(FakeVideoTrack(AtomicUsize::new(0)));
        let current = Publication {
            track: new_track.clone(),
            audio: None,
            video: Some(VideoSourceConfig {
                width: 2,
                height: 2,
            }),
            lease: new_lease,
        };
        assert!(matches!(old.set_muted(false), Err(MediaError::CallEnded)));
        assert!(matches!(
            old.push_video(tiny_frame()),
            Err(MediaError::CallEnded)
        ));
        assert_eq!(old_track.0.load(Ordering::SeqCst), 1);
        current.push_video(tiny_frame()).unwrap();
        assert_eq!(new_track.0.load(Ordering::SeqCst), 1);
        drop(old);
        assert_eq!(old_track.0.load(Ordering::SeqCst), 1);
        drop(current);
        assert_eq!(new_track.0.load(Ordering::SeqCst), 2);
    }
}
