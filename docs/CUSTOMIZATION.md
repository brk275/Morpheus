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

Settings edits appearance presets, interface/message/monospace fonts, message
layout, avatars, timestamps, sender colors, spacing, corner radius and rail
width. Existing controls cover font scale (75–200%), room/message density,
unread-first sorting, pane widths and the details panel. These preferences use
the same configuration path in live sessions and the offline preview.

The appearance expansion is implemented in source. Native compilation,
rendered layout, font fallback and keyboard/screen-reader acceptance on all
supported desktops remain required; see [verification](VERIFICATION.md).

## Appearance starting points

**Settings → Appearance → Starting points** offers three original presets:

| Preset | Typography and layout | Surfaces and geometry |
| --- | --- | --- |
| Nocturne | Sans-serif messages, stacked names, avatars and room context | Charcoal, pale mint accents, soft edges |
| Terminal | Monospaced messages, aligned timestamp/nickname columns, no avatars | Dark green charcoal, square edges, narrow rail and tight spacing |
| Paper | Serif messages, compact headers, no avatars | Warm light surfaces, forest accents, broad conversation inset |

Choosing a preset fills in ordinary appearance, typography, geometry and chat
settings, including pane sizes and details-panel visibility. Subsequent edits
remain yours: there is no separate preset layer overriding them. Presets keep
text scale, unread sorting, notifications, shortcuts, media preferences and
account settings intact. **Base palette** changes the palette alone.

The corresponding built-in theme names are `nocturne`, `terminal` and `paper`.
Their palette files are embedded in the app. Setting only
`appearance.theme_pack = "terminal"` selects its colors; use the in-app preset
or set the fields below to select its fonts and layout too.

## Typography, geometry and messages

These tables are optional in schema version 1. Older configuration files receive
defaults for omitted fields. Unknown fields and unsupported values reject the
edit, so spelling mistakes do not silently become inactive settings.

| Field | Default | Accepted values / effect |
| --- | --- | --- |
| `appearance.font_family` | `"Inter"` | Interface font family |
| `typography.message_font_family` | Omitted | Message and composer family; omission follows the interface font |
| `typography.monospace_font_family` | `"monospace"` | Timestamp and security-code family |
| `typography.message_size` | `14` | 10–32 logical pixels before text scaling |
| `geometry.corner_radius` | `10` | 0–24; base radius for core controls and conversation surfaces |
| `geometry.message_gap` | `12` | 0–32; base vertical message padding, adjusted by density |
| `geometry.content_padding` | `24` | 0–48; conversation inset and composer margin |
| `geometry.rail_width` | `70` | 44–100; Space rail width |
| `chat.layout` | `"stacked"` | `"stacked"`, `"compact"` or `"columns"` |
| `chat.show_avatars` | `true` | Show message avatars; hiding them also removes their gutter |
| `chat.show_timestamps` | `true` | Show optional message timestamps; pending/failed delivery labels remain visible |
| `chat.color_senders` | `false` | Use the current sender palette for name text |
| `chat.nickname_width` | `128` | 64–240; desired nickname column width |

Font names must be nonblank, at most 128 UTF-8 bytes, and contain no control or
direction-format characters. The app selects installed font families; it does
not fetch font files. Missing families use the renderer's fallback. In Settings,
leave the message family blank to inherit the interface font and choose
**Apply fonts**. In TOML, omit `message_font_family` to inherit; an explicitly
empty string is invalid.

Geometry values are logical pixels, independent of desktop DPI. Global text
scale still multiplies message size. Compact density uses half the configured
message padding; comfortable uses it unchanged; spacious uses about 1.33 times
that padding. Density also changes room row height. Core controls use
proportions of the base radius, so setting it to zero makes those controls
square.

Stacked places names above messages. Compact uses smaller name headers and
avatars. Columns aligns nicknames and timestamps beside the message body;
long names may elide visually but keep their complete accessibility label.
When the conversation is narrower than 640 logical pixels, or 640 multiplied by
a text scale above 100%, columns uses a compact header. Nickname columns also
stay within 20% of the conversation width. Desired room/details widths shrink
when necessary to preserve conversation space in a smaller window.

