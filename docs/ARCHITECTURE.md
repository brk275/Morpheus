# Architecture

Morpheus is a local desktop client. It does not run a homeserver, SFU, or push
gateway. Calls are available only when the signed-in homeserver advertises a
compatible MatrixRTC transport.

## Boundaries

- `morpheus-app`: Slint views and application controller. It owns presentation
  state, not Matrix access tokens or encryption keys.
- `morpheus-matrix`: one Matrix SDK client, persistent store, and sync task per
  signed-in Matrix device. The active account syncs by default. Optional
  background sync for inactive accounts is planned after the account isolation
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
Morpheus may add a state event with type `io.github.brk275.morpheus.voice`, an
empty state key, and content `{"v":1,"enabled":true}` to mark shared voice
presentation. The marker never grants access or replaces Matrix power levels.
The configuration schema supports local-only voice-room designations for users
without permission to send the marker; the settings UI for this is pending.

Native sliding sync is used when available; a classic `/sync` adapter keeps
older homeservers usable. Both feed the same room/timeline presentation model.

## Local data and security

Configuration lives in the platform's standard config directory. Cached room
state and encryption stores live in separate per-account data directories.
Tokens live in the operating-system credential vault; if the vault cannot store
a token, the user signs in again next launch. The app never stores a token in
TOML or diagnostics.

Matrix SSO launches the system browser and returns through a short-lived
loopback callback listener. MSC3861 OIDC is a later authentication milestone.
The app does not embed WebViews. Rich remote HTML and URL previews require
sanitization and privacy controls before they are enabled in the interface.
