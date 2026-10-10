use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use cpal::{
    SampleFormat, Stream, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures_util::StreamExt;
use matrix_rtc_call_sdk::CallEvent;
use matrix_rtc_transport::AudioFrame;
use tokio::sync::mpsc;

use crate::{MediaError, Publication, RtcService};

const SAMPLE_RATE: u32 = 48_000;
const CHUNK_SAMPLES_PER_CHANNEL: usize = 480; // 10 ms
const MAX_OUTPUT_BUFFER_SAMPLES: usize = SAMPLE_RATE as usize; // 500 ms stereo

/// Native microphone PCM capture. Frames are 10 ms at 48 kHz, with one or two
/// channels matching the selected input device. Keep this value alive while
/// pumping `next_frame()` into a microphone `Publication`.
pub struct MicrophoneCapture {
    _stream: Stream,
    rx: mpsc::Receiver<AudioFrame>,
    channels: u32,
    stream_error: Arc<Mutex<Option<String>>>,
}

impl MicrophoneCapture {
    /// List input device names for a device picker. This proof currently opens
    /// the default device; names do not serve as stable device identifiers.
    pub fn devices() -> Result<Vec<String>, MediaError> {
        cpal::default_host()
            .input_devices()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .map(|device| {
                device
                    .description()
                    .map(|description| description.name().to_owned())
                    .map_err(|error| MediaError::Device(error.to_string()))
            })
            .collect()
    }

    pub fn open_default() -> Result<Self, MediaError> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| MediaError::Device("no default microphone".into()))?;
        let config = select_48k_config(&device, true)?;
        let channels = u32::from(config.channels());
        let sample_format = config.sample_format();
        let chunk_len = CHUNK_SAMPLES_PER_CHANNEL * channels as usize;
        let (tx, rx) = mpsc::channel(32);
        let stream_error = Arc::new(Mutex::new(None));
        let error_for_callback = Arc::clone(&stream_error);
        let mut pending = VecDeque::<i16>::with_capacity(chunk_len * 2);
        let stream = device
            .build_input_stream_raw(
                config.config(),
                sample_format,
                move |data, _| {
                    match sample_format {
                        SampleFormat::I16 => {
                            if let Some(samples) = data.as_slice::<i16>() {
                                pending.extend(samples.iter().copied());
                            }
                        }
                        SampleFormat::F32 => {
                            if let Some(samples) = data.as_slice::<f32>() {
                                pending.extend(samples.iter().map(|sample| {
                                    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
                                }));
                            }
                        }
                        SampleFormat::U16 => {
                            if let Some(samples) = data.as_slice::<u16>() {
                                pending.extend(
                                    samples
                                        .iter()
                                        .map(|sample| (i32::from(*sample) - 32768) as i16),
                                );
                            }
                        }
                        _ => return,
                    }
                    while pending.len() >= chunk_len {
                        let frame = AudioFrame {
                            data: pending.drain(..chunk_len).collect(),
                            sample_rate: SAMPLE_RATE,
                            num_channels: channels,
                            samples_per_channel: CHUNK_SAMPLES_PER_CHANNEL as u32,
                        };
                        // The device callback must never wait for the network.
                        let _ = tx.try_send(frame);
                    }
                },
                move |error| {
                    if let Ok(mut last) = error_for_callback.lock() {
                        *last = Some(error.to_string());
                    }
                },
                None,
            )
            .map_err(|error| MediaError::Device(error.to_string()))?;
        stream
            .play()
            .map_err(|error| MediaError::Device(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            rx,
            channels,
            stream_error,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    fn discard_buffered_frames(&mut self) {
        // Publishing can take longer than the capture queue. Never start a
        // call by transmitting audio recorded before the track was unmuted.
        while self.rx.try_recv().is_ok() {}
    }

    pub async fn next_frame(&mut self) -> Result<AudioFrame, MediaError> {
        if let Some(error) = self
            .stream_error
            .lock()
            .ok()
            .and_then(|mut last| last.take())
        {
            return Err(MediaError::Device(error));
        }
        self.rx
            .recv()
            .await
            .ok_or_else(|| MediaError::Device("microphone stream stopped".into()))
    }

    /// Forward microphone frames until the publication closes or this future
    /// is cancelled. Capture errors and transport errors remain distinct.
    pub async fn pump_into(&mut self, publication: &Publication) -> Result<(), MediaError> {
        loop {
            publication.push_audio(self.next_frame().await?).await?;
        }
    }
}

/// Native speaker playback for one remote PCM stream. The queue is bounded so
/// a slow output device does not accumulate seconds of call latency. Group
/// calls still need an audio mixer; this adapter is a two-person proof.
pub struct AudioOutput {
    _stream: Stream,
    samples: Arc<Mutex<VecDeque<i16>>>,
    channels: u32,
    stream_error: Arc<Mutex<Option<String>>>,
}

impl AudioOutput {
    /// List output device names for a device picker. This proof currently
    /// opens the default device.
    pub fn devices() -> Result<Vec<String>, MediaError> {
        cpal::default_host()
            .output_devices()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .map(|device| {
                device
                    .description()
                    .map(|description| description.name().to_owned())
                    .map_err(|error| MediaError::Device(error.to_string()))
            })
            .collect()
    }

    pub fn open_default() -> Result<Self, MediaError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| MediaError::Device("no default speaker".into()))?;
        let config = select_48k_config(&device, false)?;
        let channels = u32::from(config.channels());
        let sample_format = config.sample_format();
        let samples = Arc::new(Mutex::new(VecDeque::<i16>::new()));
        let samples_for_callback = Arc::clone(&samples);
        let stream_error = Arc::new(Mutex::new(None));
        let error_for_callback = Arc::clone(&stream_error);
        let stream = device
            .build_output_stream_raw(
                config.config(),
                sample_format,
                move |data, _| {
                    // A contended mutex must produce silence: output buffers
                    // are not guaranteed to be pre-filled by the backend.
                    let mut queue = samples_for_callback.try_lock().ok();
                    match sample_format {
                        SampleFormat::I16 => {
                            if let Some(output) = data.as_slice_mut::<i16>() {
                                for sample in output {
                                    *sample =
                                        queue.as_mut().and_then(|q| q.pop_front()).unwrap_or(0);
                                }
                            }
                        }
                        SampleFormat::F32 => {
                            if let Some(output) = data.as_slice_mut::<f32>() {
                                for sample in output {
                                    *sample = f32::from(
                                        queue.as_mut().and_then(|q| q.pop_front()).unwrap_or(0),
                                    ) / f32::from(i16::MAX);
                                }
                            }
                        }
                        SampleFormat::U16 => {
                            if let Some(output) = data.as_slice_mut::<u16>() {
                                for sample in output {
                                    *sample = (i32::from(
                                        queue.as_mut().and_then(|q| q.pop_front()).unwrap_or(0),
                                    ) + 32768) as u16;
                                }
                            }
                        }
                        _ => {}
                    }
                },
                move |error| {
                    if let Ok(mut last) = error_for_callback.lock() {
                        *last = Some(error.to_string());
                    }
                },
                None,
            )
            .map_err(|error| MediaError::Device(error.to_string()))?;
        stream
            .play()
            .map_err(|error| MediaError::Device(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            samples,
            channels,
            stream_error,
        })
    }

    pub fn push_frame(&self, frame: &AudioFrame) -> Result<(), MediaError> {
        if let Some(error) = self
            .stream_error
            .lock()
            .ok()
            .and_then(|mut last| last.take())
        {
            return Err(MediaError::Device(error));
        }
        if frame.sample_rate != SAMPLE_RATE
            || !(1..=2).contains(&frame.num_channels)
            || frame.data.len() != frame.samples_per_channel as usize * frame.num_channels as usize
        {
            return Err(MediaError::FrameFormatMismatch);
        }
        let mut queue = self
            .samples
            .lock()
            .map_err(|_| MediaError::Device("speaker queue poisoned".into()))?;
        if queue.len() > MAX_OUTPUT_BUFFER_SAMPLES {
            queue.clear(); // Drop stale audio rather than increasing call latency.
        }
        remix_channels(&frame.data, frame.num_channels, self.channels, &mut queue);
        Ok(())
    }
}

