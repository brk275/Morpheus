# Morpheus

Morpheus is an original, native Rust Matrix desktop client in development. The
interface is inspired by familiar community-chat navigation, with a distinct
design, deep configuration, and fast Matrix account switching.

**Status:** private pre-alpha. Password sign-in, account switching, Spaces,
room creation and joining, invitations, basic room timelines, plain-text
replies and edits, reactions, text threads, encrypted per-room drafts, and
native attachment selection and save controls are wired into the desktop app.
Encrypted attachment transfer passed a local core roundtrip; the desktop
controls still need live UI acceptance. The Matrix SSO token flow is wired but has
not passed a browser login test. An opt-in native call path exposes controls to
open or join a MatrixRTC room call on a compatible homeserver, plus a remote
video tile and participant roster. Local encrypted two-device calls passed
synthetic audio and camera transport, a shared voice-space marker, and real
macOS display/window screen-track transport. Live call-stage rendering, real
camera capture, packaged-app permissions, and Element Call interoperability
remain to be verified. Rich formatting, media previews, full call controls, recovery
screens, and platform acceptance tests remain in progress. Do not rely on this
client for important conversations or encryption-key recovery yet. The first
beta requires the cross-platform checks in [the feature matrix](docs/FEATURE_MATRIX.md).

![Morpheus desktop preview](docs/screenshots/desktop-preview.png)

## Direction

- Native Slint interface; no embedded browser views.
- Matrix Rust SDK for chat, room state, and encryption.
- Separate persistent data and encryption keys for each account.
- Native MatrixRTC/LiveKit calls where the homeserver supports them.
- In-app settings and a reloadable TOML configuration.
- macOS, Linux, and Windows releases after platform testing.

The source is original. Cinny, Discord, and Halloy are design and product
references, and their code, artwork, names, and icons are not included.

## Development

Install the toolchain in `rust-toolchain.toml`, then run:

```sh
cargo fmt --all -- --check
cargo test --locked -p morpheus-config -p morpheus-matrix -p morpheus-rtc
cargo run -p morpheus-app
cargo check --locked -p morpheus-app --features native-calls
cargo run -p morpheus-app --features native-calls
```

The app opens on Matrix sign-in and includes an explicitly labeled interactive
offline preview. A preview conversation does not send network messages or join
a call. Browser sign-in uses the Matrix SSO login-token flow; MSC3861 OIDC is
not implemented yet. The default build has a labeled call preview; the
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

The package configuration is in [Packager.toml](Packager.toml). Install
`cargo-packager` to build a local installer with `cargo packager --release`.
Private preview packages are not signed or offered as public downloads.

## License

Morpheus is licensed under AGPL-3.0-or-later. See [LICENSE](LICENSE). The
application's About panel includes Slint's required attribution widget.
