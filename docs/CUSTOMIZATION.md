# Customization

Morpheus stores non-secret preferences in `config.toml` in the operating
system's standard application configuration directory. On macOS this is under
`~/Library/Application Support/io.github.brk275.Morpheus/`, on Linux under the
XDG configuration directory, and on Windows under the user's application data
directory. The app creates a default file on first run.

Settings and this TOML file represent the same preferences. The file is watched
while the app runs. A broken edit produces an error, and the last valid
configuration remains active. The file never contains Matrix access tokens or
recovery keys.

The current settings screen edits dark/light/high-contrast/system theme,
font scale (75–200%), compact/comfortable/spacious density, unread-first room
sorting, pane widths, and the details panel. These values apply to the live
Matrix view and the offline preview. Additional font, highlight, shortcut,
notification, and media controls are tracked in the beta feature matrix.

Set `appearance.theme_pack = "aurora"` and place a copy of
[`aurora.toml`](../themes/aurora.toml) at `themes/aurora.toml` beside your
`config.toml` to try the theme format. A theme is data only; it supplies color
tokens and cannot execute code. Names use ASCII letters, digits, `_`, or `-`
and cannot be paths. Changing either the settings file or active theme file
causes a reload.

The schema is versioned. Morpheus rejects an unknown schema version instead of
silently discarding settings.
