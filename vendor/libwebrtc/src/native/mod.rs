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

// Modified by Morpheus: bounded native stats parsing; see MORPHEUS_PATCH.md.

#[cfg(target_os = "android")]
pub mod android;
pub mod apm;
pub mod audio_mixer;
pub mod audio_resampler;
pub mod audio_source;
pub mod audio_stream;
pub mod audio_track;
pub mod data_channel;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub mod desktop_capturer;
pub mod frame_cryptor;
pub mod ice_candidate;
pub mod media_stream;
pub mod media_stream_track;
pub mod packet_trailer;
pub mod peer_connection;
pub mod peer_connection_factory;
pub mod rtp_parameters;
pub mod rtp_receiver;
pub mod rtp_sender;
pub mod rtp_transceiver;
pub mod session_description;
pub mod video_frame;
pub mod video_source;
pub mod video_stream;
pub mod video_track;
pub mod yuv_helper;

use webrtc_sys::{rtc_error as sys_err, webrtc as sys_rtc};

use crate::{MediaType, RtcError, RtcErrorType};

impl From<sys_err::ffi::RtcErrorType> for RtcErrorType {
    fn from(value: sys_err::ffi::RtcErrorType) -> Self {
        match value {
            sys_err::ffi::RtcErrorType::InvalidState => Self::InvalidState,
            _ => Self::Internal,
        }
    }
}

impl From<sys_err::ffi::RtcError> for RtcError {
    fn from(value: sys_err::ffi::RtcError) -> Self {
        Self { error_type: value.error_type.into(), message: value.message }
    }
}

impl From<MediaType> for sys_rtc::ffi::MediaType {
    fn from(value: MediaType) -> Self {
        match value {
            MediaType::Audio => Self::Audio,
            MediaType::Video => Self::Video,
            MediaType::Data => Self::Data,
            MediaType::Unsupported => Self::Unsupported,
        }
    }
}

// Check the native-owned String before any JSON deserialization. The String has
// already crossed CXX; this does not bound native collection or serialization.
const MAX_STATS_REPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STATS_REPORT_RECORDS: usize = 4096;

fn invalid_stats_report() -> RtcError {
    RtcError {
        error_type: RtcErrorType::Internal,
        message: "invalid native stats report".to_owned(),
    }
}

pub(crate) fn parse_stats_report(report: &str) -> Result<Vec<crate::stats::RtcStats>, RtcError> {
    if report.len() > MAX_STATS_REPORT_BYTES {
        return Err(invalid_stats_report());
    }
    // Preserve the native API's original empty-string behavior.
    if report.is_empty() {
        return Ok(Vec::new());
    }

    struct ReportVisitor;

    impl<'de> serde::de::Visitor<'de> for ReportVisitor {
        type Value = Vec<crate::stats::RtcStats>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a bounded native stats array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            // Do not reserve from an untrusted sequence size hint.
            let mut records = Vec::new();
            while let Some(record) = sequence.next_element::<crate::stats::RtcStats>()? {
                if records.len() == MAX_STATS_REPORT_RECORDS {
                    return Err(serde::de::Error::custom("native stats record limit exceeded"));
                }
                records.push(record);
            }
            Ok(records)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(report);
    let records = serde::Deserializer::deserialize_seq(&mut deserializer, ReportVisitor)
        .map_err(|_| invalid_stats_report())?;
    // A valid array followed by another JSON value must remain an error.
    deserializer.end().map_err(|_| invalid_stats_report())?;
    Ok(records)
}

#[cfg(test)]
mod stats_tests {
    use super::{parse_stats_report, MAX_STATS_REPORT_BYTES, MAX_STATS_REPORT_RECORDS};
    use crate::{stats::RtcStats, RtcErrorType};

    fn assert_fixed_error(report: &str) {
        let error = parse_stats_report(report).expect_err("invalid report was accepted");
        assert_eq!(error.error_type, RtcErrorType::Internal);
        assert_eq!(error.message, "invalid native stats report");
    }

    #[test]
    fn native_stats_parser_preserves_empty_and_typed_reports() {
        assert!(parse_stats_report("").expect("empty native report").is_empty());
        assert!(parse_stats_report(" [] ").expect("empty array").is_empty());
        let records = parse_stats_report(
            r#"[{"type":"peer-connection","id":"fixture","timestamp":1,"dataChannelsOpened":2,"dataChannelsClosed":1}]"#,
        ).expect("valid native report");
        assert_eq!(records.len(), 1);
        let RtcStats::PeerConnection(record) = &records[0] else {
            panic!("native report lost its typed record");
        };
        assert_eq!(record.pc.data_channels_opened, 2);
        assert_eq!(record.pc.data_channels_closed, 1);
    }

    #[test]
    fn native_stats_parser_rejects_malformed_and_unknown_reports() {
        for report in [
            "[",
            "null",
            "{}",
            r#"[{"type":"unknown","id":"fixture"}]"#,
            r#"[{"type":"peer-connection","timestamp":"invalid"}]"#,
            "[] []",
        ] {
            assert_fixed_error(report);
        }
    }

    #[test]
    fn native_stats_parser_enforces_byte_and_record_limits() {
        let mut exact = " ".repeat(MAX_STATS_REPORT_BYTES - 2);
        exact.push_str("[]");
        assert!(parse_stats_report(&exact).expect("exact byte bound").is_empty());
        exact.push(' ');
        assert_fixed_error(&exact);

        let entry = r#"{"type":"track"}"#;
        let records = vec![entry; MAX_STATS_REPORT_RECORDS].join(",");
        let exact_records = format!("[{records}]");
        assert_eq!(
            parse_stats_report(&exact_records).expect("exact record bound").len(),
            MAX_STATS_REPORT_RECORDS
        );
        assert_fixed_error(&format!("[{records},{entry}]"));
    }
}
