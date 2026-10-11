# Stock Element Call protocol acceptance

The Linux Matrix acceptance harness includes an opt-in Rust peer test paired
with the **unmodified official Element Call v0.28.0-rc.1 image**. This is a
release candidate, not the latest stable release. Its first hosted result is
pending; adding the harness is not interoperability evidence.

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
- The browser leaves through the normal UI and Morpheus observes departure.
- Both child processes succeed, the exact named native test runs once, and all
  fixed evidence markers exist. Zero, skipped, ignored or other tests cannot
  establish a pass.

Chromium uses file-backed synthetic capture devices. There is no
`getUserMedia` replacement, browser refresh, `refreshRoom` call, membership
injection or SDK patch. Any inherited `MORPHEUS_ELEMENT_DIAG_REFRESH` value is
rejected before services or children start.

The native-to-browser **audio** path is not asserted. Reconnect, rotation,
screen sharing, larger groups, late-join behavior, physical devices and
macOS/Windows runtime interoperability remain separate acceptance gates.

### Protocol distinction

The RC's `matrix_2_0` still uses uppercase `m.call#ROOM`, explicit member
user/device IDs and the older media-key envelope, although its transport field
has been modernized. It matches the pinned Rust core's **`Sticky2025`**
compatibility dialect, not its 2026 **`Current`** dialect. The test keeps the
Rust internal lowercase slot and uses that dialect's existing wire conversion.

Morpheus's production default remains `Current` with lowercase `m.call#room`.
Even a successful RC test does **not** close production-default interoperability
or the stable v0.26.1 Matrix 2.0 gate. No app call-setting/default change is part
of this fixture. The previously observed legacy `RoomState` success is also a
different protocol path.

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
There are no screenshot, browser trace, media or raw-log uploads. Subprocesses
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
