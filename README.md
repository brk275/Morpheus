# Morpheus

A native Matrix client built in Rust for **macOS, Linux and Windows**.
Multiple accounts, configurable layouts, and themes you can make your own.

[Screenshots](docs/screenshots/README.md) · [Customization](docs/CUSTOMIZATION.md) ·
[Documentation](docs/README.md) · [Contributing](CONTRIBUTING.md)

**Pre-alpha · No installer release yet.** See [development status](#development).

![Morpheus with the Terminal preset and bottom-left account switcher](docs/screenshots/account-switcher.png)

*Terminal preset. Real native capture with fictional accounts.*

## Your setup

Start with a preset or create a TOML theme. Customize:

- **Type:** separate interface, message and monospace fonts.
- **Messages:** stacked, compact or nickname columns; avatars, timestamps and spacing.
- **Navigation:** icon rail or text sidebar, Space labels and room ordering.
- **Layout:** pane widths, headers, density and corners.
- **Color:** theme packs with dedicated focus, unread, link, voice and sender colors.

Settings and `config.toml` control the same preferences. Valid edits reload live;
invalid edits keep the last working configuration.
[Customization guide](docs/CUSTOMIZATION.md)

| Nocturne | Paper |
| :---: | :---: |
| ![Nocturne: dark surfaces and stacked messages](docs/screenshots/desktop.png) | ![Paper: light surfaces and serif messages](docs/screenshots/paper.png) |

## Multiple accounts

The bottom-left profile menu switches between accounts on different homeservers.
Each account has separate sessions, encryption stores, drafts and vault entries.
Inactive accounts pause by default.

An optional local app password protects switches and startup restore.
Remove accounts from **Manage accounts**, with retries for incomplete cleanup.
[Account controls](docs/ACCOUNT_ACCESS.md)

## Build from source

Follow the [build policy](AGENTS.md), install the
[native dependencies](docs/PACKAGING.md) and use the pinned Rust toolchain:

```sh
cargo run --locked -p morpheus-app
```

The app opens at Matrix sign-in and offers an offline preview. Native calls
require a compatible MatrixRTC service.

## Development

Morpheus is still in active development. Messaging, encryption and native calls
have open acceptance checks, alongside accessibility, installed-app behavior,
device compatibility, signing and updates. Do not rely on it for important
conversations or encryption-key recovery yet.

[Feature status](docs/FEATURE_MATRIX.md) · [Release checklist](docs/RELEASE_CHECKLIST.md) ·
[Private security reports](SECURITY.md)

## License

[AGPL-3.0-or-later](LICENSE). Third-party licenses and provenance are recorded
in [vendor notices](vendor/README.md). The About panel includes Slint's required
attribution.
