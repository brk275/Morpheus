<p align="center">
  <img src="assets/icon.png" width="80" height="80" alt="Morpheus icon">
</p>

<h1 align="center">Morpheus</h1>

<p align="center">
  <strong>A native Matrix client. Your accounts. Your setup.</strong><br>
  Built in Rust for macOS, Linux and Windows.
</p>

<p align="center">
  <a href="docs/CUSTOMIZATION.md">Customization</a> ·
  <a href="docs/screenshots/README.md">Screenshots</a> ·
  <a href="docs/README.md">Documentation</a> ·
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

![Morpheus with the Terminal preset, text sidebar and nickname-column messages](docs/screenshots/terminal-sidebar.png)

<p align="center"><sub>Terminal preset · Native app capture · Fictional accounts</sub></p>

## Make it yours

Choose **Nocturne**, **Terminal** or **Paper**, then tune the details:

- **Typography** — independent fonts for the interface, messages and code.
- **Messages** — stacked, compact or nickname-column layouts, with adjustable avatars and spacing.
- **Navigation** — an icon rail or text sidebar, Space labels and room ordering.
- **Geometry** — pane widths, headers, density and corner radius.
- **Color** — TOML palettes for surfaces, text, sender names, unread messages and focus.

Settings and `config.toml` control the same preferences. Changes reload live;
invalid edits show an error and keep the last working configuration.

[Explore customization →](docs/CUSTOMIZATION.md)

## All your accounts, close at hand

Use multiple Matrix accounts for different aliases, and switch from the
bottom-left profile menu. Each account has its own session, encryption store,
drafts and credential-vault entry. Inactive accounts pause by default.

One optional local app password protects account switching and startup restore.
**Manage accounts** provides direct removal and a retry for incomplete cleanup.

[Account controls →](docs/ACCOUNT_ACCESS.md)

## Development status

**Pre-alpha.** Source is available; installer releases are still pending the
platform and security checks. The [feature matrix](docs/FEATURE_MATRIX.md)
tracks implementation and the [release checklist](docs/RELEASE_CHECKLIST.md)
tracks readiness.

### Run from source

Install the [native build dependencies](docs/PACKAGING.md) and pinned Rust toolchain:

```sh
cargo run --locked -p morpheus-app
```

Sign in with a Matrix account or explore the offline preview. Calls require a
compatible MatrixRTC service.

## Contribute

[Contribution guide](CONTRIBUTING.md) · [Issues](https://github.com/brk275/Morpheus/issues) · [Build policy](AGENTS.md) · [Private security reports](SECURITY.md)

## License

[AGPL-3.0-or-later](LICENSE). Dependency licenses and provenance are recorded in
the [third-party notices](vendor/README.md). Required UI framework attribution
is included in About.
