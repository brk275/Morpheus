# Architecture

Morpheus is a local desktop client. It does not run a homeserver, SFU, or push
gateway. Calls are available only when the signed-in homeserver advertises a
compatible MatrixRTC transport.

## Boundaries

- `morpheus-app`: Slint views and application controller. It owns presentation
  state, not Matrix access tokens or encryption keys.
- `morpheus-matrix`: one Matrix SDK client, persistent store, and sync task per
  signed-in Matrix device. The active account syncs and runs its send queue by
  default. Queued text and encrypted attachments keep their SDK transaction ID
  after a send error; recoverable room failures retry with bounded backoff.
  Optional background sync for inactive accounts is planned after the account isolation
  and notification checks.
- `morpheus-rtc`: native MatrixRTC call integration. The evolving wire protocol
  and pinned RTC dependency remain behind this boundary.
- `morpheus-config`: versioned TOML preferences and themes. Secrets never enter
  the TOML file.

Each Matrix account has its own SDK client and store. Account switching changes
the active presentation model and resumes that account's sync. Switching away
from an active call requires leaving the call first.

## Matrix interoperability

Spaces are a graph. Home contains DMs and rooms outside visible Spaces. A room
can appear in multiple Space lists.

Drop-in voice spaces are ordinary Matrix rooms with standard MatrixRTC calls.
The native default room slot is `m.call#room`, matching the pinned RTC core and
its passing two-device test. Element Call v0.26.1 uses `m.call#ROOM` for its
newer room slot; that current-format interoperation remains unresolved.
Morpheus exposes an explicit `RoomState` compatibility option for the older
Element Call membership carrier, which passed encrypted browser-peer testing.
Morpheus may add a state event with type `io.github.brk275.morpheus.voice`, an
empty state key, and content `{"v":1,"enabled":true}` to mark shared voice
presentation. The marker never grants access or replaces Matrix power levels.
The configuration schema supports local-only voice-room designations for users
without permission to send the marker; the settings UI for this is pending.

Native sliding sync is used when available; a classic `/sync` adapter keeps
older homeservers usable. Both feed the same room/timeline presentation model.

## Incoming call ownership

The native incoming-call path has separate admission, discovery, presentation
and media owners. Discovery watches unsolicited encrypted sticky receipts for
the active account, then asks the admission boundary to validate the caller,
room, original reference, current membership, trust and notification policy.
A process-wide replay ledger survives account switches. It never evicts old
invitations to permit replay.

The UI presents at most four offers. Answer carries an opaque single-use key;
it cannot replace the offer's account, room or protocol mode with the selected
conversation. The accepted capability is revalidated under the account-switch
lock before a quiet join. After joining, the actual call and observer must agree
on the exact local user, device and membership generation before media starts.
An absent own participant waits on the two actual roster streams within the
original invitation deadline; conflicting identities, closed sources and
cancellation fail the join. The call worker owns that authorization until cleanup. Camera capture stays off
until requested. Dismiss removes the local offer; it sends no protocol decline.

In-app notification and voice-room preference edits synchronously revoke pending
authorizations before the settings callback completes. External config edits take
effect at live reload; only observed versions trigger revocation. SDK room,
receipt and crypto changes use streams and bounded cache reconciliation; these
are not atomic hooks into remote server or crypto-store mutations. Full admission
and pre/post-join validation remain required. The pinned SDK device update
stream omits deletion entries, so fresh device lookups at Answer and post-join
remain mandatory. Account revocation and shutdown
also guard capture directly. Ordinary invitation expiry after an established
join does not terminate that call.

Discovery currently watches up to 128 joined rooms, with bounded commands,
candidates, cached receipts and membership scans. A relevant uncertainty can
withdraw every pending offer. Accounts beyond the room bound have incomplete
incoming discovery. Audible ringing, incoming OS notifications, protocol decline,
real-device Answer behavior and accessibility acceptance remain release gates.
Source review and unit guards alone do not establish those behaviors.

## Local data and security

Configuration lives in the platform's standard config directory. Cached room
state, encryption stores, and the local message search index live in separate
per-account data directories. The SDK encrypts each account's search index
with that account's store passphrase. Search queries run locally against
indexed events; older history becomes searchable as it syncs or paginates.
Tokens live in the operating-system credential vault; if the vault cannot store
a token, the user signs in again next launch. The app never stores a token in
TOML or diagnostics.

Browser sign-in launches the system browser and returns through a short-lived
loopback callback listener. Delegated OAuth/OIDC uses the pinned Matrix SDK's
PKCE and native client registration; unsupported discovery falls back to Matrix
SSO login tokens. OAuth session and refresh credentials share the OS-vault
boundary with legacy sessions. Provider browser acceptance remains pending.
The app does not embed WebViews. Rich remote HTML and URL previews require
sanitization and privacy controls before they are enabled in the interface.

Attachment filenames are sanitized before they enter the UI. A downloaded
attachment is decrypted for the selected account and rejects an oversized
declared size when present. It is written only to a path selected by the user.
Saves started from a message wait in the account's transfer center, one in
memory at a time. They are bound to the account selection rather than the
room, hold the account-switch guard through the local write, and publish only
under a new name. Existing entries are never overwritten, including ones that
appear after the Save dialog closes. A name conflict offers Retry with another
destination; the dialog path is not treated as proof of overwrite consent.
Upload admission follows the chosen order within each account and room,
while different rooms share a bounded preparation pool. Cancellation remains
independent of the bounded click queue. At most eight downloads run or wait;
recent failed-download and completed-transfer histories each retain at most
24 records across accounts. Older failures expire without removing active work.
Morpheus builds the media request with Ruma
and downloads through
a dedicated HTTP client with automatic redirects disabled. Each 307/308 target
is checked before the next request; cross-origin targets must be public-looking
HTTPS domain names, and the Matrix access token is never forwarded to them.
Cross-origin requests use a separate no-proxy client whose DNS resolver rejects
any answer set containing a private or otherwise non-public IP address. Reqwest
connects to those checked socket addresses, and each new connection resolves
and checks again; the original homeserver retains normal proxy support. CDN
fetches that require a system proxy fail closed. The client checks each streamed
chunk before adding it to the 25 MiB download buffer, verifies the ciphertext
hash during decryption, and does not cache the result. The IPv6 public-prefix
allowlist needs maintenance as IANA allocates new ranges. Live public CDN
redirect, token-refresh, and packaged app save checks remain open.
