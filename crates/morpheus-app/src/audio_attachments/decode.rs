//! Bounded memory-only WAV/MP3 decoding. No file/network/generic tag or artwork reader.
use super::AudioError;
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::Arc,
};
use symphonia_bundle_mp3::{MpaDecoder, MpaReader};
use symphonia_core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions},
    formats::{FormatOptions, FormatReader, TrackType},
    io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions},
};
use zeroize::Zeroizing;

pub(super) const MAX_INPUT: usize = morpheus_matrix::MAX_AUDIO_PLAYBACK_BYTES;
pub(super) const MAX_SAMPLES: usize = 12 * 1024 * 1024;
const MAX_SECONDS: usize = 180;
const MAX_PACKETS: usize = 100_000;
pub(super) type Authority = Arc<dyn Fn() -> bool + Send + Sync>;
pub(super) struct Clip {
    pub samples: Vec<f32>,
    pub rate: u32,
    pub channels: u16,
}

fn shape(rate: u32, channels: usize, samples: usize) -> Result<(), AudioError> {
    if !(8_000..=96_000).contains(&rate) || !(1..=2).contains(&channels) {
        return Err(AudioError::Unsupported);
    }
    if samples > MAX_SAMPLES || samples / channels > rate as usize * MAX_SECONDS {
        return Err(AudioError::TooLarge);
    }
    Ok(())
}
fn active(allowed: &Authority) -> Result<(), AudioError> {
    if allowed() {
        Ok(())
    } else {
        Err(AudioError::Cancelled)
    }
}
pub(super) fn decode(
    bytes: Zeroizing<Vec<u8>>,
    mime: &str,
    allowed: Authority,
) -> Result<Clip, AudioError> {
    active(&allowed)?;
    if bytes.is_empty() || bytes.len() > MAX_INPUT {
        return Err(AudioError::TooLarge);
    }
    match mime {
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => wav(&bytes, &allowed),
        "audio/mpeg" | "audio/mp3" => mp3(bytes, allowed),
        _ => Err(AudioError::Unsupported),
    }
}
fn u16le(bytes: &[u8], at: usize) -> Result<u16, AudioError> {
    let data = bytes
        .get(at..at.checked_add(2).ok_or(AudioError::Invalid)?)
        .ok_or(AudioError::Invalid)?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}
fn u32le(bytes: &[u8], at: usize) -> Result<u32, AudioError> {
    let data = bytes
        .get(at..at.checked_add(4).ok_or(AudioError::Invalid)?)
        .ok_or(AudioError::Invalid)?;
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}
fn wav(bytes: &[u8], allowed: &Authority) -> Result<Clip, AudioError> {
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(AudioError::Unsupported);
    }
    let end = (u32le(bytes, 4)? as usize)
        .checked_add(8)
        .ok_or(AudioError::Invalid)?;
    if end != bytes.len() {
        return Err(AudioError::Invalid);
    }
    let (mut format, mut data) = (None, None);
    let mut at = 12usize;
    let mut chunks = 0;
    while at < end {
        active(allowed)?;
        chunks += 1;
        if chunks > 512 {
            return Err(AudioError::TooLarge);
        }
        let header = bytes
            .get(at..at.checked_add(8).ok_or(AudioError::Invalid)?)
            .ok_or(AudioError::Invalid)?;
        let len = u32le(header, 4)? as usize;
        let start = at.checked_add(8).ok_or(AudioError::Invalid)?;
        let stop = start.checked_add(len).ok_or(AudioError::Invalid)?;
        let chunk = bytes.get(start..stop).ok_or(AudioError::Invalid)?;
        match &header[..4] {
            b"fmt " => {
                if format.is_some() || !matches!(len, 16 | 18) {
                    return Err(AudioError::Unsupported);
                }
                if len == 18 && u16le(chunk, 16)? != 0 {
                    return Err(AudioError::Unsupported);
                }
                let (kind, channels, rate, align, bits) = (
                    u16le(chunk, 0)?,
                    u16le(chunk, 2)?,
                    u32le(chunk, 4)?,
                    u16le(chunk, 12)?,
                    u16le(chunk, 14)?,
                );
                if !matches!((kind, bits), (1, 8 | 16 | 24 | 32) | (3, 32)) {
                    return Err(AudioError::Unsupported);
                }
                shape(rate, channels as usize, 0)?;
                let expected = channels.checked_mul(bits / 8).ok_or(AudioError::Invalid)?;
                if align != expected
                    || u32le(chunk, 8)?
                        != rate
                            .checked_mul(u32::from(expected))
                            .ok_or(AudioError::Invalid)?
                {
                    return Err(AudioError::Invalid);
                }
                format = Some((kind, channels, rate, align as usize, bits));
            }
            b"data" => {
                if data.is_some() {
                    return Err(AudioError::Invalid);
                }
                data = Some(start..stop);
            }
            _ => {} // Unknown chunks are bounds-checked and skipped; never tags/artwork.
        }
        at = stop.checked_add(len & 1).ok_or(AudioError::Invalid)?;
        if at > end {
            return Err(AudioError::Invalid);
        }
    }
    let (kind, channels, rate, align, bits) = format.ok_or(AudioError::Invalid)?;
    let data = &bytes[data.ok_or(AudioError::Invalid)?];
    if data.is_empty() || data.len() % align != 0 {
        return Err(AudioError::Invalid);
    }
    let width = usize::from(bits / 8);
    let count = data.len() / width;
    shape(rate, channels as usize, count)?;
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(count)
        .map_err(|_| AudioError::TooLarge)?;
    for (i, sample) in data.chunks_exact(width).enumerate() {
        if i % 1024 == 0 {
            active(allowed)?;
        }
        let value = match (kind, bits) {
            (1, 8) => (f32::from(sample[0]) - 128.0) / 128.0,
            (1, 16) => f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0,
            (1, 24) => {
                let raw = i32::from_le_bytes([0, sample[0], sample[1], sample[2]]) >> 8;
                raw as f32 / 8_388_608.0
            }
            (1, 32) => {
                i32::from_le_bytes(sample.try_into().map_err(|_| AudioError::Invalid)?) as f32
                    / 2_147_483_648.0
            }
            (3, 32) => f32::from_le_bytes(sample.try_into().map_err(|_| AudioError::Invalid)?),
            _ => return Err(AudioError::Unsupported),
        };
        if !value.is_finite() {
            return Err(AudioError::Invalid);
        }
        samples.push(value.clamp(-1.0, 1.0));
    }
    active(allowed)?;
    Ok(Clip {
        samples,
        rate,
        channels,
    })
}

