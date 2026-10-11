# Morpheus

Morpheus is an original, native Rust Matrix desktop client in development. The
interface is inspired by familiar community-chat navigation, with a distinct
design, deep configuration, and fast Matrix account switching.

**Status:** public source pre-alpha; no installer release. Password sign-in, account switching, Spaces,
room creation and joining, invitations, public room discovery, basic room
timelines, plain-text replies and edits, reactions, text threads, encrypted
per-room drafts, local message search, and native attachment selection and
save controls are wired into the desktop app. Encrypted attachment transfer
and persisted search passed disposable two-homeserver tests; the desktop
attachment controls still need live UI acceptance. The Matrix SSO token flow
and delegated OAuth/OIDC paths are wired but have not passed provider browser
login acceptance. An opt-in native call path
exposes room calls, a remote video tile, participant roster, mute/deafen, and
an explicit legacy Element Call compatibility choice. Local encrypted
two-device calls passed synthetic audio and camera transport, a shared
voice-space marker, and real macOS display/window screen-track transport. A
real Element Call v0.26.1 browser peer passed encrypted microphone/video
receive and departure in the legacy room-state mode. Current Element Call
mode, live remote-frame rendering, packaged-app permissions, and three-platform
runtime behavior remain unverified. Rich formatting, media previews, full
call controls, recovery screens, and platform acceptance tests remain in
progress. Do not rely on this client for important conversations or
encryption-key recovery yet. The first beta requires the cross-platform
checks in [the feature matrix](docs/FEATURE_MATRIX.md).

![Morpheus native desktop preview with fictional accounts](docs/screenshots/desktop.png)

Actual application capture using fictional preview accounts.

## Direction

- Native Slint interface; no embedded browser views.
- Matrix Rust SDK for chat, room state, and encryption.
- Separate persistent data and encryption keys for each account.
- Native MatrixRTC/LiveKit calls where the homeserver supports them.
- In-app settings and a reloadable TOML configuration.
- macOS, Linux, and Windows releases after platform testing.

Morpheus's application code and assets are original. Cinny, Discord, and Halloy
are design and product references; their code and artwork are not included.
Third-party Rust dependencies and narrowly patched vendored crates retain their
upstream licenses and notices. See [vendor provenance](vendor/README.md).

Contributions are welcome; start with [CONTRIBUTING.md](CONTRIBUTING.md).
Use [private vulnerability reporting](SECURITY.md) for security issues.

## Development

Read [the build policy](AGENTS.md) and install the toolchain in
`rust-toolchain.toml`. Choose checks relevant to your changes; examples:

```sh
cargo fmt --all -- --check
cargo test --locked -p morpheus-config -p morpheus-matrix -p morpheus-rtc
cargo run -p morpheus-app
cargo check --locked -p morpheus-app --features native-calls
cargo run -p morpheus-app --features native-calls
```

The app opens on Matrix sign-in and includes an explicitly labeled interactive
offline preview. A preview conversation does not send network messages or join
a call. Browser sign-in discovers delegated OAuth/OIDC with PKCE where
available and otherwise uses Matrix SSO login tokens. The OAuth provider must
support native dynamic client registration. The default build has a labeled call preview; the
`native-calls` feature enables experimental microphone audio and remote video
on homeservers that provide a compatible MatrixRTC/LiveKit service. Calls
require the SDK's sticky sliding-sync path. Live room drafts use the account's encrypted Matrix
store and survive restart. A send that finishes after Morpheus quits can leave
a stale draft because pending transaction metadata is not yet restored.
Inactive accounts pause both sync and queued sends by default; a request
already in flight can finish after an account switch. The selected account's
room queues retry recoverable send errors with bounded backoff. A retained
request is never labeled a terminal failure, so sending a duplicate is not
suggested. An unrecoverable wedged request still needs complete message-level
controls in the app.
See [architecture](docs/ARCHITECTURE.md) for the intended boundaries.

See [packaging](docs/PACKAGING.md) for local installer builds. Public CI uses
standard hosted runners and publishes validation results, without installer
downloads. The [release checks](docs/RELEASE_CHECKLIST.md) still apply before
binary distribution; public source availability does not establish beta readiness.

## License

Morpheus is licensed under AGPL-3.0-or-later. See [LICENSE](LICENSE). The
application's About panel includes Slint's required attribution widget.
