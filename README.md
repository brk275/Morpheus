# Morpheus

A native Matrix client for macOS, Linux and Windows, built in Rust.
Multiple accounts. Live themes. A workspace you control.

[Customization](docs/CUSTOMIZATION.md) · [Screenshots](docs/screenshots/README.md) · [Documentation](docs/README.md)

![Morpheus: Terminal preset with text navigation and nickname-column messages](docs/screenshots/terminal-sidebar.png)

## Make it yours

Start with **Nocturne**, **Terminal** or **Paper**, then adjust:

- **Type:** separate fonts for the interface, messages and code.
- **Messages:** stacked, compact or nickname columns; spacing and avatars.
- **Layout:** icon rail or text sidebar, pane widths, headers and room ordering.
- **Color:** theme files for surfaces, text, senders, unread messages and focus.

Settings and `config.toml` edit the same preferences. Changes reload live.
Invalid edits keep the last working configuration and show what needs fixing.

## Keep your accounts together

Switch Matrix accounts from the bottom-left profile menu. Each account keeps
its own session, encryption store, drafts and credential-vault entry. Inactive
accounts pause by default.

An optional local app password protects switching and startup restore.
Remove accounts and retry incomplete cleanup in **Manage accounts**.
[Read about account controls →](docs/ACCOUNT_ACCESS.md)

## Run from source

**Pre-alpha · No installer release yet.** Check the [feature status](docs/FEATURE_MATRIX.md)
and [release checklist](docs/RELEASE_CHECKLIST.md) before relying on Morpheus.

Install the [native build dependencies](docs/PACKAGING.md) and pinned Rust toolchain:

```sh
cargo run --locked -p morpheus-app
```

Sign in with a Matrix account or explore the offline preview. Calls require a
compatible MatrixRTC service. Screenshots are native captures with fictional accounts.

## Get involved

[Contributing](CONTRIBUTING.md) · [Issues](https://github.com/brk275/Morpheus/issues) · [Build policy](AGENTS.md) · [Private security reports](SECURITY.md)

## License

[AGPL-3.0-or-later](LICENSE). See [third-party notices](vendor/README.md) for
dependency licenses. Required UI framework attribution is included in About.
