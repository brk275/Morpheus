# Native video and screen sharing feasibility

Status: **synthetic Slint rendering and real macOS display/window capture through encrypted MatrixRTC passed separately; a remote call-stage tile now compiles, but live received-track playback in the app remains unverified**. This note describes the pinned dependencies in this workspace, not a completed feature.

## Isolated synthetic Slint image proof

[`experiments/native-video-image`](../experiments/native-video-image/README.md) is a standalone, unreleased crate that uses the pinned RTC `VideoFrame` type and Slint 1.18.1. It validates I420 dimensions and padded plane strides, converts limited-range BT.601 to RGB8 on a worker, applies the frame's rotation, and sends a one-frame bounded `SharedPixelBuffer` to a native Slint `Image`. It uses moving synthetic color bars; no camera, screen capture, network, or call is involved.

On macOS, `cargo check --manifest-path experiments/native-video-image/Cargo.toml` completed, and `cargo test` passed all three conversion tests (padded planes and color range, clockwise quarter-turn, and invalid-input rejection). The standalone native window then displayed colored bars in its Slint `Image`; the visible frame counter advanced from 75 at 90° to 146 at 270°, and the bars changed orientation accordingly. This verifies local synthetic image updates on macOS. This proof alone does **not** consume a remote MatrixRTC track or establish sustained GPU upload performance or Windows/Linux behavior. The RTC frame type has no color-space metadata, so the proof's BT.601 conversion needs validation against actual decoded media before production use.

## Local synthetic MatrixRTC video transport proof

The gated macOS test in [`crates/morpheus-rtc/tests/local_rtc.rs`](../crates/morpheus-rtc/tests/local_rtc.rs) passed on a fresh disposable encrypted Alice/Bob call in 18.27 seconds. Alice published red/green 640×360 I420 frames first as `Camera`, then as `ScreenShare`; Bob received each corresponding remote track and decoded the expected Y/U/V colors at 640×360. The same run imported Alice's media key, decoded synthetic microphone audio, and passed Bob's leave/rejoin. The `ScreenShare` track used generated pixels, **not** an OS display or window capture. Neither received track was rendered in the Morpheus call stage. The local fixture does not establish Element Call interoperability, call UI integration, or Windows/Linux behavior.

A follow-up macOS proof captured native display and window images, scaled and letterboxed them to 640×360 I420, and published each as an encrypted screen track in separate fresh disposable-device calls. Bob decoded nonblank pixels in both runs (20.03 and 18.32 seconds). The direct capture test also checked two consecutive frames and increasing timestamps without saving images. This proves the development capture-to-transport path on macOS; it does not test the packaged app's permission flow, sustained capture, or call-stage rendering.

## Morpheus call-stage integration

The native-call app now subscribes to MatrixRTC participant changes and remote
video frames. Its call stage shows a participant list and a single remote tile.
When several streams are available, it chooses a screen share before a camera.
The video worker retains only the newest decoded frame, converts validated
I420 planes to RGB at about 15 frames/s, limits output to 2.07 million pixels
with a 1920-pixel edge limit,
and uses a call-generation token to clear stale frames on leave or account
switch. The focused macOS app tests cover padded planes, rotation, invalid
dimensions, and stale-call cleanup; the native-call app compiles. These are
code and unit-test checks. The tile has not yet displayed a frame received
from a live call, and its participant list has not passed a native UI
click-through with two connected accounts.

## Existing interfaces

| Boundary | Pinned API and current Morpheus path |
| --- | --- |
| Receive | `matrix-rust-rtc v0.5.0-rc.2`: `LiveKitCall::subscribe_call_events()` announces `StreamStarted`/`StreamStopped`; `remote_track(member_id, MediaStreamKind::Camera or ScreenShare)?.video_frames()` yields owned `VideoFrame`s. `engine().subscribe_tiles()` supplies a ranked camera/screen roster. See `matrix-rtc-call-sdk/src/livekit/call.rs`, `matrix-rtc-transport/src/connection.rs` and `frame.rs` in the pinned checkout. |
| Frame format | `VideoFrame` holds I420 planes, explicit plane strides, a `VideoRotation`, and a capture timestamp. Morpheus reexports this type from `crates/morpheus-rtc/src/lib.rs`. A renderer must respect strides and rotate frames. |
| Native Slint display | The app now uses Slint 1.18.1 `SharedPixelBuffer<Rgb8Pixel>`, `Image::from_rgb8`, and `Weak<AppWindow>::upgrade_in_event_loop` for its remote tile. The `Image::try_from<wgpu::Texture>` path remains future work and would need a selected WGPU renderer and rendering notifier. The pixel-buffer path has compiled and passed converter tests, but live RTC playback in the app has not been viewed. |
| Publish | `RtcService::publish_camera()` / `publish_screen()` return `Publication`; `Publication::push_video()` checks I420 planes and exact publication dimensions before forwarding to libwebrtc. `CameraCapture::next_frame()` and `ScreenCapture::next_frame()` already produce I420 frames. See `crates/morpheus-rtc/src/media.rs` and `video_device.rs`. |
| Capture | Nokhwa 0.10.11 opens a native camera. XCap 0.9.8 provides `Monitor::all()`, `Monitor::capture_image()`, `Window::all()`, `Window::capture_image()`, and `Monitor::video_recorder() -> (VideoRecorder, Receiver<Frame>)`. Morpheus wraps display and window still capture with fixed-size letterboxed I420 output; streaming capture remains open work. |

