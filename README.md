# Morpheus

**Your Matrix. Your layout.**

A native Rust desktop client for **macOS, Linux and Windows**, with multiple
accounts and deep control over how it looks.

[Screenshots](docs/screenshots/README.md) · [Customization](docs/CUSTOMIZATION.md) ·
[Documentation](docs/README.md) · [Contributing](CONTRIBUTING.md)

![Morpheus with the Terminal preset and bottom-left account switcher](docs/screenshots/account-switcher.png)

*Terminal theme · native app screenshot · fictional preview accounts*

## Make it yours

Start with **Nocturne**, **Terminal** or **Paper**, then shape the interface:

- **Type:** separate interface, message and monospace fonts.
- **Messages:** stacked, compact or nickname columns; avatars, timestamps and spacing.
- **Navigation:** icon rail or text sidebar, Space labels and room ordering.
- **Layout:** pane widths, headers, density and corners.
- **Color:** theme packs with dedicated focus, unread, link, voice and sender colors.

Use the settings panel or edit `config.toml`. Valid changes reload live; invalid
edits keep the last working configuration.
[Browse customization →](docs/CUSTOMIZATION.md)

## Keep your accounts close

Switch accounts from the bottom-left profile menu, including accounts on
different homeservers. Sessions, encryption stores, drafts and vault entries
stay separate. Inactive accounts pause by default.

An optional local app password protects switches and startup restore.
**Manage accounts** includes direct removal and retries for incomplete cleanup.
[Account controls →](docs/ACCOUNT_ACCESS.md)

## Development status

**Pre-alpha. No installer release is available yet.**

Messaging, encryption and native calls are in development. Calls require a
compatible MatrixRTC service. Accessibility, installed-app behavior, device
interoperability, signing and updates still have open checks. Do not rely on
Morpheus for important conversations or encryption-key recovery yet.

[Feature status](docs/FEATURE_MATRIX.md) · [Release checklist](docs/RELEASE_CHECKLIST.md)

## Build from source

Follow the [build policy](AGENTS.md), install the
[native dependencies](docs/PACKAGING.md) and use the pinned Rust toolchain:

```sh
cargo run --locked -p morpheus-app
```

The app opens at Matrix sign-in and offers an offline preview.

[Contribute](CONTRIBUTING.md) · [Report a vulnerability privately](SECURITY.md)

## License

[AGPL-3.0-or-later](LICENSE). Third-party licenses and provenance are recorded
in [vendor notices](vendor/README.md). The About panel includes Slint's required
attribution.
