// Copyright 2025 LiveKit, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Modified by Morpheus: expose explicit capture-status observations.
// See MORPHEUS_PATCH.md in the crate root.

use crate::{enum_dispatch, imp::video_source as vs_imp};

#[derive(Debug, Clone)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
}

impl Default for VideoResolution {
    // Default to 720p
    fn default() -> Self {
        VideoResolution { width: 1280, height: 720 }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum RtcVideoSource {
    // TODO(theomonnom): Web video sources (eq. to tracks on browsers?)
    #[cfg(not(target_arch = "wasm32"))]
    Native(native::NativeVideoSource),
}

// TODO(theomonnom): Support enum dispatch with conditional compilation?
impl RtcVideoSource {
    enum_dispatch!(
        [Native];
        pub fn video_resolution(self: &Self) -> VideoResolution;
    );
}

#[cfg(not(target_arch = "wasm32"))]
pub mod native {
    use std::fmt::{Debug, Formatter};

    use super::*;
    use crate::native::packet_trailer::PacketTrailerHandler;
    #[cfg(target_os = "linux")]
    use crate::video_frame::FrameMetadata;
    use crate::video_frame::{VideoBuffer, VideoFrame};

    /// Positive high-water observations for explicit frames submitted to one source.
    ///
    /// Warm-up frames are excluded. Native admission is not encoding, encryption,
    /// RTP transmission, remote reception, or successful decryption.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VideoCaptureStatus {
        /// An explicit capture reached the native submission boundary.
        pub submitted: bool,
        /// The native source returned true after adaptation and its OnFrame call.
        pub admitted: bool,
    }

    #[derive(Clone)]
    pub struct NativeVideoSource {
        pub(crate) handle: vs_imp::NativeVideoSource,
    }

    impl Debug for NativeVideoSource {
        fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
            f.debug_struct("NativeVideoSource").finish()
        }
    }

    impl Default for NativeVideoSource {
        fn default() -> Self {
            Self::new(VideoResolution::default(), false)
        }
    }

    impl NativeVideoSource {
        pub fn new(resolution: VideoResolution, is_screencast: bool) -> Self {
            Self { handle: vs_imp::NativeVideoSource::new(resolution, is_screencast) }
        }

        pub fn capture_frame<T: AsRef<dyn VideoBuffer>>(&self, frame: &VideoFrame<T>) {
            self.handle.capture_frame(frame)
        }

        /// Observe explicit capture progress on this same source without taking a
        /// native lock. False is not a diagnosis: a concurrent call can still be
        /// entering or executing. These sticky flags reset only with a new source.
        pub fn capture_status(&self) -> VideoCaptureStatus {
            let (submitted, admitted) = self.handle.capture_status();
            VideoCaptureStatus { submitted, admitted }
        }

        /// Captures a Jetson DMA-buffer backed video frame.
        ///
        /// `pixel_format` is `0` for NV12 and `1` for YUV420M.
        #[cfg(target_os = "linux")]
        pub fn capture_dmabuf_frame(
            &self,
            dmabuf_fd: i32,
            width: u32,
            height: u32,
            pixel_format: i32,
            timestamp_us: i64,
        ) -> bool {
            self.handle.capture_dmabuf_frame(dmabuf_fd, width, height, pixel_format, timestamp_us)
        }

        /// Captures a Jetson DMA-buffer backed video frame with packet trailer metadata.
        ///
        /// `pixel_format` is `0` for NV12 and `1` for YUV420M.
        #[cfg(target_os = "linux")]
        pub fn capture_dmabuf_frame_with_metadata(
            &self,
            dmabuf_fd: i32,
            width: u32,
            height: u32,
            pixel_format: i32,
            timestamp_us: i64,
            frame_metadata: Option<FrameMetadata>,
        ) -> bool {
            self.handle.capture_dmabuf_frame_with_metadata(
                dmabuf_fd,
                width,
                height,
                pixel_format,
                timestamp_us,
                frame_metadata,
            )
        }

        /// Set the packet trailer handler used by this source.
        ///
        /// When set, any frame captured with a `user_timestamp` value will
        /// automatically have its timestamp stored in the handler (keyed by
        /// the TimestampAligner-adjusted capture timestamp) so the
        /// `PacketTrailerTransformer` can embed it into the encoded frame.
        pub fn set_packet_trailer_handler(&self, handler: PacketTrailerHandler) {
            self.handle.set_packet_trailer_handler(handler)
        }

        pub fn video_resolution(&self) -> VideoResolution {
            self.handle.video_resolution()
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub mod web {}