/// Only skip the checked envelope. No ID3 payload is decoded or retained.
fn mp3_range(bytes: &[u8]) -> Result<std::ops::Range<usize>, AudioError> {
    let mut start = 0usize;
    if bytes.starts_with(b"ID3") {
        let h = bytes.get(..10).ok_or(AudioError::Invalid)?;
        if !matches!(h[3], 2..=4) || h[4] == 0xff || h[6..10].iter().any(|v| v & 0x80 != 0) {
            return Err(AudioError::Invalid);
        }
        let size = h[6..10]
            .iter()
            .fold(0usize, |n, b| (n << 7) | usize::from(*b));
        start = 10usize
            .checked_add(size)
            .and_then(|n| n.checked_add(if h[3] == 4 && h[5] & 0x10 != 0 { 10 } else { 0 }))
            .ok_or(AudioError::Invalid)?;
        if start > bytes.len() {
            return Err(AudioError::Invalid);
        }
    }
    let mut end = bytes.len();
    if end >= 128 && bytes.get(end - 128..end - 125) == Some(b"TAG") {
        end -= 128;
    }
    if start >= end {
        return Err(AudioError::Invalid);
    }
    Ok(start..end)
}
/// A capped, revocable memory source. No path, external I/O, or metadata probe.
struct Memory {
    bytes: Zeroizing<Vec<u8>>,
    range: std::ops::Range<usize>,
    position: u64,
    allowed: Authority,
}
impl Memory {
    fn check(&self) -> io::Result<()> {
        if (self.allowed)() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "audio playback retired",
            ))
        }
    }
}
impl Read for Memory {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let length = self.range.len();
        let position = usize::try_from(self.position).map_err(|_| io::ErrorKind::InvalidInput)?;
        let count = output.len().min(length.saturating_sub(position));
        let start = self
            .range
            .start
            .checked_add(position)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let end = start
            .checked_add(count)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let input = self
            .bytes
            .get(start..end)
            .ok_or(io::ErrorKind::InvalidInput)?;
        output[..count].copy_from_slice(input);
        self.position += count as u64;
        Ok(count)
    }
}
impl Seek for Memory {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let next = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => self.range.len() as i128 + i128::from(n),
        };
        if next < 0 || next > self.range.len() as i128 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.position = next as u64;
        Ok(self.position)
    }
}
impl MediaSource for Memory {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.range.len() as u64)
    }
}
fn mp3(bytes: Zeroizing<Vec<u8>>, allowed: Authority) -> Result<Clip, AudioError> {
    active(&allowed)?;
    let range = mp3_range(&bytes)?;
    let memory = Memory {
        bytes,
        range,
        position: 0,
        allowed: allowed.clone(),
    };
    let stream = MediaSourceStream::new(Box::new(memory), MediaSourceStreamOptions::default());
    // Upstream handles bounded MPEG frames and fixed Xing/Info/VBRI headers.
    // No generic metadata reader is linked. Advertised duration never sizes PCM.
    let mut reader = MpaReader::try_new(stream, FormatOptions::default()).map_err(|_| {
        if allowed() {
            AudioError::Invalid
        } else {
            AudioError::Cancelled
        }
    })?;
    active(&allowed)?;
    let parameters = reader
        .default_track(TrackType::Audio)
        .and_then(|track| track.codec_params.as_ref())
        .and_then(|parameters| parameters.audio())
        .ok_or(AudioError::Unsupported)?;
    let mut decoder = MpaDecoder::try_new(parameters, &AudioDecoderOptions::default())
        .map_err(|_| AudioError::Unsupported)?;
    let mut clip = Clip {
        samples: Vec::new(),
        rate: 0,
        channels: 0,
    };
    for _ in 0..MAX_PACKETS {
        active(&allowed)?;
        let packet = reader.next_packet().map_err(|_| {
            if allowed() {
                AudioError::Invalid
            } else {
                AudioError::Cancelled
            }
        })?;
        let Some(packet) = packet else {
            active(&allowed)?;
            // MPEG has no hard end marker: upstream may discard an incomplete
            // final frame. Duration always describes only PCM actually decoded.
            return if clip.samples.is_empty() {
                Err(AudioError::Invalid)
            } else {
                Ok(clip)
            };
        };
        if packet.data.len() > 16 * 1024 {
            return Err(AudioError::TooLarge);
        }
        let decoded = decoder.decode(&packet).map_err(|_| AudioError::Invalid)?;
        let rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count();
        if decoded.frames() > 4096 {
            return Err(AudioError::TooLarge);
        }
        let count = decoded
            .frames()
            .checked_mul(channels)
            .ok_or(AudioError::TooLarge)?;
        let total = clip
            .samples
            .len()
            .checked_add(count)
            .ok_or(AudioError::TooLarge)?;
        shape(rate, channels, total)?;
        if clip.rate == 0 {
            clip.rate = rate;
            clip.channels = channels as u16;
        } else if clip.rate != rate || usize::from(clip.channels) != channels {
            return Err(AudioError::Unsupported);
        }
        if clip.samples.capacity() < total {
            let target = total
                .max(clip.samples.capacity().saturating_mul(2))
                .min(MAX_SAMPLES);
            clip.samples
                .try_reserve_exact(target - clip.samples.len())
                .map_err(|_| AudioError::TooLarge)?;
        }
        let start = clip.samples.len();
        clip.samples.resize(total, 0.0);
        decoded.copy_to_slice_interleaved(&mut clip.samples[start..]);
        for value in &mut clip.samples[start..] {
            if !value.is_finite() {
                return Err(AudioError::Invalid);
            }
            *value = value.clamp(-1.0, 1.0);
        }
    }
    Err(AudioError::TooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn allowed() -> Authority {
        Arc::new(|| true)
    }
    fn wave(kind: u16, channels: u16, bits: u16, data: &[u8]) -> Vec<u8> {
        let align = channels * (bits / 8);
        let mut b = b"RIFF".to_vec();
        b.extend((36u32 + data.len() as u32 + (data.len() as u32 & 1)).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(kind.to_le_bytes());
        b.extend(channels.to_le_bytes());
        b.extend(48_000u32.to_le_bytes());
        b.extend((48_000u32 * u32::from(align)).to_le_bytes());
        b.extend(align.to_le_bytes());
        b.extend(bits.to_le_bytes());
        b.extend(b"data");
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
        if data.len() & 1 != 0 {
            b.push(0);
        }
        b
    }
    #[test]
    fn wav_signed_samples_and_actual_duration_are_decoded() {
        let b = wave(1, 1, 16, &[0, 128, 0, 0, 255, 127]);
        let c = wav(&b, &allowed()).unwrap();
        assert_eq!((c.rate, c.channels, c.samples.len()), (48_000, 1, 3));
        assert_eq!(c.samples[0], -1.0);
        assert_eq!(c.samples[1], 0.0);
        assert!(c.samples[2] > 0.99);
    }
    #[test]
    fn wav_supported_integer_widths_and_float_are_converted_without_sign_confusion() {
        for (kind, bits, bytes, expected) in [
            (1, 8, vec![0], -1.0f32),
            (1, 24, vec![0, 0, 128], -1.0),
            (1, 32, i32::MIN.to_le_bytes().to_vec(), -1.0),
            (3, 32, 0.25f32.to_le_bytes().to_vec(), 0.25),
        ] {
            let clip = wav(&wave(kind, 1, bits, &bytes), &allowed()).unwrap();
            assert_eq!(clip.samples, vec![expected]);
        }
    }
    #[test]
    fn wav_rejects_truncation_alignment_nonfinite_and_unsupported_variants() {
        let mut b = wave(1, 2, 16, &[0, 0, 0, 0]);
        b.pop();
        assert!(wav(&b, &allowed()).is_err());
        assert!(wav(&wave(1, 2, 16, &[0, 0]), &allowed()).is_err());
        assert!(wav(&wave(3, 1, 32, &f32::NAN.to_le_bytes()), &allowed()).is_err());
        assert!(wav(&wave(0xfffe, 1, 16, &[0, 0]), &allowed()).is_err());
        let mut b = wave(1, 1, 16, &[0, 0]);
        b[28..32].copy_from_slice(&1u32.to_le_bytes());
        assert!(wav(&b, &allowed()).is_err());
    }
    #[test]
    fn wav_unknown_chunks_are_skipped_not_parsed_and_duplicate_data_is_refused() {
        let mut b = wave(1, 1, 16, &[0, 0]);
        b.extend(b"JUNK");
        b.extend(1u32.to_le_bytes());
        b.extend([0xff, 0]);
        let n = (b.len() - 8) as u32;
        b[4..8].copy_from_slice(&n.to_le_bytes());
        assert!(wav(&b, &allowed()).is_ok());
        b.extend(b"data");
        b.extend(0u32.to_le_bytes());
        let n = (b.len() - 8) as u32;
        b[4..8].copy_from_slice(&n.to_le_bytes());
        assert!(wav(&b, &allowed()).is_err());
    }
    #[test]
    fn bounds_and_cancellation_precede_pcm_allocation() {
        assert!(shape(48_000, 2, MAX_SAMPLES + 2).is_err());
        assert!(shape(8_000, 1, 8_000 * 181).is_err());
        assert!(shape(48_000, 3, 0).is_err());
        let denied: Authority = Arc::new(|| false);
        assert!(matches!(
            wav(&wave(1, 1, 16, &[0, 0]), &denied),
            Err(AudioError::Cancelled)
        ));
    }
    #[test]
    fn mp3_decodes_bounded_synthetic_layer_three_silence_without_tags() {
        // Four MPEG1 LayerIII 128kb/s 44.1kHz mono frames. Zero side-info
        // and main-data encode zero spectral coefficients, not injected PCM.
        let mut bytes = vec![0u8; 417 * 4];
        for frame in bytes.chunks_mut(417) {
            frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0xc0]);
        }
        let clip = decode(Zeroizing::new(bytes), "audio/mpeg", allowed()).unwrap();
        assert_eq!((clip.rate, clip.channels), (44_100, 1));
        assert!(!clip.samples.is_empty() && clip.samples.len() <= 4 * 1152);
        assert!(clip.samples.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    }
    #[test]
    fn truncated_mp3_or_unsupported_mime_never_produces_a_clip() {
        assert!(
            decode(
                Zeroizing::new(vec![0xff, 0xfb, 0x90]),
                "audio/mpeg",
                allowed()
            )
            .is_err()
        );
        assert!(matches!(
            decode(Zeroizing::new(vec![1]), "audio/ogg", allowed()),
            Err(AudioError::Unsupported)
        ));
    }
    #[test]
    fn id3_envelope_and_memory_reads_are_bounded_and_revocable() {
        use std::sync::atomic::{AtomicBool, Ordering};
        assert!(mp3_range(b"ID3\x04\0\0\x7f\x7f\x7f\x7f").is_err());
        assert!(mp3_range(b"ID3\x04\0\0\x80\0\0\0junk").is_err());
        let permitted = Arc::new(AtomicBool::new(true));
        let check = permitted.clone();
        let mut memory = Memory {
            bytes: Zeroizing::new(vec![1, 2, 3, 4]),
            range: 1..3,
            position: 0,
            allowed: Arc::new(move || check.load(Ordering::Acquire)),
        };
        let mut output = [0u8; 8];
        assert_eq!(memory.read(&mut output).unwrap(), 2);
        assert_eq!(&output[..2], &[2, 3]);
        assert_eq!(memory.read(&mut output).unwrap(), 0);
        assert!(memory.seek(SeekFrom::Start(3)).is_err());
        assert!(memory.seek(SeekFrom::Current(i64::MIN)).is_err());
        assert_eq!(memory.seek(SeekFrom::End(-1)).unwrap(), 1);
        permitted.store(false, Ordering::Release);
        assert_eq!(
            memory.read(&mut output).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            memory.seek(SeekFrom::Start(0)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(matches!(
            mp3(
                Zeroizing::new(vec![0xff, 0xfb, 0x90, 0xc0]),
                Arc::new(|| false)
            ),
            Err(AudioError::Cancelled)
        ));
    }
}
