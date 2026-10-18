# Editing appearance with an assistant

Settings and `config.toml` use the same keys. An assistant can make a small
text edit, validate it offline, and let the running app reload it. Theme packs
contain data, without executable plugins.

This guide applies to the customization build with the `config` commands.
The earlier preview does not provide those commands. These features are still
under beta verification; the [appearance contract](APPEARANCE.md) distinguishes
implemented controls from remaining work.

## Read the contract from the build being edited

Use the actual application executable in place of `APP` below; `APP` is a
placeholder, not another program to install:

```text
APP config schema --json
APP config validate --file CONFIG.toml --json
```

`schema` provides supported fields, defaults, ranges, enum values, color tokens
and diagnostic codes. Its JSON format is `morpheus-config-schema`, with
`schema_version = 1`; it is a field reference, not a JSON Schema document. Use
that build's reference instead of inventing keys or copying another version's settings. Fields marked
inactive are saved preferences without a current UI effect.

`validate` checks the configuration and its selected theme. Exit status is 0
for valid input, 1 for invalid input and 2 for invalid command arguments. It
creates no profile, opens no window and starts no Matrix session or media
device. On Windows, redirect output to a file or pipe when using the GUI
executable. Diagnostics use fixed codes and schema names; they do not echo
file values, account IDs or file paths.

## Make a focused edit

1. Use the configuration path shown in **Settings → Settings file**, or a path
   the owner provided. Keep backups private and outside the repository.
2. Read the existing file. Preserve unrelated preferences, account overrides
   and shortcuts. Merge keys into existing tables; TOML rejects duplicate
   tables and duplicate keys.
3. Prepare a candidate file beside the real configuration. Custom themes are
   resolved from `themes/` beside that file. A copy in an unrelated temporary
   folder can therefore give a misleading missing-theme error.
4. Run `config validate --file CANDIDATE.toml --json`. Correct the reported
   field and code until validation succeeds. Replace the real file atomically
   with the valid candidate, preserving private file permissions.
5. The app reloads valid edits while running. Confirm the requested appearance
   when the owner is ready for UI inspection. Validation proves syntax and
   supported settings; rendering and accessibility need their own checks.

Avoid simultaneous writes from multiple editors. Settings re-reads the file
before saving and serializes the configuration, which can normalize formatting
and remove hand-written comments. Keep a private backup if those matter.

## Colors and transparency

The [sparse example](../examples/appearance/config.toml) demonstrates a few
overrides. Merge its keys instead of replacing an existing configuration:

```toml
[colors]
accent = "#C6A0F6"
link = "#91D7E3"

[window]
opacity_percent = 90
```

- Use token IDs from `config schema`, such as `canvas`, `link` or
  `presence_online`. Display labels are not necessarily configuration keys.
- Quote colors. Supported forms are `#RRGGBB` and `#RRGGBBAA`; alpha is last.
- Omitted overrides inherit their color. Reset a color by removing its entry
  from the configuration's `[colors]` table.
- Window opacity scales pane backgrounds, while text and controls keep their
  own colors. Sign-in, account-unlock and recovery surfaces stay opaque.
  Native desktop compositing remains subject to platform verification.
- Theme files have a different structure: their `[colors]` holds base palette
  colors and `[roles]` holds semantic colors. Do not paste a theme pack
  wholesale into `config.toml`.

See the [appearance contract](APPEARANCE.md) for inheritance and the
[customization reference](CUSTOMIZATION.md) for typography, layout, geometry,
shortcuts and theme packs. Access tokens, passwords and recovery material do
not belong in appearance files or shared examples.
