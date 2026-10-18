# Morpheus

**A native Matrix client. Make it yours.**

Built in Rust for macOS, Linux and Windows, with multiple accounts and live
control over themes, typography and layout.

[Customization](docs/CUSTOMIZATION.md) · [Screenshots](docs/screenshots/README.md) · [Documentation](docs/README.md) · [Contributing](CONTRIBUTING.md)

![Morpheus with the Terminal preset and text sidebar](docs/screenshots/terminal-sidebar.png)

*The native app, using fictional accounts. Terminal is one of three built-in presets.*

## Your setup

Start with **Nocturne**, **Terminal** or **Paper**, then tune it to fit:

- **Typography:** independent fonts for the interface, messages and code.
- **Messages:** stacked, compact or nickname-column layouts; spacing and avatars.
- **Navigation:** icon rail or text sidebar, Space labels and room ordering.
- **Geometry:** pane widths, headers, density and corner radius.
- **Color:** TOML palettes for surfaces, senders, links, focus, unread messages and voice.

Use Settings or edit `config.toml`. Both control the same preferences, and changes
reload live. Invalid edits show an error and preserve the last working setup.

[Explore customization →](docs/CUSTOMIZATION.md)

## Your accounts

The bottom-left profile menu keeps account switching close at hand. Each account
has its own session, encryption store, drafts and credential-vault entry.
Inactive accounts pause by default.

An optional local app password protects switching and startup restore.
**Manage accounts** includes direct removal and a retry for incomplete cleanup.

[Account controls →](docs/ACCOUNT_ACCESS.md)

## Try the source

**Pre-alpha — no installer release yet.** Morpheus is still in development and
is not ready for important conversations or key recovery. See the
[feature status](docs/FEATURE_MATRIX.md) and [release checklist](docs/RELEASE_CHECKLIST.md).

Install the [native build dependencies](docs/PACKAGING.md), then use the pinned
Rust toolchain:

```sh
cargo run --locked -p morpheus-app
```

Sign in to Matrix or explore the offline preview. Calls require a compatible
MatrixRTC service. Read the [build policy](AGENTS.md) before running hosted checks.

## Contribute

[Contribution guide](CONTRIBUTING.md) · [Open an issue](https://github.com/brk275/Morpheus/issues) · [Report a vulnerability privately](SECURITY.md)

## License

[AGPL-3.0-or-later](LICENSE). See [third-party notices](vendor/README.md) for
dependency licenses and provenance. Slint attribution is included in About.
