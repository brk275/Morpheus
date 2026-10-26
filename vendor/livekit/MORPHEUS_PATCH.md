# Morpheus LiveKit settings ordering

Same-version livekit0.7.48 path patch. Original registry archive SHA256:
3db88772d10f8ef1ab43a0aa0634a1d4807d0567b65665274af3e084589dbec6.
Its recorded source is livekit/rust-sdks commit
af369aae0fe074762ef0a2051529e7ec63045042, path livekit. The original76
regular archive members are retained;74 are exact. Both changed runtime
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
