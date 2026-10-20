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

### Style examples

[`examples/appearance/styles`](../examples/appearance/styles) holds four
sparse configurations that combine the settings on this page. Each one uses
only registry keys, needs no extra theme file, changes no shortcuts or account
entries, and is checked by the configuration tests. Merge a file's keys into
your `config.toml` rather than replacing it, then adjust from Settings.

| Example | Character | Settings it combines |
| --- | --- | --- |
| [Harbor](../examples/appearance/styles/harbor.toml) | Roomy, cool slate, soft | Spacious density, large radii and padding, circular 40 px avatars, composer and navigation states, sender palette, 140/180 ms fades |
| [Ledger](../examples/appearance/styles/ledger.toml) | Dense, square, light | Paper palette, compact density, small type, 2 px radii, 24 px avatars, columns, 24-hour clock, text sidebar with direct messages first |
| [Lantern](../examples/appearance/styles/lantern.toml) | Warm, reading-focused | Amber palette, serif messages at 17 px, larger sender and timestamp text, broad inset, icon and selection states |
| [Beacon](../examples/appearance/styles/beacon.toml) | Maximum legibility | High-contrast palette, 2 px outlines, 4 px focus ring, larger icons and type, reduced motion |

Custom colors are applied as written; check contrast with your own surfaces
and text scale. Beacon keeps the built-in high-contrast colors.

### Theme studio

Look changes made in Settings are a draft: they show at once and are saved
only by **Apply** in the bar at the top, with Undo, Redo and Discard, a
per-section reset and an explicit choice when the settings file changed the
same setting meanwhile. Text size, Reduce motion, the focus ring and behavior
settings still save at once. Five variations of the starting points (Nocturne
Compact, Nocturne Large Print, Terminal Soft, Paper Wide, Contrast Reading)
can be tried the same way. See [theme studio](THEME_STUDIO.md).

### Style profiles

**Settings → Appearance → Style profiles** saves the look you see under a
name, previews a saved look without changing anything until you apply it, and
imports or exports looks as small TOML files. A profile holds appearance
only; text size, Reduce motion, the focus ring, notifications, shortcuts,
devices and accounts are never saved in it or changed by it. See
[style profiles](STYLE_PROFILES.md) for the file format and limits.

### Live preview and finding a setting

The top of **Settings → Appearance** shows a live preview: sample rooms,
messages, a focused composer, an account and a call tile, buttons in their
ordinary, hovered, pressed, focused, disabled and primary states, and the
error, success, selection, unread, mention and link colors. It uses the same
controls and color rules as the real views, so a theme, font, size, density,
state color or previewed style profile shows there as soon as it applies. The
samples are invented and inert: they take no focus or clicks and never reach
an account, the network, a call or a camera. **Hide preview** folds it away
for this session only; it is not saved. **Pin above settings** keeps a compact
copy under the search field while you edit anything further down; **Unpin**
removes it. Its height stays fixed, so the
settings below do not move while you edit; the samples scroll inside it and
rearrange at narrow widths and large text. **Edit colors**, **Edit sizes and
shapes** and **Edit base palette** below it jump to the detailed editors.

Settings search (`Mod+Shift+F`) finds sections, every color token and every
size or shape metric. Results show the `config.toml` keys they edit, such as
`colors.link`, `typography.sender_size` or, for Fonts, the three font keys;
typing a key as shown, or a part of a metric key such as `sender_size`, finds
its field or section. Choosing a result focuses that exact field;
**Back to results**, Down in the search field or `Mod+Shift+F` brings the
same results back. Each color and metric editor has its own Reset and Undo.

Pinned messages, message search, polls and the call stage follow the same
corner, outline, focus ring and control state settings as the rest of the
window, and the control padding, field padding and icon size where they have
those controls. Dropdowns (sticker packs, call audio devices, the room join
rule) and the window transparency slider are drawn the same way, with the
usual keys: Up and Down choose, Return or Space opens a list, Escape closes
it; Left, Right, Home and End move the slider.

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
Favorites and Low priority sections come from the account's Matrix room tags
and keep their tag order; unread-first sorting does not reorder them. See
[room organization](ROOM_ORGANIZATION.md).

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

## Pane order and density

**Settings → Layout** also arranges the panes without hiding navigation:

```toml
[layout]
details_side = "left"          # "right" (default) or "left" of the conversation
direct_messages_first = true   # default false; grouped lists only
room_density = "compact"       # omitted follows appearance.density
message_density = "spacious"   # omitted follows appearance.density
```

The details, members, thread and search panel can sit on either side of the
conversation. The rail, room list and account control stay where they are, and
Tab and screen-reader order are unchanged. With grouped lists, direct messages
can be shown above rooms; next/previous unread shortcuts and the quick switcher
follow that order. Room-list and message density can each override the global
density: compact halves message padding and spacious uses about 1.33 times it;
room rows use the compact, comfortable or spacious row heights. No option can
hide every navigation surface or the account control, and the protected
account switch, logout and Manage accounts actions keep their existing
confirmations. Starting points keep these choices.

### Pane arrangement

```toml
[layout]
pane_order = ["spaces", "conversation", "details", "rooms"]  # each pane once; [] follows details_side
hidden_panes = ["spaces"]        # any of spaces, rooms, details; never conversation
collapsed_panes = ["details"]    # rooms or details: a narrow strip with a restore control
```

The four pane IDs are stable: `spaces` (the icon rail), `rooms` (the room
list), `conversation` (conversation, call stage, settings and search) and
`details` (members, threads and files). An empty `pane_order` keeps the
classic order with the details panel on `details_side`. Settings › Layout ›
Pane arrangement offers Standard, Focused, Conversation first and Minimal
presets, per-pane move, hide and collapse, Reset order, Show all panes and
Restore default layout.

The active account's control always stays at the bottom left: over the room
list when it comes first at full width, otherwise in its own narrow column,
which also offers Restore whenever the arrangement is customized. A window
narrower than the saved panes gives way details first (to a strip, then away),
then the room list, then the Space rail; the conversation keeps at least
360 px. Panes are moved, never rebuilt, so timelines, drafts, scroll position
and focus survive any change. Dialogs, call indicators and security, recovery
and password screens are not part of any pane and are never hidden. An invalid
edit is rejected and the last valid arrangement stays in effect.

Opening a thread, members, search, pins or transfers explicitly reveals that
panel for the current view even when the saved layout hides or collapses it.
At narrow widths, navigation panes temporarily give way so the requested panel
and conversation remain usable. Closing the panel, changing accounts or
applying a configuration ends the temporary reveal; the TOML file is unchanged.

## Keyboard shortcuts

**Settings → Keyboard shortcuts** lists every action with its combination and
a filter by action, group or combination. Activate a combination to record the
next one you press. Escape or Tab cancels and returns focus to that row, and
clicking elsewhere also cancels, so focus is never trapped and shortcuts never
stay paused. A combination can also be typed, for example `Mod+Shift+K`.
Each change saves at once. A combination already used by another action is not
saved, and the message names that action. **Default**, **Disable**, **Undo**
and **Restore all defaults** are available; Undo never overwrites a change made
elsewhere.

| Action (`[shortcuts]` key) | Default |
| --- | --- |
| Switch room (`quick_switcher`) | `Mod+K` |
| Search current room (`search`) | `Mod+F` |
| Next / previous unread room (`next_unread`, `previous_unread`) | `Alt+Down`, `Alt+Up` |
| Next / previous room (`next_room`, `previous_room`) | `Alt+Shift+Down`, `Alt+Shift+Up` |
| Go to Home (`go_home`) | `Mod+1` |
| Open Settings (`open_settings`) | `Mod+Shift+S` |
| Search Settings (`search_settings`) | `Mod+Shift+F` |
| Focus the message composer (`focus_composer`) | `Mod+L` |
| Open the emoji picker (`open_emoji_picker`) | `Mod+E` |
| Show or hide the details panel (`toggle_details_panel`) | `Mod+Shift+D` |
| Show members (`show_members_panel`) | `Mod+Shift+P` |
| Show the open thread (`show_thread_panel`) | `Mod+Shift+T` |
| Open the account menu (`open_account_menu`) | `Mod+Shift+U` |

```toml
# An excerpt: a [shortcuts] table lists every action you keep.
[shortcuts]
quick_switcher = "Mod+K"
toggle_details_panel = "Ctrl+Shift+D"
next_room = "Alt+PageDown"
```