## Integration steps and next proof

1. **Implemented in code, live UI pending:** The call stage has a remote
   `image` property, placeholder, participant list, and account/call-scoped
   clearing. A connected two-device UI run still needs to exercise it.
2. **Implemented in code, live UI pending:** The call worker subscribes to
   roster changes, reconciles remote camera/screen tracks, consumes
   `video_frames()`, and retains only the newest frame. Track replacement and
   participant departure still need a live UI check.
3. **Unit tested, live UI pending:** A worker converts I420 to RGB after
   validating dimensions, strides, and planes; it applies rotation and caps
   output resolution. A bounded frame cadence forwards the image through
   `upgrade_in_event_loop`, guarded by a call-generation token. Check visible
   playback and sustained performance with a real remote track.
4. In the same call worker, add explicit camera/share commands. Open the chosen capture source on a blocking worker, capture the first frame, publish a matching camera or screen stream, and pace `push_video(frame)` at 5–10 frames/s initially. `ScreenCapture::next_frame_sized()` keeps the published screen dimensions stable when a source resizes. `Publication::set_muted()` and `unpublish()` provide controls. The microphone stays published independently, so the first screen share is picture plus microphone audio.
5. The private loopback MatrixRTC stack confirms synthetic camera and screen-track frames and real macOS display/window frames reach Bob's `video_frames()`. Next, view a received frame in the Morpheus call stage and inspect packaged-app capture permissions on macOS. Repeat capture, receive, leave/rejoin, and participant departure on Windows x64, Linux X11, and Linux Wayland before beta acceptance. Test a real Element Call peer separately for protocol interoperation.

## Platform and performance risks

- **Current RTC baseline:** the upstream macOS `-ObjC` link workaround and a native loopback LiveKit instance resolved the local Docker Desktop ICE problem. Gated macOS tests connected two disposable participants over UDP, delivered synthetic microphone/camera plus real display/window screen-track frames in encrypted calls, and passed leave/rejoin. The call-stage tile now compiles, but no received video has been viewed in Morpheus. See [the RTC runbook](../tests/LOCAL_MATRIX_RTC.md) and [verification record](VERIFICATION.md).
- **Wayland capture:** XCap's `capture_image()` can fall back to a Screenshot portal PNG per frame. That path is unsuitable for continuous sharing. XCap's `video_recorder()` uses the ScreenCast portal and PipeWire on Wayland; its stream behavior and permissions remain unverified in Morpheus. X11 still capture and recorder paths also need profiling.
- **Permissions:** the macOS package has microphone, camera, and screen-capture usage strings in `packaging/macos/Info.plist`. Packaged-app consent, revoked permission, source-picker behavior, and Windows/Linux equivalents need hands-on checks. Never start capture while merely probing homeserver support.
- **Frame budget:** I420-to-RGB conversion plus Slint texture upload copies each frame. At 1920×1080 RGB, one output frame is about 6 MiB; 30 fps exceeds 180 MiB/s before extra copies. Bound resolution/frame rate and drop stale frames. A WGPU texture path can follow only after renderer compatibility and resource-lifetime tests.
- **Source selection:** `ScreenCapture` exposes displays and windows by enumeration index. Add stable identifiers and an explicit picker before shipping; a source can disappear between enumeration and opening.
- **Release gate:** synthetic native rendering, encrypted real macOS capture transport, and call-stage code/unit checks have been verified separately. Received-track playback in the call UI, packaged-app capture permissions, participant departure updates, sustained rendering performance, Element Call interoperability, and cross-platform capture remain **unverified**.
