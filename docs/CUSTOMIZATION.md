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
Matrix view and the offline preview. The Notifications section edits the
desktop notification switch, sounds, message previews, and local highlight
words or phrases. Each highlight is trimmed, repeated whitespace is collapsed,
duplicates are rejected without regard to case, and hidden control or direction
characters are rejected. The editor accepts up to 32 highlights of at most 64
characters each. Changes are saved to the same versioned TOML file and appear
in the app when that file changes externally. Additional font, shortcut, and
media controls are tracked in the beta feature matrix.

The corresponding TOML fields are:

```toml
[notifications]
enabled = true
sounds = true
show_message_preview = false
highlight_words = ["release", "urgent"]
```

Message previews are off by default because a desktop banner may show
decrypted text. Matrix push rules and operating-system notification settings
still decide whether a banner or sound is delivered.

Set `appearance.theme_pack = "aurora"` and place a copy of
[`aurora.toml`](../themes/aurora.toml) at `themes/aurora.toml` beside your
`config.toml` to try the theme format. A theme is data only; it supplies color
tokens and cannot execute code. Names use ASCII letters, digits, `_`, or `-`
and cannot be paths. Changing either the settings file or active theme file
causes a reload.

The schema is versioned. Morpheus rejects an unknown schema version instead of
silently discarding settings.

The call stage can remember **Legacy Element Call for this room** separately
for each signed-in account. This is also an account override in TOML:

```toml
[account_overrides."<local account ID>"]
legacy_element_call_rooms = ["!room-id:example.org"]
```

It joins Element Call's older room-state call format and does not open a
MatrixRTC call slot. Standard calls remain the default for every other room.
This setting does not add support for Element Call's current `matrix_2_0`
format. Logging out removes this account's saved room choices.
