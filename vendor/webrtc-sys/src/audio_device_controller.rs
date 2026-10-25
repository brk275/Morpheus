// Copyright 2026 LiveKit, Inc.
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

pub use cxx::SharedPtr;

use crate::impl_thread_safety;

#[cxx::bridge(namespace = "livekit_ffi")]
pub mod ffi {
    unsafe extern "C++" {
        include!("livekit/audio_device_controller.h");
        include!("livekit/peer_connection_factory.h");

        type AudioDeviceController;
        type PeerConnectionFactory = crate::peer_connection_factory::ffi::PeerConnectionFactory;

        fn audio_device(self: &PeerConnectionFactory) -> SharedPtr<AudioDeviceController>;

        fn initialized(self: &AudioDeviceController) -> bool;
        fn synthetic_initialized(self: &AudioDeviceController) -> bool;

        fn playout_devices(self: &AudioDeviceController) -> i16;
        fn recording_devices(self: &AudioDeviceController) -> i16;
        fn playout_device_name(self: &AudioDeviceController, index: u16) -> String;
        fn recording_device_name(self: &AudioDeviceController, index: u16) -> String;
        fn playout_device_guid(self: &AudioDeviceController, index: u16) -> String;
        fn recording_device_guid(self: &AudioDeviceController, index: u16) -> String;

        fn set_playout_device(self: &AudioDeviceController, index: u16) -> bool;
        fn set_recording_device(self: &AudioDeviceController, index: u16) -> bool;
        fn set_playout_device_by_guid(self: &AudioDeviceController, guid: String) -> bool;
        fn set_recording_device_by_guid(self: &AudioDeviceController, guid: String) -> bool;

        fn stop_recording(self: &AudioDeviceController) -> bool;
        fn init_recording(self: &AudioDeviceController) -> bool;
        fn start_recording(self: &AudioDeviceController) -> bool;
        fn recording_is_initialized(self: &AudioDeviceController) -> bool;

        fn stop_playout(self: &AudioDeviceController) -> bool;
        fn init_playout(self: &AudioDeviceController) -> bool;
        fn start_playout(self: &AudioDeviceController) -> bool;
        fn playout_is_initialized(self: &AudioDeviceController) -> bool;

        fn builtin_aec_is_available(self: &AudioDeviceController) -> bool;
        fn builtin_agc_is_available(self: &AudioDeviceController) -> bool;
        fn builtin_ns_is_available(self: &AudioDeviceController) -> bool;
        fn enable_builtin_aec(self: &AudioDeviceController, enable: bool) -> bool;
        fn enable_builtin_agc(self: &AudioDeviceController, enable: bool) -> bool;
        fn enable_builtin_ns(self: &AudioDeviceController, enable: bool) -> bool;

        fn set_adm_recording_enabled(self: &AudioDeviceController, enabled: bool);
        fn adm_recording_enabled(self: &AudioDeviceController) -> bool;

        fn set_adm_playout_enabled(self: &AudioDeviceController, enabled: bool);
        fn adm_playout_enabled(self: &AudioDeviceController) -> bool;

        fn acquire_platform_adm(self: &AudioDeviceController) -> bool;
        fn release_platform_adm(self: &AudioDeviceController);
        fn platform_adm_ref_count(self: &AudioDeviceController) -> i32;
        fn is_platform_adm_active(self: &AudioDeviceController) -> bool;
    }
}

impl_thread_safety!(ffi::AudioDeviceController, Send + Sync);

