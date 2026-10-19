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

For assistant-assisted edits, see the [editing guide](EDITING_APPEARANCE.md)
and its sparse, validated configuration example.

Settings edits appearance presets, interface/message/monospace fonts, message
layout, avatars, timestamps, sender colors, spacing, corner radius and rail
width. Existing controls cover font scale (75–200%), room/message density,
unread-first sorting, pane widths and the details panel. These preferences use
the same configuration path in live sessions and the offline preview.

At `2fddb7f`, [native validation](https://github.com/brk275/Morpheus/actions/runs/39074067812)
passed on all four targets, including 39 configuration tests, message/theme
projections and the generated-Slint keyboard guards at 100%/200% text sizes.
Twelve Linux captures passed privacy and visual review; eight are in the
[native screenshot gallery](screenshots/README.md). Full platform font coverage,
installed interaction and screen-reader acceptance remain open. See
[verification](VERIFICATION.md), including the separate failed system Wayland
capture check.

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
text scale, navigation mode, Space labels, DM grouping, unread sorting, notifications, shortcuts, media preferences and
account settings intact. **Base palette** changes the palette alone.

The corresponding built-in theme names are `nocturne`, `terminal` and `paper`.
Their palette files are embedded in the app. Setting only
`appearance.theme_pack = "terminal"` selects its colors; use the in-app preset
or set the fields below to select its fonts and layout too.

## Navigation

**Settings → Layout → Navigation** edits the same schema-version-1 fields as
this optional table:

```toml
[layout]
navigation = "sidebar"       # "rail" (default) or "sidebar"
show_space_labels = true      # default false; labels under icon-rail Spaces
group_direct_messages = false # default true; separate rooms and DMs
sort_unread_first = true      # default false
```

`rail` preserves the existing icon column. `sidebar` places text routes for Home,
Discover, Calls and Settings, followed by the same Spaces, into the scrollable
room list. The active Matrix account control stays at the bottom left. Space
names are always visible in sidebar mode; `show_space_labels` affects only the
icon rail, where long names are shortened visually and retain their complete
accessible label. Rail width remains saved while the rail is hidden.

With grouping off, rooms and DMs share one list ordered by case-folded name and
room ID. Unread-first sorting moves unread conversations above read ones and
preserves that ordering within each section. DM and voice-room identities,
unread badges and normal open/join actions remain distinct. The quick switcher
and next/previous unread shortcuts follow the displayed order. Invitations keep
their separate controls. Space membership and account selection do not change.

Settings and file edits update the current view without restarting sync. If a
mode change removes the focused navigation control, focus moves to the account
control before removal; typing in a composer or modal keeps its existing focus.
Omitting the table restores rail mode, hidden rail labels and separate DMs.
Unknown modes or misspelled fields reject the edit; the app keeps its last valid
configuration. Appearance presets preserve these three navigation preferences.

The native projection/focus guards passed on all four targets at `2fddb7f`;
the sidebar and labeled rail passed Linux visual review. Broader installed
keyboard and screen-reader checks remain required, including long names, many
Spaces and 200% text scale.

## Room header

**Settings → Appearance → Room header** controls the header independently of
message layout:

| Mode | Visible conversation header |
| --- | --- |
| `full` (default) | Existing room header, topic and introductory block; the introduction still follows the message layout |
| `compact` | Room-name header and one topic line, without the second introduction/topic block |
| `minimal` | Room-name header and room actions, without topic or introductory decoration |

```toml
[conversation]
header = "compact"
```

Omitting this table or field preserves the existing `full` behavior. Unknown
modes and misspelled fields reject the edit. Settings and file changes use the
same live-reload path; an invalid edit retains the last valid appearance.
Appearance starting points preserve this independent choice.

All modes retain the active room name, room settings/search/call/details
controls, call notices, voice-room controls, status and error messages. The
complete room topic remains available in Room settings. This setting affects
the conversation page; Settings, discovery and the call stage keep their
headers. Composer tools and navigation are outside this change.

The three modes appear in the reviewed native Nocturne, Terminal and Paper
captures. Full installed keyboard/accessibility verification remains open.

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
| `typography.sender_weight` | Omitted | 100–900; sender-name weight in the timeline and thread headings/replies; omission preserves each surface's existing weight |
| `geometry.corner_radius` | `10` | 0–24; base radius for core controls and conversation surfaces |
| `geometry.message_gap` | `12` | 0–32; base vertical message padding, adjusted by density |
| `geometry.content_padding` | `24` | 0–48; conversation inset and composer margin |
| `geometry.rail_width` | `70` | 44–100; Space rail width |
| `chat.layout` | `"stacked"` | `"stacked"`, `"compact"` or `"columns"` |
| `chat.show_avatars` | `true` | Show message avatars; hiding them also removes their gutter |
| `chat.show_timestamps` | `true` | Show optional message timestamps; pending/failed delivery labels remain visible |
| `chat.timestamp_format` | `"twelve_hour"` | `"twelve_hour"`, `"twenty_four_hour"` or `"twenty_four_hour_seconds"`; local-clock format in the timeline and thread replies |
| `chat.timestamp_brackets` | `false` | Put square brackets around actual timestamps; delivery labels and missing clocks stay unchanged |
| `chat.timestamp_width` | `72` | 48–160 logical pixels before text scaling; timestamp gutter in Columns layout |
| `chat.color_senders` | `false` | Use the current sender palette for name text |
| `chat.nickname_width` | `128` | 64–240; desired nickname column width |
| `chat.nickname_brackets` | `false` | Show the Columns nickname as `<name>`; the accessible name stays plain |
| `chat.nickname_alignment` | `"right"` | `"left"` or `"right"`; text alignment inside the Columns nickname column |

Font names must be nonblank, at most 128 UTF-8 bytes, and contain no control or
direction-format characters. The app selects installed font families; it does
not fetch font files. `monospace`, `serif` and `sans-serif` resolve to a usable
installed family from the operating system's generic-font mapping; the portable
name remains in your configuration. Other names pass through unchanged. If no
usable generic family is available, the renderer uses its normal fallback,
which may have different metrics or a proportional design. Missing named families
also use that fallback. Resolution runs again on each configuration reload.
In Settings,
leave the message family blank to inherit the interface font and choose
**Apply fonts**. In TOML, omit `message_font_family` to inherit; an explicitly
empty string is invalid.

Sender weight is independent of message-body formatting. Choose a number in
Settings or **Restore sender weight defaults** to remove the override. Omission
keeps the existing 650 timeline weight, 700 thread-reply weight and 750 thread-root
heading weight. Combined compact headers apply the same weight to their time
label. Installed fonts may approximate weights they do not provide.

Timestamp formats use the operating system's local timezone: `3:04 PM`, `15:04`
or `15:04:05`. Square brackets are optional. These fixed formats do not accept
an arbitrary formatting language. Changes update existing timeline rows and
thread replies immediately, including after config reload; search results keep
their existing date-and-time labels. Missing timestamps remain blank and the
`Sending…` / `Failed to send` labels remain visible even when timestamps are
hidden. Narrow timestamp columns elide visually and retain the full accessible
label. Increase the column width for larger fonts, seconds or brackets.

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
a text scale above 100%, columns uses a compact header. A timestamp width above
72 adds the extra scaled width to that threshold, preserving message/action
space. Nickname columns also
stay within 20% of the conversation width. **Nickname angle brackets** and
**Nickname alignment** in Settings → Appearance decorate and align that column
only; stacked and compact headers, thread replies and every accessible label
keep the plain name. The Terminal preset turns brackets on. Pane sizes contribute to the minimum
window width, preserving at least 360 logical pixels for the main content.
Hide the details panel or reduce the pane widths for a smaller workspace.

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
sender_weight = 500

[geometry]
corner_radius = 0
message_gap = 2
content_padding = 10
rail_width = 52

[chat]
layout = "columns"
show_avatars = false
show_timestamps = true
timestamp_format = "twenty_four_hour_seconds"
timestamp_brackets = true
timestamp_width = 88
color_senders = true
nickname_width = 112
nickname_brackets = true
nickname_alignment = "right"
```

The appearance controls cover the main conversation, navigation, composer and
reusable controls. Some call and modal surfaces still have fixed geometry.
Theme packs add optional semantic colors and a sender-name palette to the
original eleven color tokens; see [Theme files](#theme-files). Message-body font
weights, line-height controls, background images and arbitrary pane arrangements
remain future work. The pinned rich-text component does not expose body-weight
or line-height controls. Theme packs cannot execute code.

### Motion

Buttons and navigation use a short native fade for decorative hover feedback.
Selection colors, focus outlines, click targets and account contents change
immediately. Hover fades default to 90 ms and do not use a repeating animation timer.
Choose **Appearance → Hover fade duration**, or search Settings for “animation speed”,
to set 0–1,000 ms. The same value is editable in TOML:

```toml
[appearance]
hover_duration_ms = 120
```

A value of 0 gives immediate feedback. Reduce motion overrides the duration while
preserving it, so turning Reduce motion off restores your chosen timing.

Choose **Appearance → Reduce motion** for immediate hover feedback, or set:

```toml
[appearance]
reduce_motion = true
```

The setting applies live, including an effect already in progress. The setting
currently covers Morpheus's own hover effects. Transitions inherited from standard
controls and scrollbars, and automatic OS motion preferences, still need separate
accessibility work. This is not complete reduced-motion or display-performance
acceptance.

### Preferences awaiting implementation

The following older fields can be stored in TOML but currently have no runtime
consumer. Changing them does not enable the described behavior:

| Field | Pending behavior |
| --- | --- |
| `media.camera_device` | Saved camera selection |
| `media.noise_suppression` | Configurable noise suppression |
| `media.autoplay_animated_media` | Animated-media autoplay policy |
| `experimental.background_account_sync` | Background syncing of inactive accounts |

Microphone and speaker selection, notification overrides and the appearance
controls documented above do have consumers. The monospace font setting covers
timestamps and security codes; it does not replace every rich-message code
font.

## Per-account theme

Each saved account can show its own theme mode. The override is a TOML
preference keyed by the local account ID, the same key the notification and
call-mode overrides use:

```toml
[appearance]
theme = "dark"

[account_overrides."<local account ID>"]
theme = "light"
```

The window shows an account's `theme` only after that account has been
activated: a successful switch, a protected activation with the right app
password, or a startup restore. A wrong password, a cancelled dialog or a
switch that became stale keeps the current account's appearance. Removing or
logging out of the active account, or locking every account, returns the
window to the global `appearance.theme`; it never shows another account's
choice. An account without an override, or an unknown ID, also uses the
global theme. The value accepts `dark`, `light`, `high_contrast` and `system`.

**Settings → Appearance → Base palette for all accounts** edits the global
`appearance.theme` and highlights that global choice, not the palette on
screen. While the active account has an override, a notice under the picker
names the override, the window keeps showing it, and the global edit applies
to the other accounts; the TOML entry has to change or be removed for this
account to follow the global choice. Theme packs (`appearance.theme_pack`),
fonts, geometry and layout remain global.
Invalid edits keep the last valid configuration, including each account's
last valid theme, until the file is valid again. The override is a local
presentation preference; it is not written to Matrix.

## Notifications

The Notifications section edits desktop notifications, sounds, message previews,
room and sender names in banners, local highlight words or phrases, a switch
for the visible account only, the account's default levels and searchable
per-room levels. Each highlight is trimmed, repeated
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
show_room_and_sender = true
highlight_words = ["release", "urgent"]
```

Message previews are off by default because a desktop banner may show
decrypted text. With `show_room_and_sender = false` a banner names only the
account and never previews text. Per-room levels and default levels are Matrix
push rules, chosen in the same section; see [notifications](NOTIFICATIONS.md). Matrix push rules and operating-system notification settings
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

### Semantic colors and sender names

The original `[colors]` table remains required and unchanged. A theme file may
also include `[roles]` and `[senders]`. These belong in the theme TOML beside
`[colors]`, **not** in `config.toml`. Every semantic role is optional:

| Role | Used for |
| --- | --- |
| `error` | Inline error text |
| `error_surface` | Error-banner background |
| `error_ink` | Text on an error banner |
| `success` | Inline success text |
| `success_surface` | Success-banner background |
| `success_ink` | Text on a success banner |
| `selection` | Selected navigation, message/search surfaces and input text selections |
| `selection_ink` | Text and icons on selected surfaces; selected input text |
| `focus` | Keyboard-focus indicators |
| `link` | Rich-message links |
| `unread` | Unread-count badge backgrounds |
| `unread_ink` | Unread-count badge text |
| `destructive` | Destructive-action backgrounds and logout text |
| `destructive_ink` | Their text |
| `voice` | Voice-room icons and labels |
| `timestamp` | Clock and delivery-status text in every timeline header, including the compact narrow header, and in thread replies; omitted keeps the muted color |
| `sender` | Sender names while colored sender names are off; omitted keeps ink |
| `message` | Message body text; omitted keeps ink |
| `own_message` | Name and body text of your own messages in the timeline, the thread root and thread replies; omitted falls back to `sender`, `message`, then ink |
| `own_surface` | Background of your own messages (timeline rows, the thread root and thread replies) when the row is neither selected nor hovered; omitted keeps the previous row background |
| `scrim` | Backdrop behind dialogs and the image viewer, usually translucent; omitted keeps the built-in dark backdrop |
| `shadow` | Shadow beneath popups, usually translucent; omitted keeps `#00000044` |
| `media_backdrop` | Background behind an opened image; omitted keeps `#11131B` |
| `presence_online` | The online dot beside member names; omitted keeps `#64C9A7` |
| `avatar_ink` | Initials on avatar backgrounds, whose colors stay identity colors; omitted keeps `#FFFFFF` |

Every built-in preset theme sets every role explicitly.

For example, keep the `[colors]` table from the dark Nocturne theme and set
these optional sections in that theme file:

```toml
[roles]
error = "#F0A9AF"
error_surface = "#38272D"
error_ink = "#FFDDE0"
success = "#9FD5BD"
success_surface = "#20382E"
success_ink = "#D5F4E2"
selection = "#3C4657"
selection_ink = "#F0F4FA"
focus = "#C7B4FF"
link = "#AFCDFF"
unread = "#B7A4EA"
unread_ink = "#201733"
destructive = "#F1A1AC"
destructive_ink = "#39141F"
voice = "#9DD8CD"
timestamp = "#A5AFBE"
sender = "#E8EDF2"
message = "#E8EDF2"
own_message = "#F3F6FA"
own_surface = "#20283A"
scrim = "#000000AA"
shadow = "#00000044"
media_backdrop = "#111216"
presence_online = "#9FD5BD"
avatar_ink = "#FFFFFF"

[senders]
colors = ["#C2AEFA", "#9DD8CD", "#F0C28F", "#ACCEFF", "#E7AED1", "#C7D59B"]
```

All colors must be `#RRGGBB` or `#RRGGBBAA` (alpha last); names and malformed
strings are rejected. See [the appearance contract](APPEARANCE.md) for the hex
convention, sparse `[colors]` overrides in `config.toml` and window
transparency. `senders.colors` accepts **1–16 colors**. An empty list is invalid;
omit it to restore the previous sender colors. Repeated colors and a one-color
palette are allowed. Sender colors apply when **Settings → Appearance → Colored
sender names** is on (`chat.color_senders = true`). It takes precedence over the
`sender` and `own_message` roles for name text. Selection colors take precedence
over every message role on a selected row, and hover keeps its surface over
`own_surface`. Own-message styling follows the account that sent the event, not
a display name; an opened thread carries that ownership from the root message.
Loading a theme replaces the whole role set, so a role the new file omits falls
back to ink, muted or the previous row background as if it were never set.
This palette changes name text in timeline messages, thread roots and thread
replies, independently of avatar backgrounds, account badges and Space identity
colors. Search result authors retain their existing text colors, with selection
colors when applicable. Users choose their own contrast; syntactically valid
low-contrast colors are not silently replaced. The three shipped presets include
semantic roles and sender text palettes with source contrast checks.

Old eleven-token themes keep their previous per-control fallbacks. Omitting a
role restores that control's existing color; it does not borrow the value from
the previously selected theme. Editing colors or removing optional fields
reloads the active palette. Invalid edits retain the last valid appearance and
show an error. Any token can also be overridden in the `[colors]` table of `config.toml`,
which the Settings color editor writes; sender palettes are edited in the theme
file.

To reset custom colors, choose a **Base palette** in Settings; this clears the
custom theme selection and preserves fonts and geometry. To reset an embedded
theme override, remove its file from the themes directory; Morpheus then uses
that embedded theme. Choosing a **Starting point** also resets the documented
font/layout/geometry fields, so use that when resetting the whole style.

The new roles cover their documented controls. They do not recolor images,
video, avatar backgrounds, modal scrims or theme-preview swatches. Native
preview renders are recorded in the screenshot gallery. Live-session interaction,
file-watcher edge cases and accessibility checks remain required.

Morpheus rejects an unknown schema version instead of silently discarding
settings. New versions read existing schema 1 configuration and theme files
with defaults for the new tables; older app versions may reject files
containing those tables.

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
outside this change. Native compilation and automated guards passed on all four
targets at `a38fe43`. Selector interaction, permissions, duplicate-name hardware,
unplug/rejoin and actual routing acceptance on all supported desktops remain
pending.

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
does not select a protocol. Native compilation passed on all four targets at
`a38fe43`; live UI interaction and production Element Call acceptance for the
Matrix 2.0 choice remain pending. The controlled peer fixture has a narrower
scope; see
[the exact peer assertions and limits](ELEMENT_CALL_ACCEPTANCE.md).
