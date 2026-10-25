/*
 * Copyright 2025 LiveKit, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "livekit/webrtc.h"

#include <algorithm>
#include <atomic>
#include <iostream>
#include <memory>
#include <mutex>

#include "livekit/audio_track.h"
#include "livekit/media_stream_track.h"
#include "livekit/rtp_receiver.h"
#include "livekit/rtp_sender.h"
#include "livekit/video_track.h"
#include "rtc_base/logging.h"
#include "rtc_base/crypto_random.h"
#include "rtc_base/synchronization/mutex.h"

#ifdef WEBRTC_WIN
#include "rtc_base/win32.h"
#endif

namespace livekit_ffi {

static webrtc::Mutex g_mutex{};
// Can't be atomic, we're using a Mutex because we need to wait for the
// execution of the first init
static uint32_t g_release_counter(0);

void ensure_native_log_safety() {
  static std::once_flag once;
  std::call_once(once, [] {
    // Some native key derivation sites log key bytes at INFO. Disable the
    // independent debugger/stderr route before a runtime or provider exists.
    webrtc::LogMessage::LogToDebug(webrtc::LS_NONE);
    webrtc::LogMessage::SetLogToStderr(false);
  });
}

RtcRuntime::RtcRuntime() {
  ensure_native_log_safety();
  RTC_LOG(LS_VERBOSE) << "RtcRuntime()";

  {
    // Not the best way to do it...
    webrtc::MutexLock lock(&g_mutex);
    if (g_release_counter == 0) {
      RTC_CHECK(webrtc::InitializeSSL()) << "Failed to InitializeSSL()";

#ifdef WEBRTC_WIN
      WSADATA data;
      WSAStartup(MAKEWORD(1, 0), &data);
#endif
    }
    g_release_counter++;
  }

  network_thread_ = webrtc::Thread::CreateWithSocketServer();
  network_thread_->SetName("network_thread", &network_thread_);
  network_thread_->Start();
  worker_thread_ = webrtc::Thread::Create();
  worker_thread_->SetName("worker_thread", &worker_thread_);
  worker_thread_->Start();
  signaling_thread_ = webrtc::Thread::Create();
  signaling_thread_->SetName("signaling_thread", &signaling_thread_);
  signaling_thread_->Start();
}

RtcRuntime::~RtcRuntime() {
  RTC_LOG(LS_VERBOSE) << "~RtcRuntime()";

  worker_thread_->Stop();
  signaling_thread_->Stop();
  network_thread_->Stop();

  {
    webrtc::MutexLock lock(&g_mutex);
    g_release_counter--;
    if (g_release_counter == 0) {
      RTC_CHECK(webrtc::CleanupSSL()) << "Failed to CleanupSSL()";

#ifdef WEBRTC_WIN
      WSACleanup();
#endif
    }
  }
}

webrtc::Thread* RtcRuntime::network_thread() const {
  return network_thread_.get();
}

webrtc::Thread* RtcRuntime::worker_thread() const {
  return worker_thread_.get();
}

webrtc::Thread* RtcRuntime::signaling_thread() const {
  return signaling_thread_.get();
}

std::shared_ptr<MediaStreamTrack> RtcRuntime::get_or_create_media_stream_track(
    webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> rtc_track) {
  webrtc::MutexLock lock(&mutex_);
  for (std::weak_ptr<MediaStreamTrack> weak_existing_track :
       media_stream_tracks_) {
    if (std::shared_ptr<MediaStreamTrack> existing_track =
            weak_existing_track.lock()) {
      if (existing_track->rtc_track() == rtc_track) {
        return existing_track;
      }
    }
  }

  if (rtc_track->kind() == webrtc::MediaStreamTrackInterface::kVideoKind) {
    std::shared_ptr<VideoTrack> video_track =
        std::shared_ptr<VideoTrack>(new VideoTrack(
            shared_from_this(),
            webrtc::scoped_refptr<webrtc::VideoTrackInterface>(
                static_cast<webrtc::VideoTrackInterface*>(rtc_track.get()))));

    media_stream_tracks_.push_back(
        std::static_pointer_cast<MediaStreamTrack>(video_track));
    return video_track;
  } else {
    std::shared_ptr<AudioTrack> audio_track =
        std::shared_ptr<AudioTrack>(new AudioTrack(
            shared_from_this(),
            webrtc::scoped_refptr<webrtc::AudioTrackInterface>(
                static_cast<webrtc::AudioTrackInterface*>(rtc_track.get()))));

    media_stream_tracks_.push_back(
        std::static_pointer_cast<MediaStreamTrack>(audio_track));
    return audio_track;
  }
}

std::shared_ptr<AudioTrack> RtcRuntime::get_or_create_audio_track(
    webrtc::scoped_refptr<webrtc::AudioTrackInterface> track) {
  return std::static_pointer_cast<AudioTrack>(
      get_or_create_media_stream_track(track));
}

std::shared_ptr<VideoTrack> RtcRuntime::get_or_create_video_track(
    webrtc::scoped_refptr<webrtc::VideoTrackInterface> track) {
  return std::static_pointer_cast<VideoTrack>(
      get_or_create_media_stream_track(track));
}

LogSink::LogSink(
    rust::Fn<void(rust::String message, LoggingSeverity severity)> fnc)
    : fnc_(fnc) {
  ensure_native_log_safety();
  webrtc::LogMessage::AddLogToStream(this, webrtc::LS_WARNING);
}

LogSink::~LogSink() {
  webrtc::LogMessage::RemoveLogToStream(this);
}

void LogSink::OnLogMessage(const std::string& message,
                           webrtc::LoggingSeverity severity) {
  if (severity != webrtc::LS_WARNING && severity != webrtc::LS_ERROR) {
    return;
  }
  fnc_(rust::String::lossy(message), static_cast<LoggingSeverity>(severity));
}

std::unique_ptr<LogSink> new_log_sink(
    rust::Fn<void(rust::String, LoggingSeverity)> fnc) {
  return std::make_unique<LogSink>(fnc);
}

// Fixed, private Rust-test support. These functions never accept message text
// or key material and have no application callers.
bool native_log_policy_for_test_debug_disabled() {
  return webrtc::LogMessage::GetLogToDebug() == webrtc::LS_NONE;
}

bool native_log_policy_for_test_sink_is_warning(LogSink& sink) {
  return webrtc::LogMessage::GetLogToStream(&sink) == webrtc::LS_WARNING;
}

void native_log_policy_for_test_emit(LogSink& sink) {
  RTC_LOG(LS_VERBOSE) << "MORPHEUS_NATIVE_LOG_POLICY_native_verbose";
  RTC_LOG(LS_INFO) << "MORPHEUS_NATIVE_LOG_POLICY_native_info";
  RTC_LOG(LS_WARNING) << "MORPHEUS_NATIVE_LOG_POLICY_native_warning";
  RTC_LOG(LS_ERROR) << "MORPHEUS_NATIVE_LOG_POLICY_native_error";
  // Exercise the forwarding guard independently of stream registration.
  sink.OnLogMessage("MORPHEUS_NATIVE_LOG_POLICY_direct_verbose", webrtc::LS_VERBOSE);
  sink.OnLogMessage("MORPHEUS_NATIVE_LOG_POLICY_direct_info", webrtc::LS_INFO);
  sink.OnLogMessage("MORPHEUS_NATIVE_LOG_POLICY_direct_none", webrtc::LS_NONE);
  sink.OnLogMessage("MORPHEUS_NATIVE_LOG_POLICY_direct_warning", webrtc::LS_WARNING);
  sink.OnLogMessage("MORPHEUS_NATIVE_LOG_POLICY_direct_error", webrtc::LS_ERROR);
}

rust::String create_random_uuid() {
  return webrtc::CreateRandomUuid();
}

}  // namespace livekit_ffi
