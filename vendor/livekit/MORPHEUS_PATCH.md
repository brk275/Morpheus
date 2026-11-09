# Morpheus LiveKit settings ordering

Same-version livekit0.7.48 path patch. Original registry archive SHA256:
3db88772d10f8ef1ab43a0aa0634a1d4807d0567b65665274af3e084589dbec6.
Its recorded source is livekit/rust-sdks commit
af369aae0fe074762ef0a2051529e7ec63045042, path livekit. The original76
regular archive members are retained;68 are exact. All eight changed original runtime
files have prominent modification notices and exact .upstream preimages.
UPSTREAM_FILES.sha256 always records original archive bytes. Cargo.toml and
Cargo.toml.orig preserve the normalized and original declarations; an extra
Cargo.toml.upstream copy and exact upstream workspace manifest are included.
LICENSE and NOTICE are restored from the exact same official SDK commit root.

The added RemoteParticipant::update_track_settings selects only that
participant's subscribed publication by SID, retains its actual track,
changes local enabled/dimension state without the three detached callbacks,
and awaits at most one complete TrackSetting through the existing engine.
Morpheus's sole constraints setter seam now awaits this method. Original
synchronous SDK methods remain unchanged for other callers.

Dimensions always send disabled+size. Active simulcast Quality sends the
requested limit with zero wire dimensions. Unchanged Auto remains a no-op;
otherwise Auto and inactive/non-simulcast Quality retain the original
enabled-transition dimension behavior. No forced High for a lone Low layer,
new subscription default, clock, capture, key, E2EE, auth or URL policy.
No publication, map or callback lock is held over the await.

Completion is existing SignalClient send/queue admission, not an SFU ACK or
media receipt. Admitted operations may drain after removal. The engine
captures its original current session before awaiting; full reconnect's
old-session discard and current-constraints reapplication remain intact.
Existing SyncState disabled-track replay limitations and the separately
reviewed SignalInner lock-order correction remain separate composition
requirements; no global signaling or durable-delivery guarantee is made.

The package is excluded from workspace membership and used through the
existing normal native app dependency graph. All original features, build.rs,
dev tables and12 test targets remain byte-exact. Root1174lock records and
all edges are exact except livekit's removed registry source/checksum.
No test/dev edge or new selector is added. Do not select blanket upstream
package tests using this application lock graph.

The existing actual encrypted RTC outage/recovery case is required runtime
regression: decoded initial media, owned SFU stop/restart, new keys/join/media,
no duplicate ring and original cleanup/deadlines remain unchanged. This source
fix does not prove the historical initial_media failure's cause or a pass.
No native prebuilt source attribution, license-completeness or distribution
readiness is claimed. This recreated packet has new identities; lost earlier
temporary packets are not represented as recovered byte-for-byte evidence.

## Plain video encoding shape

The ordinary `simulcast: false` path now constructs one RTP encoding without
an RID. Previously it reused the simulcast preset helper, which always gave
that sole encoding RID `q` and advertised it as a Low simulcast layer. This
contradicted the transport's existing small-camera plain-stream selection.
The plain encoding keeps the same bitrate, frame rate, scale and active
state. Existing layer derivation consequently advertises the one full stream;
no receiver quality is forced and no additional stream is enabled. Explicit
SVC/scalability modes and true simulcast paths are unchanged.

The production helper contract is checked through matrix-rtc-livekit library
tests for small/large/portrait cameras, screen sharing and true simulcast.
The original encrypted media/recovery case remains the runtime check. The
historical first-frame failure is not attributed to this defect before that
case passes. E2EE, timestamps, capture and acceptance requirements are unchanged.

## Encrypted remote-track admission and lifetime

Adapt the public rc.7 clear-track refusal (matrix-rust-rtc revision
5b8e73539878e40fa5bd88d637eb8eb2b6b2f0d0) at the private synchronous SDK
receiver setup boundary. A configured E2EE room refuses an explicit None
declaration before native preparation or RoomEvent export. Gcm/Custom keep
normal cryptor setup; ordinary open SDK rooms without E2EE options retain
packet-trailer preparation and event export without a frame cryptor.

