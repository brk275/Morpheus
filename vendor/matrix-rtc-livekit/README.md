# Morpheus MatrixRTC transport patch

Source: Element's `matrix-rust-rtc`, tag `v0.5.0-rc.2`, commit
`53880ef1ef66b62df555ce71ccd1d6c85b813596`, crate `matrix-rtc-livekit`.
The original AGPL/commercial license files and copyright notices are retained.
Morpheus distributes this code under AGPL-3.0-only.

Local changes (October 10, 2026):

- Validate local and remote authorization focus URLs before requesting an OpenID token.
- Require HTTPS, except exact loopback development endpoints. Reject credentials,
  queries and fragments; normalize `localhost` to `127.0.0.1`.
- Token POSTs use an internally constructed client with normal TLS validation,
  no redirects, a 30 second timeout and a 64 KiB response limit. The existing
  client parameter remains for source compatibility but its policy is not used:
  reqwest cannot override or inspect a caller's redirect policy per request.
- Reject non-2xx responses without retaining their possibly sensitive body.
- Require WSS for returned SFU URLs except exact loopback, before any JWT is sent.
- Flatten the workspace manifest while preserving dependency versions; sibling
  RTC crates retain the same upstream tag/revision.

The companion `livekit-api` patch applies the policy to reconnects, cloud region
fallbacks and authenticated diagnostic requests. Focused regression tests cover
invalid endpoints, response redaction and all common HTTP redirect statuses.
