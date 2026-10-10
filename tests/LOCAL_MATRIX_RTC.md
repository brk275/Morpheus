# Local MatrixRTC acceptance environment

This is a disposable, loopback-only call backend for testing Morpheus. It uses
the [official Synapse](https://github.com/element-hq/synapse) and
[MatrixRTC authorization service](https://github.com/element-hq/lk-jwt-service)
images, plus the [LiveKit](https://github.com/livekit/livekit) image or native
macOS binary. It creates no public account, email address, or hosted call
service.

## Start

```sh
python3 scripts/local_matrix_env.py up
python3 scripts/local_matrix_rtc.py up-native
python3 scripts/local_matrix_rtc.py probe
python3 scripts/local_matrix_rtc.py rtc-fixture
```

`up-native` requires `livekit-server` **1.13.9** on macOS (`brew install
livekit` at the time this fixture was verified). It runs the SFU as a local
process with signaling bound to `127.0.0.1:18011`, UDP media bound only to
`lo0` at `127.0.0.1:18012`, and TCP media disabled. The authorization and
federation bridge still run in Docker. `rtc-fixture` prints the path to a new
mode-0600 JSON file with disposable Alice/Bob accounts and an encrypted room;
use a **fresh** file for each cross-signing run. Keep its passwords private.

`up` remains the Docker-only alternative, primarily for Linux with working
host networking. Docker Desktop on this Mac could issue JWTs but its isolated
loopback prevented ICE checks from reaching the native client.

The Synapse A client API is `127.0.0.1:18008`; the authorization service is
`127.0.0.1:18010`; LiveKit signaling is `morpheus-rtc.localhost:18011`, and
native media uses UDP `127.0.0.1:18012`. In Docker-only mode, Docker publishes
UDP `127.0.0.1:18012` and TCP fallback `127.0.0.1:18013` on loopback. A private HTTPS bridge inside
the Docker network lets the authorization service verify Synapse A's OpenID
tokens through its federation API; that bridge has no host port. The service
uses a self-signed certificate bypass only for this local bridge.

`up` and `up-native` turn on Synapse's simplified sliding sync, MSC4354 sticky events,
MSC4143 transport discovery, and delayed events for this disposable server.
The script generates an API key and secret into mode-0600 files under the
mode-0700 temp test directory, verifies authenticated transport discovery, and
reuses the same values on later runs. Do not commit the temp directory or paste
its secrets into logs or chat. The pinned Docker digests in the script make
the container components reproducible. The Docker-only `up` refuses to start
below 8 GiB free disk.

`probe` signs in the disposable Alice account, checks authenticated RTC
discovery, obtains a Matrix OpenID token, exchanges it using the pinned native
RTC library's MSC4195 `/get_token` request shape, verifies that a nonempty
LiveKit JWT and the expected local SFU URL were returned, and checks that the
SFU HTTP listener is reachable. It prints no access token or JWT.

## Exercise the native client

Set `MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS` to the path printed by `rtc-fixture`,
then run:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-rtc --features native-rtc --test local_rtc -- --nocapture
```

The gated test signs in Alice and Bob as separate Matrix devices, bootstraps
and verifies each disposable user's own cross-signing identity, requires
sticky sliding sync, opens their encrypted room's standard call slot, joins
both through `RtcService`, checks each participant through Morpheus's roster
snapshot, publishes synthetic microphone PCM and red/green I420 camera and
screen tracks, checks that Bob
imports Alice's media key and decodes audio plus the expected 16:9 color pattern
from each video kind, then exercises leave and rejoin. It needs no audio, camera,
or screen-capture hardware. A
successful run logs out and deletes the temporary client stores; a failed run
leaves its store for diagnosis.

### Element Call browser interoperability (opt-in)

The separate `element_call_peer` test pairs Morpheus's native RTC engine with
an actual Element Call v0.26.1 browser peer. It uses a fresh disposable
`rtc-fixture` file for each attempt. The RTC fixture grants both users room
state power because Element Call's `compatibility` mode publishes
`org.matrix.msc3401.call.member` as room state. No external Matrix account or
email address is involved.

Check out the [Element Call v0.26.1 tag](https://github.com/element-hq/element-call/tree/v0.26.1)
into a separate temporary directory, install its locked dependencies with
`corepack pnpm install --frozen-lockfile`, and install Chromium with
`corepack pnpm exec playwright install chromium`. Set that checkout's
`public/config.json` for the local fixture:

```json
{
  "default_server_config": {
    "m.homeserver": {
      "base_url": "http://127.0.0.1:18008",
      "server_name": "morpheus-a.test"
    }
  },
  "matrix_rtc_mode": "compatibility",
  "livekit": { "livekit_service_url": "http://127.0.0.1:18010" }
}
```

Start its local HTTPS Vite server with `corepack pnpm dev:full --host
127.0.0.1 --port 5173 --strictPort`. The browser test connects to
`https://127.0.0.1:5173` and accepts only this development certificate. Set
the following environment variables in two terminals, using the same fresh
fixture path and private coordination directory in both:

```sh
export MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS=/path/to/fresh/rtc-fixture.json
export MORPHEUS_ELEMENT_CALL_CHECKOUT=/path/to/element-call-v0.26.1
export MORPHEUS_ELEMENT_INTEROP_MODE=room-state
export MORPHEUS_ELEMENT_PEER_GO_FILE=/path/to/private-coordination/go
export MORPHEUS_ELEMENT_PEER_DONE_FILE=/path/to/private-coordination/done
```

Create the coordination directory mode 0700 and remove old `go`/`done` files.
Run `node scripts/element_call_peer.mjs` first, then run the native side:

```sh
cargo test --locked -p morpheus-rtc --features native-rtc \
  --test element_call_peer -- --nocapture
```

The browser harness uses a canvas with red/green halves and a Web Audio tone
as synthetic camera and microphone tracks. The local macOS browser can enumerate
fake devices, but its hardware `getUserMedia` call does not resolve even on a
blank localhost page. The harness replaces only that browser API; Element Call's
real MatrixRTC signaling, encryption, and LiveKit code still carry the tracks.
The native side checks roster arrival, decoded audio/video, and departure. A
passing test establishes only those behaviors in this local fixture, not
hardware capture, public homeserver compatibility, or cross-platform behavior.

On 2026-10-10, Element Call v0.26.1 (commit `9f7c35c`, matrix-js-sdk 42.2.0)
passed the `compatibility`-mode run against a fresh local room: Morpheus saw
the browser in its roster, subscribed to its microphone and camera, decoded
nonzero PCM and three red/green I420 frames, and saw the browser leave. The
native test passed in 12.08 seconds and the browser exited successfully. The
browser's disposable device was bootstrapped and cross-signed before joining.
After the default slot was restored to lowercase, a second fresh-fixture run
also passed with the explicit `RoomState` helper in 16.48 seconds: encrypted
microphone PCM, red/green I420 video, and browser departure all succeeded.
Without that step, both peers saw each other and Morpheus subscribed to the
tracks, but neither audio nor video yielded decoded frames in 45 seconds.

The `compatibility` format uses an empty `call_id` in
`org.matrix.msc3401.call.member` state. The pinned Rust RTC core represents
that room session as lowercase `m.call#room`; Element Call's current room slot
is uppercase `m.call#ROOM`. `CallOptions::room_state_compatibility()` selects the
legacy carrier explicitly. Morpheus's current default remains lowercase for
the pinned RTC core's native room slot. The disposable fixture gives Bob room
state power so his compatibility membership does not fail with `M_FORBIDDEN`.

A native Alice/Bob current-mode regression with uppercase `m.call#ROOM` failed
to show Bob in Alice's roster. Restoring the pinned core's lowercase default
and opening that same slot passed encrypted synthetic audio, camera, screen,
leave, and rejoin on a fresh fixture in 19.04 seconds. The uppercase slot is
therefore not Morpheus's default until the current-format interop problem is
resolved.

Element Call's `matrix_2_0` mode has **not** passed here. In a separate run it
opened LiveKit but sent only a sticky membership tombstone, so Morpheus never
saw a browser join. Adding the unstable `org.matrix.msc4143.rtc.slot` room-state
event manually did not cure that failure. The local homeserver also returned
404 for the MSC4195 delegated-leave endpoint and the authorization service
returned 400 for its delegated-leave request. These are separate protocol
integration gaps; the successful legacy-format test does not verify current
sticky-event interoperability. Element Call's
[`openIDSFU.ts`](https://github.com/element-hq/element-call/blob/v0.26.1/src/livekit/openIDSFU.ts)
and the [matrix-js-sdk session manager](https://github.com/matrix-org/matrix-js-sdk/blob/v42.2.0/src/matrixrtc/MatrixRTCSessionManager.ts)
both use the uppercase room slot in the current mode.

A further 2026-10-10 loopback test isolated two wire differences. With
`Sticky2025`, the pinned Rust RTC engine normalizes Element Call's uppercase
`m.call#ROOM` membership to its internal lowercase `m.call#room` slot. When
only the uppercase slot was open, native join failed with “slot
`m.call#room` is not open.” Opening both spellings let Morpheus observe the
browser's joined sticky membership, but the browser's MatrixRTC roster stayed
empty and native camera subscription timed out. The browser's raw sticky store
contained both joined memberships and logged 24 missing media-key messages.
Rust `open_slot` wrote `m.rtc.slot` state; the Element Call build read only
`org.matrix.msc4143.rtc.slot`, so it reported no slot. Publishing an additional
uppercase unstable slot state changed the browser's slot status to `open`, but
its roster still stayed empty and camera subscription still timed out. This
alias experiment was removed from the interop harness; no current-mode
compatibility claim follows from it.

The app needs an explicit room-call compatibility choice or a trusted
deployment setting to select `RoomState`. Detecting legacy state alone cannot
choose the mode safely: the room may be empty when Morpheus starts the call,
legacy state memberships remain until their stated expiry, and changing modes
changes the membership carrier, key exchange, and SFU token endpoint.

Before joining, Alice also writes the shared
`io.github.brk275.morpheus.voice` state event with `{"v":1,"enabled":true}`;
Bob must read the enabled marker from the room's homeserver. Native sliding
sync does not reliably include this custom state type, so the reader performs
an explicit room-state GET. This checks the shared marker's wire path, not its
voice-space settings or navigation UI.

### Real macOS display and window capture

The separate capture test is opt-in because macOS may request Screen Recording
permission. It reads two frames, letterboxes them to 640×360 I420, checks that
they are nonblank and have valid planes and increasing timestamps, and saves no
pixels. It can use either the first display or the first application window:

```sh
MORPHEUS_REAL_CAPTURE=1 cargo test --locked -p morpheus-rtc --features device-screen --test real_capture -- --nocapture
MORPHEUS_REAL_CAPTURE=1 MORPHEUS_CAPTURE_SOURCE=window cargo test --locked -p morpheus-rtc --features device-screen --test real_capture -- --nocapture
```

Set `MORPHEUS_CAPTURE_INDEX` to select a different zero-based display or window.
The index is resolved from a fresh OS enumeration, so a disappearing or
reordered window can make the test fail. A permission denial, protected window,
blank source, or unavailable capture session is a test failure when opted in.

To additionally carry real captured pixels through native MatrixRTC, start the
local RTC fixture and create a **fresh** `rtc-fixture` file as above. Then run
the existing test with `device-screen` and
`MORPHEUS_RTC_REAL_SCREEN_CAPTURE=1`:

```sh
MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS=/path/to/fresh/rtc-fixture.json \
MORPHEUS_RTC_REAL_SCREEN_CAPTURE=1 \
cargo test --locked -p morpheus-rtc --features device-screen --test local_rtc -- --nocapture
```

Add `MORPHEUS_CAPTURE_SOURCE=window` to share a window rather than a display.
The test publishes 640×360 I420 frames to Alice's encrypted `ScreenShare`
track and requires Bob to decode nonblank pixels. It does not write image
content to files or logs. The capture test runs inside the current macOS
user's permission context and is not a packaged-app permission test.

For a bounded connectivity check, set `MORPHEUS_RTC_JOIN_TIMEOUT_SECS=12`.
The default is 60 seconds per join. The test exits with a failure when a join
times out.

### Current macOS result

Authenticated discovery and JWT issuance pass. The native call reaches the
LiveKit signaling server. The final macOS executable must link with `-ObjC`:
without it, LiveKit's static WebRTC library aborts at
`+[NSString stringForAbslStringView:]` during peer-connection initialization.
The app and RTC test build scripts supply that flag for their final macOS
executables, following the [upstream LiveKit crash report](https://github.com/livekit/rust-sdks/issues/795)
and [confirmed fix](https://github.com/foxglove/foxglove-sdk/pull/944).

The native loopback SFU resolved the Docker Desktop ICE problem: both clients
established UDP connections and appeared in the participant roster. An earlier
test on unsigned, fresh devices then stalled on media-key import. The pinned
RTC SDK uses Matrix's identity-based strategy for Olm key delivery, which
requires a verified own cross-signing identity and signed receiving devices.
The gated test now bootstraps each disposable identity, refreshes its device
keys, and signs its own device if needed. On 2026-10-09, the fresh-fixture test
passed in 17.07 seconds: Bob imported Alice's encrypted media key, subscribed
to her synthetic microphone, decoded nonzero audio, left, and rejoined the
same call. An expanded fresh-fixture run passed in 18.27 seconds: Bob also
received red/green synthetic I420 camera and `ScreenShare` frames at 640×360.
On 2026-10-10, the separate real-capture test passed for a display and a
window. Two subsequent fresh-fixture MatrixRTC runs also passed: Bob decoded
nonblank encrypted 640×360 display pixels (20.03 seconds) and window pixels
(18.32 seconds), then both tests completed participant leave and rejoin. The
display path passed once more in 19.83 seconds after narrowing the decoded
nonblank check to visible rows. A randomly generated test password beginning
with `-` briefly broke `rtc-fixture` registration; the registration script now
passes it in `--password=value` form, and a fresh fixture succeeded. The
original red/green test still uses generated pixels unless
`MORPHEUS_RTC_REAL_SCREEN_CAPTURE=1` is set.

Another fresh-fixture run passed in 18.16 seconds after the shared voice-space
marker and Morpheus roster assertions were added. Its first attempt exposed
the missing custom-state event in native sliding sync; the explicit state GET
fixed that lookup. The call stage now compiles with a participant list and
remote video tile, but live received-track playback in the app remains
unverified.

Morpheus rejects an encrypted call before joining when the current
device lacks the required verification; the app still needs a clear
verification/recovery action for real accounts.

This local test does not establish federation between the two Synapse
homeservers. The authorization service runs in its legacy direct mode for the
client library's `/get_token` flow; production MatrixRTC service registration,
federated calls, delegated delayed leave, current-format Element Call interoperability,
automatic network reconnect, live video playback in the Morpheus call stage,
OS-specific camera capture, packaged-app window/display sharing with permission UI, and
Linux/Windows call behavior remain separate release gates. The local TLS
bypass and loopback DNS alias are test-only configuration.

## Stop

```sh
python3 scripts/local_matrix_rtc.py down
python3 scripts/local_matrix_env.py down
```

Stop RTC first so the shared Docker network can be removed. The private
credential/config files and Synapse volumes remain for repeat runs. The RTC
script never stops Synapse B or deletes its data.
