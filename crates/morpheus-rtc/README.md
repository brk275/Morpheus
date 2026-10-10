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
`open_slot()` and `join()` check for a compatible advertised LiveKit transport
before device-verification checks; an unsupported homeserver returns
`NoLiveKitTransport`, and `open_slot()` writes no slot state. Chat remains
available there. The separate Synapse B service-gate acceptance test checks
this against authoritative room state and an encrypted chat send.
The current format needs an encrypted room: its cleartext membership does not
carry an attributable sender device, which the pinned LiveKit transport needs
to identify remote media. `open_slot()` and a default `join()` reject an
unencrypted room before publishing a call; explicit legacy joins remain a
separate compatibility path.

Call `install_crypto_provider()` at process startup, before creating or logging
in a Matrix SDK client. The combined Matrix SDK and LiveKit dependency graph
enables two rustls providers; choosing AWS-LC explicitly prevents TLS startup
panics. The service also calls the helper as a fallback.

The optional device features connect native OS media to those frame streams:

- `device-audio`: CPAL 48 kHz mono/stereo microphone capture and speaker
  playback. After `join()`, `run_audio(room_id)` publishes the microphone,
  mixes subscribed remote microphones with bounded per-participant queues,
  and stops on call end. Cancel the future to stop capture, then call `leave()`
  on the service. `MicrophoneCapture::devices()` and
  `AudioOutput::devices()` enumerate names.
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
`RtcService::audio_controls(room_id)` creates a handle bound to the current
account, device, room, and call membership. Pass it to
`run_audio_controlled(room_id, &controls)` and retain a clone for UI commands.
`set_microphone_muted(true)` mutes the native publication immediately;
unmuting waits until buffered microphone frames are discarded.
`set_deafened(true)` silences only local speaker playback and clears buffered
remote PCM; it does not tell peers to stop sending. `applied_state()` and its
watch receiver report the actual state. Default `run_audio()` still starts
with an open microphone and speaker, as before. Controls and camera/screen
`Publication` handles are revoked when the call leaves or the service drops;
an old handle cannot affect a later join to the same room. A publication also
exposes its call and account/device identity, and its `set_muted()` and
`unpublish()` methods forward to the native transport. Dropping a live
publication mutes it as a cancellation safeguard; call `unpublish()` to remove
its stream from peers' rosters.

`local_device_support()` reports which adapters were compiled into the binary;
actual OS availability is checked when a device is opened. The audio loop is a
native proof. It does not select non-default audio devices, resample unsupported
device formats, or provide echo cancellation. Mute and deafen controls are
wired to the call stage with applied-state feedback, but live device acceptance
remains open. Camera/screen pacing and controls, permission UI, and
Windows/Linux runtime verification still need application integration.
Do not present this as a complete end-user call feature yet.

An opt-in macOS test captures real display or window pixels without saving an
image, and the local MatrixRTC fixture can publish that capture to a second
encrypted device. See [`tests/LOCAL_MATRIX_RTC.md`](../../tests/LOCAL_MATRIX_RTC.md)
for commands and the exact verified scope.

The disposable two-device RTC test also waits for a new local media-key index
after a peer leaves, then requires a fresh peer join to decode new encrypted
camera frames. This exercises a member-departure key rotation. The same test
forces LiveKit signalling loss, observes a degraded-to-healthy transition, and
decodes newly published media after automatic resume. A full network outage
or SFU restart remains a separate check.

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