#[cfg(test)]
mod tests {
    use super::SharedPtr;
    use crate::{
        candidate::ffi::Candidate,
        data_channel::ffi::DataChannel,
        jsep::ffi::IceCandidate,
        media_stream::ffi::MediaStream,
        peer_connection::ffi::{
            ContinualGatheringPolicy, IceConnectionState, IceGatheringState, IceTransportsType,
            PeerConnectionState, RtcConfiguration, SignalingState,
        },
        peer_connection_factory::{
            ffi::{create_peer_connection_factory, CandidatePairChangeEvent},
            PeerConnectionObserver, PeerConnectionObserverWrapper,
        },
        rtp_receiver::ffi::RtpReceiver,
        rtp_transceiver::ffi::RtpTransceiver,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    struct BarePeerObserver {
        released: Arc<AtomicUsize>,
    }

    impl Drop for BarePeerObserver {
        fn drop(&mut self) {
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl PeerConnectionObserver for BarePeerObserver {
        fn on_signaling_change(&self, _state: SignalingState) {}
        fn on_add_stream(&self, _stream: SharedPtr<MediaStream>) {}
        fn on_remove_stream(&self, _stream: SharedPtr<MediaStream>) {}
        fn on_data_channel(&self, _channel: SharedPtr<DataChannel>) {}
        fn on_renegotiation_needed(&self) {}
        fn on_negotiation_needed_event(&self, _event: u32) {}
        fn on_ice_connection_change(&self, _state: IceConnectionState) {}
        fn on_standardized_ice_connection_change(&self, _state: IceConnectionState) {}
        fn on_connection_change(&self, _state: PeerConnectionState) {}
        fn on_ice_gathering_change(&self, _state: IceGatheringState) {}
        fn on_ice_candidate(&self, _candidate: SharedPtr<IceCandidate>) {}
        fn on_ice_candidate_error(
            &self,
            _address: String,
            _port: i32,
            _url: String,
            _error_code: i32,
            _error_text: String,
        ) {
        }
        fn on_ice_candidates_removed(&self, _removed: Vec<SharedPtr<Candidate>>) {}
        fn on_ice_connection_receiving_change(&self, _receiving: bool) {}
        fn on_ice_selected_candidate_pair_changed(&self, _event: CandidatePairChangeEvent) {}
        fn on_add_track(
            &self,
            _receiver: SharedPtr<RtpReceiver>,
            _streams: Vec<SharedPtr<MediaStream>>,
        ) {
        }
        fn on_track(&self, _transceiver: SharedPtr<RtpTransceiver>) {}
        fn on_remove_track(&self, _receiver: SharedPtr<RtpReceiver>) {}
        fn on_interesting_usage(&self, _pattern: i32) {}
    }

    #[test]
    fn test_audio_device_controller_bridge() {
        let _guard = TEST_MUTEX.lock().expect("test mutex poisoned");
        let factory = create_peer_connection_factory();
        let audio = factory.audio_device();

        // Synthetic initialization needs no physical device. Do not start audio.
        assert!(audio.initialized());
        assert!(audio.synthetic_initialized());
        let released = Arc::new(AtomicUsize::new(0));
        for expected_releases in 1..=2 {
            // Each cycle has exactly one bare PC, no SDP, tracks or ICE servers.
            // Keep this same factory across both last-PC releases.
            let peer = factory
                .create_peer_connection(
                    RtcConfiguration {
                        ice_servers: Vec::new(),
                        continual_gathering_policy: ContinualGatheringPolicy::GatherOnce,
                        ice_transport_type: IceTransportsType::None,
                    },
                    Box::new(PeerConnectionObserverWrapper::new(Arc::new(
                        BarePeerObserver {
                            released: Arc::clone(&released),
                        },
                    ))),
                )
                .expect("bare peer connection creation failed");
            assert!(!peer.is_null());
            assert!(audio.initialized());
            assert!(audio.synthetic_initialized());
            peer.close();
            drop(peer);
            // The C++ PC releases its native PC member before this observer.
            assert_eq!(released.load(Ordering::Relaxed), expected_releases);
            assert!(audio.initialized(), "last peer release terminated the ADM");
            assert!(
                audio.synthetic_initialized(),
                "last peer release terminated the synthetic ADM"
            );
        }

        let recording_count = audio.recording_devices();
        let playout_count = audio.playout_devices();
        assert!(recording_count >= 0);
        assert!(playout_count >= 0);

        if recording_count > 0 {
            let _ = audio.recording_device_name(0);
            let guid = audio.recording_device_guid(0);
            let _ = audio.set_recording_device_by_guid(guid);
        }

        if playout_count > 0 {
            let _ = audio.playout_device_name(0);
            let guid = audio.playout_device_guid(0);
            let _ = audio.set_playout_device_by_guid(guid);
        }

        let initial_recording = audio.adm_recording_enabled();
        audio.set_adm_recording_enabled(!initial_recording);
        assert_eq!(audio.adm_recording_enabled(), !initial_recording);
        audio.set_adm_recording_enabled(initial_recording);
        assert_eq!(audio.adm_recording_enabled(), initial_recording);

        let initial_playout = audio.adm_playout_enabled();
        audio.set_adm_playout_enabled(!initial_playout);
        assert_eq!(audio.adm_playout_enabled(), !initial_playout);
        audio.set_adm_playout_enabled(initial_playout);
        assert_eq!(audio.adm_playout_enabled(), initial_playout);

        assert!(audio.platform_adm_ref_count() >= 0);
        let _ = audio.is_platform_adm_active();
    }
}
