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
to identify remote media. `open_slot()` and both modern `join()` modes reject an
unencrypted room before publishing a call. Both also require advertised sticky
event support; explicit legacy room-state joins retain their separate policy.

`CallOptions::element_call_matrix_2_0()` selects the pinned `Sticky2025` adapter
for Element Call's Matrix 2.0 setting. It keeps the internal lowercase
`m.call#room` slot and lets the adapter translate the outgoing membership and
authorization slot together. The app exposes it as an explicit account/room
choice; the default remains `Current`. No protocol negotiation, automatic
fallback, or room-marker override is performed. A room must still have the
lowercase slot opened by a member with sufficient power. The stock RC test now
uses this same constructor; compilation and new production UI/runtime
acceptance remain pending. See [the peer scope](../../docs/ELEMENT_CALL_ACCEPTANCE.md).

Call `install_crypto_provider()` at process startup, before creating or logging
in a Matrix SDK client. The combined Matrix SDK and LiveKit dependency graph
enables two rustls providers; choosing AWS-LC explicitly prevents TLS startup
panics. The service also calls the helper as a fallback.

## Ringing implementation boundary

The app has a separate **Call and notify** action for Standard and Element Call
Matrix 2.0. Existing Join and voice-space entry remain quiet; Legacy cannot
send invitations through this action. This outgoing path still needs hosted
compilation, live encrypted-peer and UI acceptance before it is considered
usable. Incoming ringing is not connected to the app. The native `ringing`
module provides bounded parsing of the pinned, deployed
`m.rtc.notification` format (flat, nested, and consistent duplicated fields).
It checks explicit recipients/room-mention permission, caps lifetime at two
minutes, and correlates a referenced membership with the same live logical
participation across event refreshes. The caller must independently establish
decryption authenticity, joined room, open slot and current participation;
parsing alone never authorizes a ring.

An outgoing attempt belongs to the captured account selection and call
generation, checks the live local/shared voice designation and current roster,
and requires a verified encrypted membership from this user, device and logical
join. It references that membership's actual event ID. Direct-chat targets must
match both current joined members and direct-chat metadata; ordinary rooms
require room-mention permission. The narrow `NotificationDialect` adapter uses
the pinned RTC library's wire format without a dependency upgrade or vendor
patch. A room becoming occupied suppresses the invitation; Matrix cannot make
the final roster check and send atomic.

The worker attempts one transaction, keeps a successful server receipt, and
reports ambiguous completion without offering a retry. Server acceptance does
not establish that any recipient rang. Notification lifetime is 30 seconds; sticky
retention is 60 seconds, following the pinned library's
`notification_sticky_duration_ms` rule. No acknowledgement extension is
implemented. Leaving clears local invitation status and stops media before
a bounded best-effort retraction using the original account's receipt. An
accepted request with no returned receipt cannot be retracted by this path;
the original expiry remains the fallback. Invitation failure leaves the call
usable.

The remaining incoming controller must use the account-selection cancellation token,
deduplicate events, stop on expiry/departure/other-device acceptance or decline,
keep voice spaces quiet, and request no capture device before Accept. The pinned SDK drops keyless events from its
sticky map while the pinned notification builder omits a sticky key. A local
`matrix-sdk-base` patch adds a separate bounded receipt cache that retains actual
SDK decryption metadata, original deadlines and room-lifetime generations. It
handles delayed keys and redactions without inventing a sticky key or changing
the existing membership map. Overflow invalidates the cache; receivers must
reject invalidation, lag, changed generations and expired receipts.

The hosted invitation cases now require Bob to discover the authenticated
notification through sync before receiving Alice's event ID, then verify GET,
the referenced participation, and sync-cache removal on redaction before expiry.
[Run 38127191100](https://github.com/brk275/Morpheus/actions/runs/38127191100)
passed those receipt checks for all four Current/Sticky2025 DM/room cases at
`139460d`. The cache is not persisted;
fresh-sync/offline replay, delayed-key integration, sound, incoming call UI and
stock Element receipt remain explicit acceptance work. A receipt alone grants
no permission to start capture or join a call.
The newer `m.rtc.invite` proposal requires a separate protocol migration.

### Passive signalling observation

`PassiveCallObserver` owns a cancellable room feed without joining, publishing
membership, connecting media or opening capture devices. Its open/snapshot
operations have deadlines and its snapshots enforce participant/event limits.
Fresh snapshots validate the encrypted room-wide slot and cross-check current roster
entries against decrypted SDK sticky events, including device, logical join,
event identity and the explicitly selected Current/Sticky2025 dialect. Dropping
the observer detaches its feed. Its result is not a device-trust claim: the
caller must independently require SDK verification and retain its revocable
account selection.

The four hosted invitation cases now also require Bob's passive observer to
see only Alice's authenticated current membership, then observe its removal
after Alice leaves and before natural expiry. These checks reuse the existing
fixtures and time budget. On [run 38129843715](https://github.com/brk275/Morpheus/actions/runs/38129843715)
all four cases failed before notification receipt in the combined membership/
observer stage, despite passing the five observer unit tests. The next candidate
separates open/roster/leave stages and reports fixed failure categories; live
acceptance remains open. This adapter does not implement an incoming-call
banner, Accept/Dismiss actions or audible ringing.

The app's incoming admission boundary additionally checks current local device
trust against the original encryption/signing keys, strict push/ignore policy,
the referenced membership and this passive roster. It keeps one bounded replay
ledger across account switches, captures the semantic deadline before network
work, and permanently withdraws an offer on failed or cancelled revalidation. The four live
invitation cases exercise this boundary before and after receipt redaction;
hosted validation of the new admission path is pending. UI change subscriptions,
Accept with cancellation through joining, sound and offline replay are still
separate work. Admission itself neither joins a call nor enables capture.

## Native devices

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