For example, this creates a compact, square, monospaced workspace:

```toml
schema_version = 1

[appearance]
theme = "dark"
theme_pack = "terminal"
font_family = "monospace"
density = "compact"
scale_percent = 100

[typography]
message_font_family = "monospace"
monospace_font_family = "monospace"
message_size = 13

[geometry]
corner_radius = 0
message_gap = 2
content_padding = 10
rail_width = 52

[chat]
layout = "columns"
show_avatars = false
show_timestamps = true
color_senders = true
nickname_width = 112
```

The first appearance expansion covers the main conversation, navigation,
composer and reusable controls. Some call and modal surfaces still have fixed
geometry. Theme packs currently contain eleven color tokens; semantic status
colors, custom sender palettes, font weights/line-height controls, timestamp
formatting, background images and arbitrary pane arrangements remain future
work. Theme packs cannot execute code. The presence of older configuration
fields such as `reduce_motion`, `show_space_labels` or `group_direct_messages`
does not imply those controls are implemented.

## Notifications

The Notifications section edits desktop notifications, sounds, message previews
and local highlight words or phrases. Each highlight is trimmed, repeated
whitespace is collapsed, duplicates are rejected without regard to case, and
hidden control or direction characters are rejected. The editor accepts up to 32
highlights of at most 64 characters each. Saved changes appear in the app when
that file changes externally.

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

## Theme files

Set `appearance.theme_pack = "aurora"` and place a copy of
[`aurora.toml`](../themes/aurora.toml) at `themes/aurora.toml` beside your
`config.toml` to create an editable theme. A theme is data only and supplies
color tokens. Names use ASCII letters, digits, `_` or `-`; they cannot be paths.
Colors use `#RRGGBB`. Changing the settings file or active theme file causes a
reload.

The same mechanism overrides an embedded theme: place your own `nocturne.toml`,
`terminal.toml` or `paper.toml` in that themes directory. A missing override uses
the embedded palette. A present but malformed or unreadable override shows an
error and retains the last valid appearance. Other missing theme names are
errors. The original palettes are available in
[Nocturne](../themes/nocturne.toml), [Terminal](../themes/terminal.toml) and
[Paper](../themes/paper.toml).

Morpheus rejects an unknown schema version instead of silently discarding
settings. New versions read existing schema 1 files with defaults for the new
appearance tables; older app versions may reject files containing those new
tables.

## Call audio devices

In builds with native calls, **Settings → Call audio devices → Refresh audio
devices** lists microphones and speakers from the platform's current CPAL
backend. Refresh opens no recording or playback stream. Selectors save
immediately to the existing `media.microphone_device` and
`media.speaker_device` fields. **System default** removes the corresponding
field. These are global machine preferences, shared by all signed-in accounts.

Each saved value is a complete host-qualified CPAL device ID, not its display
name or position in the list. The picker shows a bounded friendly name and a short stable ID fingerprint
so two identically named devices remain distinguishable. Selecting a row
writes the complete ID to TOML; IDs and availability differ across operating
systems. Malformed IDs
reject the configuration edit and retain the last valid preferences.

Choices apply to the **next join**. Changing Settings or TOML does not reroute
an active call; leave and rejoin to apply them. At startup of each call,
Morpheus resolves the exact selected ID again. A missing or ambiguous ID
produces an explicit audio error without falling back to a different device.
An unavailable saved choice remains visible until you explicitly change it.
The call can remain joined with local audio unavailable; choose another device
and rejoin to retry.

Device discovery runs off the UI thread and times out after 20 seconds. A
driver query still blocked in the operating system prevents another query
until it returns; late results cannot replace a newer list. Discovery uses the
platform default backend (CoreAudio, WASAPI, or the configured Linux ALSA
backend), not every optional CPAL backend. Audio still requires the existing
48 kHz mono/stereo PCM support. Operating-system default and virtual endpoints
can themselves reroute audio. Automatic device hotplug recovery, live
reconfiguration, camera selection, and extra noise-processing controls are
outside this change. Native compilation, selector interaction, permissions,
duplicate-name hardware, unplug/rejoin, and actual routing acceptance on all
supported desktops remain pending for this batch.

## Call membership mode

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
