# Morpheus

A native Matrix client for macOS, Linux and Windows. Built in Rust, with
multiple accounts for separate aliases and an interface you can shape around
your setup.

![Morpheus running the Terminal theme with text navigation](docs/screenshots/terminal-sidebar.png)

[More screenshots](docs/screenshots/README.md) · [Customization](docs/CUSTOMIZATION.md) · [Documentation](docs/README.md)

## Shape your workspace

Start with **Nocturne**, **Terminal** or **Paper**, then change the details:

- Separate fonts for the interface, messages and code.
- Stacked, compact or nickname-column messages, with adjustable spacing and avatars.
- An icon rail or text sidebar, pane widths, room ordering and header density.
- TOML color palettes for surfaces, text, senders, unread messages and focus.

Settings and `config.toml` control the same preferences. Changes reload live;
invalid edits keep the last working configuration and explain the error.

## Your accounts and aliases

Keep different Matrix aliases in one app and switch from the bottom-left
profile menu. Each account has its own session, encryption store, drafts and
credential-vault entry. Inactive accounts pause by default.

One optional local app password protects account switching and startup restore.
Manage accounts includes direct removal and cleanup retries.
[Account controls](docs/ACCOUNT_ACCESS.md).

## Development

**Pre-alpha. No installer release yet.** See the [feature status](docs/FEATURE_MATRIX.md)
and [release checklist](docs/RELEASE_CHECKLIST.md) before relying on Morpheus.

Install the [native build dependencies](docs/PACKAGING.md) and pinned Rust toolchain:

```sh
cargo run --locked -p morpheus-app
```

Sign in with a Matrix account or explore the offline preview. Calls require a
compatible MatrixRTC service. Screenshots use fictional accounts.

[Contribute](CONTRIBUTING.md) · [Build policy](AGENTS.md) · [Issues](https://github.com/brk275/Morpheus/issues) · [Private security reports](SECURITY.md)

## License

[AGPL-3.0-or-later](LICENSE). [Third-party notices](vendor/README.md).
Slint attribution is included in About.
