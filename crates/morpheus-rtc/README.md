# Morpheus native RTC boundary

This crate wraps the native-only MatrixRTC path in
[`matrix-rust-rtc` v0.5.0-rc.2](https://github.com/element-hq/matrix-rust-rtc/tree/v0.5.0-rc.2).
It has no browser or WebView implementation.

The default build offers `local_support()` and pure transport-advertisement
classification. Enable `native-rtc` to get `RtcService`, which takes the same
`matrix_sdk::Client` as the chat account. `discover_transport()` reads the
homeserver's MatrixRTC transport advertisement; `join()` connects the native
LiveKit/libwebrtc transport; `leave()` sends a membership leave and closes it.
The service is account scoped, so an account switch must retain or leave each
account's active calls explicitly.

`VoiceSpaceMarker::ENABLED.content()` makes the optional
`io.github.brk275.morpheus.voice` room-state content `{"v":1,"enabled":true}`
with an empty state key. `parse_voice_space_state_event()` validates the event
type, key, version, and boolean field. This hint only changes Morpheus's room
presentation; it does not grant room permissions or open a MatrixRTC slot.

The application must run `matrix-sdk-ui`'s `SyncService` with MSC4354 sticky
events enabled, and the joined room must have an open `m.rtc.slot`. The service
can open one through `open_slot()` if the user has the room power level. Current,
2025 sticky, and older room-state membership formats are explicit
`InteropMode` choices. They are not interchangeable calls.

Call `install_crypto_provider()` at process startup, before creating or logging
in a Matrix SDK client. The combined Matrix SDK and LiveKit dependency graph
enables two rustls providers; choosing AWS-LC explicitly prevents TLS startup
panics. The service also calls the helper as a fallback.

The optional device features connect native OS media to those frame streams:

- `device-audio`: CPAL 48 kHz mono/stereo microphone capture and speaker
  playback. After `join()`, `run_two_person_audio(room_id)` publishes the
  microphone, plays the first remote microphone stream, and stops on call end.
  Cancel the future to stop capture, then call `leave()` on the service.
  `MicrophoneCapture::devices()` and `AudioOutput::devices()` enumerate names.
- `device-camera`: Nokhwa camera capture. `CameraCapture::open(index)` and
  `next_frame()` return I420 frames for a `publish_camera()` publication;
  `cameras()` enumerates available devices.
- `device-screen`: XCap display and application-window enumeration and
  capture. `ScreenCapture::open()` selects a display; `open_window()` selects
  a window. `next_frame_sized(width, height)` preserves the source aspect ratio
  with black bars and returns a fixed-size I420 frame for `publish_screen()`;
  the caller paces captures. `next_frame()` returns the native source size.

These APIs report native device failures as `MediaError::Device` and reject
mismatched PCM and I420 frames before entering libwebrtc.
`local_device_support()` reports which adapters were compiled into the binary;
actual OS availability is checked when a device is opened. The audio loop is a
two-person proof. It does not mix group audio, select non-default audio devices,
resample unsupported device formats, or provide echo cancellation. Remote video
rendering, camera/screen pacing and controls, permission UI, macOS bundle usage
descriptions, and Windows/Linux runtime verification still need application
integration. Do not present this as a complete end-user call feature yet.

An opt-in macOS test captures real display or window pixels without saving an
image, and the local MatrixRTC fixture can publish that capture to a second
encrypted device. See [`tests/LOCAL_MATRIX_RTC.md`](../../tests/LOCAL_MATRIX_RTC.md)
for commands and the exact verified scope.

The Element dependency is **AGPL-3.0-only or a paid Element Commercial
License**. Distribution must comply with the chosen license. Element's RTC
repository is not published as crates.io packages at this tag; the native
feature pins its Git tag and the matching `matrix-rust-sdk` commit. The root
`Cargo.toml` carries the two patches required by that SDK revision.

The upstream tag's lockfile also fixes a set of LiveKit/libwebrtc versions.
Fresh semver resolution was verified to fail against newer 2026 releases, so
the root `Cargo.lock` must retain `livekit 0.7.48`, `livekit-api 0.5.3`,
`livekit-datatrack 0.1.9`, `livekit-protocol 0.7.9`, `libwebrtc 0.3.38`,
`webrtc-sys 0.3.35`, and `webrtc-sys-build 0.3.18`. Use `cargo ... --locked` in
CI to prevent an accidental upgrade from silently changing the native build.

```sh
cargo test --manifest-path crates/morpheus-rtc/Cargo.toml
cargo check --manifest-path crates/morpheus-rtc/Cargo.toml --features native-rtc
cargo check --manifest-path crates/morpheus-rtc/Cargo.toml --features native-devices
```
