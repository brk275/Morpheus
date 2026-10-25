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

use crate::impl_thread_safety;

#[cxx::bridge(namespace = "livekit_ffi")]
pub mod ffi {
    #[derive(Debug)]
    #[repr(i32)]
    pub enum MediaType {
        Audio,
        Video,
        Data,
        Unsupported,
    }

    #[derive(Debug)]
    #[repr(i32)]
    pub enum Priority {
        VeryLow,
        Low,
        Medium,
        High,
    }

    #[derive(Debug)]
    #[repr(i32)]
    pub enum RtpTransceiverDirection {
        SendRecv,
        SendOnly,
        RecvOnly,
        Inactive,
        Stopped,
    }

    #[derive(Debug)]
    #[repr(i32)]
    pub enum LoggingSeverity {
        Verbose,
        Info,
        Warning,
        Error,
        None,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(i32)]
    pub enum VideoEncoderBackend {
        Auto,
        Software,
        Hardware,
        Nvenc,
        Vaapi,
        VideoToolbox,
    }

    unsafe extern "C++" {
        include!("livekit/webrtc.h");

        type LogSink;

        fn create_random_uuid() -> String;
        fn video_encoder_backend_list() -> Vec<VideoEncoderBackend>;
        fn new_log_sink(fnc: fn(String, LoggingSeverity)) -> UniquePtr<LogSink>;
    }
}

impl_thread_safety!(ffi::LogSink, Send + Sync);

// CXX's build script explicitly does not support cfg(test) on bridge items.
// Keep these fixed test shims in a private module; only the tests below call
// them. No message/key input or externally reachable Rust API is exposed.
#[cxx::bridge(namespace = "livekit_ffi")]
mod log_policy_test_ffi {
    unsafe extern "C++" {
        include!("livekit/webrtc.h");

        #[cxx_name = "LogSink"]
        type PolicyTestLogSink = super::ffi::LogSink;

        fn native_log_policy_for_test_debug_disabled() -> bool;
        fn native_log_policy_for_test_sink_is_warning(sink: Pin<&mut PolicyTestLogSink>) -> bool;
        fn native_log_policy_for_test_emit(sink: Pin<&mut PolicyTestLogSink>);
    }
}

#[cfg(test)]
mod tests {
    use super::{ffi, log_policy_test_ffi};
    use crate::frame_cryptor::ffi as cryptor;
    use std::sync::Mutex;

    static RECORDS: Mutex<Vec<(&'static str, &'static str)>> = Mutex::new(Vec::new());

    fn record(message: String, severity: ffi::LoggingSeverity) {
        let kind = [
            "native_verbose",
            "native_info",
            "native_warning",
            "native_error",
            "direct_verbose",
            "direct_info",
            "direct_none",
            "direct_warning",
            "direct_error",
        ]
        .into_iter()
        .find(|kind| message.contains(&format!("MORPHEUS_NATIVE_LOG_POLICY_{kind}")));
        let Some(kind) = kind else {
            return;
        };
        let severity = match severity {
            ffi::LoggingSeverity::Warning => "warning",
            ffi::LoggingSeverity::Error => "error",
            ffi::LoggingSeverity::Info => "info",
            ffi::LoggingSeverity::Verbose => "verbose",
            _ => "other",
        };
        RECORDS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((kind, severity));
    }

    #[test]
    fn native_log_policy_keeps_warnings_and_errors_without_info_or_debug_fallback() {
        RECORDS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        // A provider and actual synthetic key derivation precede this test's
        // sink: factory order must not be the safety prerequisite.
        let provider = cryptor::new_key_provider(cryptor::KeyProviderOptions {
            shared_key: true,
            ratchet_window_size: 16,
            ratchet_salt: vec![],
            failure_tolerance: -1,
            key_ring_size: 16,
            key_derivation_algorithm: cryptor::KeyDerivationAlgorithm::HKDF,
        });
        let fallback_disabled_before_sink =
            log_policy_test_ffi::native_log_policy_for_test_debug_disabled();
        let key_set = provider.set_shared_key(0, vec![0x3a; 32]);
        let mut sink = ffi::new_log_sink(record);
        let warning_floor =
            log_policy_test_ffi::native_log_policy_for_test_sink_is_warning(sink.pin_mut());
        log_policy_test_ffi::native_log_policy_for_test_emit(sink.pin_mut());
        drop(sink);
        drop(provider);
        let records =
            std::mem::take(&mut *RECORDS.lock().unwrap_or_else(|error| error.into_inner()));
        assert!(fallback_disabled_before_sink);
        assert!(key_set);
        assert!(warning_floor);
        assert_eq!(
            records,
            [
                ("native_warning", "warning"),
                ("native_error", "error"),
                ("direct_warning", "warning"),
                ("direct_error", "error"),
            ]
        );
    }
}