A private gate serializes receiver preparation, cryptor registration, instance
commit and dispatch against cleanup. Cleanup first waits out the gate and
closes it, then disables/clears cryptors; detached subscription callbacks
after close cannot prepare or export receivers. A callback panic remains a
failure; gate poison recovery closes later admission while permitting cleanup.
No manager lock is held while acquiring the gate; gated operations acquire
ManagerInner only after the gate, with no inverse nested lock order.

Each remote audio/video wrapper gets one private Arc<()> instance token at
construction. Clones retain it and metadata updates never replace it. The gate
records the latest token for (identity,SID) only after native setup completes.
Unsubscribe must match that exact token before removing the cryptor or exporting
removal, so an old or refused overlapping same-SID receiver cannot retire the
current one. Tokens retain no keys, clients, tracks, native handles or tasks.
Ordinary matched unsubscribe consumes them; closing clears all and permanently
refuses late callbacks. Existing encrypted callback admission order is retained;
this is not a new SFU generation-ordering or reconnect recovery protocol.

Refusal disables only the actual rejected RemoteTrack; it deliberately does not
call publication-wide set_subscribed(false), which can race another receiver.
No SFU unsubscribe or network-cessation receipt is claimed. A refused track has
no exported frame handle. No post-queue mutable publication check, native-pointer
comparison, public RoomEvent/API, CXX ABI, pin, dependency or feature is added.

The exact private gate source is compiled into the existing matrix-rtc-livekit
library test target. It covers None/Gcm/Custom, general non-E2EE behavior, setup
unwind/closed recovery, overlapping same-SID instance ownership, entered export
versus close, and detached post-close refusal. No SDK test factory, dev edge or
workflow selector is added. Native RoomEvent integration and encrypted call
acceptance still need separately admitted hosted verification.

This supersedes the held SID-only/source-task-join proposal, whose quiescence
assumption missed detached SDK media tasks. Matrix membership authority, key
trust/history/recipient policy, URLs, capture, original deadlines/23 facts and
stock peer remain unchanged. This is not a Bob Megolm membership cure claim.

## Remote publication retirement

Each RemoteTrackPublication owns a private lifetime gate shared by its clones.
Receiver enable, registration and synchronous export complete under that gate.
Removal first retires the publication, waits out an entered export, disables its
current receiver and exports unsubscribe before removing the registry entry.
A detached completion retained before retirement is refused and only its exact
receiver is disabled. Pending subscription completion no longer re-registers a
publication or installs callbacks after it was removed. Remote handlers are
installed before the shared registry exposes a new publication; common mute
handler setup remains protected by the registry write lock. A detached media
completion cannot discover a partially configured entry. Temporary unsubscribe
clears a receiver without retiring the publication, so ordinary resubscribe is
unchanged. A callback unwind remains failure and closes later admission while
allowing cleanup. No SID tombstone collection or retained native handle is added.

The gate nests before the existing E2EE receiver admission gate and ManagerInner.
The existing same-SID Arc token checks and publication-SID pool removal fence are
unchanged. Actual synchronous callbacks dispatch channel messages or perform the
existing crypto setup; no await or publication reentry occurs under this gate.
There is no new task, endpoint, permission, dependency, feature or log text.
The new publication source has a prominent modification notice and its original
registry bytes are retained as remote.rs.upstream. The original source inventory,
licenses and manifests remain unchanged.

The production helper is included by the existing matrix-rtc-livekit library
unit-test harness, alongside the existing E2EE gate. Source regressions cover
retirement before a late clone, repeated live admission, entered export versus
retirement, and failure/poison closure with cleanup. These tests were not run
locally. This correction does not identify the process that exited in an older
installed package run or claim interoperability, installed-package or release
acceptance before affected hosted checks pass.
