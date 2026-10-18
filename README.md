# Morpheus

**A native Matrix client you can make your own.**

Multiple accounts, configurable layouts and a native Rust interface for
macOS, Linux and Windows.

[Customization](docs/CUSTOMIZATION.md) · [Screenshots](docs/screenshots/README.md) ·
[Documentation](docs/README.md) · [Contributing](CONTRIBUTING.md)

![Morpheus with the Terminal preset and bottom-left account switcher](docs/screenshots/account-switcher.png)

*The native app, using fictional offline preview accounts. [More views and capture details](docs/screenshots/README.md).*

## Your accounts, together

Switch between Matrix accounts from the bottom-left profile menu, including
accounts on different homeservers. Each keeps its own session, encryption
store, drafts and credential-vault entry. Inactive accounts pause by default.

Use one optional local app password to protect account switches and startup
restore. Remove a saved account directly from **Manage accounts**; incomplete
cleanup stays visible for retry. See [account controls](docs/ACCOUNT_ACCESS.md).

## Make it yours

Start with **Nocturne**, **Terminal** or **Paper**, then change the details:

- **Typography:** separate interface, message and monospace fonts, plus text size.
- **Messages:** stacked, compact or aligned nickname columns; optional avatars
  and timestamps; adjustable spacing and corners.
- **Navigation:** an icon rail or text sidebar, Space labels, room ordering and
  separate or combined DM lists.
- **Layout:** pane widths, conversation headers and the details panel.
- **Colors:** editable theme packs with dedicated selection, focus, unread,
  link, voice and sender colors.

In-app settings and live-reloaded `config.toml` edit the same preferences.
Invalid edits keep the last valid configuration and show an error.
[Explore the settings and theme format →](docs/CUSTOMIZATION.md)

## Development status

**Pre-alpha. No installer release is available yet.**

Messaging, encryption, account controls and customization are under active
development. Native calls are experimental and require a compatible MatrixRTC
service. Installed-app behavior, accessibility, device interoperability, signing
and updates still have open release checks. Avoid relying on Morpheus for
important conversations or encryption-key recovery at this stage.

See the [feature matrix](docs/FEATURE_MATRIX.md) for implementation status and
the [release checklist](docs/RELEASE_CHECKLIST.md) for what remains.

## Build from source

Read the [build and verification policy](AGENTS.md) and use the Rust toolchain
pinned in `rust-toolchain.toml`. Then run the desktop app:

```sh
cargo run --locked -p morpheus-app
```

The app opens at Matrix sign-in and offers an offline preview.
[Architecture](docs/ARCHITECTURE.md) and [packaging](docs/PACKAGING.md) cover
native dependencies, experimental calls and platform checks.

To help, read [Contributing](CONTRIBUTING.md). Report vulnerabilities through
[private security reporting](SECURITY.md).

## License

[AGPL-3.0-or-later](LICENSE). Third-party licenses and provenance are recorded
in [vendor notices](vendor/README.md). The About panel includes Slint's required
attribution.