impl RtcService {
    /// Run the default microphone and speaker for a joined two-person call.
    /// Call this only after an explicit user action that authorizes microphone
    /// capture. It returns when the call ends or the media path fails; dropping
    /// the future mutes the publication before releasing the microphone.
    ///
    /// This is a native voice-call proof, with one remote audio stream. A
    /// production group call needs per-speaker mixing and device switching.
    pub async fn run_two_person_audio(&self, room_id: &str) -> Result<(), MediaError> {
        let call = self.call(room_id).ok_or(MediaError::NotJoined)?;
        let mut microphone = MicrophoneCapture::open_default()?;
        let speaker = AudioOutput::open_default()?;
        let publication = self
            .publish_microphone(
                room_id,
                microphone.sample_rate(),
                microphone.channels(),
                true,
            )
            .await?;
        let _mute_on_drop = MuteOnDrop(&publication);
        let mut events = call.subscribe_call_events();
        let (mut remote_member, mut remote_audio) = first_remote_microphone(call);

        microphone.discard_buffered_frames();
        publication.set_muted(false)?;
        let result = async {
            loop {
                tokio::select! {
                    frame = microphone.next_frame() => {
                        publication.push_audio(frame?).await?;
                    }
                    event = events.recv() => {
                        match event {
                            Ok(CallEvent::StreamStarted { member_id, kind })
                                if kind == matrix_rtc_transport::MediaStreamKind::Microphone
                                    && member_id != call.membership_id()
                                    && remote_audio.is_none() => {
                                remote_audio = call
                                    .remote_track(&member_id, kind)
                                    .and_then(|track| track.audio_frames());
                                if remote_audio.is_some() {
                                    remote_member = Some(member_id);
                                }
                            }
                            Ok(CallEvent::StreamStopped { member_id, kind })
                                if kind == matrix_rtc_transport::MediaStreamKind::Microphone
                                    && remote_member.as_deref() == Some(member_id.as_str()) => {
                                remote_audio = None;
                                remote_member = None;
                            }
                            Ok(CallEvent::Ended { .. }) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break Ok(());
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                (remote_member, remote_audio) = first_remote_microphone(call);
                            }
                            Ok(_) => {}
                        }
                    }
                    frame = async {
                        match remote_audio.as_mut() {
                            Some(stream) => stream.next().await,
                            None => std::future::pending::<Option<AudioFrame>>().await,
                        }
                    } => {
                        match frame {
                            Some(frame) => speaker.push_frame(&frame)?,
                            None => {
                                remote_audio = None;
                                remote_member = None;
                            }
                        }
                    }
                }
            }
        }
        .await;
        let _ = publication.set_muted(true);
        let _ = publication.unpublish().await;
        result
    }
}