`Mod` is Command on macOS and Ctrl elsewhere; recorded combinations are saved
with `Mod` so the file works on every platform, and a conflict on either
platform is rejected. Combinations need Mod, Ctrl, Meta or Alt with A–Z, 0–9, an
arrow, Home, End, PageUp, PageDown or F1–F12; editing keys (`Mod+A`, `C`, `V`,
`X`, `Y`, `Z`), window keys (`Q`, `W`, `H`, `M`, `Alt+F4`), Alt with a letter or
digit, and Shift with a digit are reserved. An existing `[shortcuts]` table is
used exactly as written: an action it omits stays disabled, so an older file does
not gain new bindings until you choose **Default** or **Restore all defaults**.
Unknown actions and malformed combinations reject the edit and keep the last
valid configuration.

Shortcuts pause in Settings, while recording and in dialogs, and keys a focused
text field uses keep priority. Room cycling opens conversations only and never
joins a voice space. No action can send, remove an account, log out, switch
accounts or change a protected setting; the account menu shortcut only opens the
menu, whose entries keep their own confirmations. Shortcuts cannot run commands
or scripts.

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

The header lays out from the width the conversation pane actually has, beside
the details panel and the other panes. When not every action fits (a small
window, large text, a large `icon_size` or an open details panel), the room
name keeps a readable width, the topic gives way first, and the lower-priority
actions move into one **More room actions** menu at the end of the header, in
their header order. Room organization and Search always stay in the header,
and **Leave this room…** stays while the room name keeps its shortest readable
width beside it. Each menu item is the same action, with the same label, state
and availability; an unavailable one is listed greyed out. The menu closes
before an action runs. Up, Down, Home and End move through the available
items, Tab stays in the menu, and Escape closes it and returns focus to More.
More does not open while another panel or dialog owns the window.

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
| `typography.message_font_family` | Omitted | Message family, and the composers' unless set below; omission follows the interface font |
| `typography.composer_font_family` | Omitted | Message and thread composer family; omission follows the message family |
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
leave the message family blank to inherit the interface font, or the composer
family blank to follow the message font, and choose **Apply fonts**. In TOML,
omit `message_font_family` or `composer_font_family` to inherit; an
explicitly empty string is invalid. Fonts are the same for every account and
room: account and room levels hold theme and colors only.

**Browse installed fonts** (Interface, Messages, Composer, Monospaced) opens a
list for that role inside the Fonts card:
- It lists the families of the font collection the window draws with, read
  on a background thread. No folder is walked and nothing is downloaded.
- `sans-serif`, `serif` and `monospace` are listed first, with the family
  each resolves to now.
- Search matches every typed word. **Monospaced only** keeps the families
  the system lists as monospace and those named "… Mono". That is not a
  measurement of glyph widths.
- The list holds at most 4096 families and shows at most 200 matches, and it
  says how many it left out.
- Selecting a family shows a sample at the role's own size and weight, and
  says when the family has no font of that weight (the renderer then uses
  the nearest one). Nothing is written until **Use**.
- **Use** writes the role's key through the normal Settings path. On the
  Settings page the theme studio drafts it (shown at once, saved by
  **Apply** in the bar). **Follow the inherited font** removes the message
  or composer value; **Use the built-in font** returns the interface or
  monospaced font to its default.
- **Refresh cached list** reads the font families already known to the app,
  including application fonts. The toolkit keeps a snapshot of system fonts:
  restart Morpheus after installing or removing them. A family typed by hand
  that is absent from the snapshot is kept as typed and shown as not found;
  the renderer uses a fallback.
- The list closes when the account changes (including switching away and
  back) or is detached, and a choice made after such a change is refused.

