# LiveKit Server APIs

The official server API crate for [LiveKit](https://livekit.com).

Use this crate to generate access tokens and invoke LiveKit server APIs for agent dispatch, ingress, egress, SIP, and more.

## Morpheus security patch

Vendored from crates.io `livekit-api` 0.5.3 (Apache-2.0), upstream
`livekit/rust-sdks` commit `af369aae0fe074762ef0a2051529e7ec63045042`
(`livekit-api` subdirectory), preserving upstream source copyright notices and `rustfmt.toml`. `LICENSE`
contains the Apache 2.0 license text.

Local changes (October 10, 2026):

- Credential endpoint validation requires HTTPS/WSS except exact loopback;
  localhost is normalized to a literal loopback address. Userinfo, fragments
  and externally supplied URL queries are rejected.
- Every initial, reconnect and cloud region signaling URL is checked before a
  bearer JWT is attached. HTTP(S) aliases in LiveKit's connection API are
  converted to WS(S) before applying the same policy.
- Authenticated validation and region GETs cannot follow redirects and use a
  bounded client. Loopback connections bypass environment proxies.
- The signaling CONNECT proxy supports only `http://` proxy URLs, including
  when selected through `HTTPS_PROXY`. HTTPS and other proxy schemes are
  rejected before opening a socket: TLS to a proxy is not implemented, so
  treating an HTTPS proxy as plain TCP could expose proxy credentials. A local
  listener regression checks zero connections for rejected schemes.
- Region error bodies and websocket HTTP error bodies/headers are removed
  before logging or display, preventing reflection of request credentials.
- Tests cover URL policy, redirect refusal, and rejected region endpoints.

Morpheus uses the native Tokio backend. These patches target that backend; the
unused async-std backend is not approved for Morpheus distribution.

- The `log` dependency enables `max_level_info` in every build. Tungstenite's
  TRACE path serializes complete bearer-authenticated handshakes, and its DEBUG
  path can print peer-controlled close reasons. Runtime trace settings cannot
  re-enable these compiled-out messages. A captured-log websocket test includes
  the transitive logger and checks dummy bearer/proxy credentials remain absent.

- A failed direct signal send releases its selected stream read guard before
  waiting for the queue. This breaks the queue/stream/reconnect-writer lock
  inversion while keeping the original stream protected through that send.
  Queue FIFO, pass-through drops, and the existing failed-flush message policy
  are unchanged. The focused Tokio test drives the real send/flush/read/write
  locks and internal oneshots with a controlled writer; it does not establish
  WebSocket or RTC recovery.