fn first_remote_microphone(
    call: &matrix_rtc_call_sdk::LiveKitCall,
) -> (
    Option<String>,
    Option<matrix_rtc_transport::FrameStream<'static, AudioFrame>>,
) {
    for participant in call.participants() {
        if !participant.is_local
            && participant
                .streams
                .iter()
                .any(|stream| stream.kind == matrix_rtc_transport::MediaStreamKind::Microphone)
        {
            if let Some(stream) = call
                .remote_track(
                    &participant.member_id,
                    matrix_rtc_transport::MediaStreamKind::Microphone,
                )
                .and_then(|track| track.audio_frames())
            {
                return (Some(participant.member_id), Some(stream));
            }
        }
    }
    (None, None)
}

struct MuteOnDrop<'a>(&'a Publication);

impl Drop for MuteOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.set_muted(true);
    }
}

fn select_48k_config(
    device: &cpal::Device,
    input: bool,
) -> Result<SupportedStreamConfig, MediaError> {
    let configs: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|error| MediaError::Device(error.to_string()))?
            .collect()
    };
    configs
        .into_iter()
        .filter(|range| {
            (1..=2).contains(&range.channels())
                && range.min_sample_rate() <= SAMPLE_RATE
                && range.max_sample_rate() >= SAMPLE_RATE
                && matches!(
                    range.sample_format(),
                    SampleFormat::I16 | SampleFormat::F32 | SampleFormat::U16
                )
        })
        .min_by_key(|range| {
            // Prefer stereo output but mono input; f32 and i16 are common.
            let channel_preference = if input {
                range.channels() - 1
            } else {
                2 - range.channels()
            };
            (
                channel_preference,
                !matches!(range.sample_format(), SampleFormat::F32),
            )
        })
        .map(|range| range.with_sample_rate(SAMPLE_RATE))
        .ok_or_else(|| {
            MediaError::Device("device has no supported 48 kHz mono/stereo PCM mode".into())
        })
}

fn remix_channels(
    source: &[i16],
    source_channels: u32,
    target_channels: u32,
    out: &mut VecDeque<i16>,
) {
    match (source_channels, target_channels) {
        (1, 2) => {
            for sample in source {
                out.push_back(*sample);
                out.push_back(*sample);
            }
        }
        (2, 1) => {
            for stereo in source.chunks_exact(2) {
                out.push_back(((i32::from(stereo[0]) + i32::from(stereo[1])) / 2) as i16);
            }
        }
        _ => out.extend(source.iter().copied()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_to_mono_mix_does_not_overflow() {
        let mut output = VecDeque::new();
        remix_channels(&[i16::MAX, i16::MAX], 2, 1, &mut output);
        assert_eq!(output.pop_front(), Some(i16::MAX));
    }

    #[test]
    fn mono_to_stereo_duplicates_samples() {
        let mut output = VecDeque::new();
        remix_channels(&[10, -10], 1, 2, &mut output);
        assert_eq!(output.into_iter().collect::<Vec<_>>(), [10, 10, -10, -10]);
    }
}
