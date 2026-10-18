# Morpheus

A native Matrix client for **macOS, Linux and Windows**, built in Rust.
Separate accounts. Live configuration. An interface you can make your own.

[Screenshots](docs/screenshots/README.md) · [Customization](docs/CUSTOMIZATION.md) ·
[Documentation](docs/README.md) · [Contributing](CONTRIBUTING.md)

**Pre-alpha.** Source is available; installers are not released yet.

![Morpheus in the Terminal preset, with text navigation and nickname columns](docs/screenshots/terminal-sidebar.png)

*Terminal preset · Native app capture · Fictional preview accounts*

## Make it yours

Start with **Nocturne**, **Terminal** or **Paper**, then change the details:

- Separate fonts for the interface, messages and code.
- Stacked, compact or nickname-column messages, with adjustable spacing and avatars.
- An icon rail or text sidebar, Space labels and room ordering.
- Pane widths, headers, density and corner radius.
- TOML palettes with colors for focus, unread messages, links, voice and senders.

Settings and `config.toml` edit the same preferences. Changes reload live;
invalid edits show an error and keep the last working configuration.

[Customization guide](docs/CUSTOMIZATION.md) · [All presets and screenshots](docs/screenshots/README.md)

## Multiple accounts

Switch accounts from the bottom-left profile menu. Each account has its own
session, encryption store, drafts and credential-vault entry. Inactive accounts
pause by default.

One optional local app password protects account switches and startup restore.
**Manage accounts** provides direct removal and a retry control for incomplete
cleanup. [Account controls](docs/ACCOUNT_ACCESS.md)

## Build from source

Install the [native dependencies](docs/PACKAGING.md) and use the repository's
pinned Rust toolchain:

```sh
cargo run --locked -p morpheus-app
```

The app opens at Matrix sign-in and offers an offline preview. Calls require a
compatible MatrixRTC service. See the [build and verification policy](AGENTS.md)
before running hosted checks.

## Development

Morpheus is not ready for daily use. Messaging, encryption, calls, accessibility
and installation still have open release checks. Do not rely on it for important
conversations or key recovery yet.

[Feature status](docs/FEATURE_MATRIX.md) · [Release checklist](docs/RELEASE_CHECKLIST.md) ·
[Contributing](CONTRIBUTING.md) · [Report a vulnerability privately](SECURITY.md)

## License

[AGPL-3.0-or-later](LICENSE). Third-party licenses and provenance are recorded
in [vendor notices](vendor/README.md). The About panel includes Slint's required
attribution.
