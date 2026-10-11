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

The call stage remembers one **Call mode** separately for each account and
room: **Standard** (the default), **Element Call Matrix 2.0**, or **Legacy**.
The same choices are available in TOML:

```toml
[account_overrides."<local account ID>"]
element_call_matrix_2_0_rooms = ["!sticky-call:example.org"]
legacy_element_call_rooms = ["!older-call:example.org"]
```

A room in neither list uses Standard. Existing `legacy_element_call_rooms`
preferences retain their meaning. A room cannot appear in both lists; invalid
edits retain the last valid configuration. The UI moves a room between modes
atomically. Logging out clears both lists for that account.

Element Call Matrix 2.0 uses the pinned RTC library's `Sticky2025` adapter.
It requires an encrypted room, advertised sticky-event and MatrixRTC service
support, working sticky sync, and a verified/recovered device. Use **Open room
call** when the required slot has not been created; that explicit action needs
room permission. Legacy uses older room-state membership without that slot.
These formats do not negotiate or automatically fall back to each other.

The mode chosen when starting is fixed until the call ends. Mode controls are
disabled while opening, connecting, joined, or cleaning up a call. External
TOML edits apply to the next call, never an existing one. The voice-space marker
does not select a protocol. Native compilation, UI interaction, and production
Element Call acceptance for the new Matrix 2.0 choice remain pending; see
[the exact peer assertions and limits](ELEMENT_CALL_ACCEPTANCE.md).
