# Stock Element Call protocol acceptance

The Linux Matrix acceptance harness includes an opt-in Rust peer test paired
with the **unmodified official Element Call v0.28.0-rc.1 image**. This is a
release candidate, not the latest stable release. The reverse-audio extension
below has not been executed. Earlier eleven-fact results do not establish its
new twelve-fact acceptance requirement.

## Why this peer

The recorded stable **v0.26.1** `matrix_2_0` attempt remains failed: encrypted
sticky events reached the browser's update callback before decryption, and
the live roster remained empty afterward. The explicit `refreshRoom`
experiment established only media transport. See the historical results in
[the RTC runbook](../tests/LOCAL_MATRIX_RTC.md) and
[verification](VERIFICATION.md).

The RC's SDK contains upstream fixes absent from the stable peer:

- [9ba30b7 / #5544](https://github.com/matrix-org/matrix-js-sdk/commit/9ba30b7550a7628268be0cf059600358605598be)
  defers sticky-event indexing until decryption and emits the update afterward.
- [#5584](https://github.com/matrix-org/matrix-js-sdk/pull/5584) also handles
  events that already failed decryption before entering the sticky store.
  [The regression tests](https://github.com/matrix-org/matrix-js-sdk/blob/fa4772a4ddd335a5a8bfbf8bb21df49b70f7a999/spec/unit/models/room-sticky-events.spec.ts)
  cover late keys and the later update notification.
- [#5523](https://github.com/matrix-org/matrix-js-sdk/pull/5523) prevents
  overlapping roster recalculations from installing stale results.

These are reasons to test the RC. They do not establish new historical-key
sharing or a passing Morpheus call.

## Immutable fixture inputs

| Input | Pin |
| --- | --- |
| Element Call | `v0.28.0-rc.1` |
| Element Call source | `9b3c3569ecefbb79cf527ea24e3756ba50553a8f` |
| Official image | `ghcr.io/element-hq/element-call:v0.28.0-rc.1@sha256:6d83fe15c425e1139edafdffe2b2073256608dcafdc554da5bf840de496891fe` |
| Linux amd64 image manifest | `sha256:910de310c07f0e646963b6e3331f99a0f7b403d6b4153ecd6dceba88d160585e` |
| Bundled Matrix JS SDK | `fa4772a4ddd335a5a8bfbf8bb21df49b70f7a999` |
| Fixture browser tooling | `playwright-core` **1.62.1**, exact npm integrity in `scripts/element-peer/package-lock.json` |

Registry manifest/config reads confirmed the image's source revision. The
fixture checks the pulled image's immutable digest and OCI version, revision
and source labels before starting it. Only deployment configuration changes:
the disposable homeserver/service URLs, `matrix_rtc_mode`, and nginx's
loopback listener/private log paths. Application assets and SDK code remain
unchanged. Playwright is installed in the private external fixture directory;
it changes no Morpheus production dependency.

## Assertions and limits

The run is labelled **`element-call-rc-sticky2025-presynced`**. A fresh
`rtc-fixture` provides disposable users and an encrypted room. The browser
signs in through Element Call's normal UI, then uses the stock SDK to bootstrap
and cross-sign that disposable account. This explicit fixture identity setup
is required by native media-key trust checks; it is not an end-user
verification-UI test.

Before any call membership is published, the browser waits for its own device
verification and room sync, then signals readiness. The native peer signs in,
syncs and bootstraps its own device before joining. This **pre-synced baseline
does not prove fresh-device late joining** or historical Megolm key recovery.

A pass requires all of the following:

- Both the browser's existing session-manager object and its call-view object
  show exactly the two expected live memberships; the rendered UI shows two
  participant tiles. The observer reads the existing SDK map; it does not call
  a getter that could construct a missing session.
- Morpheus observes the browser participant, decrypts nonzero microphone PCM,
  and decodes its red/green camera pattern.
- Element Call decodes Morpheus's distinct white/black video pattern. Three
  consecutive pixel samples must match; its local red/green preview cannot
  satisfy this assertion.
- Element Call decodes Morpheus's 997 Hz microphone tone, distinct from the
  browser's 440 Hz file-backed microphone. The browser reads the pinned RC's
  existing React `AudioTrack` binding without changing it. The Matrix user,
  device, join and backend identity must match the native publication's private
  lease marker. The participant must be remote; its microphone publication map,
  SID, attached audio element and decoded `MediaStreamTrack` must agree. Only a
  clone of that exact remote track feeds the analyser. Three consecutive
  samples require nonzero RMS, a 997 Hz spectral peak and dominant native-tone
  energy; the browser's own capture cannot satisfy `browser_audio`.
- The browser leaves through the normal UI and Morpheus observes departure.
- Both child processes succeed, the exact named native test runs once, and all
  fixed evidence markers exist. Zero, skipped, ignored or other tests cannot
  establish a pass.

Chromium uses file-backed synthetic capture devices. There is no
`getUserMedia` replacement, browser refresh, `refreshRoom` call, membership
injection or SDK patch. Any inherited `MORPHEUS_ELEMENT_DIAG_REFRESH` value is
rejected before services or children start.

The new native-to-browser **audio** assertion is required but unexecuted. The
observer fails closed if the pinned RC's binding cannot be resolved or changes
during sampling; it never falls back to an arbitrary audio element or local
microphone. This extends synthetic transport acceptance, not physical speaker
output. Reconnect, rotation, screen sharing, larger groups, late-join behavior,
physical devices and macOS/Windows runtime interoperability remain separate
acceptance gates.

### Protocol distinction

The RC's `matrix_2_0` still uses uppercase `m.call#ROOM`, explicit member
user/device IDs and the older media-key envelope, although its transport field
has been modernized. It matches the pinned Rust core's **`Sticky2025`**
compatibility dialect, not its 2026 **`Current`** dialect. The test keeps the
Rust internal lowercase slot and uses that dialect's existing wire conversion.

Morpheus's production default remains `Current` with lowercase `m.call#room`.
The app now implements an explicit **Element Call Matrix 2.0** choice for one
account and room; it uses the same `CallOptions::element_call_matrix_2_0()`
constructor as this test. It does not silently select or fall back to that mode.
The existing legacy room-state choice is preserved. Both modern modes require
encrypted rooms, sticky-event support and the existing service/device checks;
opening the lowercase slot remains an explicit action requiring room power.

This source integration has not yet passed native compilation, targeted tests,
or desktop interaction acceptance. The fixture calls `RtcService` directly;
it does not verify the app's configuration, selector, controller or physical
devices. Even a successful RC test does **not** close production-default
interoperability or the stable v0.26.1 Matrix 2.0 gate. The previously observed
legacy `RoomState` success is also a different protocol path.

## Privacy and lifecycle

The shared Linux harness starts the existing loopback Synapse/RTC services.
The peer helper adds one nonce-labelled, digest-pinned container, bound only
to `127.0.0.1` using Linux host networking. The helper records its full container
ID before start, checks ownership before removal, and recovers interrupted
creation using its random ownership label. It never removes shared services
or unowned containers.

Credentials, SDK stores, raw logs and synthetic capture files stay outside the
checkout under directories mode 0700 and files mode 0600. Raw logs are scanned
before only fixed stage/failure names and allowlisted booleans are emitted.
There are no screenshot, browser trace, media or raw-log uploads. The new audio
observer retains the existing outer deadlines. Binding, context start and
sampling share a 45-second deadline. Context closing can extend beyond it and
remains governed by the outer process deadline. The observer inspects at most
32 audio elements and 32 React ancestors per element. Its source/analyser
nodes disconnect, its clone stops and its context
closes before the reverse-audio fact is emitted. The stock receive track is
never stopped or redirected. The bounded native PCM task is stopped, awaited
and unpublished on success. Failure or cancellation requests abort of its owned
task; dropping an active publication mutes it. Runtime and owned-process
teardown remain the final failure boundary. No PCM is written or emitted by
the new path. Subprocesses
have deadlines; their group leaders remain unreaped until the owned group is
killed. A QA-only Linux PID namespace also contains detached browser/native
sessions. Trusted absolute `sudo`, `unshare` and `setpriv` binaries create it;
the original runner UID, GID and supplementary groups are restored, all
capabilities are dropped, and `no-new-privs` is set before repository Python,
Cargo or browser code executes. The isolated entry checks it is PID 1 in the
new namespace with the expected identity and zero capabilities. Environment
restoration uses a private file, never credential-bearing command arguments.

Before launching a peer, a mandatory hosted regression uses the same non-TTY
sudo invocation, creates a detached child, kills the unreaped outer wrapper's
group, and checks that both namespace processes disappear. A stable init
pidfd is retained only for cleanup if that assertion fails; using it cannot
satisfy the assertion. Its first hosted result is pending. System browser
dependencies install in the trusted CI apt step before privilege dropping;
the contained Playwright command only downloads the pinned browser.

Host Docker containers are outside that PID namespace. Both the inner runner
and outer acceptance harness still attempt nonce-verified owned container
cleanup on failure, including the outer timeout path.

The harness runs within the existing free standard Linux acceptance job; no
extra workflow or paid runner is introduced. Offline tests exercise pinning,
ownership, evidence rejection and redaction. Native compilation and the real
peer run must be evaluated on the affected hosted candidate before recording
any acceptance result.