Rich message bodies are drawn as styled text, which has no line spacing
setting in the toolkit; `typography.line_height_percent` applies to the
composer and plain-text previews only.

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
original eleven color tokens; see [Theme files](#theme-files). Default body
weight reaches rich messages through the window's font inheritance. Rich-message
line spacing and background images remain future work. Pane order, visibility
and collapse are covered by [Pane arrangement](#pane-arrangement). Theme packs
cannot execute code.

### Typography and geometry details

**Settings → Appearance → Typography and geometry details** edits one size at a
time. Its categories are Text sizes; Corners, borders and focus; Padding; and
Icons and avatars. A filter and Settings search reach each metric directly, for
example “focus ring”, “avatar roundness” or “composer padding”. The steppers
apply at once; a typed whole number applies with Enter, Escape restores the
field, and an out-of-range number only shows a hint. Undo, Reset to default /
Use built-in and Reset all work like the color editor: each change is decided
on the re-read file, and Undo never overwrites a later edit made elsewhere.

Every value is a whole number of logical pixels before text scaling unless the
unit says percent. An omitted optional field keeps each surface's built-in
value, so removing it from the file restores the previous look.

| Field | Default | Range | Affects |
| --- | --- | --- | --- |
| `typography.sidebar_size` | `13` | 10–24 | Room-list rows, text-sidebar routes and the Space heading |
| `typography.sender_size` | Omitted | 10–32 | Timeline sender names in every layout; omitted follows each layout's message-based size |
| `typography.timestamp_size` | Omitted | 8–20 | Timeline clocks and thread-reply clocks; omitted keeps 10 |
| `typography.composer_size` | Omitted | 10–32 | Message and thread composer text and the composer placeholder; omitted follows `message_size` |
| `typography.line_height_percent` | Omitted | 100–200 | Line spacing of the message and thread composers, in percent of the font's natural line height; omitted keeps it natural. Message bodies keep their natural spacing: they are styled text, which has no line spacing setting in this toolkit version |
| `typography.body_weight` | Omitted | 300–800 | Default weight for rich message bodies, composers and other text without an explicit weight; omitted keeps platform defaults. Heading, sender and Markdown span weights remain explicit |
| `geometry.control_radius` | Omitted | 0–24 | Text buttons and icon buttons; omitted keeps their share of `corner_radius` |
| `geometry.input_radius` | Omitted | 0–24 | The text fields listed for `input_padding`; omitted keeps each field's corners |
| `geometry.composer_radius` | Omitted | 0–32 | Message and thread composers |
| `geometry.border_width` | `1` | 0–4 | Text buttons, text fields, the message composer, attachment cards and reactions |
| `geometry.focus_width` | `2` | 1–6 | Keyboard focus rings of buttons, actions and text fields |
| `geometry.control_padding` | `12` | 4–24 | Horizontal padding inside text buttons, scaled with text |
| `geometry.input_padding` | `10` | 4–20 | Text fields in Settings, discovery, the quick switcher, highlight words and recovery |
| `geometry.composer_padding` | `10` | 4–24 | Horizontal padding inside both composers |
| `geometry.icon_size` | `18` | 12–28 | Icons in icon buttons |
| `geometry.avatar_size` | Omitted | 16–64 | Timeline avatars; omitted keeps 38 in Stacked and 26 in Compact and Columns |
| `geometry.avatar_roundness` | Omitted | 0–50 % | Every avatar, as a share of its size; 50 draws a circle |

Icon buttons at their default size grow to fit a larger icon (34 logical pixels
up to 44 at `icon_size = 28`). Room headers, both composers, attachment cards,
Settings number rows and picker search rows grow by the same amount, so a large
icon never clips. Buttons with a smaller fixed size, such as the message
actions, keep their size and draw the icon as large as fits. A narrow timeline
row reserves room for the tallest of the sender name, message text and shown
clock before placing the message actions below them.

The composer's attachment queue and the image viewer follow the same
settings: corner radius and control radius, focus width, control padding,
control hover, pressed and disabled states and the error and focus colors.
Viewer arrows, zoom symbols and the placeholder follow `icon_size`, and queue
rows gain an outline in high contrast. Their buttons and the queue header
grow with large text instead of clipping.

High contrast draws borders at least one pixel wide even when `border_width` is
`0`; other palettes respect `0`. Starting points reset these metrics to their
defaults like other typography and geometry, except `focus_width`, which they
keep as an accessibility choice. Rich message bodies inherit the default body
weight from the window. Their line spacing remains the toolkit default;
`line_height_percent` controls composer and plain-preview spacing.

```toml
[typography]
sidebar_size = 14
timestamp_size = 11

[geometry]
focus_width = 3
border_width = 2
icon_size = 22
avatar_roundness = 50
```

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

Two further decorative fades are off by default and are set separately, in
**Typography, geometry and motion → Motion** or in TOML:

```toml
[appearance]
popup_fade_ms = 120  # account menu, mention/emoji/sticker pickers, quick switcher
panel_fade_ms = 160  # details panel when it opens
```

Each accepts 0–1,000 ms. They only fade content in; they never delay focus,
input or layout, and selection, focus outlines and content change immediately.
Reduce motion makes every fade immediate without changing the saved values.

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

## Per-account appearance

Each saved account can show its own theme mode and its own colors. The
overrides are TOML preferences keyed by the local account ID, the same opaque
key the notification and call-mode overrides use; they are never looked up by
display name or Matrix ID:

```toml
[appearance]
theme = "dark"

[colors]
accent = "#9CD7CD"

[account_overrides."<local account ID>"]
theme = "light"

[account_overrides."<local account ID>".colors]
accent = "#B7A4EA"
canvas = "#F4F1FA"
```

An account's colors are drawn over the global `[colors]` table, which is drawn
over the theme pack and the built-in palette; any registry token can be set.
In **Settings → Appearance → Colors and window**, choose **This account** to
edit the active account's colors and **All accounts** for the global table.
With **All accounts**, a color the active account sets for itself shows the
global value when `config.toml` holds one, and the source line says the
account uses its own color. Undo and Reset all never cross accounts: switching accounts drops their pending
steps, and with no active account the editor returns to **All accounts**.
Removing or logging out of an account removes that account's theme and colors
and leaves every other account and the global preferences untouched.

The window shows an account's `theme` and colors only after that account has been
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
fonts, geometry, sender palettes and layout remain global.
Invalid edits keep the last valid configuration, including each account's
last valid theme, until the file is valid again. The override is a local
presentation preference; it is not written to Matrix.

## Per-room appearance

A room can have its own theme mode and colors in one account. Precedence is
global -> account -> room: while the room is open, its values draw over the
account's, which draw over the global ones. A value a level omits inherits
from the level below; there is no other merging.

```toml
[account_overrides."<local account ID>".room_appearance."!room:example.org"]
theme = "light"

[account_overrides."<local account ID>".room_appearance."!room:example.org".colors]
accent = "#C46A3F"
```

- Keys are the local account ID, then the Matrix room ID (quoted, because
  room IDs contain `!` and `:`). Invalid room IDs, unknown color tokens and
  any other key are refused. Error messages never repeat an account or room
  ID.
- Each account holds at most 64 rooms with their own appearance. A room that
  sets nothing is not stored.
- In **Settings → Appearance → Colors and window**, choose **This room**
  (available while an account is active and a room is open) to edit the open
  room's colors. Reset removes only that room's color, so it follows the
  account again; Reset all removes all of that room's colors.
- The editor stays on the room it shows. If the open room changes before the
  editor has shown the new one, the edit is refused and nothing is written;
  check the color and try again. With **This room** chosen and no room open,
  nothing is edited.
- A room's theme mode is set in `config.toml`. The theme picker notice names
  a room override while that room is open.
- Room settings are drafted by the theme studio like any look setting, and
  belong to their account: switching accounts drops another account's
  drafted room colors.
- Removing or logging out of an account removes its rooms' appearance with
  the rest of its appearance. The global and account levels keep working
  with no account active (offline settings).
- Files without `room_appearance` load unchanged. Older versions of the app
  refuse a file that uses it, as with any newer key.

## Notifications

The Notifications section edits desktop notifications, sounds, message previews,
room and sender names in banners, local highlight words or phrases, a switch
for the visible account only, the account's default levels, Matrix keywords
and searchable per-room levels. Each highlight is trimmed, repeated
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

The bell in the room header opens one room's panel: its level, and three
switches for this device that can only make that room quieter or more
private. They are stored per account and room, separately from any room
appearance:

```toml
[account_overrides."<local account ID>".room_notifications."!room:example.org"]
quiet = true          # no sound for this room
hide_preview = true   # never preview its text
no_highlights = true  # local highlight words do not mark it
```

A room appears only while one of its switches is on, at most 256 rooms per
account; unknown keys and invalid room IDs are rejected like any other field.
## Typing and read receipts

**Settings › Typing and read receipts** (search for "typing" or "read
receipts") decides what other people learn about your activity. Morpheus shares
none of it until you turn it on:

```toml
[activity]
send_typing = false           # tell a room while you are typing
public_read_receipts = false  # others see how far you have read (m.read)
read_tracking = false         # move your read position as you view messages
```

With public read receipts off, read tracking and **Mark as read** (in a room's
menu) still move your own read position, privately (`m.read.private` and the
fully-read marker), so unread counts match on your other devices. These are
global settings; there are no per-room or per-account overrides. See
[ROOM_ACTIVITY.md](ROOM_ACTIVITY.md).

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

### Interaction states

A theme file may also include an optional `[states]` table, and every state can
be overridden in `[colors]` of `config.toml` and in **Settings → Appearance →
Colors and window** (categories *Inputs and controls* and *Navigation and
badges*). An unset state follows the color named below, or keeps each
control's own built-in color, so themes without the table and the three shipped
presets look exactly as before.

| State | Used for | When unset |
| --- | --- | --- |
| `message_hover` | Timeline message under the pointer | Follows `elevated` |
| `control_hover` | Hover of text buttons and icon buttons | Each button's hover color |
| `control_pressed` | Layer while a button, room, route or Space is pressed | No pressed layer |
| `control_disabled` | Text and icons of unavailable buttons | Each button's disabled look |
| `input_surface` | Text fields in Settings, discovery, the quick switcher, highlight words and recovery | Each field's background |
| `input_border` | Those fields' borders without keyboard focus | Follows `divider` |
| `composer_surface` | Message and thread composers | Follows `elevated` |
| `composer_border` | Message composer border | Follows `divider` |
| `composer_focus` | Composer border while typing; setting it also outlines the focused thread composer | Follows `composer_border` |
| `icon` | Quiet icon buttons: message actions, composer tools, room header actions | Follows `muted` |
| `icon_active` | Primary or switched-on icons such as Send and open pickers | Follows `accent` |
| `icon_navigation` | Route icons of the navigation rail and sidebar (Home, Explore, Calls, Settings, create or join) | Follows `icon` |
| `icon_header` | Conversation header actions (room settings, search, pinned messages, call, details panel) | Follows `icon` |
| `icon_composer` | Composer tools in the message and thread composers (attach, emoji, stickers, formatting, cancel reply) | Follows `icon` |
| `icon_message` | Quick actions on a message (reply, edit, react, thread, redact) | Follows `icon` |
| `nav_hover` | Room, direct message, route and Space rows under the pointer | Each row's hover color |
| `nav_selected` | The open room, route or Space | Follows `selection` |
| `nav_selected_ink` | Its text and icons | Follows `selection_ink` |
| `mention` | Badge of rooms with unread messages that mention you | Follows `destructive` |
| `mention_ink` | Number on the mention badge | Follows `destructive_ink` |

Rooms with unread mentions or highlights show a separate `@` badge with the
mention count instead of the plain unread count; the count comes from the
Matrix client's read receipts for the active account. Screen readers hear both
counts in the row description. Badges widen for long counts and large text.

### Sender name colors

**Settings → Appearance → Sender name colors** edits a palette stored in
`config.toml`; it replaces the theme pack's `[senders]` palette and survives
choosing a starting point:

```toml
[chat]
color_senders = true

[senders]
colors = ["#C2AEFA", "#9DD8CD", "#F0C28F", "#ACCEFF"]
```

Each row shows a sample name on the conversation background with its hex value.
A complete `#RRGGBB` or `#RRGGBBAA` value applies at once and joins the
[theme studio](THEME_STUDIO.md) draft until Apply; partial input only shows
a hint. Colors can be moved earlier or later, removed (one always remains) or
added up to 16. Undo restores the previous palette only while the file still
holds the palette you saved, and **Use theme colors** removes your palette.
Colors with less than 4.5:1 contrast against the conversation background are
marked *Low contrast*. Contrast is measured as the color is drawn: its alpha is
blended over the background first, so a mostly transparent color is reported
even when its opaque form would be readable. Colors are never changed for you,
including their alpha and in the high contrast palette. Each sender keeps the
slot derived from their Matrix user ID, so a name has the same color in every
room and account; nothing about senders is stored. With no palette at all,
names use each sender's identity color, and the first edit starts from those
colors.

Old eleven-token themes keep their previous per-control fallbacks. Omitting a
role restores that control's existing color; it does not borrow the value from
the previously selected theme. Editing colors or removing optional fields
reloads the active palette. Invalid edits retain the last valid appearance and
show an error. Any token can also be overridden in the `[colors]` table of `config.toml`,
which the Settings color editor writes; a `[senders]` palette in `config.toml`
replaces the theme's sender palette (see [Sender name colors](#sender-name-colors)).

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

Saved choices apply to **new calls**. Choosing in a list, including arrow-key
and wheel steps, and external TOML edits never reroute an active call. During
a call, **Use for this call** switches that call to the saved choices without
leaving. While local audio is running, Morpheus opens each new device and
swaps it in only once it is open, keeping the published microphone track, its
encryption, and your mute and deafen settings; call audio pauses in both
directions while a device opens (up to 30 seconds). If the chosen microphone
or speaker cannot be opened, the one in use stays and the call page says so.
If local audio had stopped (for example, a chosen device was missing when the
call started), **Use for this call** starts it again inside the same call with
a new microphone track, keeping your mute and deafen settings. Morpheus
resolves the exact selected ID again at the start of each call and on every
switch. At call start a missing or ambiguous ID produces an audio error; on a
switch it leaves the device in use and the call page says so. Neither picks a
different device. An unavailable saved choice remains visible until you
explicitly change it. The call can remain joined with local audio unavailable;
choose another device and **Use for this call** to retry without rejoining.

The call page names the microphone and speaker in use and the last change to
them. If the microphone is disconnected, nothing is sent from it and no
different microphone is opened for you: Morpheus reopens only the chosen
microphone by its exact device ID (or, when System default is chosen, the
exact microphone that was lost). If the speaker is disconnected, call audio
plays through the system default output, the call page says so, and Morpheus
moves back to the chosen speaker when it returns. Returning devices are
detected by checking every 3 seconds for up to 10 minutes after the last
change; a check that could not finish within that time is not started, so
Morpheus stops looking by then and says so. Each check pauses call audio
while it opens a device. A device saved only by a name or an ambiguous alias
is never reopened on its own. A microphone reopened for System default stays
on that device even if the system default changes later. A request that
changes only the speaker never opens a microphone in place of a disconnected
one; choosing Use for this call with nothing changed from the devices the call
is using counts as asking for the microphone again.

When the call itself reconnects, Morpheus picks up where it left off. It
reopens the microphone it was using only by that device's exact ID; if the
platform gave none (for example an ALSA alias), the microphone is not reopened
by its route, because System default or an alias may now be a different
microphone, and the call page says so until you choose a microphone and Use
for this call. The speaker reopens on its route (System default follows the
operating system). Morpheus keeps waiting for a disconnected microphone
without opening another, keeps a stand-in speaker until the chosen one
returns, and keeps either side working if the other cannot be opened. It then
switches to your choices again where they differ, saying so if they still
cannot be opened. This holds even when the connection drops during the very
first join, before its microphone was published: once that join opened a
microphone, the reconnect reopens it by the exact ID recorded then, even if
you had chosen System default rather than that device; if no exact ID was
ever recorded (the join was lost before any microphone opened, or the
platform gave none), a reconnect reopens a microphone only if you chose it by
its exact device, and a System default or alias choice stays closed until you
choose again. A reconnect never opens System default or an alias afresh. If a
reconnect interrupts Use for this call, the notice asks you to
check the devices and choose again rather than promising the switch. Mute,
deafen and the camera are never changed by a switch or a reconnect.

Device discovery runs off the UI thread and times out after 20 seconds. A
driver query still blocked in the operating system prevents another query
until it returns; late results cannot replace a newer list. Discovery uses the
platform default backend (CoreAudio, WASAPI, or the configured Linux ALSA
backend), not every optional CPAL backend. Audio still requires the existing
48 kHz mono/stereo PCM support. Operating-system default and virtual endpoints
can themselves reroute audio. Camera selection and extra noise-processing
controls are outside this change. Native compilation and automated guards
passed on all four targets at `a38fe43`, before in-call switching and
disconnect recovery were added; those are not yet compiled or run. Selector
interaction, permissions, duplicate-name hardware, physical unplug and replug,
in-call switching and actual routing acceptance on all supported desktops
remain pending.

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
