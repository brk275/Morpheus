# Morpheus

Morpheus is a native Rust Matrix client for keeping your separate identities
in one app. Switch between personal, community and project aliases from the
bottom-left account menu, with familiar community-chat navigation and deep
customization.

## Separate aliases, one app

- **Multiple Matrix accounts, including different homeservers.** Keep the right
  identity, rooms and conversations together, without repeatedly signing out.
- **Independent account data.** Each account has its own session, device identity,
  encrypted room/crypto store, drafts and credential-vault entry. Inactive
  accounts pause by default.
- **Optional local switch password and direct account removal.** The new
  [account controls](docs/ACCOUNT_ACCESS.md) use one app password for protected
  switches and startup restore. Remove any saved account without opening it;
  incomplete cleanup stays visible for retry. Core regressions pass on all four
  targets; real-vault and interactive acceptance remain open.

Each alias remains a normal Matrix account with its own homeserver identity.

## Development status

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
receive and departure in the legacy room-state mode. A stock Element Call v0.28.0-rc.1 peer passed the pre-synced Matrix 2.0
(`Sticky2025`) protocol fixture. Fresh-device native peers passed Current and
Sticky2025 membership renewal in Linux CI. Fresh-device Element interoperability,
live remote-frame rendering, packaged-app permissions, and three-platform
runtime behavior remain unverified. Native rich-text, mention and image-preview
paths are implemented; their interactive UI acceptance, broader media support,
full call controls and platform release checks remain in progress. Do not rely on this client for important conversations or
encryption-key recovery yet. The first beta requires the cross-platform
checks in [the feature matrix](docs/FEATURE_MATRIX.md).

The composer includes explicit mention selection, typed Plain/Markdown drafts,
mention-aware replies, threads and edits, and a searchable emoji picker. The
new [Unicode catalog](docs/EMOJI_CATALOG.md) expands its 44 starter choices to
3,944 canonical entries, including skin-tone combinations, with offline name,
shortcode and glyph search. Its native regressions passed on all four targets
at `65b4ceb`; performance and platform font behavior remain unverified. The prior composer
and starter-picker guards passed on all four platforms at `c02a1b7`. Expanded
encrypted formatting and typed-draft fixtures also passed in Linux at that
revision. Keyboard, IME and accessibility acceptance remain open.

A native click-to-preview image viewer is now implemented for bounded PNG and
baseline JPEG attachments in rooms and threads. It keeps preview data in memory
and leaves encrypted-room previews off by default. Native builds and 13 focused
guards passed on all four platforms. The expanded encrypted/plain-image API
fixture passed in Linux at `c02a1b7`; viewer/accessibility acceptance remains pending. Animation, WebP, audio
and video previews are not supported. See [image preview limits](docs/IMAGE_PREVIEWS.md).

A [room-settings extension](docs/ROOM_SETTINGS.md) adds guarded name/topic,
avatar and join-rule controls.
At `65b4ceb`, all four native builds and unit suites passed. Linux live checks
still failed on encrypted restart, a rate-limited topic write, OAuth refresh
persistence, and a fresh incoming-offer timeout before account switching.
See [the verification record](docs/VERIFICATION.md) for the exact results.
Native [incoming-call sound](docs/INCOMING_CALLS.md) passed its automated guards;
audible output and device acceptance remain open. Protocol Decline is unimplemented.

![Morpheus native desktop preview with fictional accounts](docs/screenshots/desktop.png)

Actual native application capture with fictional preview accounts.
[Account switcher, sign-in, and capture details](docs/screenshots/README.md).

## Make it yours

Morpheus is growing a TOML-driven style system that changes more than colors.
The first expansion adds independent interface/message/monospace fonts,
message size, spacing, corners, rail and pane widths, optional avatars and
timestamps, and stacked, compact or aligned nickname-column layouts.
Full, compact and minimal room headers can be selected independently.
Theme files also expose status, selection, focus, link, unread and voice colors,
plus a separate palette for sender names.
**Nocturne, Terminal and Paper** are editable starting points. In-app controls
and live TOML reload use the same preferences; invalid edits keep the last valid
style. Native Nocturne and Terminal captures have been reviewed; follow-up
font and layout refinements still require new capture verification. A theme editor and
more layout controls remain in development; Halloy-level breadth is the
target, not a completed claim. See [customization](docs/CUSTOMIZATION.md).

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
